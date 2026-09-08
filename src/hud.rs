//! Passive, bottom-anchored Wayland status instrument.
//!
//! A signed pixel instrument, fluid work states, and words for actionable exceptions.
//! The daemon owns operations, outcomes and acknowledgement. The HUD never sends
//! mutations, takes focus, handles pointer input, or invents audio/progress.

use ab_glyph::{point, Font, FontRef, ScaleFont};
use anyhow::{Context, Result};
use clap::ValueEnum;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::{
    borrow::Cow,
    fs,
    io::Read,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

use crate::{
    ipc::{
        self, AudioSignal, AudioWaveform, Cleanup, Completeness, Delivery, StateKind,
        StatusSnapshot, TerminalOutcome, AUDIO_WAVEFORM_BINS,
    },
    pipeline::Stage,
    theme::{self, Palette},
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const PREFERENCE_INTERVAL: Duration = Duration::from_secs(2);
const SIGNAL_GRACE: Duration = Duration::from_secs(8);
const SIGNAL_RETURN: Duration = Duration::from_millis(300);
const MONITOR_DELAY: Duration = Duration::from_secs(5);
const DISCONNECT_DELAY: Duration = Duration::from_millis(450);
const STATUS_STALE_AFTER: Duration = Duration::from_secs(2);
// Time to cover 95% of a measured change; release must outlive a short speech gap.
const WAVEFORM_EASE: Duration = POLL_INTERVAL;
const WAVEFORM_RELEASE: Duration = Duration::from_millis(420);
const SETTLE: Duration = Duration::from_millis(280);
// Full-opacity dwell after the completion mark has finished settling.
const SUCCESS_HOLD: Duration = Duration::from_millis(700);
const RESULT_FADE: Duration = Duration::from_millis(140);
const NOTICE_HOLD: Duration = Duration::from_secs(4);
const INTERACTION_HOLD: Duration = Duration::from_secs(2);
const SURFACE_WIDTH: u32 = 420;
const SURFACE_HEIGHT: u32 = 56;
const CONTAINER_WIDTH: f32 = 336.0;
const TRACK_HEIGHT: f32 = 44.0;
const CELLS: usize = AUDIO_WAVEFORM_BINS;
const CELL_SIZE: f32 = 3.0;
const CELL_PITCH: f32 = 5.0;
const TRACK_WIDTH: f32 = CELLS as f32 * CELL_PITCH;
// Public samples/jfk.wav at 9.0 s: the newest 100 ms of chronological PCM pairs.
const SCREENSHOT_WAVEFORM: AudioWaveform = [
    [-1326, 1927],
    [-1231, 823],
    [-2107, 1200],
    [-1480, 2177],
    [-1285, 1225],
    [-2567, 1483],
    [-3029, 1821],
    [-1610, 2811],
    [-3118, 2632],
    [-2468, 1960],
    [-1685, 5453],
    [-4074, 3135],
    [-3410, 1396],
    [-1814, 5200],
    [-4244, 4094],
    [-3195, 3552],
    [-178, 5113],
    [-7377, 2521],
    [-8633, 5478],
    [-688, 5681],
    [-12014, 2144],
    [-7734, 5984],
    [-6926, 8550],
    [-10549, 2679],
    [-1487, 9118],
    [-10879, 4210],
    [-3222, 5799],
    [-9823, 8692],
    [-10547, 1050],
    [-371, 6464],
    [-9794, 3058],
    [-2591, 7100],
    [-9829, 7049],
    [-5955, 246],
    [-917, 7823],
    [-8873, 76],
    [-4226, 8040],
    [-9462, 5885],
    [-6002, 4069],
    [-8595, 9187],
    [-6203, 874],
    [-4844, 8242],
    [-9695, 2548],
    [-4982, 6095],
    [-8982, 7812],
    [-5724, 1743],
    [-3242, 7561],
    [-9862, 3491],
    [-4704, 8069],
    [-10155, 2021],
    [-4365, 6482],
    [-10898, 7932],
    [-6480, 4558],
    [-4327, 10154],
    [-12531, 3104],
    [-5318, 9564],
    [-10779, 8470],
    [-8316, 2315],
    [-5163, 10340],
    [-11476, 3049],
];

/// Shared with the daemon's HUD-presence check. Keep the file alive while running.
pub(crate) fn acquire_instance_lock() -> Result<Option<fs::File>> {
    acquire_lock_on(&crate::paths::hud_lock_path()?)
}

fn acquire_lock_on(path: &Path) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening HUD lock {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(error).with_context(|| format!("locking HUD instance file {}", path.display()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Recording,
    Working,
    Finishing,
    Resolved,
    Neutral,
    Attention,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Caption {
    title: String,
    detail: String,
    action: String,
}

impl Caption {
    fn title(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Visibility {
    Persistent,
    Until(Instant),
}

impl Visibility {
    fn alpha(self, now: Instant, reduced: bool) -> Option<f32> {
        match self {
            Self::Persistent => Some(1.0),
            Self::Until(until) if now < until => Some(if reduced {
                1.0
            } else {
                (until.duration_since(now).as_secs_f32() / RESULT_FADE.as_secs_f32()).min(1.0)
            }),
            Self::Until(_) => None,
        }
    }
}

struct ResultView {
    event_id: Option<u64>,
    kind: Kind,
    caption: Caption,
    visibility: Visibility,
}

/// Signal absence at the beginning is actionable; silence after real input is
/// usually thinking. An amplitude meter cannot diagnose mute/device failure.
#[derive(Default)]
struct SignalHistory {
    monitored: bool,
    heard_input: bool,
    warning: bool,
    returning_since: Option<Instant>,
}

impl SignalHistory {
    fn update(&mut self, signal: Option<AudioSignal>, age: Duration, now: Instant) {
        match signal {
            Some(signal) => {
                self.monitored = true;
                if signal.level > 0 {
                    if self.warning {
                        let since = self.returning_since.get_or_insert(now);
                        if now.duration_since(*since) >= SIGNAL_RETURN {
                            self.warning = false;
                            self.heard_input = true;
                        }
                    } else {
                        self.heard_input = true;
                    }
                } else {
                    self.returning_since = None;
                    if !self.heard_input && signal.silent && age >= SIGNAL_GRACE {
                        self.warning = true;
                    }
                }
            }
            None => {
                self.returning_since = None;
            }
        }
    }
}

struct Model {
    snapshot: Option<StatusSnapshot>,
    operation: Option<String>,
    phase_since: Instant,
    signal: SignalHistory,
    outcome_event: Option<u64>,
    notice_event: Option<u64>,
    result: Option<ResultView>,
    interaction: Option<(String, Instant)>,
    lost_since: Option<Instant>,
    lost_active: bool,
    kind: Option<Kind>,
    caption: Caption,
    activity_since: Instant,
    caption_revision: u64,
    progress: Option<(u32, u32)>,
    waveform: Option<AudioWaveform>,
    elapsed_label: String,
    elapsed_value: Option<u64>,
    reduced_motion: bool,
    desktop_reduced_motion: bool,
    track: TrackMotion,
}

impl Model {
    fn new(now: Instant) -> Self {
        Self {
            snapshot: None,
            operation: None,
            phase_since: now,
            signal: SignalHistory::default(),
            outcome_event: None,
            notice_event: None,
            result: None,
            interaction: None,
            lost_since: None,
            lost_active: false,
            kind: None,
            caption: Caption::default(),
            activity_since: now,
            progress: None,
            waveform: None,
            elapsed_label: String::new(),
            elapsed_value: None,
            caption_revision: 0,
            reduced_motion: false,
            desktop_reduced_motion: false,
            track: TrackMotion::new(now),
        }
    }

    fn active(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| matches!(s.state, StateKind::Recording | StateKind::Processing))
    }

    fn apply(&mut self, status: StatusSnapshot, now: Instant) {
        let first = self.snapshot.is_none();
        let epoch_changed = self
            .snapshot
            .as_ref()
            .is_some_and(|old| old.epoch != status.epoch);
        let active = matches!(status.state, StateKind::Recording | StateKind::Processing);
        let was_active = self.active();
        let phase_changed = self
            .snapshot
            .as_ref()
            .is_none_or(|old| old.state != status.state);
        let new_operation =
            active && (epoch_changed || !was_active || self.operation != status.operation_id);
        let was_disconnected = self.lost_since.take().is_some();
        let lost_active = self.lost_active;
        self.lost_active = false;
        self.reduced_motion = status
            .hud
            .reduced_motion
            .unwrap_or(self.desktop_reduced_motion);
        if epoch_changed {
            self.outcome_event = None;
            self.notice_event = None;
            self.result = None;
            self.interaction = None;
        }
        if new_operation {
            self.operation = status.operation_id.clone();
            self.phase_since = now;
            self.signal = SignalHistory::default();
            self.result = None;
            self.interaction = None;
            self.elapsed_value = None;
            // A deliberate operation interrupts even an unfinished result fade.
            self.track.reset(now);
            self.kind = None;
            self.waveform = None;
            self.progress = None;
        } else if phase_changed {
            self.phase_since = now;
        }
        if active {
            self.result = None;
        }
        if matches!(status.state, StateKind::Recording) {
            let age = Duration::from_secs(status.elapsed).max(now.duration_since(self.phase_since));
            self.signal.update(status.signal, age, now);
            if status.hud.labels && self.elapsed_value != Some(status.elapsed) {
                self.elapsed_value = Some(status.elapsed);
                self.elapsed_label = format!("Recording · {}", format_elapsed(status.elapsed));
            }
        }
        if let Some(outcome) = &status.outcome {
            let fresh = self
                .outcome_event
                .is_none_or(|event| outcome.event_id > event);
            let belongs = self.operation.is_none() || outcome.operation_id == self.operation;
            if outcome.dismissed
                && self
                    .result
                    .as_ref()
                    .is_some_and(|r| r.event_id == Some(outcome.event_id))
            {
                self.result = None;
            }
            if fresh {
                self.outcome_event = Some(outcome.event_id);
                // Seed cached successes silently on attach/restart. Once idle,
                // a fresh daemon event is authoritative even if its complete
                // operation happened between polls; active work still wins.
                let initial = first || epoch_changed;
                if !active && !outcome.dismissed && (!initial || persistent(outcome)) {
                    let mut result = present_outcome(outcome, &status, now, self.reduced_motion);
                    if !initial
                        && belongs
                        && outcome.completeness == Completeness::Empty
                        && self.signal.warning
                    {
                        result.caption.title = "No input signal detected".to_owned();
                        result.caption.detail = if outcome.artifacts.audio {
                            "Audio saved. Check the microphone input."
                        } else {
                            "Check the microphone input."
                        }
                        .to_owned();
                        if result.caption.action.is_empty() {
                            result.caption.action = "Cantrip actions → Settings".to_owned();
                        }
                    }
                    self.result = Some(result);
                }
            }
            if !fresh
                && persistent(outcome)
                && self
                    .snapshot
                    .as_ref()
                    .is_some_and(|old| old.capabilities != status.capabilities)
            {
                if let Some(result) = &mut self.result {
                    if result.event_id == Some(outcome.event_id) {
                        result.caption.action =
                            present_outcome(outcome, &status, now, self.reduced_motion)
                                .caption
                                .action;
                    }
                }
            }
        }
        if (was_disconnected && lost_active || epoch_changed && was_active)
            && !active
            && self.result.is_none()
        {
            let title = if epoch_changed {
                "Cantrip restarted"
            } else {
                "Connection restored"
            };
            let matching = status
                .outcome
                .as_ref()
                .filter(|outcome| outcome.operation_id == self.operation);
            self.result = Some(ResultView {
                event_id: None,
                kind: Kind::Neutral,
                caption: Caption {
                    title: title.to_owned(),
                    detail: if matching
                        .is_some_and(|outcome| outcome.artifacts.audio || outcome.artifacts.text)
                    {
                        "The previous take is available in saved recordings.".to_owned()
                    } else {
                        "The previous take's result is unknown.".to_owned()
                    },
                    action: "Cantrip → Recordings and recovery".to_owned(),
                },
                visibility: Visibility::Until(now + NOTICE_HOLD),
            });
        }
        if status.notice.is_none() {
            self.interaction = None;
        }
        if let Some(notice) = &status.notice {
            let fresh = self
                .notice_event
                .is_none_or(|event| notice.event_id > event);
            if fresh {
                self.notice_event = Some(notice.event_id);
                if !first && !epoch_changed {
                    self.interaction = Some((notice.message.clone(), now + INTERACTION_HOLD));
                }
            }
        }
        self.snapshot = Some(status);
        self.refresh(now);
    }

    fn refresh_connection(&mut self, last_status: Instant, now: Instant) {
        if self.lost_since.is_none()
            && now.saturating_duration_since(last_status) >= STATUS_STALE_AFTER
        {
            self.disconnected(last_status + STATUS_STALE_AFTER);
        }
        self.refresh(now);
    }

    fn disconnected(&mut self, now: Instant) {
        if self.lost_since.is_none() {
            self.lost_since = Some(now);
            self.lost_active = self.active();
        }
        self.refresh(now);
    }

    fn refresh(&mut self, now: Instant) {
        if self
            .interaction
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.interaction = None;
        }
        if self
            .result
            .as_ref()
            .is_some_and(|r| r.visibility.alpha(now, self.reduced_motion).is_none())
        {
            self.result = None;
        }
        if let Some(since) = self.lost_since {
            if self.lost_active && now.duration_since(since) >= DISCONNECT_DELAY {
                self.set_composition(
                    Some(Kind::Attention),
                    Caption {
                        title: "Cantrip connection lost".to_owned(),
                        detail: "Recording and saved-audio status unknown.".to_owned(),
                        action: "Cantrip actions → Check setup".to_owned(),
                    },
                    None,
                    None,
                    now,
                );
            }
            // Coalesce short outages without erasing evidence or continuing
            // a supposedly live waveform from stale data.
            if self.lost_active {
                return;
            }
        }
        let Some(status) = &self.snapshot else {
            return;
        };
        let phase_age = now
            .duration_since(self.phase_since)
            .max(Duration::from_secs(status.elapsed));
        match &status.state {
            StateKind::Recording => {
                let (kind, caption) = if status.signal.is_none() {
                    let unavailable = self.signal.monitored || phase_age >= MONITOR_DELAY;
                    (
                        if unavailable {
                            Kind::Attention
                        } else {
                            Kind::Working
                        },
                        if unavailable {
                            Caption {
                                title: "Input status unavailable".to_owned(),
                                detail: "Capture is not confirmed by the input monitor.".to_owned(),
                                action: "Cantrip actions → Settings".to_owned(),
                            }
                        } else if status.hud.labels {
                            Caption::title("Starting microphone…")
                        } else {
                            Caption::default()
                        },
                    )
                } else if self.signal.warning {
                    (
                        Kind::Attention,
                        Caption {
                            title: "No input signal".to_owned(),
                            detail: "Check the microphone input.".to_owned(),
                            action: String::new(),
                        },
                    )
                } else {
                    (
                        Kind::Recording,
                        if status.hud.labels {
                            Caption::title(&self.elapsed_label)
                        } else {
                            Caption::default()
                        },
                    )
                };
                self.set_composition(
                    Some(kind),
                    caption,
                    None,
                    status.signal.map(|s| s.waveform),
                    now,
                );
            }
            StateKind::Processing => {
                let stage = status.stage.as_ref();
                let progress = stage.and_then(Stage::measured_progress);
                let caption = if status.hud.labels
                    || matches!(stage, Some(Stage::Cancelling | Stage::RemovingRecording))
                {
                    Caption::title(match stage {
                        Some(Stage::FinalizingAudio) => Cow::Borrowed("Finishing recording…"),
                        Some(Stage::RemovingRecording) => Cow::Borrowed("Removing saved audio…"),
                        Some(Stage::CleaningUp) => Cow::Borrowed("Finishing text…"),
                        Some(Stage::Delivering) => Cow::Borrowed("Delivering text…"),
                        Some(Stage::Cancelling) => {
                            Cow::Borrowed("Cancelling… Waiting for current work.")
                        }
                        Some(Stage::Transcribing { .. }) => {
                            let verb =
                                if status.operation_kind == Some(ipc::OperationKind::Recovery) {
                                    "Recovering recording"
                                } else {
                                    "Transcribing"
                                };
                            match progress {
                                Some((completed, total)) => {
                                    Cow::Owned(format!("{verb} · {completed} of {total} complete"))
                                }
                                None => Cow::Owned(format!("{verb}…")),
                            }
                        }
                        _ => Cow::Borrowed("Working…"),
                    })
                } else {
                    Caption::default()
                };
                let kind = if matches!(
                    stage,
                    Some(Stage::FinalizingAudio | Stage::CleaningUp | Stage::Delivering)
                ) {
                    Kind::Finishing
                } else {
                    Kind::Working
                };
                self.set_composition(Some(kind), caption, progress, None, now);
            }
            StateKind::Idle => {
                if let Some(result) = &self.result {
                    self.set_composition(
                        Some(result.kind),
                        result.caption.clone(),
                        None,
                        None,
                        now,
                    );
                } else if self.interaction.is_some() {
                    self.set_composition(Some(Kind::Neutral), Caption::default(), None, None, now);
                } else {
                    self.set_composition(None, Caption::default(), None, None, now);
                }
            }
            StateKind::Unknown(_) => {
                self.set_composition(
                    Some(Kind::Attention),
                    Caption {
                        title: "Cantrip status unavailable".to_owned(),
                        detail: "Recording and delivery status unknown.".to_owned(),
                        action: "Cantrip actions → Check setup".to_owned(),
                    },
                    None,
                    None,
                    now,
                );
            }
        }
    }

    fn set_composition(
        &mut self,
        kind: Option<Kind>,
        caption: Caption,
        progress: Option<(u32, u32)>,
        waveform: Option<AudioWaveform>,
        now: Instant,
    ) {
        let changed = kind != self.kind;
        if changed {
            self.activity_since = now;
        }
        if self.caption != caption {
            self.caption_revision = self.caption_revision.wrapping_add(1);
        }
        let transition = changed || progress != self.progress;
        if transition || waveform != self.waveform {
            let heights = match kind {
                Some(Kind::Recording) => recording_frame(waveform),
                Some(kind @ (Kind::Working | Kind::Finishing)) => activity_frame(
                    kind,
                    now.saturating_duration_since(self.activity_since),
                    progress,
                    self.reduced_motion,
                ),
                Some(Kind::Resolved) => resolved_frame(),
                Some(Kind::Attention) => [TrackColumn::centered(3.0).with_opacity(0.55); CELLS],
                _ => [TrackColumn::centered(3.0).with_opacity(0.35); CELLS],
            };
            self.track.target(
                heights,
                now,
                if transition { SETTLE } else { WAVEFORM_EASE },
                transition,
                self.reduced_motion,
            );
        }
        self.kind = kind;
        self.caption = caption;
        self.progress = progress;
        self.waveform = waveform;
    }

    fn frame(&self, now: Instant) -> TrackFrame {
        if self.lost_since.is_some() && self.kind != Some(Kind::Attention) {
            return self.track.presented;
        }
        let target = match self.kind {
            Some(kind @ (Kind::Working | Kind::Finishing)) => activity_frame(
                kind,
                now.saturating_duration_since(self.activity_since),
                self.progress,
                self.reduced_motion,
            ),
            _ => self.track.to,
        };
        if self.reduced_motion {
            target
        } else {
            self.track.frame_toward(now, target)
        }
    }

    fn alpha(&self, now: Instant) -> f32 {
        if self.active() || self.lost_since.is_some() {
            return 1.0;
        }
        self.result
            .as_ref()
            .and_then(|r| r.visibility.alpha(now, self.reduced_motion))
            .unwrap_or(1.0)
    }

    fn animate(&self, now: Instant) -> bool {
        // Keep polling responsive between samples; unchanged frames do not repaint.
        !self.reduced_motion
            && self.kind.is_some()
            && (self.lost_since.is_none()
                && (matches!(
                    self.kind,
                    Some(Kind::Recording | Kind::Working | Kind::Finishing)
                ) || self
                    .result
                    .as_ref()
                    .is_some_and(|r| matches!(r.visibility, Visibility::Until(_))))
                || (self.lost_since.is_none() || self.kind == Some(Kind::Attention))
                    && self.track.moving(now))
    }
}

fn persistent(outcome: &TerminalOutcome) -> bool {
    !matches!(
        outcome.completeness,
        Completeness::Cancelled | Completeness::Empty
    ) && (matches!(
        outcome.completeness,
        Completeness::Failed | Completeness::Partial
    ) || matches!(
        outcome.delivery,
        Delivery::Failed | Delivery::Uncertain | Delivery::Deferred
    ))
}

fn present_outcome(
    outcome: &TerminalOutcome,
    status: &StatusSnapshot,
    now: Instant,
    reduced_motion: bool,
) -> ResultView {
    let attention = persistent(outcome);
    let delivered = matches!(outcome.delivery, Delivery::Typed | Delivery::Pasted);
    let copied = outcome.delivery == Delivery::Copied;
    let mut caption = Caption::default();
    let mut kind = if attention {
        Kind::Attention
    } else {
        Kind::Neutral
    };
    match outcome.completeness {
        Completeness::Cancelled => {
            caption.title = "Cancelled".to_owned();
            if outcome.artifacts.audio {
                caption.detail = "Audio saved.".to_owned();
            }
        }
        Completeness::Empty => {
            caption.title = if outcome.artifacts.audio {
                "No transcript produced. Audio saved."
            } else {
                "No speech found"
            }
            .to_owned()
        }
        Completeness::Partial => {
            caption.title = if copied {
                "Partial text copied."
            } else if outcome.artifacts.text {
                "Partial transcript saved."
            } else {
                "Partial transcription"
            }
            .to_owned()
        }
        Completeness::Failed => caption.title = outcome.message.clone(),
        Completeness::Complete => match outcome.delivery {
            Delivery::Uncertain => caption.title = "Delivery uncertain. Check the app.".to_owned(),
            Delivery::Deferred => {
                caption.title = "Delivery paused. Text was not inserted.".to_owned()
            }
            Delivery::Failed => caption.title = "Delivery failed.".to_owned(),
            Delivery::Cancelled => caption.title = "Cancelled".to_owned(),
            Delivery::Copied => {
                kind = Kind::Resolved;
                caption.title = outcome.message.clone();
            }
            Delivery::Typed | Delivery::Pasted => {
                kind = Kind::Resolved;
                if status.hud.labels {
                    caption.title = "Sent".to_owned();
                }
            }
            Delivery::None => caption.title = outcome.message.clone(),
        },
    }
    if outcome.cleanup == Cleanup::Failed && (delivered || copied) {
        caption.title = if copied {
            "Original text copied. Cleanup unavailable."
        } else {
            "Original text sent. Cleanup unavailable."
        }
        .to_owned();
    }
    if matches!(
        outcome.completeness,
        Completeness::Partial | Completeness::Failed
    ) || matches!(
        outcome.delivery,
        Delivery::Failed | Delivery::Uncertain | Delivery::Deferred
    ) {
        caption.detail = match (outcome.artifacts.audio, outcome.artifacts.text) {
            (true, true) => "Audio and text saved.",
            (true, false) => "Audio saved.",
            (false, true) => "Text saved. No saved audio is available.",
            (false, false) => "No saved audio or text is available.",
        }
        .to_owned();
    }
    // Actions are descriptions of the deliberately opened menu, not fake controls.
    // Require the matching artifact and capability; never retarget a changing last take.
    let named = outcome.artifacts.take_id.is_some();
    let audio = named && outcome.artifacts.audio;
    let text = named && outcome.artifacts.text;
    if !delivered && !(copied && outcome.completeness == Completeness::Complete) {
        caption.action =
            if text && status.capabilities.copy && outcome.completeness == Completeness::Complete {
                "Cantrip actions → Copy this transcript"
            } else if audio && status.capabilities.recover && status.capabilities.local_model {
                "Cantrip actions → Recover locally to clipboard"
            } else if audio && status.capabilities.recover && status.capabilities.remote_configured
            {
                "Cantrip actions → Recover with configured provider to clipboard"
            } else if audio && !status.capabilities.local_model {
                "Cantrip actions → Install local model"
            } else if text && status.capabilities.copy {
                "Cantrip actions → Copy this transcript"
            } else if attention {
                "Cantrip actions → Check setup"
            } else {
                ""
            }
            .to_owned();
    }
    if attention && status.capabilities.dismiss {
        if !caption.action.is_empty() {
            caption.action.push('\n');
        }
        caption
            .action
            .push_str("Dismiss outcome keeps saved recordings.");
    }
    let dwell = if kind == Kind::Resolved && delivered && outcome.cleanup != Cleanup::Failed {
        if reduced_motion {
            SUCCESS_HOLD
        } else {
            SETTLE + SUCCESS_HOLD + RESULT_FADE
        }
    } else {
        NOTICE_HOLD
    };
    ResultView {
        event_id: Some(outcome.event_id),
        kind,
        caption,
        visibility: if attention {
            Visibility::Persistent
        } else {
            Visibility::Until(now + dwell)
        },
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrackColumn {
    // Signed distances from the center row: a completion stroke may lie wholly
    // above or below it without adding a second pixel representation.
    upper: f32,
    lower: f32,
    opacity: f32,
    center_opacity: f32,
}

impl TrackColumn {
    const fn centered(height: f32) -> Self {
        Self {
            upper: height / 2.0,
            lower: height / 2.0,
            opacity: 1.0,
            center_opacity: 1.0,
        }
    }

    const fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self.center_opacity = opacity;
        self
    }

    fn interpolate(self, to: Self, amount: f32) -> Self {
        Self {
            upper: self.upper + (to.upper - self.upper) * amount,
            lower: self.lower + (to.lower - self.lower) * amount,
            opacity: self.opacity + (to.opacity - self.opacity) * amount,
            center_opacity: self.center_opacity
                + (to.center_opacity - self.center_opacity) * amount,
        }
    }
}

type TrackFrame = [TrackColumn; CELLS];

struct TrackMotion {
    from: TrackFrame,
    to: TrackFrame,
    presented: TrackFrame,
    since: Instant,
    duration: Duration,
    signal_easing: bool,
}

impl TrackMotion {
    fn new(now: Instant) -> Self {
        let quiet = [TrackColumn::centered(2.0).with_opacity(0.3); CELLS];
        Self {
            from: quiet,
            to: quiet,
            presented: quiet,
            since: now,
            duration: Duration::ZERO,
            signal_easing: false,
        }
    }

    fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }

    fn target(
        &mut self,
        to: TrackFrame,
        now: Instant,
        duration: Duration,
        state_change: bool,
        reduced: bool,
    ) {
        // A phase/progress transition begins at the actual attached appearance,
        // including boundary-cell opacity, even if the prior phase was moving.
        self.from = if state_change {
            self.presented
        } else {
            self.frame(now)
        };
        self.to = to;
        self.since = now;
        self.duration = if reduced { Duration::ZERO } else { duration };
        self.signal_easing = !state_change;
    }

    fn frame(&self, now: Instant) -> TrackFrame {
        self.frame_toward(now, self.to)
    }

    fn frame_toward(&self, now: Instant, to: TrackFrame) -> TrackFrame {
        let elapsed = now.saturating_duration_since(self.since);
        if self.duration.is_zero() {
            return to;
        }
        if elapsed.is_zero() {
            return self.from;
        }
        if self.signal_easing {
            // Exponential release composes across repeated 100 ms measurements:
            // a new quiet sample cannot repeatedly restart an ease-in and pin peaks.
            let attack = (-3.0 * elapsed.as_secs_f32() / self.duration.as_secs_f32()).exp();
            let release = (-3.0 * elapsed.as_secs_f32() / WAVEFORM_RELEASE.as_secs_f32()).exp();
            let damp = |from: f32, to: f32, epsilon: f32| {
                let residual = (from - to) * if to >= from { attack } else { release };
                if residual.abs() < epsilon {
                    to
                } else {
                    to + residual
                }
            };
            return std::array::from_fn(|index| TrackColumn {
                upper: damp(self.from[index].upper, to[index].upper, 0.01),
                lower: damp(self.from[index].lower, to[index].lower, 0.01),
                opacity: damp(self.from[index].opacity, to[index].opacity, 1.0 / 255.0),
                center_opacity: damp(
                    self.from[index].center_opacity,
                    to[index].center_opacity,
                    1.0 / 255.0,
                ),
            });
        }
        if elapsed >= self.duration {
            return to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = t * t * (3.0 - 2.0 * t);
        std::array::from_fn(|index| self.from[index].interpolate(to[index], eased))
    }

    fn moving(&self, now: Instant) -> bool {
        self.from != self.to && self.frame(now) != self.to
    }
}

fn recording_frame(waveform: Option<AudioWaveform>) -> TrackFrame {
    fn extent(magnitude: u16) -> f32 {
        let amplitude = if magnitude <= 32 {
            0.0
        } else {
            (1.6 * f32::from(magnitude) / 32767.0)
                .clamp(0.0, 1.0)
                .sqrt()
        };
        1.0 + 15.5 * amplitude
    }

    waveform
        .unwrap_or([[0; 2]; AUDIO_WAVEFORM_BINS])
        .map(|[minimum, maximum]| {
            let upper = extent(maximum.max(0) as u16);
            let lower = extent(minimum.min(0).unsigned_abs());
            let opacity = 0.3 + 0.7 * (upper.max(lower) - 1.0) / 15.5;
            TrackColumn {
                upper,
                lower,
                opacity,
                center_opacity: opacity,
            }
        })
}

fn activity_frame(
    kind: Kind,
    age: Duration,
    progress: Option<(u32, u32)>,
    reduced: bool,
) -> TrackFrame {
    let seconds = if reduced { 0.0 } else { age.as_secs_f32() };
    let phase = seconds * std::f32::consts::TAU;
    if kind == Kind::Finishing {
        // Equal-height packets gather in mirrored pairs, then repeat. Nothing
        // accumulates or sweeps across a determinate center row.
        let mut frame = [TrackColumn::centered(13.0).with_opacity(0.0); CELLS];
        let midpoint = (CELLS / 6 - 1) as f32 / 2.0;
        for (group, columns) in frame.as_chunks_mut::<6>().0.iter_mut().enumerate() {
            let distance = (group as f32 - midpoint).abs() / midpoint;
            let pulse = if reduced {
                0.5
            } else {
                let wave = (1.0 + (phase / 1.8 + distance * std::f32::consts::PI).cos()) / 2.0;
                wave * wave * wave
            };
            columns[1..5].fill(TrackColumn::centered(13.0).with_opacity(0.22 + 0.7 * pulse));
        }
        return frame;
    }
    let filled = progress.map(completed_cells);
    std::array::from_fn(|index| {
        let position = index as f32 / (CELLS - 1) as f32;
        // Broad counter-moving lobes have a phrase-like rhythm and quiet edges,
        // rather than one bright cursor that could imply a completion front.
        let envelope = (std::f32::consts::PI * position).sin().powi(2);
        let forward = (1.0 + (std::f32::consts::TAU * position - phase / 2.6).cos()) / 2.0;
        let returning = (1.0 + (std::f32::consts::TAU * position + phase / 3.9).cos()) / 2.0;
        let energy = (0.2 + 0.8 * envelope) * (0.65 * forward + 0.35 * returning);
        let mut column =
            TrackColumn::centered(5.0 + 20.0 * energy).with_opacity(0.3 + 0.65 * energy);
        if let Some(filled) = filled {
            // The center row is the only determinate channel. Activity above and
            // below it remains free to flow without advancing the measured fill.
            column.center_opacity = if index < filled { 1.0 } else { 0.16 };
        }
        column
    })
}

fn resolved_frame() -> TrackFrame {
    // Two adjacent cells per column make a short downstroke and a longer rising
    // arm. The rest of the track disappears; a quiet baseline is not success.
    const ROWS: [f32; 10] = [0.0, 1.0, 2.0, 2.0, 1.0, 0.0, -1.0, -1.0, -2.0, -3.0];
    let mut frame = [TrackColumn::centered(CELL_SIZE).with_opacity(0.0); CELLS];
    let first = (CELLS - ROWS.len()) / 2;
    for (index, row) in ROWS.into_iter().enumerate() {
        frame[first + index] = TrackColumn {
            upper: CELL_SIZE / 2.0 - row * CELL_PITCH,
            lower: (row + 1.0) * CELL_PITCH + CELL_SIZE / 2.0,
            opacity: 1.0,
            center_opacity: 1.0,
        };
    }
    frame
}

fn completed_cells(progress: (u32, u32)) -> usize {
    let (completed, total) = progress;
    if total <= 1 || completed > total {
        return 0;
    }
    (u64::from(completed) * CELLS as u64 / u64::from(total)) as usize
}

fn format_elapsed(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

struct PollUpdate {
    status: std::result::Result<StatusSnapshot, ()>,
    palette: Palette,
    reduced_motion: Option<bool>,
}

struct Poller {
    receiver: mpsc::Receiver<PollUpdate>,
    stop: Arc<AtomicBool>,
}

impl Poller {
    fn start() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        thread::Builder::new()
            .name("cantrip-hud-status".to_owned())
            .spawn(move || {
                let mut palette = theme::load();
                let mut reduced_motion = desktop_reduced_motion();
                let mut preference_at = Instant::now();
                while !stopped.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    if started.duration_since(preference_at) >= PREFERENCE_INTERVAL {
                        palette = theme::load();
                        reduced_motion = desktop_reduced_motion();
                        preference_at = Instant::now();
                    }
                    let update = PollUpdate {
                        status: ipc::status().map_err(|_| ()),
                        palette,
                        reduced_motion,
                    };
                    match sender.try_send(update) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                    // A blocked IPC request or desktop preference provider never
                    // stalls Wayland dispatch or waveform settling.
                    thread::sleep(POLL_INTERVAL.saturating_sub(started.elapsed()));
                }
            })
            .context("starting HUD status reader")?;
        Ok(Self { receiver, stop })
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Query the preference belonging to the running desktop, not an unrelated
/// installed settings service. Unknown desktops can use the explicit config.
fn desktop_reduced_motion() -> Option<bool> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if desktop.split(':').any(|name| name == "hyprland") {
        let output = preference_output("hyprctl", &["-j", "getoption", "animations:enabled"])?;
        let value: serde_json::Value = serde_json::from_str(&output).ok()?;
        value
            .get("bool")
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                value
                    .get("int")
                    .and_then(serde_json::Value::as_i64)
                    .map(|value| value != 0)
            })
            .map(|enabled| !enabled)
    } else if desktop.split(':').any(|name| name == "gnome") {
        match preference_output(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "enable-animations"],
        )?
        .trim()
        {
            "false" => Some(true),
            "true" => Some(false),
            _ => None,
        }
    } else {
        None
    }
}

fn preference_output(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if !success {
        return None;
    }
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Run the native layer-shell HUD. Screenshot mode skips IPC and the instance
/// lock and renders a composed scenario, including deliberately aged transitions.
pub fn run(screenshot: Option<PathBuf>, state: Option<ScreenshotState>) -> Result<()> {
    let visual_proof = screenshot.is_some();
    let result = run_native(screenshot, state);
    if visual_proof {
        return result;
    }
    if let Err(error) = result {
        tracing::warn!("[HUD] unavailable: {error:#}");
    }
    Ok(())
}

fn run_native(screenshot: Option<PathBuf>, state: Option<ScreenshotState>) -> Result<()> {
    let _lock = if screenshot.is_some() {
        None
    } else {
        match acquire_instance_lock()? {
            Some(file) => Some(file),
            None => return Ok(()),
        }
    };
    let connection = Connection::connect_to_env().context("connecting HUD to Wayland")?;
    let (globals, mut queue) =
        registry_queue_init(&connection).context("reading Wayland globals")?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("binding HUD compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("binding HUD layer shell")?;
    let shm = Shm::bind(&globals, &qh).context("binding HUD shared memory")?;
    let pool = SlotPool::new((SURFACE_WIDTH * SURFACE_HEIGHT * 8) as usize, &shm)
        .context("allocating HUD buffer pool")?;
    let now = Instant::now();
    let model = if screenshot.is_some() {
        screenshot_model(state.unwrap_or(ScreenshotState::Recording), now)
    } else {
        Model::new(now)
    };
    let mut hud = HudState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        layer: None,
        layer_output: None,
        configured: false,
        visible: false,
        frame_pending: false,
        width: SURFACE_WIDTH,
        height: SURFACE_HEIGHT,
        requested_size: (SURFACE_WIDTH, SURFACE_HEIGHT),
        buffer_scale: 1,
        font: FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR)
            .context("loading HUD typeface")?,
        palette: theme::load(),
        model,
        last_render: None,
        last_refresh: now,
        last_status: now,
        screenshot,
        screenshot_at: now,
        screenshot_done: false,
        surface_lifecycle: SurfaceLifecycle::WaitingForOutput,
    };
    hud.create_layer(&qh)?;
    queue
        .roundtrip(&mut hud)
        .context("configuring HUD surface")?;
    let poller = if hud.screenshot.is_none() {
        Some(Poller::start()?)
    } else {
        None
    };
    loop {
        let now = Instant::now();
        if let Some(poller) = &poller {
            while let Ok(update) = poller.receiver.try_recv() {
                hud.palette = update.palette;
                if let Some(reduced) = update.reduced_motion {
                    hud.model.desktop_reduced_motion = reduced;
                }
                match update.status {
                    Ok(status) => hud.model.apply(status, now),
                    Err(()) => hud.model.disconnected(now),
                }
                hud.last_refresh = now;
                hud.last_status = now;
            }
            if now.duration_since(hud.last_refresh) >= POLL_INTERVAL {
                hud.model.refresh_connection(hud.last_status, now);
                hud.last_refresh = now;
            }
        }
        if hud.layer.is_none()
            && hud.surface_lifecycle.can_create()
            && hud.output_state.outputs().next().is_some()
        {
            hud.create_layer(&qh)?;
        }
        hud.redraw(&qh, now)?;
        if hud.screenshot_done {
            return Ok(());
        }
        if hud.screenshot.is_some()
            && now.duration_since(hud.screenshot_at) > Duration::from_secs(5)
        {
            anyhow::bail!("compositor did not configure the HUD screenshot surface");
        }
        let interval = if !hud.frame_pending && hud.model.animate(now) {
            FRAME_INTERVAL
        } else {
            POLL_INTERVAL
        };
        timed_dispatch(&mut queue, &mut hud, interval)?;
    }
}

fn timed_dispatch(
    queue: &mut EventQueue<HudState>,
    data: &mut HudState,
    timeout: Duration,
) -> Result<()> {
    queue.flush()?;
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(data)?;
        return Ok(());
    };
    let mut pollfd = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout.as_millis() as i32) };
    if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        return Err(std::io::Error::last_os_error()).context("polling HUD Wayland socket");
    }
    if ready > 0 && pollfd.revents & libc::POLLIN != 0 {
        guard.read().context("reading HUD Wayland events")?;
    } else {
        drop(guard);
    }
    queue.dispatch_pending(data)?;
    Ok(())
}

