//! Offline fixture playback through the production HUD model and software painter.

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use ab_glyph::FontRef;
use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use eframe::egui;

use super::{
    layout_height, present_outcome, preview_snapshot, screenshot_model, Canvas, Model, RenderKey,
    ScreenshotState, Visibility, CONTAINER_WIDTH, FRAME_INTERVAL, NOTICE_HOLD, RESULT_FADE, SETTLE,
    SURFACE_WIDTH,
};
use crate::{
    ipc::{StateKind, StatusSnapshot},
    pipeline::Stage,
    settings::{apply_theme, color},
    theme::{self, Palette},
};

const SCREENSHOT_DELAY_FRAMES: u32 = 6;
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(5);

type CaptureResult = Rc<RefCell<Option<Result<()>>>>;

#[derive(Clone, Copy)]
enum Journey {
    Measured,
    Raw,
    Cancellation,
    Interruption,
    Failure,
    Uncertain,
    Deferred,
    Reconnect,
    Restart,
}

impl Journey {
    const ALL: [Self; 9] = [
        Self::Measured,
        Self::Raw,
        Self::Cancellation,
        Self::Interruption,
        Self::Failure,
        Self::Uncertain,
        Self::Deferred,
        Self::Reconnect,
        Self::Restart,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Measured => "Measured dictation",
            Self::Raw => "Single chunk, raw text",
            Self::Cancellation => "Cancel during transcription",
            Self::Interruption => "Next take interrupts success",
            Self::Failure => "Failure and dismissal",
            Self::Uncertain => "Uncertain delivery",
            Self::Deferred => "Deferred delivery",
            Self::Reconnect => "Disconnect and reconnect",
            Self::Restart => "Daemon epoch changes",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Measured => "Recorded PCM fixture, a speech gap, reported chunk completions, cleanup, delivery, success hold, fade, and idle.",
            Self::Raw => "One backend chunk stays indeterminate. Its completion goes directly to delivery without cleanup, then success fades to idle.",
            Self::Cancellation => "A cancellation request waits for current work, then a cancelled outcome expires. No backend work runs here.",
            Self::Interruption => "A new operation arrives during the success transition. The next take owns the instrument immediately and completes independently.",
            Self::Failure => "A failed take remains actionable until its fixture outcome is dismissed. Dismissal does not remove the saved-recording metadata.",
            Self::Uncertain => "Delivery cannot be confirmed. The production outcome remains visible until the fixture dismisses it; no retry or insertion is attempted.",
            Self::Deferred => "Text was not inserted. Review the persistent recovery guidance and its dismissal without touching any real clipboard or recording.",
            Self::Reconnect => "A brief outage is coalesced, a longer one becomes visible, then the same epoch returns idle with the previous take's result unknown.",
            Self::Restart => "Recording loses its connection. An idle snapshot from a new epoch clears stale event history and presents the production restart notice.",
        }
    }
}

#[derive(Clone, Copy)]
enum Source {
    Journey(Journey),
    State(ScreenshotState),
}

struct Entry {
    source: Source,
    title: String,
}

impl Entry {
    fn catalog() -> Vec<Self> {
        let mut entries =
            Vec::with_capacity(Journey::ALL.len() + ScreenshotState::value_variants().len());
        entries.extend(Journey::ALL.into_iter().map(|journey| Self {
            source: Source::Journey(journey),
            title: journey.title().to_owned(),
        }));
        entries.extend(
            ScreenshotState::value_variants()
                .iter()
                .copied()
                .map(|state| Self {
                    source: Source::State(state),
                    title: state
                        .to_possible_value()
                        .expect("HUD fixture has a clap name")
                        .get_name()
                        .to_owned(),
                }),
        );
        entries
    }

    fn description(&self) -> &'static str {
        match self.source {
            Source::Journey(journey) => journey.description(),
            Source::State(ScreenshotState::Disconnected | ScreenshotState::Reconnected) =>
                "Connection events are replayed, not represented by an idle snapshot alone. The fixture includes the grace interval and recovery notice.",
            Source::State(ScreenshotState::Dismissed) =>
                "The failed outcome is first presented, then dismissed by a later status event. Seek backward to inspect the undismissed result.",
            Source::State(ScreenshotState::Interrupted) => Journey::Interruption.description(),
            Source::State(ScreenshotState::ReducedMotion | ScreenshotState::ReducedMotionCleaning | ScreenshotState::ReducedMotionSent) =>
                "This fixture starts with reduced motion enabled. Its full history uses the production reduced-motion transitions and outcome lifetime.",
            Source::State(ScreenshotState::Idle) =>
                "Idle is a transparent production surface. The preview boundary belongs to the gallery, not to the HUD.",
            Source::State(_) =>
                "Paused at the named production screenshot fixture. Replay starts earlier and includes its transition history, result lifetime, and return to idle.",
        }
    }

    fn reduced_motion(&self) -> bool {
        matches!(
            self.source,
            Source::State(
                ScreenshotState::ReducedMotion
                    | ScreenshotState::ReducedMotionCleaning
                    | ScreenshotState::ReducedMotionSent
            )
        )
    }
}

struct Event {
    at: Duration,
    // No snapshot is a lost connection; idle is an explicit status.
    input: Option<StatusSnapshot>,
}

struct Checkpoint {
    at: Duration,
    label: &'static str,
}