struct HudState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    layer_output: Option<wl_output::WlOutput>,
    configured: bool,
    visible: bool,
    frame_pending: bool,
    width: u32,
    height: u32,
    requested_size: (u32, u32),
    buffer_scale: u32,
    font: FontRef<'static>,
    palette: Palette,
    model: Model,
    last_render: Option<RenderKey>,
    last_refresh: Instant,
    last_status: Instant,
    screenshot: Option<PathBuf>,
    screenshot_at: Instant,
    screenshot_done: bool,
    surface_lifecycle: SurfaceLifecycle,
}

/// A compositor close is an instruction, not proof that an output vanished.
/// Only an actual output event can authorize another surface after closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceLifecycle {
    WaitingForOutput,
    Open,
    Closed,
}

impl SurfaceLifecycle {
    fn can_create(self) -> bool {
        self == Self::WaitingForOutput
    }
    fn opened(&mut self) {
        *self = Self::Open;
    }
    fn closed(&mut self) {
        *self = Self::Closed;
    }
    fn output_changed(&mut self) {
        *self = Self::WaitingForOutput;
    }
}

#[derive(PartialEq, Eq)]
struct RenderKey {
    kind: Option<Kind>,
    caption_revision: u64,
    interaction_event: Option<u64>,
    heights: [[i16; 2]; CELLS],
    opacities: [[u8; 2]; CELLS],
    alpha: u8,
    size: (u32, u32, u32),
    palette: Palette,
}

impl HudState {
    fn create_layer(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("cantrip-hud"),
            None,
        );
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_margin(0, 0, 36, 0);
        layer.set_size(self.requested_size.0, self.requested_size.1);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let empty = Region::new(&self.compositor).context("creating HUD pass-through region")?;
        layer.set_input_region(Some(empty.wl_region()));
        layer.commit();
        self.layer = Some(layer);
        self.surface_lifecycle.opened();
        self.layer_output = None;
        self.buffer_scale = 1;
        self.configured = false;
        self.visible = false;
        self.frame_pending = false;
        self.last_render = None;
        Ok(())
    }

    fn current_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.layer
            .as_ref()
            .is_some_and(|layer| layer.wl_surface() == surface)
    }

    fn available_width(&self) -> u32 {
        self.layer_output
            .as_ref()
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.logical_size)
            .map(|(width, _)| width.max(1) as u32)
            .unwrap_or(SURFACE_WIDTH)
            .min(SURFACE_WIDTH)
    }

    fn redraw(&mut self, qh: &QueueHandle<Self>, now: Instant) -> Result<()> {
        if !self.configured || self.frame_pending {
            return Ok(());
        }
        let Some(layer) = &self.layer else {
            return Ok(());
        };
        let model_now = if self.screenshot.is_some() {
            self.screenshot_at
        } else {
            now
        };
        let shown = self.model.kind.is_some();
        let logical_width = self.available_width().max(80);
        let container_width = CONTAINER_WIDTH.min(self.width.min(logical_width) as f32 - 12.0);
        let text_width = (container_width - 32.0).max(16.0);
        let interaction = self
            .model
            .interaction
            .as_ref()
            .map(|(text, _)| text.as_str());
        let primary_height = caption_height(&self.font, &self.model.caption, None, text_width);
        let feedback_height = if interaction.is_some() {
            caption_height(&self.font, &self.model.caption, interaction, text_width)
                .saturating_sub(primary_height)
        } else {
            0
        };
        let target_height = SURFACE_HEIGHT + primary_height + feedback_height;
        let desired = (logical_width, target_height);
        if self.requested_size != desired {
            self.requested_size = desired;
            layer.set_size(desired.0, desired.1);
            layer.commit();
            self.configured = false;
            // No old-size frame can become the final screenshot.
            return Ok(());
        }
        let heights = self.model.frame(model_now);
        let alpha = if self.screenshot.is_some() {
            1.0
        } else {
            self.model.alpha(now)
        };
        let key = RenderKey {
            kind: self.model.kind,
            caption_revision: self.model.caption_revision,
            interaction_event: self.model.interaction.as_ref().and(self.model.notice_event),
            heights: heights.map(|column| {
                [
                    (column.upper * 16.0).round() as i16,
                    (column.lower * 16.0).round() as i16,
                ]
            }),
            opacities: heights.map(|column| {
                [
                    (column.opacity * 255.0).round() as u8,
                    (column.center_opacity * 255.0).round() as u8,
                ]
            }),
            alpha: (alpha * 255.0).round() as u8,
            size: (self.width, self.height, self.buffer_scale),
            palette: self.palette,
        };
        if self.last_render.as_ref() == Some(&key)
            && shown == self.visible
            && self.screenshot.is_none()
        {
            return Ok(());
        }
        let width = self
            .width
            .checked_mul(self.buffer_scale)
            .context("HUD buffer width overflow")?;
        let height = self
            .height
            .checked_mul(self.buffer_scale)
            .context("HUD buffer height overflow")?;
        let stride = width.checked_mul(4).context("HUD buffer stride overflow")?;
        let (buffer, bytes) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .context("creating HUD frame")?;
        bytes.fill(0);
        if let Some(kind) = self.model.kind {
            let mut canvas = Canvas {
                bytes: &mut *bytes,
                width,
                height,
                scale: self.buffer_scale as f32,
                alpha: 1.0,
            };
            let left = (self.width as f32 - container_width) / 2.0;
            let top = 6.0;
            let body_height = self.height as f32 - 12.0;
            canvas.rect(
                left,
                top,
                container_width,
                body_height,
                self.palette.border,
                1.0,
            );
            canvas.rect(
                left + 1.0,
                top + 1.0,
                container_width - 2.0,
                body_height - 2.0,
                self.palette.surface,
                1.0,
            );
            if kind == Kind::Attention {
                canvas.rect(left, top, 2.0, body_height, self.palette.attention, 1.0);
            }
            let rgb = match kind {
                Kind::Recording | Kind::Working | Kind::Finishing => self.palette.accent,
                Kind::Attention => self.palette.attention,
                Kind::Resolved | Kind::Neutral => self.palette.foreground,
            };
            let track_width = TRACK_WIDTH.min(container_width - 32.0);
            let slot_width = track_width / CELLS as f32;
            let cell_width = slot_width * CELL_SIZE / CELL_PITCH;
            let track_left = (self.width as f32 - track_width) / 2.0;
            for (index, column) in heights.iter().copied().enumerate() {
                canvas.pixel_column(
                    track_left + index as f32 * slot_width + (slot_width - cell_width) / 2.0,
                    top + TRACK_HEIGHT / 2.0,
                    cell_width,
                    column,
                    rgb,
                );
            }
            let mut y = top + TRACK_HEIGHT;
            y += canvas.text(
                &self.font,
                &self.model.caption.title,
                left + 16.0,
                y,
                text_width,
                12.0,
                3,
                self.palette.foreground,
            );
            if !self.model.caption.detail.is_empty() {
                y += 3.0;
                y += canvas.text(
                    &self.font,
                    &self.model.caption.detail,
                    left + 16.0,
                    y,
                    text_width,
                    11.0,
                    3,
                    self.palette.foreground,
                );
            }
            if !self.model.caption.action.is_empty() {
                y += 7.0;
                y += canvas.text(
                    &self.font,
                    &self.model.caption.action,
                    left + 16.0,
                    y,
                    text_width,
                    10.0,
                    4,
                    if kind == Kind::Attention {
                        self.palette.attention
                    } else {
                        self.palette.foreground
                    },
                );
            }
            if let Some(interaction) = interaction {
                y += 7.0;
                canvas.rect(left + 16.0, y, text_width, 1.0, self.palette.border, 1.0);
                y += 6.0;
                canvas.text(
                    &self.font,
                    interaction,
                    left + 16.0,
                    y,
                    text_width,
                    11.0,
                    2,
                    self.palette.foreground,
                );
            }
        }
        // Fade the composed premultiplied frame once, not each overlapping
        // primitive; the resting surface is always fully opaque.
        if alpha < 1.0 {
            for byte in bytes.iter_mut() {
                *byte = (f32::from(*byte) * alpha).round() as u8;
            }
        }
        let _ = layer.set_buffer_scale(self.buffer_scale);
        layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(layer.wl_surface())
            .context("attaching HUD frame")?;
        if self.screenshot.is_none() {
            // Present at the compositor's cadence, never on buffer-release timing.
            layer
                .wl_surface()
                .frame(qh, FrameCallbackData(layer.wl_surface().clone()));
            self.frame_pending = true;
        }
        layer.commit();
        self.model.track.presented = heights;
        self.visible = shown;
        self.last_render = Some(key);
        // Keep a transparent mapped frame while idle: remapping requires a
        // second configure handshake some compositors do not send.
        if let Some(path) = &self.screenshot {
            save_screenshot(path, bytes, width, height)?;
            self.screenshot_done = true;
        }
        Ok(())
    }
}