struct Timeline {
    events: Vec<Event>,
    checkpoints: Vec<Checkpoint>,
    end: u32,
    focus: u32,
}

fn milliseconds(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn frame_at(at: Duration) -> u32 {
    at.as_nanos().div_ceil(FRAME_INTERVAL.as_nanos()) as u32
}

fn fixture(state: ScreenshotState, origin: Instant) -> StatusSnapshot {
    // Only fixture payloads are borrowed. Never borrow the still's pre-aged model,
    // track targets, or presented frame: those must arise from this replay.
    screenshot_model(state, origin + Duration::from_secs(3))
        .snapshot
        .expect("HUD screenshot fixtures contain a status snapshot")
}

struct TimelineBuilder {
    origin: Instant,
    reduced_motion: bool,
    events: Vec<Event>,
    checkpoints: Vec<Checkpoint>,
}

impl TimelineBuilder {
    fn new(origin: Instant, reduced_motion: bool) -> Self {
        let mut builder = Self {
            origin,
            reduced_motion,
            events: Vec::new(),
            checkpoints: Vec::new(),
        };
        builder.status(Duration::ZERO, preview_snapshot(StateKind::Idle), "Idle");
        builder
    }

    fn mark(&mut self, at: Duration, label: &'static str) {
        self.checkpoints.push(Checkpoint { at, label });
    }

    fn status(&mut self, at: Duration, snapshot: StatusSnapshot, label: &'static str) {
        self.events.push(Event {
            at,
            input: Some(snapshot),
        });
        self.mark(at, label);
    }

    fn state(&mut self, at: Duration, state: ScreenshotState, label: &'static str) {
        self.status(at, fixture(state, self.origin), label);
    }

    fn disconnected(&mut self, at: Duration) {
        self.events.push(Event { at, input: None });
        self.mark(at, "Connection lost; grace interval");
        self.mark(at + super::DISCONNECT_DELAY, "Connection warning");
    }

    fn recording(&mut self, start: Duration, until: Duration) {
        let measured = fixture(ScreenshotState::Recording, self.origin);
        let quiet = fixture(ScreenshotState::NoSignal, self.origin).signal;
        let mut at = start;
        let mut sample = 0;
        self.mark(start, "Recording: measured PCM fixture");
        while at < until {
            let mut snapshot = measured.clone();
            snapshot.elapsed = (at - start).as_secs();
            // Repeat the existing recorded sample, with a real zero-input fixture
            // between phrases. These are input measurements, not drawn waveforms.
            if (7..10).contains(&sample) {
                snapshot.signal = quiet;
            }
            if sample == 7 {
                self.mark(at, "Measured speech gap");
            } else if sample == 10 {
                self.mark(at, "Measured input returns");
            }
            self.events.push(Event {
                at,
                input: Some(snapshot),
            });
            sample += 1;
            at += super::POLL_INTERVAL;
        }
    }

    fn chunks(&mut self, at: Duration, completed: u32, total: u32, label: &'static str) {
        let mut snapshot = fixture(ScreenshotState::Transcribing, self.origin);
        snapshot.elapsed = 0;
        snapshot.stage = Some(Stage::Transcribing { completed, total });
        self.status(at, snapshot, label);
    }

    fn outcome(
        &mut self,
        at: Duration,
        mut snapshot: StatusSnapshot,
        label: &'static str,
    ) -> Duration {
        snapshot.hud.reduced_motion = Some(self.reduced_motion);
        let outcome = snapshot
            .outcome
            .as_ref()
            .expect("terminal fixture contains an outcome");
        let visibility =
            present_outcome(outcome, &snapshot, self.origin + at, self.reduced_motion).visibility;
        self.status(at, snapshot.clone(), label);
        match visibility {
            Visibility::Persistent => {
                let dismissed_at = at + NOTICE_HOLD;
                snapshot
                    .outcome
                    .as_mut()
                    .expect("terminal fixture contains an outcome")
                    .dismissed = true;
                self.status(dismissed_at, snapshot, "Dismiss outcome; idle");
                dismissed_at
            }
            Visibility::Until(until) => {
                let expires = until.duration_since(self.origin);
                if !self.reduced_motion {
                    self.mark(at + SETTLE, "Result held");
                    self.mark(expires - RESULT_FADE, "Result fading");
                }
                self.mark(expires, "Idle: result expired");
                expires
            }
        }
    }

    fn finish(mut self, end: Duration, focus: Duration) -> Timeline {
        self.events.sort_by_key(|event| event.at);
        self.checkpoints.sort_by_key(|checkpoint| checkpoint.at);
        Timeline {
            events: self.events,
            checkpoints: self.checkpoints,
            end: frame_at(end),
            focus: frame_at(focus),
        }
    }
}

fn dictation(origin: Instant, reduced_motion: bool, raw: bool) -> Timeline {
    let mut builder = TimelineBuilder::new(origin, reduced_motion);
    builder.recording(milliseconds(320), milliseconds(2240));
    builder.state(
        milliseconds(2240),
        ScreenshotState::FinalizingAudio,
        "Finalize audio",
    );
    let result_at = if raw {
        builder.chunks(milliseconds(2720), 0, 1, "Single chunk running (0/1)");
        builder.chunks(
            milliseconds(4320),
            1,
            1,
            "Single chunk reported complete (1/1)",
        );
        builder.state(
            milliseconds(4640),
            ScreenshotState::Delivering,
            "Deliver raw text; no cleanup",
        );
        milliseconds(5040)
    } else {
        const CHUNKS: [&str; 7] = [
            "Reported chunks: 0/6",
            "Reported chunks: 1/6",
            "Reported chunks: 2/6",
            "Reported chunks: 3/6",
            "Reported chunks: 4/6",
            "Reported chunks: 5/6",
            "Reported chunks: 6/6",
        ];
        for (completed, label) in CHUNKS.into_iter().enumerate() {
            builder.chunks(
                milliseconds(2720 + completed as u64 * 640),
                completed as u32,
                6,
                label,
            );
        }
        builder.state(milliseconds(6960), ScreenshotState::Cleaning, "Cleanup");
        builder.state(
            milliseconds(8640),
            ScreenshotState::Delivering,
            "Deliver text",
        );
        milliseconds(9040)
    };
    let mut sent = fixture(ScreenshotState::Sent, origin);
    if !raw {
        sent.outcome
            .as_mut()
            .expect("success fixture contains an outcome")
            .cleanup = crate::ipc::Cleanup::Applied;
    }
    let end = builder.outcome(result_at, sent, "Success");
    builder.finish(
        end + milliseconds(960),
        milliseconds(if raw { 3360 } else { 4960 }),
    )
}

fn connection(origin: Instant, reduced_motion: bool, restart: bool, focus_lost: bool) -> Timeline {
    let mut builder = TimelineBuilder::new(origin, reduced_motion);
    builder.recording(milliseconds(320), milliseconds(1280));
    if !restart {
        // The first outage ends before the production disconnect delay. It must
        // not become a warning merely because a later seek skips its interval.
        builder.events.push(Event {
            at: milliseconds(1280),
            input: None,
        });
        builder.mark(milliseconds(1280), "Brief connection loss");
        builder.state(
            milliseconds(1440),
            ScreenshotState::Recording,
            "Same-epoch connection returns",
        );
    }
    builder.disconnected(milliseconds(2080));
    let mut idle = if restart {
        fixture(ScreenshotState::Reconnected, origin)
    } else {
        preview_snapshot(StateKind::Idle)
    };
    idle.outcome = None;
    builder.status(
        milliseconds(3520),
        idle,
        if restart {
            "New epoch returns idle"
        } else {
            "Same epoch returns idle"
        },
    );
    let end = milliseconds(3520) + NOTICE_HOLD;
    if !reduced_motion {
        builder.mark(end - RESULT_FADE, "Connection notice fading");
    }
    builder.mark(end, "Idle: connection notice expired");
    builder.finish(
        end + milliseconds(960),
        milliseconds(if focus_lost { 2880 } else { 3840 }),
    )
}

fn interruption(origin: Instant, reduced_motion: bool) -> Timeline {
    let mut builder = TimelineBuilder::new(origin, reduced_motion);
    builder.recording(milliseconds(320), milliseconds(1600));
    builder.chunks(milliseconds(1600), 0, 1, "First take transcribing");
    builder.state(
        milliseconds(2880),
        ScreenshotState::Sent,
        "First take succeeds",
    );
    let next = fixture(ScreenshotState::Interrupted, origin);
    builder.status(
        milliseconds(3000),
        next.clone(),
        "Next take interrupts success",
    );
    let mut processing = fixture(ScreenshotState::Transcribing, origin);
    processing.operation_id = next.operation_id.clone();
    builder.status(milliseconds(4320), processing, "Next take transcribing");
    let mut sent = fixture(ScreenshotState::Sent, origin);
    if let Some(outcome) = &mut sent.outcome {
        outcome.event_id = 2;
        outcome.operation_id = next.operation_id.clone();
        outcome.artifacts.take_id = next.operation_id;
    }
    let end = builder.outcome(milliseconds(5920), sent, "Next take succeeds");
    builder.finish(end + milliseconds(960), milliseconds(3180))
}

fn cancellation(origin: Instant, reduced_motion: bool) -> Timeline {
    let mut builder = TimelineBuilder::new(origin, reduced_motion);
    builder.recording(milliseconds(320), milliseconds(1600));
    builder.chunks(milliseconds(1600), 0, 4, "Reported chunks: 0/4");
    builder.chunks(milliseconds(2240), 1, 4, "Reported chunks: 1/4");
    builder.state(
        milliseconds(2880),
        ScreenshotState::Cancelling,
        "Cancellation requested; current work finishes",
    );
    let end = builder.outcome(
        milliseconds(4160),
        fixture(ScreenshotState::Cancelled, origin),
        "Cancelled",
    );
    builder.finish(end + milliseconds(960), milliseconds(3360))
}

fn catalog_timeline(state: ScreenshotState, origin: Instant, reduced_motion: bool) -> Timeline {
    match state {
        ScreenshotState::Disconnected => return connection(origin, reduced_motion, false, true),
        ScreenshotState::Reconnected => return connection(origin, reduced_motion, true, false),
        ScreenshotState::Interrupted => return interruption(origin, reduced_motion),
        _ => {}
    }
    let mut builder = TimelineBuilder::new(origin, reduced_motion);
    let target = fixture(state, origin);
    if state == ScreenshotState::Idle {
        return builder.finish(milliseconds(2400), Duration::ZERO);
    }
    if state == ScreenshotState::Dismissed {
        builder.recording(milliseconds(320), milliseconds(1600));
        builder.chunks(milliseconds(1600), 0, 1, "Transcribing");
        let mut undismissed = target.clone();
        undismissed
            .outcome
            .as_mut()
            .expect("dismissed fixture contains an outcome")
            .dismissed = false;
        builder.status(
            milliseconds(2880),
            undismissed,
            "Failure remains actionable",
        );
        builder.status(milliseconds(4960), target, "Dismiss outcome; idle");
        return builder.finish(milliseconds(5920), milliseconds(5280));
    }
    if target.state == StateKind::Recording {
        let long_startup = matches!(
            state,
            ScreenshotState::NoSignal | ScreenshotState::MonitoringUnavailable
        );
        let target_at = if long_startup {
            milliseconds(target.elapsed * 1000 + 320)
        } else {
            milliseconds(320)
        };
        if long_startup {
            let mut initial = target.clone();
            initial.elapsed = 0;
            builder.status(milliseconds(320), initial.clone(), "Recording begins");
            for elapsed in 1..=target.elapsed {
                initial.elapsed = elapsed;
                builder.events.push(Event {
                    at: milliseconds(320 + elapsed * 1000),
                    input: Some(initial.clone()),
                });
            }
            builder.mark(target_at, "Named recording fixture");
        } else {
            builder.status(target_at, target, "Named recording fixture");
        }
        let focus = target_at + milliseconds(640);
        let processing_at = target_at
            + milliseconds(if state == ScreenshotState::Starting {
                1600
            } else {
                2240
            });
        if state == ScreenshotState::Starting {
            builder.state(
                processing_at,
                ScreenshotState::Recording,
                "Measured input arrives",
            );
        }
        builder.chunks(processing_at + milliseconds(960), 0, 1, "Transcribing");
        let outcome = if state == ScreenshotState::NoSignal {
            ScreenshotState::EmptySaved
        } else {
            ScreenshotState::Sent
        };
        let end = builder.outcome(
            processing_at + milliseconds(2560),
            fixture(outcome, origin),
            "Terminal outcome",
        );
        return builder.finish(end + milliseconds(960), focus);
    }
    builder.recording(milliseconds(320), milliseconds(1600));
    if target.state == StateKind::Processing {
        let at = milliseconds(1600);
        let focus = at
            + if state == ScreenshotState::Settling {
                milliseconds(60)
            } else {
                milliseconds(640)
            };
        let operation_kind = target.operation_kind;
        if state == ScreenshotState::Busy {
            let mut before_notice = target.clone();
            before_notice.notice = None;
            builder.status(at, before_notice, "Transcribing");
            builder.status(
                at + milliseconds(320),
                target,
                "Already-working interaction notice",
            );
            builder.mark(
                at + milliseconds(320) + super::INTERACTION_HOLD,
                "Interaction notice expires",
            );
        } else {
            builder.status(at, target, "Named processing fixture");
        }
        if state == ScreenshotState::RemovingRecording {
            builder.status(
                milliseconds(4320),
                preview_snapshot(StateKind::Idle),
                "Removal finished; idle",
            );
            return builder.finish(milliseconds(5280), focus);
        }
        let result_at = if state == ScreenshotState::Cancelling {
            milliseconds(4320)
        } else {
            if state == ScreenshotState::FinalizingAudio {
                builder.chunks(milliseconds(3680), 0, 1, "Transcribing");
            } else if !matches!(
                state,
                ScreenshotState::Cleaning
                    | ScreenshotState::ReducedMotionCleaning
                    | ScreenshotState::Delivering
            ) {
                let mut cleaning = fixture(ScreenshotState::Cleaning, origin);
                cleaning.operation_kind = operation_kind;
                let cleaning_at = milliseconds(if state == ScreenshotState::Busy {
                    4320
                } else {
                    3680
                });
                builder.status(cleaning_at, cleaning, "Cleanup");
            }
            let mut delivering = fixture(ScreenshotState::Delivering, origin);
            delivering.operation_kind = operation_kind;
            builder.status(milliseconds(4960), delivering, "Deliver text");
            milliseconds(5280)
        };
        let outcome_state = match state {
            ScreenshotState::Cancelling => ScreenshotState::Cancelled,
            ScreenshotState::Recovery => ScreenshotState::Copied,
            _ => ScreenshotState::Sent,
        };
        let end = builder.outcome(
            result_at,
            fixture(outcome_state, origin),
            "Terminal outcome",
        );
        return builder.finish(end + milliseconds(960), focus);
    }
    builder.chunks(milliseconds(1600), 0, 1, "Transcribing");
    if matches!(
        state,
        ScreenshotState::DeliveryFailed
            | ScreenshotState::DeliveryUncertain
            | ScreenshotState::Deferred
    ) {
        builder.state(
            milliseconds(2480),
            ScreenshotState::Delivering,
            "Attempt delivery",
        );
    }
    let result_at = milliseconds(2880);
    let end = builder.outcome(result_at, target, "Named terminal fixture");
    let focus = result_at
        + if state == ScreenshotState::SentSettling {
            milliseconds(200)
        } else {
            milliseconds(480)
        };
    builder.finish(end + milliseconds(960), focus)
}

fn timeline(source: Source, origin: Instant, reduced_motion: bool) -> Timeline {
    match source {
        Source::Journey(Journey::Measured) => dictation(origin, reduced_motion, false),
        Source::Journey(Journey::Raw) => dictation(origin, reduced_motion, true),
        Source::Journey(Journey::Cancellation) => cancellation(origin, reduced_motion),
        Source::Journey(Journey::Interruption) => interruption(origin, reduced_motion),
        Source::Journey(Journey::Reconnect) => connection(origin, reduced_motion, false, false),
        Source::Journey(Journey::Restart) => connection(origin, reduced_motion, true, false),
        Source::Journey(Journey::Failure) => {
            catalog_timeline(ScreenshotState::Failed, origin, reduced_motion)
        }
        Source::Journey(Journey::Uncertain) => {
            catalog_timeline(ScreenshotState::DeliveryUncertain, origin, reduced_motion)
        }
        Source::Journey(Journey::Deferred) => {
            catalog_timeline(ScreenshotState::Deferred, origin, reduced_motion)
        }
        Source::State(state) => catalog_timeline(state, origin, reduced_motion),
    }
}

#[derive(Clone, Copy, Default)]
struct ViewOptions {
    labels: bool,
    reduced_motion: bool,
}

#[derive(Default)]
struct Preview {
    bytes: Vec<u8>,
    size: [usize; 2],
    last_render: Option<RenderKey>,
    image: Arc<egui::ColorImage>,
    texture: Option<egui::TextureHandle>,
    dirty: bool,
}

impl Preview {
    fn paint(&mut self, model: &mut Model, font: &FontRef<'_>, palette: Palette, now: Instant) {
        let height = layout_height(model, font, CONTAINER_WIDTH);
        let frame = model.frame(now);
        let alpha = model.alpha(now);
        let key = model.render_key(&frame, (SURFACE_WIDTH, height, 1), palette, alpha);
        if self.last_render.as_ref() == Some(&key) {
            return;
        }
        self.size = [SURFACE_WIDTH as usize, height as usize];
        self.bytes.resize(self.size[0] * self.size[1] * 4, 0);
        let mut canvas = Canvas {
            bytes: &mut self.bytes,
            width: SURFACE_WIDTH,
            height,
            scale: 1.0,
            alpha: 1.0,
        };
        canvas.paint_hud(model, font, palette, &frame, CONTAINER_WIDTH);
        canvas.fade(alpha);
        // Every simulated presentation is really painted, including frames crossed
        // during seeking. A later phase starts from this production appearance.
        model.track.presented = frame;
        self.last_render = Some(key);
        self.dirty = true;
    }

    fn upload(&mut self, ctx: &egui::Context) {
        if !self.dirty {
            return;
        }
        // Eframe normally releases the preceding texture delta before update.
        // If it still owns it, defer this upload rather than allocate/copy a frame.
        let Some(image) = Arc::get_mut(&mut self.image) else {
            ctx.request_repaint();
            return;
        };
        image.size = self.size;
        image
            .pixels
            .resize(self.size[0] * self.size[1], egui::Color32::TRANSPARENT);
        for (color, bgra) in image.pixels.iter_mut().zip(self.bytes.as_chunks::<4>().0) {
            *color = egui::Color32::from_rgba_premultiplied(bgra[2], bgra[1], bgra[0], bgra[3]);
        }
        if let Some(texture) = &mut self.texture {
            texture.set(Arc::clone(&self.image), egui::TextureOptions::NEAREST);
        } else {
            self.texture = Some(ctx.load_texture(
                "production-hud",
                Arc::clone(&self.image),
                egui::TextureOptions::NEAREST,
            ));
        }
        self.dirty = false;
    }
}

struct Replay {
    origin: Instant,
    timeline: Timeline,
    model: Model,
    frame: Option<u32>,
    next_event: usize,
}

impl Replay {
    fn new(origin: Instant, timeline: Timeline) -> Self {
        Self {
            origin,
            timeline,
            model: Model::new(origin),
            frame: None,
            next_event: 0,
        }
    }

    fn seek(
        &mut self,
        target: u32,
        options: ViewOptions,
        preview: &mut Preview,
        font: &FontRef<'_>,
        palette: Palette,
    ) {
        let target = target.min(self.timeline.end);
        if self.frame.is_some_and(|current| target < current) {
            self.model = Model::new(self.origin);
            self.frame = None;
            self.next_event = 0;
        }
        if self.frame.is_none() {
            // caption_revision belongs to a Model lifetime, not to this texture.
            preview.last_render = None;
        }
        let start = self.frame.map_or(0, |current| current + 1);
        for frame in start..=target {
            let elapsed = FRAME_INTERVAL * frame;
            while let Some(event) = self
                .timeline
                .events
                .get(self.next_event)
                .filter(|event| event.at <= elapsed)
            {
                let now = self.origin + event.at;
                match &event.input {
                    Some(status) => {
                        let mut status = status.clone();
                        status.hud.labels = options.labels;
                        status.hud.reduced_motion = Some(options.reduced_motion);
                        self.model.apply(status, now);
                    }
                    None => self.model.disconnected(now),
                }
                self.next_event += 1;
            }
            let now = self.origin + elapsed;
            self.model.refresh(now);
            preview.paint(&mut self.model, font, palette, now);
            self.frame = Some(frame);
        }
    }

    fn checkpoint(&self) -> usize {
        let elapsed = FRAME_INTERVAL * self.frame.unwrap_or(0);
        self.timeline
            .checkpoints
            .partition_point(|checkpoint| checkpoint.at <= elapsed)
            .saturating_sub(1)
    }
}

struct Capture {
    path: PathBuf,
    frames: u32,
    deadline: Option<Instant>,
    result: CaptureResult,
}

struct GalleryApp {
    entries: Vec<Entry>,
    selected: usize,
    replay: Replay,
    preview: Preview,
    font: FontRef<'static>,
    palette: Palette,
    options: ViewOptions,
    zoom: u32,
    speed: f64,
    playing: bool,
    playback_anchor: Instant,
    anchor_frame: u32,
    capture: Option<Capture>,
}

impl GalleryApp {
    fn new(
        ctx: &egui::Context,
        font: FontRef<'static>,
        screenshot: Option<PathBuf>,
        capture_result: CaptureResult,
    ) -> Self {
        let palette = theme::load();
        apply_theme(ctx, palette);
        let entries = Entry::catalog();
        let origin = Instant::now();
        let replay = Replay::new(origin, timeline(entries[0].source, origin, false));
        let focus = replay.timeline.focus;
        let mut app = Self {
            entries,
            selected: 0,
            replay,
            preview: Preview::default(),
            font,
            palette,
            options: ViewOptions::default(),
            zoom: 2,
            speed: 1.0,
            playing: false,
            playback_anchor: origin,
            anchor_frame: 0,
            capture: screenshot.map(|path| Capture {
                path,
                frames: 0,
                deadline: None,
                result: capture_result,
            }),
        };
        app.seek(focus);
        app
    }

    fn seek(&mut self, frame: u32) {
        self.replay.seek(
            frame,
            self.options,
            &mut self.preview,
            &self.font,
            self.palette,
        );
        self.anchor_frame = self.replay.frame.unwrap_or(0);
        self.playback_anchor = Instant::now();
    }

    fn rebuild(&mut self, select: bool) {
        let current = self.replay.frame.unwrap_or(0);
        let origin = self.replay.origin;
        let timeline = timeline(
            self.entries[self.selected].source,
            origin,
            self.options.reduced_motion,
        );
        let target = if select {
            timeline.focus
        } else {
            current.min(timeline.end)
        };
        self.replay = Replay::new(origin, timeline);
        self.seek(target);
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.playing = false;
        self.options.reduced_motion = self.entries[index].reduced_motion();
        self.rebuild(true);
    }

    fn toggle_play(&mut self) {
        if !self.playing && self.replay.frame == Some(self.replay.timeline.end) {
            self.seek(0);
        }
        self.playing = !self.playing;
        self.anchor_frame = self.replay.frame.unwrap_or(0);
        self.playback_anchor = Instant::now();
    }

    fn replay(&mut self) {
        self.seek(0);
        self.playing = true;
    }

    fn step(&mut self, forward: bool) {
        self.playing = false;
        let current = self.replay.frame.unwrap_or(0);
        self.seek(if forward {
            current.saturating_add(1)
        } else {
            current.saturating_sub(1)
        });
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // Review commands remain available after using a native control.
        let review_shortcut = ctx.input(|input| {
            if input.key_pressed(egui::Key::R) {
                self.replay();
            } else if input.key_pressed(egui::Key::PageUp) {
                self.select(self.selected.saturating_sub(1));
            } else if input.key_pressed(egui::Key::PageDown) {
                self.select((self.selected + 1).min(self.entries.len() - 1));
            } else {
                return false;
            }
            true
        });
        if review_shortcut {
            // A subsequent Space belongs to playback, not the previous control.
            if let Some(focused) = ctx.memory(|memory| memory.focused()) {
                ctx.memory_mut(|memory| memory.surrender_focus(focused));
            }
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
        ctx.input(|input| {
            if input.key_pressed(egui::Key::Space) {
                self.toggle_play();
            }
            if input.key_pressed(egui::Key::ArrowLeft) {
                self.step(false);
            }
            if input.key_pressed(egui::Key::ArrowRight) {
                self.step(true);
            }
            if input.key_pressed(egui::Key::Home) {
                self.playing = false;
                self.seek(0);
            }
            if input.key_pressed(egui::Key::End) {
                self.playing = false;
                self.seek(self.replay.timeline.end);
            }
        });
    }

    fn sidebar(&mut self, ctx: &egui::Context, reveal_selected: bool) {
        let mut selected = self.selected;
        egui::SidePanel::left("fixture-catalog")
            .default_width(250.0)
            .min_width(200.0)
            .max_width(360.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Transition journeys");
                    for (index, entry) in self.entries.iter().enumerate() {
                        if index == Journey::ALL.len() {
                            ui.add_space(14.0);
                            ui.heading("Production catalog");
                            ui.weak(format!(
                                "All {} screenshot states",
                                ScreenshotState::value_variants().len()
                            ));
                        }
                        let response = ui.selectable_label(index == self.selected, &entry.title);
                        if reveal_selected && index == self.selected {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                        if response.clicked() {
                            selected = index;
                        }
                    }
                });
            });
        if selected != self.selected {
            self.select(selected);
        }
    }

    fn controls(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("playback-controls").show(ctx, |ui| {
            ui.add_space(6.0);
            let mut frame = self.replay.frame.unwrap_or(0);
            let end = self.replay.timeline.end;
            ui.horizontal(|ui| {
                ui.label(if self.playing { "Playing" } else { "Paused" });
                ui.monospace(format!("{:06.3} / {:06.3} s", (FRAME_INTERVAL * frame).as_secs_f64(), (FRAME_INTERVAL * end).as_secs_f64()));
                ui.weak(format!("Frame {frame} / {end}"));
            });
            let response = ui.scope(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 100.0).max(160.0);
                ui.add(egui::Slider::new(&mut frame, 0..=end).show_value(false).text("Timeline"))
            }).inner;
            if response.changed() {
                self.playing = false;
                self.seek(frame);
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button(if self.playing { "Pause" } else { "Play" }).clicked() { self.toggle_play(); }
                if ui.button("Replay").on_hover_text("Play the complete history from idle").clicked() { self.replay(); }
                if ui.add_enabled(frame > 0, egui::Button::new("Back frame")).clicked() { self.step(false); }
                if ui.add_enabled(frame < end, egui::Button::new("Frame step")).clicked() { self.step(true); }
                let old_speed = self.speed;
                egui::ComboBox::from_id_salt("playback-speed").selected_text(format!("{}x speed", self.speed)).show_ui(ui, |ui| {
                    for speed in [0.25, 0.5, 1.0, 2.0, 4.0] { ui.selectable_value(&mut self.speed, speed, format!("{speed}x")); }
                });
                if old_speed != self.speed {
                    self.anchor_frame = self.replay.frame.unwrap_or(0);
                    self.playback_anchor = Instant::now();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Pixel zoom");
                for zoom in [1, 2, 4] { ui.selectable_value(&mut self.zoom, zoom, format!("{zoom}x")); }
                let reduced = ui.checkbox(&mut self.options.reduced_motion, "Reduced motion").changed();
                let labels = ui.checkbox(&mut self.options.labels, "HUD labels").changed();
                if reduced || labels { self.rebuild(false); }
            });
            let active = self.replay.checkpoint();
            let mut jump = None;
            ui.horizontal_wrapped(|ui| {
                ui.label("Fixture event");
                egui::ComboBox::from_id_salt("fixture-event").selected_text(self.replay.timeline.checkpoints[active].label).width(300.0).show_ui(ui, |ui| {
                    for (index, checkpoint) in self.replay.timeline.checkpoints.iter().enumerate() {
                        if ui.selectable_label(index == active, format!("{:06.3}  {}", checkpoint.at.as_secs_f64(), checkpoint.label)).clicked() {
                            jump = Some(frame_at(checkpoint.at));
                        }
                    }
                });
            });
            if let Some(frame) = jump { self.playing = false; self.seek(frame); }
            ui.small("Space play/pause  ·  R replay  ·  Left/Right one frame  ·  Home/End seek");
            ui.small("PgUp/PgDn choose fixture  ·  Tab and Enter operate controls  ·  View options are never saved");
            ui.add_space(4.0);
        });
    }

    fn surface(&mut self, ctx: &egui::Context) {
        self.preview.upload(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            let entry = &self.entries[self.selected];
            ui.heading(&entry.title);
            ui.label(entry.description());
            ui.add_space(6.0);
            if let Some(status) = &self.replay.model.snapshot {
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("Fixture status: {}", status.state_name()));
                    if let Some(stage) = &status.stage { ui.label(stage.to_string()); }
                    ui.weak(&status.epoch);
                });
            }
            ui.weak(format!("Production surface: {} × {} pixels  ·  Nearest-neighbor {}x  ·  Scroll to inspect at full size",
                self.preview.size[0], self.preview.size[1], self.zoom));
            if self.replay.model.kind.is_none() {
                ui.label("HUD hidden: the production surface is transparent.");
            }
            ui.add_space(12.0);
            let available = ui.available_size();
            egui::ScrollArea::both().auto_shrink([false, false]).max_height(available.y).show(ui, |ui| {
                let pixels_per_point = ctx.pixels_per_point();
                let image_size = egui::vec2(self.preview.size[0] as f32, self.preview.size[1] as f32) * self.zoom as f32 / pixels_per_point;
                let area_size = egui::vec2(image_size.x + 32.0, image_size.y + 32.0).max(available);
                let (area, _) = ui.allocate_exact_size(area_size, egui::Sense::click());
                let mut min = area.center() - image_size * 0.5;
                min.x = (min.x * pixels_per_point).round() / pixels_per_point;
                min.y = (min.y * pixels_per_point).round() / pixels_per_point;
                let rect = egui::Rect::from_min_size(min, image_size);
                ui.painter().rect_stroke(rect.expand(1.0), 0.0, egui::Stroke::new(1.0_f32, color(self.palette.border)));
                if let Some(texture) = &self.preview.texture {
                    ui.painter().image(texture.id(), rect, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
            });
        });
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(capture) = &mut self.capture else {
            return;
        };
        let shot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { viewport_id, image }
                    if *viewport_id == egui::ViewportId::ROOT =>
                {
                    Some(Arc::clone(image))
                }
                _ => None,
            })
        });
        let result = if let Some(image) = shot {
            Some(
                image::save_buffer(
                    &capture.path,
                    image.as_raw(),
                    image.width() as u32,
                    image.height() as u32,
                    image::ColorType::Rgba8,
                )
                .with_context(|| format!("writing gallery screenshot {}", capture.path.display())),
            )
        } else if capture
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(Err(anyhow!(
                "gallery screenshot failed: no screenshot event was received"
            )))
        } else {
            None
        };
        if let Some(result) = result {
            if result.is_ok() {
                eprintln!("saved gallery screenshot to {}", capture.path.display());
            }
            capture.result.replace(Some(result));
            self.capture = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        capture.frames += 1;
        if let Some(deadline) = capture.deadline {
            // One bounded failure wakeup, not a polling loop or a substitute capture.
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        } else if capture.frames >= SCREENSHOT_DELAY_FRAMES && !self.preview.dirty {
            capture.deadline = Some(Instant::now() + SCREENSHOT_TIMEOUT);
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            ctx.request_repaint_after(SCREENSHOT_TIMEOUT);
        } else {
            ctx.request_repaint();
        }
    }
}