impl CompositorHandler for HudState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if !self.current_surface(surface) {
            return;
        }
        let scale = factor.max(1) as u32;
        use wayland_client::Proxy;
        self.buffer_scale = if surface.version() >= 3 { scale } else { 1 };
        self.last_render = None;
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _transform: wl_output::Transform,
    ) {
        if self.current_surface(surface) {
            self.last_render = None;
        }
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if self.current_surface(surface) {
            self.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if self.current_surface(surface) {
            self.layer_output = Some(output.clone());
            self.last_render = None;
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // Keep the last output identity until it is destroyed or a new enter
        // arrives; wl_surface.leave often precedes output_destroyed.
    }
}

impl LayerShellHandler for HudState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.current_surface(layer.wl_surface()) {
            self.layer = None;
            self.configured = false;
            self.last_render = None;
            self.visible = false;
            self.surface_lifecycle.closed();
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if !self.current_surface(layer.wl_surface()) {
            return;
        }
        self.width = if configure.new_size.0 > 0 {
            configure.new_size.0
        } else {
            self.requested_size.0
        };
        self.height = if configure.new_size.1 > 0 {
            configure.new_size.1
        } else {
            self.requested_size.1
        };
        self.configured = true;
        self.last_render = None;
    }
}

impl OutputHandler for HudState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if self.layer.is_none() {
            self.surface_lifecycle.output_changed();
        }
    }
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.layer_output.as_ref() == Some(&output) {
            self.last_render = None;
            if self.layer.is_none() {
                self.surface_lifecycle.output_changed();
            }
        }
    }
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.layer_output.as_ref() == Some(&output) || self.layer.is_none() {
            self.layer = None;
            self.layer_output = None;
            self.configured = false;
            self.last_render = None;
            self.visible = false;
            self.surface_lifecycle.output_changed();
        }
    }
}

impl ShmHandler for HudState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
delegate_registry!(HudState);
impl ProvidesRegistryState for HudState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers![OutputState];
}
smithay_client_toolkit::delegate_dispatch2!(HudState);

struct Canvas<'a> {
    bytes: &'a mut [u8],
    width: u32,
    height: u32,
    scale: f32,
    alpha: f32,
}

impl Canvas<'_> {
    fn pixel(&mut self, x: u32, y: u32, rgb: [u8; 3], coverage: f32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let index = ((y * self.width + x) * 4) as usize;
        let alpha = coverage.clamp(0.0, 1.0) * self.alpha;
        for (channel, value) in rgb.into_iter().rev().enumerate() {
            self.bytes[index + channel] = (f32::from(value) * alpha
                + f32::from(self.bytes[index + channel]) * (1.0 - alpha))
                .round() as u8;
        }
        self.bytes[index + 3] =
            (255.0 * alpha + f32::from(self.bytes[index + 3]) * (1.0 - alpha)).round() as u8;
    }

    fn rect(&mut self, x: f32, y: f32, width: f32, height: f32, rgb: [u8; 3], alpha: f32) {
        let (left, top, right, bottom) = (
            x * self.scale,
            y * self.scale,
            (x + width) * self.scale,
            (y + height) * self.scale,
        );
        for py in top.floor().max(0.0) as u32..bottom.ceil().min(self.height as f32) as u32 {
            let vertical = (bottom.min(py as f32 + 1.0) - top.max(py as f32)).clamp(0.0, 1.0);
            for px in left.floor().max(0.0) as u32..right.ceil().min(self.width as f32) as u32 {
                let horizontal = (right.min(px as f32 + 1.0) - left.max(px as f32)).clamp(0.0, 1.0);
                self.pixel(px, py, rgb, horizontal * vertical * alpha);
            }
        }
    }

    fn aligned_rect(&mut self, x: f32, y: f32, width: f32, height: f32, rgb: [u8; 3], alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let left = (x * self.scale).round().max(0.0) as u32;
        let top = (y * self.scale).round().max(0.0) as u32;
        let right = ((x + width) * self.scale).round().min(self.width as f32) as u32;
        let bottom = ((y + height) * self.scale).round().min(self.height as f32) as u32;
        for py in top..bottom {
            for px in left..right {
                self.pixel(px, py, rgb, alpha);
            }
        }
    }

    fn pixel_column(&mut self, x: f32, center: f32, width: f32, column: TrackColumn, rgb: [u8; 3]) {
        let top = center - column.upper;
        let bottom = center + column.lower;
        if bottom - top < CELL_SIZE && column.upper >= 1.0 && column.lower >= 1.0 {
            let alpha = column.center_opacity;
            // Blend the 2 px quiet sliver into the center cell; rounding a
            // growing rectangle would switch an entire physical row on at once.
            let row_top = center - CELL_SIZE / 2.0;
            let coverage = ((row_top + CELL_SIZE).min(bottom) - row_top.max(top))
                .clamp(0.0, CELL_SIZE)
                / CELL_SIZE;
            let emergence = ((bottom - top - 2.0) / (CELL_SIZE - 2.0)).clamp(0.0, 1.0);
            let cell_alpha = alpha * coverage * emergence;
            self.aligned_rect(x, row_top, width, center - 1.0 - row_top, rgb, cell_alpha);
            self.aligned_rect(
                x,
                center - 1.0,
                width,
                2.0,
                rgb,
                cell_alpha + alpha * (1.0 - emergence),
            );
            self.aligned_rect(
                x,
                center + 1.0,
                width,
                row_top + CELL_SIZE - center - 1.0,
                rgb,
                cell_alpha,
            );
            return;
        }
        for row in -3..=3 {
            let row_top = center + row as f32 * CELL_PITCH - CELL_SIZE / 2.0;
            let coverage = ((row_top + CELL_SIZE).min(bottom) - row_top.max(top))
                .clamp(0.0, CELL_SIZE)
                / CELL_SIZE;
            // The fixed pixel fades as an edge crosses it; its geometry never snaps on/off.
            let alpha = if row == 0 {
                column.center_opacity
            } else {
                column.opacity
            };
            self.aligned_rect(x, row_top, width, CELL_SIZE, rgb, alpha * coverage);
        }
    }

    #[allow(clippy::too_many_arguments)] // Raster text geometry and font are explicit.
    fn text(
        &mut self,
        font: &FontRef<'_>,
        text: &str,
        x: f32,
        y: f32,
        width: f32,
        size: f32,
        max_lines: usize,
        rgb: [u8; 3],
    ) -> f32 {
        let max_chars = line_capacity(font, width, size);
        let font_size = size * self.scale;
        let scaled = font.as_scaled(font_size);
        let advance = scaled.h_advance(font.glyph_id('M'));
        let mut remaining = text.trim();
        let mut rows = 0;
        for row in 0..max_lines {
            if remaining.is_empty() {
                break;
            }
            let (line, rest) = wrap_line(remaining, max_chars);
            remaining = rest;
            let truncated = row + 1 == max_lines && !remaining.is_empty();
            let limit = if truncated {
                max_chars.saturating_sub(1)
            } else {
                max_chars
            };
            let mut cursor = x * self.scale;
            let baseline = (y + row as f32 * (size + 5.0)) * self.scale + scaled.ascent();
            for character in line.chars().take(limit).chain(truncated.then_some('…')) {
                let glyph = font
                    .glyph_id(character)
                    .with_scale_and_position(font_size, point(cursor, baseline));
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    outlined.draw(|gx, gy, coverage| {
                        let px = bounds.min.x as i32 + gx as i32;
                        let py = bounds.min.y as i32 + gy as i32;
                        if px >= 0 && py >= 0 {
                            self.pixel(px as u32, py as u32, rgb, coverage);
                        }
                    });
                }
                cursor += advance;
            }
            rows += 1;
        }
        rows as f32 * (size + 5.0)
    }
}

fn line_capacity(font: &FontRef<'_>, width: f32, size: f32) -> usize {
    (width / font.as_scaled(size).h_advance(font.glyph_id('M')))
        .floor()
        .max(1.0) as usize
}

fn wrap_line(text: &str, max_chars: usize) -> (&str, &str) {
    let mut space = None;
    for (count, (index, character)) in text.char_indices().enumerate() {
        if character == '\n' {
            return (&text[..index], text[index..].trim_start());
        }
        if count == max_chars {
            let end = space.filter(|index| *index > 0).unwrap_or(index);
            return (text[..end].trim_end(), text[end..].trim_start());
        }
        if character.is_whitespace() {
            space = Some(index);
        }
    }
    (text, "")
}

fn caption_height(
    font: &FontRef<'_>,
    caption: &Caption,
    interaction: Option<&str>,
    width: f32,
) -> u32 {
    let measure = |text: &str, size: f32, max_lines: usize| {
        let mut remaining = text.trim();
        let mut rows = 0;
        while rows < max_lines && !remaining.is_empty() {
            remaining = wrap_line(remaining, line_capacity(font, width, size)).1;
            rows += 1;
        }
        rows as f32 * (size + 5.0)
    };
    let mut height = measure(&caption.title, 12.0, 3);
    if !caption.detail.is_empty() {
        height += 3.0 + measure(&caption.detail, 11.0, 3);
    }
    if !caption.action.is_empty() {
        height += 7.0 + measure(&caption.action, 10.0, 4);
    }
    if let Some(text) = interaction {
        height += 14.0 + measure(text, 11.0, 2);
    }
    if height > 0.0 {
        height += 12.0;
    }
    height.ceil() as u32
}

fn save_screenshot(path: &Path, bytes: &[u8], width: u32, height: u32) -> Result<()> {
    // A shared-memory slot can be larger than the visible frame. The PNG
    // encoder requires exactly width × height pixels, not the slot's padding.
    let length = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("HUD screenshot size overflow")?;
    let bytes = bytes
        .get(..length)
        .context("HUD screenshot buffer is incomplete")?;
    let mut rgba = Vec::with_capacity(bytes.len());
    for pixel in bytes.as_chunks::<4>().0 {
        let (b, g, r, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        if a == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let un = |channel: u8| {
                (f32::from(channel) * 255.0 / f32::from(a))
                    .round()
                    .min(255.0) as u8
            };
            rgba.extend_from_slice(&[un(r), un(g), un(b), a]);
        }
    }
    image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)
        .with_context(|| format!("writing HUD screenshot {}", path.display()))?;
    eprintln!("saved HUD screenshot to {}", path.display());
    Ok(())
}

/// Deterministic, offline compositions rendered by the real Wayland surface.
/// These names are also the clap values of `cantrip hud --screenshot … --state …`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScreenshotState {
    Idle,
    Recording,
    LongRecording,
    Starting,
    MonitoringUnavailable,
    NoSignal,
    Working,
    Transcribing,
    ProgressZero,
    Progress,
    ProgressComplete,
    FinalizingAudio,
    RemovingRecording,
    Cleaning,
    Delivering,
    Cancelling,
    Sent,
    SentSettling,
    Copied,
    CopiedInstead,
    CleanupFallback,
    Empty,
    EmptySaved,
    Failed,
    StorageFailed,
    Partial,
    PartialSaved,
    DeliveryFailed,
    DeliveryUncertain,
    Deferred,
    Cancelled,
    Disconnected,
    Reconnected,
    SetupMissing,
    Busy,
    Recovery,
    ReducedMotion,
    ReducedMotionCleaning,
    ReducedMotionSent,
    Settling,
    Interrupted,
    Dismissed,
}