impl eframe::App for GalleryApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let previous_selection = self.selected;
        self.keyboard(ctx);
        if self.playing {
            let elapsed_frames = (self.playback_anchor.elapsed().as_secs_f64() * self.speed
                / FRAME_INTERVAL.as_secs_f64()) as u32;
            let target = self
                .anchor_frame
                .saturating_add(elapsed_frames)
                .min(self.replay.timeline.end);
            self.replay.seek(
                target,
                self.options,
                &mut self.preview,
                &self.font,
                self.palette,
            );
            if target == self.replay.timeline.end {
                self.playing = false;
            }
        }
        egui::TopBottomPanel::top("gallery-header").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("Cantrip HUD Gallery");
            ui.label("Fixture inputs. Production HUD state and pixels. No microphone, daemon, or delivery connection.");
            ui.weak("Stage durations are scripted, not STT latency. Transitions, result hold, and fade use production timing.");
            ui.add_space(4.0);
        });
        self.sidebar(ctx, previous_selection != self.selected);
        self.controls(ctx);
        self.surface(ctx);
        self.screenshot(ctx);
        if self.playing {
            ctx.request_repaint_after(FRAME_INTERVAL.div_f64(self.speed));
        }
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

/// Open the local fixture gallery; screenshot mode captures this actual window.
pub fn run(screenshot: Option<PathBuf>) -> Result<()> {
    let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR)
        .context("loading HUD typeface")?;
    let capture_result: CaptureResult = Rc::new(RefCell::new(None));
    let app_capture_result = Rc::clone(&capture_result);
    let capturing = screenshot.is_some();
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_app_id("cantrip-hud-gallery")
            .with_title("Cantrip HUD Gallery")
            .with_inner_size([1240.0, 820.0])
            .with_min_inner_size([940.0, 640.0])
            .with_maximized(false),
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "cantrip-hud-gallery",
        options,
        Box::new(move |cc| {
            Ok(Box::new(GalleryApp::new(
                &cc.egui_ctx,
                font,
                screenshot,
                app_capture_result,
            )))
        }),
    )
    .map_err(|error| anyhow!("HUD gallery window error: {error}"))?;
    if capturing {
        let result = capture_result.borrow_mut().take();
        result.unwrap_or_else(|| Err(anyhow!("gallery closed before its screenshot was captured")))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backward_seek_restores_expired_epoch_notice_and_dismissed_outcome() {
        let origin = Instant::now();
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        for (state, visible_at) in [
            (ScreenshotState::Reconnected, milliseconds(4000)),
            (ScreenshotState::Dismissed, milliseconds(3840)),
        ] {
            let mut replay = Replay::new(origin, catalog_timeline(state, origin, false));
            let mut preview = Preview::default();
            replay.seek(
                frame_at(visible_at),
                ViewOptions::default(),
                &mut preview,
                &font,
                Palette::default(),
            );
            let visible = preview.bytes.clone();
            assert!(
                visible
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] == 255),
                "the recovery notice must be visible before it expires or is dismissed"
            );
            replay.seek(
                replay.timeline.end,
                ViewOptions::default(),
                &mut preview,
                &font,
                Palette::default(),
            );
            assert!(
                preview.bytes.iter().all(|byte| *byte == 0),
                "completed history must reach transparent idle"
            );
            replay.seek(
                frame_at(visible_at),
                ViewOptions::default(),
                &mut preview,
                &font,
                Palette::default(),
            );
            assert_eq!(preview.bytes, visible, "seeking backward must replay epoch and dismissal history, not reuse terminal freshness state");
        }
    }

    #[test]
    fn replay_shortcut_works_while_a_native_control_has_focus() {
        let ctx = egui::Context::default();
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let mut app = GalleryApp::new(&ctx, font, None, Rc::new(RefCell::new(None)));
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.button("Playback control").request_focus();
            });
        });
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::R,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| {
                assert!(
                    ctx.wants_keyboard_input(),
                    "a native control owns keyboard focus"
                );
                app.keyboard(ctx);
            },
        );
        assert!(app.playing, "the replay shortcut must start playback");
        assert_eq!(
            app.replay.frame,
            Some(0),
            "replay must return the visible timeline to its origin"
        );
        assert!(
            app.preview.bytes.iter().all(|byte| *byte == 0),
            "replay must present transparent idle before recording begins"
        );
    }
}