fn preview_snapshot(state: StateKind) -> StatusSnapshot {
    StatusSnapshot {
        epoch: "preview-epoch".to_owned(),
        operation_id: if state == StateKind::Idle {
            None
        } else {
            Some("preview-take".to_owned())
        },
        operation_kind: if state == StateKind::Idle {
            None
        } else {
            Some(ipc::OperationKind::Dictation)
        },
        state,
        elapsed: 0,
        signal: None,
        stage: None,
        outcome: None,
        notice: None,
        pending_recordings: 0,
        capabilities: ipc::Capabilities {
            stop: true,
            cancel: true,
            recover: true,
            copy: true,
            dismiss: true,
            local_model: true,
            remote_configured: false,
        },
        hud: crate::config::HudConfig::default(),
    }
}

fn preview_outcome(completeness: Completeness, delivery: Delivery) -> TerminalOutcome {
    TerminalOutcome {
        event_id: 1,
        operation_id: Some("preview-take".to_owned()),
        message: "Transcription failed.".to_owned(),
        completeness,
        delivery,
        cleanup: Cleanup::Off,
        error: None,
        artifacts: ipc::Artifacts::default(),
        dismissed: false,
    }
}

fn screenshot_model(state: ScreenshotState, now: Instant) -> Model {
    let start = now - Duration::from_secs(3);
    let mut model = Model::new(start);
    model.apply(preview_snapshot(StateKind::Idle), start);
    if state == ScreenshotState::Idle {
        return model;
    }
    let mut recording = preview_snapshot(StateKind::Recording);
    recording.elapsed = 9;
    recording.signal = Some(AudioSignal {
        level: 94,
        silent: false,
        waveform: SCREENSHOT_WAVEFORM,
    });
    match state {
        ScreenshotState::LongRecording => recording.elapsed = 888,
        ScreenshotState::Starting => {
            recording.signal = None;
            recording.elapsed = 1;
        }
        ScreenshotState::MonitoringUnavailable => {
            recording.signal = None;
            recording.elapsed = 8;
        }
        ScreenshotState::NoSignal => {
            recording.elapsed = 12;
            recording.signal = Some(AudioSignal {
                level: 0,
                silent: true,
                waveform: [[0; 2]; AUDIO_WAVEFORM_BINS],
            });
        }
        _ => {}
    }
    model.apply(recording, start);
    model.track.presented = model.track.frame(now);
    if matches!(
        state,
        ScreenshotState::Recording
            | ScreenshotState::LongRecording
            | ScreenshotState::Starting
            | ScreenshotState::MonitoringUnavailable
            | ScreenshotState::NoSignal
    ) {
        model.refresh(now);
        return model;
    }
    if matches!(
        state,
        ScreenshotState::Disconnected | ScreenshotState::Reconnected
    ) {
        model.disconnected(now - Duration::from_secs(1));
        model.refresh(now);
        if state == ScreenshotState::Reconnected {
            let mut restarted = preview_snapshot(StateKind::Idle);
            restarted.epoch = "preview-restarted".to_owned();
            model.apply(restarted, now);
        }
        model.track.since = now - SETTLE;
        return model;
    }
    let mut processing = preview_snapshot(StateKind::Processing);
    if state == ScreenshotState::Recovery {
        processing.operation_kind = Some(ipc::OperationKind::Recovery);
    }
    if state == ScreenshotState::RemovingRecording {
        processing.operation_kind = Some(ipc::OperationKind::Forget);
    }
    processing.stage = Some(match state {
        ScreenshotState::FinalizingAudio => Stage::FinalizingAudio,
        ScreenshotState::RemovingRecording => Stage::RemovingRecording,
        ScreenshotState::Cleaning | ScreenshotState::ReducedMotionCleaning => Stage::CleaningUp,
        ScreenshotState::Delivering => Stage::Delivering,
        ScreenshotState::Cancelling => Stage::Cancelling,
        ScreenshotState::ProgressZero => Stage::Transcribing {
            completed: 0,
            total: 30,
        },
        ScreenshotState::Progress
        | ScreenshotState::Busy
        | ScreenshotState::Recovery
        | ScreenshotState::ReducedMotion => Stage::Transcribing {
            completed: 12,
            total: 30,
        },
        ScreenshotState::ProgressComplete => Stage::Transcribing {
            completed: 30,
            total: 30,
        },
        _ => Stage::Transcribing {
            completed: 0,
            total: 1,
        },
    });
    processing.elapsed = if matches!(state, ScreenshotState::Working | ScreenshotState::Settling) {
        0
    } else {
        2
    };
    let reduced = matches!(
        state,
        ScreenshotState::ReducedMotion
            | ScreenshotState::ReducedMotionCleaning
            | ScreenshotState::ReducedMotionSent
    );
    processing.hud.reduced_motion = Some(reduced);
    if state == ScreenshotState::Busy {
        processing.notice = Some(ipc::InteractionNotice {
            event_id: 1,
            message: "Already working on this take.".to_owned(),
            error: None,
        });
    }
    let processing_at = match state {
        ScreenshotState::Settling => now - Duration::from_millis(60),
        ScreenshotState::Working => now - Duration::from_millis(400),
        ScreenshotState::Busy => now - Duration::from_secs(1),
        _ => now - Duration::from_secs(2),
    };
    model.apply(processing, processing_at);
    if state == ScreenshotState::Settling {
        model.track.presented = model.frame(now);
    }
    if matches!(
        state,
        ScreenshotState::Working
            | ScreenshotState::Transcribing
            | ScreenshotState::ProgressZero
            | ScreenshotState::Progress
            | ScreenshotState::ProgressComplete
            | ScreenshotState::FinalizingAudio
            | ScreenshotState::RemovingRecording
            | ScreenshotState::Cleaning
            | ScreenshotState::Delivering
            | ScreenshotState::Cancelling
            | ScreenshotState::Busy
            | ScreenshotState::Recovery
            | ScreenshotState::ReducedMotion
            | ScreenshotState::ReducedMotionCleaning
            | ScreenshotState::Settling
    ) {
        model.refresh(now);
        return model;
    }
    let (completeness, delivery) = match state {
        ScreenshotState::Empty | ScreenshotState::EmptySaved => {
            (Completeness::Empty, Delivery::None)
        }
        ScreenshotState::Failed
        | ScreenshotState::StorageFailed
        | ScreenshotState::SetupMissing
        | ScreenshotState::Dismissed => (Completeness::Failed, Delivery::None),
        ScreenshotState::Partial => (Completeness::Partial, Delivery::Copied),
        ScreenshotState::PartialSaved => (Completeness::Partial, Delivery::Deferred),
        ScreenshotState::DeliveryFailed => (Completeness::Complete, Delivery::Failed),
        ScreenshotState::DeliveryUncertain => (Completeness::Complete, Delivery::Uncertain),
        ScreenshotState::Deferred => (Completeness::Complete, Delivery::Deferred),
        ScreenshotState::Cancelled => (Completeness::Cancelled, Delivery::Cancelled),
        ScreenshotState::Copied | ScreenshotState::CopiedInstead => {
            (Completeness::Complete, Delivery::Copied)
        }
        _ => (Completeness::Complete, Delivery::Pasted),
    };
    let mut outcome = preview_outcome(completeness, delivery);
    outcome.message = match state {
        ScreenshotState::Copied => "Copied. Paste when ready.",
        ScreenshotState::CopiedInstead => "Copied instead. Paste when ready.",
        ScreenshotState::SetupMissing => "Local speech model unavailable.",
        ScreenshotState::StorageFailed => "Transcription failed. Audio could not be saved.",
        _ => "Transcription failed.",
    }
    .to_owned();
    outcome.artifacts = ipc::Artifacts {
        take_id: Some("preview-take".to_owned()),
        audio: matches!(
            state,
            ScreenshotState::Failed
                | ScreenshotState::Partial
                | ScreenshotState::PartialSaved
                | ScreenshotState::EmptySaved
                | ScreenshotState::Dismissed
        ),
        text: matches!(
            state,
            ScreenshotState::Partial
                | ScreenshotState::PartialSaved
                | ScreenshotState::DeliveryFailed
                | ScreenshotState::DeliveryUncertain
                | ScreenshotState::Deferred
        ),
    };
    if state == ScreenshotState::CleanupFallback {
        outcome.cleanup = Cleanup::Failed;
    }
    if state == ScreenshotState::Dismissed {
        outcome.dismissed = true;
    }
    let mut idle = preview_snapshot(StateKind::Idle);
    idle.hud.reduced_motion = Some(reduced);
    idle.pending_recordings = usize::from(outcome.artifacts.audio || outcome.artifacts.text);
    idle.outcome = Some(outcome);
    idle.capabilities.local_model = state != ScreenshotState::SetupMissing;
    let result_at = now
        - if matches!(
            state,
            ScreenshotState::SentSettling | ScreenshotState::Interrupted
        ) {
            Duration::from_millis(200)
        } else {
            SETTLE + Duration::from_millis(200)
        };
    // Terminal stills start from a presented processing frame, not an unseen
    // transcription target with the old listening pixels still cached.
    model.track.presented = model.frame(result_at);
    model.apply(idle, result_at);
    if state == ScreenshotState::Interrupted {
        let mut next = preview_snapshot(StateKind::Recording);
        next.operation_id = Some("next-preview-take".to_owned());
        next.signal = Some(AudioSignal {
            level: 94,
            silent: false,
            waveform: SCREENSHOT_WAVEFORM,
        });
        model.apply(next, now - Duration::from_millis(180));
    }
    model.refresh(now);
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(elapsed: u64, signal: Option<AudioSignal>) -> StatusSnapshot {
        let mut status = preview_snapshot(StateKind::Recording);
        status.elapsed = elapsed;
        status.signal = signal;
        status
    }

    fn signal(active: bool) -> AudioSignal {
        AudioSignal {
            level: if active { 94 } else { 0 },
            silent: !active,
            waveform: if active {
                SCREENSHOT_WAVEFORM
            } else {
                [[0; 2]; AUDIO_WAVEFORM_BINS]
            },
        }
    }

    #[test]
    fn silent_recording_stays_at_rest_from_its_first_frame() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(false))), now);
        let quiet = [TrackColumn::centered(2.0).with_opacity(0.3); CELLS];
        assert_eq!(model.track.frame(now), quiet);
        assert_eq!(model.track.frame(now + SETTLE / 2), quiet);
        model.apply(
            recording(1, Some(signal(false))),
            now + Duration::from_secs(1),
        );
        assert_eq!(model.track.frame(now + Duration::from_secs(1)), quiet);
    }

    fn raster_column(column: TrackColumn, scale: u32) -> Vec<u8> {
        let width = 8 * scale;
        let height = 44 * scale;
        let mut bytes = vec![0; (width * height * 4) as usize];
        Canvas {
            bytes: &mut bytes,
            width,
            height,
            scale: scale as f32,
            alpha: 1.0,
        }
        .pixel_column(2.25, 22.0, 3.0, column, [255; 3]);
        bytes
    }

    #[test]
    fn signed_columns_preserve_polarity_without_touching_neighbors() {
        let mut waveform = [[0; 2]; AUDIO_WAVEFORM_BINS];
        waveform[13] = [512, 8192];
        waveform[14] = [-8192, -512];
        let heights = recording_frame(Some(waveform));
        assert!(heights[13].upper > 8.75 && heights[13].upper < 16.5);
        assert_eq!(heights[13].lower, 1.0);
        assert_eq!(heights[14].upper, 1.0);
        assert_eq!(heights[14].lower, heights[13].upper);
        for (index, column) in heights.iter().enumerate() {
            if index != 13 && index != 14 {
                assert_eq!(*column, TrackColumn::centered(2.0).with_opacity(0.3));
            }
        }
        waveform[14] = [i16::MIN, i16::MAX];
        let louder_neighbor = recording_frame(Some(waveform));
        assert_eq!(louder_neighbor[13], heights[13]);
        assert_eq!(louder_neighbor[15], heights[15]);
    }

    #[test]
    fn raw_signal_floor_and_clipping_do_not_normalize_quiet_audio() {
        let quiet = [TrackColumn::centered(2.0).with_opacity(0.3); CELLS];
        assert_eq!(recording_frame(None), quiet);
        assert_eq!(
            recording_frame(Some([[-32, 32]; AUDIO_WAVEFORM_BINS])),
            quiet
        );
        let above_floor = recording_frame(Some([[-33, 33]; AUDIO_WAVEFORM_BINS]))[0];
        assert!(above_floor.upper > 1.0 && above_floor.upper < 2.0);
        assert_eq!(above_floor.upper, above_floor.lower);
        assert_eq!(
            recording_frame(Some([[i16::MIN, i16::MAX]; AUDIO_WAVEFORM_BINS])),
            [TrackColumn::centered(33.0); CELLS]
        );
    }

    #[test]
    fn moving_edges_fade_whole_fixed_pixels_at_each_buffer_scale() {
        for scale in [1, 2] {
            let s = scale as f32;
            let width = (8 * scale) as usize;
            let left = (2.25 * s).round() as usize;
            let right = (5.25 * s).round() as usize;
            let top = (15.5 * s).round() as usize;
            let bottom = (18.5 * s).round() as usize;
            for (upper, edge_alpha) in [(4.25, 64), (5.0, 128), (5.75, 191)] {
                let bytes = raster_column(
                    TrackColumn {
                        upper,
                        ..TrackColumn::centered(10.0)
                    },
                    scale,
                );
                let alpha = |x, y| bytes[(y * width + x) * 4 + 3];
                for y in top..bottom {
                    for x in left..right {
                        assert_eq!(alpha(x, y), edge_alpha);
                    }
                    assert_eq!(alpha(left - 1, y), 0);
                    assert_eq!(alpha(right, y), 0);
                }
                assert_eq!(alpha(left, (19.5 * s).round() as usize), 0);
                assert_eq!(alpha(left, (22.0 * s) as usize), 255);
                assert_eq!(alpha(left, (27.0 * s) as usize), 128);
                assert_eq!(alpha(left, (32.0 * s) as usize), 0);
            }
        }
    }

    #[test]
    fn quiet_and_full_scale_keep_their_pixel_geometry() {
        for scale in [1, 2] {
            let s = scale as f32;
            let width = (8 * scale) as usize;
            let x = (3 * scale) as usize;
            let quiet = raster_column(TrackColumn::centered(2.0), scale);
            let full = raster_column(TrackColumn::centered(33.0), scale);
            for y in 0..(44 * scale) as usize {
                let alpha = (y * width + x) * 4 + 3;
                let in_baseline = ((21 * scale) as usize..(23 * scale) as usize).contains(&y);
                assert_eq!(quiet[alpha], if in_baseline { 255 } else { 0 });
                let in_cell = [5.5, 10.5, 15.5, 20.5, 25.5, 30.5, 35.5]
                    .into_iter()
                    .any(|top| {
                        ((top * s).round() as usize..((top + 3.0) * s).round() as usize)
                            .contains(&y)
                    });
                assert_eq!(full[alpha], if in_cell { 255 } else { 0 });
            }
        }
    }

    #[test]
    fn quiet_onset_fades_into_the_center_cell_without_a_row_jump() {
        for scale in [1, 2] {
            let width = (8 * scale) as usize;
            let x = (3 * scale) as usize;
            let y = (23 * scale) as usize;
            let edge_alpha = |height| {
                raster_column(TrackColumn::centered(height), scale)[(y * width + x) * 4 + 3]
            };
            let steps = [2.0, 2.25, 2.5, 2.75, 3.0].map(edge_alpha);
            assert_eq!(steps[0], 0);
            assert_eq!(steps[4], 255);
            assert!(steps.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(255 - edge_alpha(2.99) <= 4);
        }
    }

    #[test]
    fn no_input_warning_is_initial_only_and_clears_with_hysteresis() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(false))), now);
        model.apply(
            recording(4, Some(signal(false))),
            now + Duration::from_secs(4),
        );
        assert_eq!(model.kind, Some(Kind::Recording));
        model.apply(
            recording(9, Some(signal(false))),
            now + Duration::from_secs(9),
        );
        assert_eq!(model.kind, Some(Kind::Attention));
        model.apply(
            recording(9, Some(signal(true))),
            now + Duration::from_millis(9100),
        );
        assert_eq!(model.kind, Some(Kind::Attention));
        model.apply(
            recording(9, Some(signal(true))),
            now + Duration::from_millis(9500),
        );
        assert_eq!(model.kind, Some(Kind::Recording));
        assert_eq!(
            model.caption,
            Caption::default(),
            "a cleared warning must not latch words"
        );
        model.apply(
            recording(40, Some(signal(false))),
            now + Duration::from_secs(40),
        );
        assert_eq!(
            model.kind,
            Some(Kind::Recording),
            "thinking after real input must not warn"
        );
    }

    #[test]
    fn startup_unknown_is_not_measured_capture() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, None), now);
        assert_eq!(model.kind, Some(Kind::Working));
        assert!(model.waveform.is_none());
        assert!(model.caption.title.is_empty());
        model.refresh(now + Duration::from_secs(1));
        assert_eq!(model.caption, Caption::default());
        model.refresh(now + MONITOR_DELAY);
        assert_eq!(model.kind, Some(Kind::Attention));
        assert!(
            !model.caption.action.is_empty(),
            "unconfirmed input needs a setup route"
        );
        model.apply(
            recording(6, Some(signal(true))),
            now + Duration::from_secs(6),
        );
        assert_eq!(model.kind, Some(Kind::Recording));
        assert_eq!(model.caption, Caption::default());
        model.apply(recording(7, None), now + Duration::from_secs(7));
        assert!(model.waveform.is_none());
        assert_eq!(model.kind, Some(Kind::Attention));
    }

    #[test]
    fn phase_changes_preserve_the_last_presented_pixels_and_can_be_interrupted() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let shown = model.frame(now + Duration::from_millis(55));
        model.track.presented = shown;
        let mut processing = preview_snapshot(StateKind::Processing);
        processing.stage = Some(Stage::Transcribing {
            completed: 0,
            total: 5,
        });
        let stop = now + Duration::from_millis(60);
        model.apply(processing.clone(), stop);
        assert_eq!(model.frame(stop), shown);
        let settling = model.frame(stop + SETTLE / 2);
        let destination = activity_frame(Kind::Working, SETTLE / 2, Some((0, 5)), false);
        for ((from, mid), to) in shown.into_iter().zip(settling).zip(destination) {
            assert!(mid.upper >= from.upper.min(to.upper) && mid.upper <= from.upper.max(to.upper));
            assert!(mid.lower >= from.lower.min(to.lower) && mid.lower <= from.lower.max(to.lower));
            assert!(
                mid.opacity >= from.opacity.min(to.opacity)
                    && mid.opacity <= from.opacity.max(to.opacity)
            );
        }
        let finishing = stop + Duration::from_secs(1);
        let flowing = model.frame(finishing - FRAME_INTERVAL);
        model.track.presented = flowing;
        processing.stage = Some(Stage::CleaningUp);
        model.apply(processing, finishing);
        assert_eq!(
            model.frame(finishing),
            flowing,
            "changing work rhythms must not pop"
        );
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle, finishing + SETTLE);
        assert_eq!(model.kind, Some(Kind::Resolved));
        let mut next = recording(0, Some(signal(true)));
        next.operation_id = Some("next".to_owned());
        model.apply(next, finishing + SETTLE + Duration::from_millis(1));
        assert_eq!(model.kind, Some(Kind::Recording));
        assert!(model.result.is_none());
    }

    #[test]
    fn short_speech_gaps_release_slowly_without_delaying_new_input() {
        let now = Instant::now();
        let mut track = TrackMotion::new(now);
        let full = recording_frame(Some([[i16::MIN, i16::MAX]; AUDIO_WAVEFORM_BINS]));
        let quiet = recording_frame(None);
        track.target(full, now, Duration::ZERO, false, true);
        track.target(quiet, now, WAVEFORM_EASE, false, false);
        assert_eq!(track.frame(now), full);
        let gap = now + POLL_INTERVAL;
        let released = track.frame(gap);
        assert!(
            released[0].upper > 8.0,
            "a 100 ms pause must retain a visible tail"
        );
        assert!(released[0].upper < full[0].upper);
        let mut previous = released[0].upper;
        for step in 1..=10 {
            let at = gap + POLL_INTERVAL * step;
            // Repeated quiet observations cannot restart a slow ease-in.
            track.target(quiet, at, WAVEFORM_EASE, false, false);
            let current = track.frame(at)[0].upper;
            assert!(current <= previous);
            previous = current;
        }
        assert_eq!(track.frame(now + Duration::from_secs(2)), quiet);

        track.target(quiet, now, Duration::ZERO, false, true);
        track.target(full, now, WAVEFORM_EASE, false, false);
        assert_eq!(track.frame(now), quiet);
        let attack = track.frame(gap);
        assert!(
            attack[0].upper > 15.0,
            "new speech must respond within one measurement"
        );
        track.target(quiet, gap, WAVEFORM_EASE, false, false);
        assert_eq!(
            track.frame(gap),
            attack,
            "retargeting must preserve continuity"
        );
    }

    #[test]
    fn listening_keeps_sampling_between_eases_but_freezes_on_disconnect() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let settled = now + SETTLE + POLL_INTERVAL;
        assert!(
            model.animate(settled),
            "the next microphone update must not wait through an idle poll"
        );
        model.disconnected(settled);
        assert!(!model.animate(settled), "lost input must not keep moving");
    }

    #[test]
    fn routine_stages_stay_wordless_regardless_of_elapsed_time() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(900, Some(signal(true))), now);
        assert_eq!(model.caption, Caption::default());
        let mut status = preview_snapshot(StateKind::Processing);
        for (index, stage) in [
            Stage::FinalizingAudio,
            Stage::Transcribing {
                completed: 0,
                total: 1,
            },
            Stage::Transcribing {
                completed: 29,
                total: 30,
            },
            Stage::CleaningUp,
            Stage::Delivering,
        ]
        .into_iter()
        .enumerate()
        {
            let at = now + Duration::from_secs((index as u64 + 1) * 60);
            status.stage = Some(stage);
            model.apply(status.clone(), at);
            model.refresh(at + Duration::from_secs(30));
            assert_eq!(model.caption, Caption::default());
        }
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle, now + Duration::from_secs(360));
        assert_eq!(model.kind, Some(Kind::Resolved));
        assert_eq!(model.caption, Caption::default());
    }

    #[test]
    fn flowing_activity_cannot_advance_measured_progress_or_outlive_its_connection() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Processing);
        status.stage = Some(Stage::Transcribing {
            completed: 12,
            total: 30,
        });
        model.apply(status.clone(), now);
        let first = model.frame(now + Duration::from_secs(1));
        let second = model.frame(now + Duration::from_secs(2));
        assert_ne!(
            first, second,
            "determinate work must still show fluid activity"
        );
        let center =
            |frame: &TrackFrame, index| raster_column(frame[index], 1)[(22 * 8 + 3) * 4 + 3];
        let completed = center(&first, 23);
        let pending = center(&first, 24);
        assert!(
            completed > pending,
            "the reported chunk boundary must remain visible"
        );
        assert_eq!(
            (center(&second, 23), center(&second, 24)),
            (completed, pending),
            "activity must not change either side of the measured boundary"
        );
        let before_report = model.frame(now + Duration::from_secs(2));
        model.apply(status.clone(), now + Duration::from_secs(2));
        assert_eq!(
            model.frame(now + Duration::from_secs(2)),
            before_report,
            "polling an unchanged chunk count must not restart the ripple"
        );
        model.track.presented = second;
        model.disconnected(now + Duration::from_secs(2));
        assert!(!model.animate(now + Duration::from_secs(2)));
        assert_eq!(model.frame(now + Duration::from_millis(2100)), second);
        status.stage = Some(Stage::CleaningUp);
        status.hud.reduced_motion = Some(true);
        model.apply(status, now + Duration::from_secs(3));
        assert!(
            model.progress.is_none(),
            "cleanup must discard transcription fill"
        );
        assert!(!model.animate(now + Duration::from_secs(4)));
        assert_eq!(
            model.frame(now + Duration::from_secs(4)),
            model.frame(now + Duration::from_secs(8))
        );
        assert_eq!(completed_cells((30, 30)), CELLS);
        assert_eq!(completed_cells((31, 30)), 0);
    }

    #[test]
    fn busy_feedback_never_replaces_or_replays_the_active_operation() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = recording(2, Some(signal(true)));
        model.apply(status.clone(), now);
        status.notice = Some(ipc::InteractionNotice {
            event_id: 10,
            message: "Busy".to_owned(),
            error: None,
        });
        model.apply(status.clone(), now + Duration::from_millis(100));
        assert_eq!(model.kind, Some(Kind::Recording));
        assert!(model.interaction.is_some());
        assert!(model.result.is_none());
        model.apply(status, now + Duration::from_secs(3));
        assert!(model.interaction.is_none());
        assert_eq!(model.kind, Some(Kind::Recording));
    }

    #[test]
    fn persistent_failure_dismisses_without_replaying_after_a_new_take() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut failed = preview_snapshot(StateKind::Idle);
        failed.outcome = Some(preview_outcome(Completeness::Failed, Delivery::None));
        model.apply(failed.clone(), now);
        model.refresh(now + Duration::from_secs(60));
        assert_eq!(model.kind, Some(Kind::Attention));
        failed.outcome.as_mut().expect("outcome").dismissed = true;
        model.apply(failed.clone(), now + Duration::from_secs(61));
        assert!(model.kind.is_none());
        let mut next = recording(0, Some(signal(true)));
        next.operation_id = Some("other-take".to_owned());
        model.apply(next, now + Duration::from_secs(62));
        failed.outcome.as_mut().expect("outcome").dismissed = false;
        model.apply(failed, now + Duration::from_secs(63));
        assert!(
            model.kind.is_none(),
            "cached old event is not completion of a new take"
        );
    }

    #[test]
    fn disconnect_coalesces_and_restart_does_not_replay_success() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut cached = preview_snapshot(StateKind::Idle);
        cached.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(cached.clone(), now);
        assert!(model.kind.is_none());
        model.apply(
            recording(2, Some(signal(true))),
            now + Duration::from_secs(1),
        );
        model.disconnected(now + Duration::from_secs(2));
        assert_eq!(model.kind, Some(Kind::Recording));
        model.refresh(now + Duration::from_secs(2) + DISCONNECT_DELAY);
        assert_eq!(model.kind, Some(Kind::Attention));
        cached.epoch = "restarted-epoch".to_owned();
        model.apply(cached, now + Duration::from_secs(3));
        assert_eq!(model.kind, Some(Kind::Neutral));
        assert!(!model.caption.title.is_empty());
    }

    #[test]
    fn cancellation_and_empty_stay_neutral_even_when_they_preserve_audio() {
        let now = Instant::now();
        for completeness in [Completeness::Empty, Completeness::Cancelled] {
            let mut status = preview_snapshot(StateKind::Idle);
            let mut outcome = preview_outcome(completeness, Delivery::None);
            outcome.artifacts = ipc::Artifacts {
                take_id: Some("saved".to_owned()),
                audio: true,
                text: false,
            };
            outcome.error = Some("not a reason to alarm on deliberate cancellation".to_owned());
            status.outcome = Some(outcome.clone());
            let view = present_outcome(&outcome, &status, now, false);
            assert_eq!(view.kind, Kind::Neutral);
            assert!(!matches!(view.visibility, Visibility::Persistent));
            assert!(
                !view.caption.action.is_empty(),
                "saved audio remains deliberately recoverable"
            );
        }
    }

    #[test]
    fn reduced_motion_preserves_measured_data_and_explicit_override_wins() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.desktop_reduced_motion = true;
        let mut status = recording(0, Some(signal(true)));
        model.apply(status.clone(), now);
        assert!(!model.animate(now));
        assert_eq!(
            model.track.frame(now),
            recording_frame(status.signal.map(|s| s.waveform))
        );
        status.hud.reduced_motion = Some(false);
        status.signal = Some(signal(false));
        model.apply(status, now + Duration::from_millis(100));
        assert!(model.animate(now + Duration::from_millis(100)));
    }

    #[test]
    fn labelled_success_is_short_and_an_idle_disconnect_cannot_pin_it() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut active = recording(0, Some(signal(true)));
        active.hud.labels = true;
        model.apply(active, now);
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.hud.labels = true;
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle, now + Duration::from_millis(100));
        assert_eq!(model.kind, Some(Kind::Resolved));
        assert!(!model.caption.title.is_empty());
        model.disconnected(now + Duration::from_millis(200));
        model.refresh(now + POLL_INTERVAL + SETTLE + SUCCESS_HOLD + RESULT_FADE);
        assert!(model.kind.is_none());
    }

    #[test]
    fn success_holds_after_settling_and_repeated_status_cannot_extend_it() {
        for reduced in [false, true] {
            let now = Instant::now();
            let mut model = Model::new(now);
            let mut active = recording(0, Some(signal(true)));
            active.hud.reduced_motion = Some(reduced);
            model.apply(active, now);
            let delivered = now + Duration::from_secs(1);
            model.track.presented = model.frame(delivered);
            let mut idle = preview_snapshot(StateKind::Idle);
            idle.hud.reduced_motion = Some(reduced);
            idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
            model.apply(idle.clone(), delivered);
            let settled = delivered + if reduced { Duration::ZERO } else { SETTLE };
            let held_frame = model.frame(settled);
            let hold_end = settled + SUCCESS_HOLD;
            model.apply(idle.clone(), hold_end - FRAME_INTERVAL);
            assert_eq!(model.kind, Some(Kind::Resolved));
            assert_eq!(model.frame(hold_end - FRAME_INTERVAL), held_frame);
            assert_eq!(
                model.alpha(hold_end - FRAME_INTERVAL),
                1.0,
                "the transition must not consume the settled hold"
            );
            if !reduced {
                let fading = hold_end + RESULT_FADE / 2;
                model.refresh(fading);
                let alpha = model.alpha(fading);
                assert!(alpha > 0.0 && alpha < 1.0);
            }
            let hidden = hold_end + if reduced { Duration::ZERO } else { RESULT_FADE };
            model.apply(idle, hidden);
            assert!(
                model.kind.is_none(),
                "unchanged snapshots must neither extend nor replay the result"
            );
        }
    }

    #[test]
    fn a_copy_finishing_between_polls_still_reports_its_new_outcome() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle.clone(), now + Duration::from_millis(100));
        let outcome = idle.outcome.as_mut().expect("terminal result");
        outcome.event_id = 2;
        outcome.operation_id = Some("copy-another-recording".to_owned());
        outcome.delivery = Delivery::Copied;
        outcome.message = "Copied. Paste when ready.".to_owned();
        model.apply(idle, now + Duration::from_millis(300));
        assert_eq!(model.kind, Some(Kind::Resolved));
        assert_eq!(
            model.result.as_ref().and_then(|result| result.event_id),
            Some(2)
        );
        assert!(!model.caption.title.is_empty());
    }

    #[test]
    fn fresh_idle_outcome_wins_when_the_last_observed_operation_was_different() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let mut idle = preview_snapshot(StateKind::Idle);
        let mut outcome = preview_outcome(Completeness::Complete, Delivery::Copied);
        outcome.operation_id = Some("unobserved-short-operation".to_owned());
        outcome.message = "Copied. Paste when ready.".to_owned();
        idle.outcome = Some(outcome);
        model.apply(idle.clone(), now + POLL_INTERVAL);
        assert_eq!(model.kind, Some(Kind::Resolved));
        let mut next = recording(0, Some(signal(true)));
        next.operation_id = Some("new-active-operation".to_owned());
        next.outcome = idle.outcome;
        model.apply(next, now + POLL_INTERVAL * 2);
        assert_eq!(model.kind, Some(Kind::Recording));
        assert!(model.result.is_none());
    }

    #[test]
    fn stalled_status_requests_stop_presenting_live_capture() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        model.refresh_connection(now, now + STATUS_STALE_AFTER);
        assert!(!model.animate(now + STATUS_STALE_AFTER));
        model.refresh_connection(now, now + STATUS_STALE_AFTER + DISCONNECT_DELAY);
        assert_eq!(model.kind, Some(Kind::Attention));
        assert!(model.waveform.is_none());
        assert!(model.progress.is_none());
        let restored = now + STATUS_STALE_AFTER + Duration::from_secs(1);
        model.apply(recording(3, Some(signal(true))), restored);
        assert_eq!(model.kind, Some(Kind::Recording));
        assert!(model.lost_since.is_none());
    }

    #[test]
    fn dismissing_a_notice_removes_feedback_without_hiding_the_take() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = recording(0, Some(signal(true)));
        model.apply(status.clone(), now);
        status.notice = Some(ipc::InteractionNotice {
            event_id: 1,
            message: "Busy".to_owned(),
            error: None,
        });
        model.apply(status.clone(), now + POLL_INTERVAL);
        assert!(model.interaction.is_some());
        status.notice = None;
        model.apply(status, now + POLL_INTERVAL * 2);
        assert!(model.interaction.is_none());
        assert_eq!(model.kind, Some(Kind::Recording));
    }

    #[test]
    fn compositor_close_waits_for_an_output_event_before_reopening() {
        let mut lifecycle = SurfaceLifecycle::Open;
        lifecycle.closed();
        assert!(
            !lifecycle.can_create(),
            "ordinary loop ticks must respect compositor closure"
        );
        lifecycle.output_changed();
        assert!(lifecycle.can_create(), "real hotplug permits a replacement");
        lifecycle.opened();
        assert!(
            !lifecycle.can_create(),
            "an existing surface must not be duplicated"
        );
    }

    #[test]
    fn screenshot_ignores_shared_memory_slot_padding() {
        let path =
            std::env::temp_dir().join(format!("cantrip-hud-screenshot-{}.png", std::process::id()));
        let mut bytes = [255_u8; 64];
        bytes[..4].copy_from_slice(&[10, 20, 30, 128]);
        save_screenshot(&path, &bytes, 1, 1).unwrap();
        let image = image::open(&path).unwrap().into_rgba8();
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.get_pixel(0, 0).0, [60, 40, 20, 128]);
        assert!(save_screenshot(&path, &bytes[..3], 1, 1).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn lock_is_exclusive_until_the_file_drops() {
        let path =
            std::env::temp_dir().join(format!("cantrip-hud-lock-test-{}", std::process::id()));
        let first = acquire_lock_on(&path).expect("first lock");
        assert!(first.is_some());
        assert!(acquire_lock_on(&path).expect("second lock").is_none());
        drop(first);
        let third = acquire_lock_on(&path).expect("reacquire lock");
        assert!(third.is_some());
        drop(third);
        let _ = fs::remove_file(path);
    }
}
