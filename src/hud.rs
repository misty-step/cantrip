//! Passive, bottom-anchored status instrument shared by every desktop.
//!
//! A signed pixel instrument, fluid work states, and words for actionable exceptions.
//! Above the field, one line names the take by the pipeline it started with, as
//! the engine reports it and never the current default: its mode's glyph, name
//! and promise, the stage at work while it is processed, and where it goes.
//! The engine owns operations, outcomes and acknowledgement. The HUD never sends
//! mutations, takes focus, handles pointer input, or invents audio/progress.
//! The model and software painter live here; `wayland` and `macos` only size,
//! place and present its pixels on their native surfaces.

use ab_glyph::{point, Font, FontRef, OutlinedGlyph, ScaleFont};
use anyhow::{Context, Result};
use cantrip_engine::{
    config::{Mode, PipelineDisplay},
    ipc::{
        self, AudioSignal, AudioWaveform, CaptureGuidance, CaptureIssue, Cleanup, Completeness,
        Delivery, KeychainAccess, KeychainAuthorization, StateKind, StatusSnapshot,
        TerminalOutcome, AUDIO_WAVEFORM_BINS, KEYCHAIN_PENDING_REASON,
    },
    pipeline::Stage,
};
use clap::ValueEnum;
use std::{
    borrow::Cow,
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::modes::{self, ModeView, Place, Step};
use crate::theme::{self, mix, Palette};

pub mod gallery;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "linux")]
mod wayland;

#[cfg(target_os = "macos")]
use macos as surface;
#[cfg(target_os = "linux")]
use wayland as surface;

/// Where captions send the operator for a deliberate action: the Linux Actions
/// window, or the macOS menu-bar menu whose items carry the same names.
#[cfg(target_os = "macos")]
macro_rules! deliberately {
    ($action:literal) => {
        concat!("Cantrip menu → ", $action)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! deliberately {
    ($action:literal) => {
        concat!("Cantrip actions → ", $action)
    };
}

#[cfg(target_os = "macos")]
const RECORDINGS_ACTION: &str = "Cantrip menu → Recordings and recovery";
#[cfg(not(target_os = "macos"))]
const RECORDINGS_ACTION: &str = "Cantrip → Recordings and recovery";

/// Capture guidance routes to Settings: the macOS menu's Open Settings item
/// beside the guidance, or the Linux actions window's Settings.
#[cfg(target_os = "macos")]
const SETTINGS_ACTION: &str = deliberately!("Open Settings");
#[cfg(not(target_os = "macos"))]
const SETTINGS_ACTION: &str = deliberately!("Settings");
/// Where a missing microphone is fixed: the macOS menu's Use item names the
/// default input the owner found at the press, beside Open Settings. Without
/// that name Settings is the only route, as it always is on Linux.
#[cfg(target_os = "macos")]
fn missing_microphone_action(default_input: Option<&str>) -> String {
    match default_input {
        Some(name) => format!("Cantrip menu → Use {name} instead, or Open Settings"),
        None => SETTINGS_ACTION.to_owned(),
    }
}
#[cfg(not(target_os = "macos"))]
fn missing_microphone_action(_default_input: Option<&str>) -> String {
    SETTINGS_ACTION.to_owned()
}

/// What the menu and the HUD say while the native Keychain prompt the
/// operator chose is open.
pub(crate) const KEYCHAIN_WAITING: &str = "Waiting for your Keychain decision…";
/// What that prompt's buttons mean for takes, said beside the menu's approval
/// item and on the HUD while the prompt waits.
pub(crate) const KEYCHAIN_BUTTONS: &str =
    "Always Allow lets takes use the key; Allow covers only this request.";

/// The cleanup credential as the HUD and the menu name it: the conventional
/// `openrouter` ID by its provider, any other configured ID as written.
pub(crate) fn keychain_key(key_id: &str) -> String {
    if key_id.eq_ignore_ascii_case("openrouter") {
        "your OpenRouter key".to_owned()
    } else {
        format!("your “{key_id}” key")
    }
}

/// The HUD and the menu when Keychain could not return the key for a reason
/// other than approval, such as a key that was never saved.
pub(crate) fn keychain_unavailable(key_id: &str) -> String {
    format!("Keychain didn’t return {}", keychain_key(key_id))
}

/// The menu item that asks macOS to let Cantrip read the key, named the same
/// by the HUD's route to it.
#[cfg(target_os = "macos")]
pub(crate) fn allow_keychain_item(key_id: &str) -> String {
    format!("Allow Cantrip to use {}", keychain_key(key_id))
}

/// Where Keychain access is allowed or retried while the owner still needs
/// it: the macOS menu item of the same name. Only macOS reports Keychain
/// access, so Linux names no route.
#[cfg(target_os = "macos")]
fn keychain_route(access: &KeychainAccess) -> Option<String> {
    match access.authorization {
        KeychainAuthorization::NeedsAuthorization => Some(format!(
            "Cantrip menu → {}",
            allow_keychain_item(&access.key_id)
        )),
        KeychainAuthorization::Unavailable => {
            Some(deliberately!("Try Keychain access again").to_owned())
        }
        KeychainAuthorization::Checking
        | KeychainAuthorization::Authorizing
        | KeychainAuthorization::Ready => None,
    }
}
#[cfg(not(target_os = "macos"))]
fn keychain_route(_access: &KeychainAccess) -> Option<String> {
    None
}

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const PREFERENCE_INTERVAL: Duration = Duration::from_secs(2);
const MONITOR_DELAY: Duration = Duration::from_secs(5);
const DISCONNECT_DELAY: Duration = Duration::from_millis(450);
const STATUS_STALE_AFTER: Duration = Duration::from_secs(2);
// Time to cover 95% of a measured change; release must outlive a short speech gap.
const WAVEFORM_EASE: Duration = POLL_INTERVAL;
const WAVEFORM_RELEASE: Duration = Duration::from_millis(420);
const LISTENING_ONSET: Duration = Duration::from_millis(280);
const SETTLE: Duration = Duration::from_millis(400);
const PROGRESS_REVEAL: Duration = Duration::from_millis(600);
const PHASE_DWELL: Duration = Duration::from_millis(180);
// One measured reveal and one cleanup transition, their settled dwells, and
// compositor-cadence margin. Success settling/holding/fading is additional.
const MAX_PRESENTATION_LAG: Duration = Duration::from_millis(1600);
// Full-opacity dwell after the completion grid has finished settling.
const SUCCESS_HOLD: Duration = Duration::from_millis(1200);
const ATTENTION_HOLD: Duration = Duration::from_secs(8);
const RESULT_FADE: Duration = Duration::from_millis(140);
const NOTICE_HOLD: Duration = Duration::from_secs(4);
const INTERACTION_HOLD: Duration = Duration::from_secs(2);
const SURFACE_WIDTH: u32 = 420;
const SURFACE_HEIGHT: u32 = 56;
const CONTAINER_WIDTH: f32 = 336.0;
const TRACK_HEIGHT: f32 = 44.0;
// Above the field, the take the instrument shows: toward a non-default target an
// upper-left "to <label>" row, then the line naming the take's pipeline. That
// target's color also takes the border, a surface wash and the active field.
const ROUTE_LABEL_SIZE: f32 = 11.0;
const ROUTE_ROW: f32 = ROUTE_LABEL_SIZE + 5.0;
const LINE_SIZE: f32 = 12.0;
const LINE_ROW: f32 = LINE_SIZE + 5.0;
/// One cell of a mode glyph, which sits a little taller than the line's capitals.
const GLYPH_CELL: f32 = 2.0;
const GLYPH_EXTENT: f32 = modes::GLYPH_SIZE as f32 * GLYPH_CELL;
/// A waiting stage's light, dimmer than a lit one and brighter than the field at rest.
const LIGHT_REST: f32 = 0.3;
const ROUTE_WASH: f32 = 0.12;
const ROUTE_LABEL_LIFT: f32 = 0.15;
const CELLS: usize = AUDIO_WAVEFORM_BINS;
const ROWS: usize = 7;
const CELL_SIZE: f32 = 3.0;
const CELL_PITCH: f32 = 5.0;
const GRID_HEIGHT: f32 = (ROWS - 1) as f32 * CELL_PITCH + CELL_SIZE;
// Every cell of the field is always painted. Active work raises cells above
// this floor; nothing ever changes the field's geometry to hide cells.
const FIELD_REST: f32 = 0.10;
// Cleanup-style flicker beyond measured fill; 0 at this many columns past the front.
const FRONTIER_RADIUS: f32 = 8.0;
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
    acquire_lock_on(&cantrip_engine::paths::hud_lock_path()?)
}

/// One process per lock file; `None` while another process holds it.
pub(crate) fn acquire_lock_on(path: &Path) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening instance lock {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(error).with_context(|| format!("locking instance file {}", path.display()))
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
struct Visibility(Instant);

impl Visibility {
    fn alpha(self, now: Instant, reduced: bool) -> Option<f32> {
        let until = self.0;
        (now < until).then(|| {
            if reduced {
                1.0
            } else {
                (until.duration_since(now).as_secs_f32() / RESULT_FADE.as_secs_f32()).min(1.0)
            }
        })
    }
}

struct ResultView {
    event_id: Option<u64>,
    kind: Kind,
    caption: Caption,
    visibility: Visibility,
    dwell: Duration,
    presented: bool,
    waiting_for_settle: bool,
    /// The outcome's non-default target; the default flow and notices have none.
    route: Option<ipc::Handoff>,
    /// Says Keychain access that needs the operator; withdrawn once that access
    /// settles on another answer.
    keychain: bool,
}

/// The take named above the field, rebuilt when its pipeline, target or stage
/// at work changes rather than per painted frame.
struct Header {
    /// The pipeline the take snapshotted when it started.
    pipeline: Option<PipelineDisplay>,
    work: Option<TakeWork>,
    line: Option<HeaderLine>,
    /// A non-default target, named "to <label>".
    handoff: Option<ipc::Handoff>,
    route: String,
}

impl Header {
    fn new(
        pipeline: Option<&PipelineDisplay>,
        work: Option<TakeWork>,
        handoff: Option<&ipc::Handoff>,
    ) -> Option<Self> {
        (pipeline.is_some() || handoff.is_some()).then(|| Self {
            line: pipeline.map(|pipeline| HeaderLine::new(pipeline, work)),
            pipeline: pipeline.cloned(),
            work,
            route: handoff
                .map(|handoff| format!("to {}", handoff.label))
                .unwrap_or_default(),
            handoff: handoff.cloned(),
        })
    }

    /// Logical height above the field: the target's row and the take's line.
    /// The first row also clears the field's top edge.
    fn height(&self) -> f32 {
        let route = if self.route.is_empty() {
            0.0
        } else {
            ROUTE_ROW
        };
        let line = if self.line.is_some() { LINE_ROW } else { 0.0 };
        route + line - 1.0
    }
}

/// What the instrument shows a take doing, as the engine reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TakeWork {
    /// This stage is at work.
    Step(Step),
    /// The take is being delivered; `refined` once its refinement was seen
    /// at work.
    Delivering { refined: bool },
}

/// One stage's light while its take is at work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Light {
    Done,
    AtWork,
    Waiting,
}

/// The line naming a take: its mode's glyph and name; its promise, or while it
/// is at work each stage's light and the stage at work; and at the right end
/// where it goes, each provider lit while it receives the take.
#[derive(Debug, PartialEq)]
struct HeaderLine {
    mode: Mode,
    name: &'static str,
    /// " · expert rewrite".
    promise: String,
    lights: Vec<Light>,
    working: Option<&'static str>,
    /// "on this Mac", or "↗ " then each provider, with whether it is lit.
    privacy: Vec<(String, bool)>,
}

impl HeaderLine {
    fn new(pipeline: &PipelineDisplay, work: Option<TakeWork>) -> Self {
        let view = ModeView::new(pipeline);
        let steps = view.steps();
        let (lights, working, sending) = match work {
            None => (Vec::new(), None, None),
            Some(TakeWork::Step(step)) => {
                let at = steps.iter().position(|candidate| *candidate == step);
                let lights = (0..steps.len())
                    .map(|index| match at {
                        Some(at) if index < at => Light::Done,
                        Some(at) if index == at => Light::AtWork,
                        _ => Light::Waiting,
                    })
                    .collect();
                (lights, Some(step.working()), Some(step))
            }
            Some(TakeWork::Delivering { refined }) => {
                let lights = steps
                    .iter()
                    .map(|step| {
                        if *step != Step::Refine || refined {
                            Light::Done
                        } else {
                            Light::Waiting
                        }
                    })
                    .collect();
                (lights, Some("delivering…"), None)
            }
        };
        let providers = view.providers();
        let privacy = if providers.is_empty() {
            vec![(format!("on {}", modes::THIS_DEVICE), false)]
        } else {
            let lit = |place: &Place| sending.is_some_and(|step| view.sends(step, place));
            let mut spans = vec![("↗ ".to_owned(), providers.iter().any(lit))];
            for (index, place) in providers.iter().enumerate() {
                if index > 0 {
                    spans.push((", ".to_owned(), false));
                }
                spans.push((place.name().into_owned(), lit(place)));
            }
            spans
        };
        Self {
            mode: view.mode(),
            name: view.name(),
            promise: format!(" · {}", modes::promise(view.mode()).to_lowercase()),
            lights,
            working,
            privacy,
        }
    }
}

/// What a header line shows in its middle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Middle {
    Nothing,
    Promise,
    Working { lights: bool },
}

/// The parts of a header line that fit, in priority order: the glyph and mode,
/// shortened only on a surface too narrow for its name; then where the take
/// goes, shortened only if it must be; then the stage at work and its lights,
/// or the promise.
#[derive(Debug, PartialEq)]
struct LineFit {
    /// Characters of the mode's name shown, the last an ellipsis when short.
    name: usize,
    middle: Middle,
    /// Characters of the privacy cue shown, the last an ellipsis when short.
    privacy: usize,
}

impl LineFit {
    fn new(line: &HeaderLine, advance: f32, width: f32) -> Self {
        let span = |text: &str| text.chars().count() as f32 * advance;
        let characters = |room: f32| (room / advance).floor().max(0.0) as usize;
        let name = line
            .name
            .chars()
            .count()
            .min(characters(width - GLYPH_EXTENT - advance));
        let lead = GLYPH_EXTENT + advance + name as f32 * advance;
        let gap = 2.0 * advance;
        let privacy: usize = line
            .privacy
            .iter()
            .map(|(text, _)| text.chars().count())
            .sum();
        let privacy = privacy.min(characters(width - lead - gap));
        let free = width - lead - gap - privacy as f32 * advance;
        let middle = match line.working {
            None if span(&line.promise) <= free => Middle::Promise,
            None => Middle::Nothing,
            Some(working) => {
                let word = advance + span(working);
                if line.lights.len() > 1 && word + lights_width(line.lights.len()) <= free {
                    Middle::Working { lights: true }
                } else if word <= free {
                    Middle::Working { lights: false }
                } else {
                    Middle::Nothing
                }
            }
        };
        Self {
            name,
            middle,
            privacy,
        }
    }
}

/// `text`'s first `shown` characters, the last an ellipsis when that is fewer
/// than all of them.
fn shortened(text: &str, shown: usize) -> impl Iterator<Item = char> + '_ {
    let short = shown < text.chars().count();
    text.chars()
        .take(shown - usize::from(short && shown > 0))
        .chain((short && shown > 0).then_some('…'))
}

/// The stage lights' width, with the space after them.
fn lights_width(count: usize) -> f32 {
    count as f32 * CELL_PITCH - (CELL_PITCH - CELL_SIZE) + CELL_PITCH
}

/// What one take's input showed: whether the owner ever measured it, and the
/// owner's silence guidance while it lasts, kept for that take's outcome. The
/// owner alone decides when measured input counts as silent.
#[derive(Default)]
struct SignalHistory {
    monitored: bool,
    silence: Option<CaptureGuidance>,
}

impl SignalHistory {
    fn update(&mut self, status: &StatusSnapshot) {
        self.monitored |= status.signal.is_some();
        self.silence = status
            .capture_guidance
            .as_ref()
            .filter(|guidance| guidance.kind != CaptureIssue::MissingMicrophone)
            .cloned();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WorkPhase {
    Finalizing,
    Transcribing,
    Cleaning,
    Delivering,
}

impl WorkPhase {
    fn from_stage(stage: Option<&Stage>) -> Option<Self> {
        match stage? {
            Stage::FinalizingAudio => Some(Self::Finalizing),
            Stage::Transcribing { .. } => Some(Self::Transcribing),
            Stage::CleaningUp => Some(Self::Cleaning),
            Stage::Delivering => Some(Self::Delivering),
            _ => None,
        }
    }

    fn kind(self) -> Kind {
        match self {
            Self::Finalizing | Self::Transcribing => Kind::Working,
            Self::Cleaning | Self::Delivering => Kind::Finishing,
        }
    }

    fn caption(self, progress: Option<(u32, u32)>, recovery: bool) -> Caption {
        Caption::title(match self {
            Self::Finalizing => Cow::Borrowed("Finishing recording…"),
            Self::Cleaning => Cow::Borrowed("Finishing text…"),
            Self::Delivering => Cow::Borrowed("Delivering text…"),
            Self::Transcribing => {
                let verb = if recovery {
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
        })
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }

    fn next(pending: u8) -> Option<Self> {
        [
            Self::Finalizing,
            Self::Transcribing,
            Self::Cleaning,
            Self::Delivering,
        ]
        .into_iter()
        .find(|phase| pending & phase.bit() != 0)
    }
}

// Coalesce measurements into one target and remember only which normal phases
// were observed. No status queue, inferred phases, or worker-side waiting.
struct WorkPresentation {
    phase: WorkPhase,
    latest: WorkPhase,
    pending: u8,
    progress: Option<(u32, u32)>,
    recovery: bool,
    ready_at: Instant,
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
    work: Option<WorkPresentation>,
    header: Option<Header>,
    header_revision: u64,
    /// The pipeline the watched take snapshotted, kept through its result.
    take_pipeline: Option<PipelineDisplay>,
    /// The engine reported the watched take's refinement at work.
    take_refined: bool,
    /// The owner's last settled Keychain answer, so a recheck that reaches the
    /// same answer is not said again.
    keychain: Option<KeychainAccess>,
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
            work: None,
            header: None,
            header_revision: 0,
            take_pipeline: None,
            take_refined: false,
            keychain: None,
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
        let new_operation = active
            && (epoch_changed
                || !was_active
                || self.operation != status.operation_id
                || status.state == StateKind::Recording && phase_changed);
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
        if epoch_changed || new_operation {
            self.work = None;
            self.track.reset(now);
            self.kind = None;
            self.waveform = None;
            self.progress = None;
        }
        if new_operation {
            self.operation = status.operation_id.clone();
            self.phase_since = now;
            self.signal = SignalHistory::default();
            self.result = None;
            self.interaction = None;
            self.elapsed_value = None;
            self.take_refined = false;
        } else if phase_changed {
            self.phase_since = now;
        }
        if active && status.stage == Some(Stage::CleaningUp) {
            self.take_refined = true;
        }
        if active {
            self.result = None;
        }
        if matches!(status.state, StateKind::Recording) {
            self.signal.update(&status);
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
                // Present cached attention once on attach/restart; seed other
                // cached outcomes silently. Fresh idle events are authoritative
                // even if their work happened between polls; active work wins.
                let initial = first || epoch_changed;
                if !active && !outcome.dismissed && (!initial || attention_outcome(outcome)) {
                    let mut result = present_outcome(outcome, &status, now, self.reduced_motion);
                    // A silent take's empty result names the input it heard
                    // nothing from. Idle owners report no guidance for it, so
                    // the route is the general Settings action.
                    if let Some(guidance) = self.signal.silence.as_ref().filter(|_| {
                        !initial && belongs && outcome.completeness == Completeness::Empty
                    }) {
                        result.caption.title = guidance.message();
                        result.caption.detail = if outcome.artifacts.audio {
                            "Audio saved."
                        } else {
                            ""
                        }
                        .to_owned();
                        if result.caption.action.is_empty() {
                            result.caption.action = deliberately!("Settings").to_owned();
                        }
                    }
                    self.result = Some(result);
                }
            }
            // A shown attention outcome names only the actions available now:
            // its routes follow the owner's capabilities and Keychain access.
            if !fresh
                && attention_outcome(outcome)
                && self.snapshot.as_ref().is_some_and(|old| {
                    old.capabilities != status.capabilities || old.keychain != status.keychain
                })
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
                    action: RECORDINGS_ACTION.to_owned(),
                },
                visibility: Visibility(now + NOTICE_HOLD),
                dwell: NOTICE_HOLD,
                presented: false,
                waiting_for_settle: false,
                route: None,
                keychain: false,
            });
        }
        // Keychain access is said once each time it settles on an answer that
        // needs the operator: at launch, after a mode change or reload, or after
        // the operator answers its prompt. A recheck reaching the same answer
        // says nothing, and a take or a result already shown says it instead.
        if epoch_changed {
            self.keychain = None;
        }
        let checking = status
            .keychain
            .as_ref()
            .is_some_and(|access| access.authorization == KeychainAuthorization::Checking);
        if !checking && self.keychain != status.keychain {
            self.keychain = status.keychain.clone();
            if self.result.as_ref().is_some_and(|result| result.keychain) {
                self.result = None;
            }
            if !active && self.result.is_none() {
                if let Some(caption) = status.keychain.as_ref().and_then(keychain_notice) {
                    let dwell = ATTENTION_HOLD
                        + if self.reduced_motion {
                            Duration::ZERO
                        } else {
                            RESULT_FADE
                        };
                    self.result = Some(ResultView {
                        event_id: None,
                        kind: Kind::Attention,
                        caption,
                        visibility: Visibility(now + dwell),
                        dwell,
                        presented: false,
                        waiting_for_settle: false,
                        route: None,
                        keychain: true,
                    });
                }
            }
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
        self.observe_work(&status, now);
        self.snapshot = Some(status);
        self.refresh(now);
    }

    /// The header names only the take on the instrument: while it records or
    /// processes, its snapshotted pipeline, the stage at work and any
    /// non-default target; then its outcome's target, and its pipeline while
    /// that outcome is the one shown. An idle engine reports the configured
    /// default, which never relabels a take. Reconciled on every refresh so an
    /// expiring result cannot leave its tint or label on later idle feedback.
    fn sync_header(&mut self) {
        let active = self.active();
        if active {
            let snapshotted = self.snapshot.as_ref().and_then(take_pipeline);
            if self.take_pipeline.as_ref() != snapshotted {
                self.take_pipeline = snapshotted.cloned();
            }
        }
        let shown = active
            || self.work.is_some()
            || self
                .result
                .as_ref()
                .is_some_and(|result| self.outcome_of_take(result));
        let pipeline = self.take_pipeline.as_ref().filter(|_| shown);
        let work = pipeline.and(self.take_work());
        let handoff = if active {
            self.snapshot
                .as_ref()
                .and_then(|status| status.handoff.as_ref())
        } else {
            self.result
                .as_ref()
                .and_then(|result| result.route.as_ref())
        };
        let current = self.header.as_ref().map_or((None, None, None), |header| {
            (
                header.pipeline.as_ref(),
                header.work,
                header.handoff.as_ref(),
            )
        });
        if current != (pipeline, work, handoff) {
            self.header = Header::new(pipeline, work, handoff);
            self.header_revision = self.header_revision.wrapping_add(1);
        }
    }

    /// What the instrument shows its take doing while the engine processes
    /// it: the work phase it presents, or while no presentation paces the work,
    /// the stage the engine reports. A finished take is at rest, and finishing
    /// the recording is not yet a stage of the pipeline.
    fn take_work(&self) -> Option<TakeWork> {
        let status = self
            .snapshot
            .as_ref()
            .filter(|status| status.state == StateKind::Processing)?;
        let phase = match &self.work {
            Some(work) => work.phase,
            None => WorkPhase::from_stage(status.stage.as_ref())?,
        };
        match phase {
            WorkPhase::Finalizing => None,
            WorkPhase::Transcribing => Some(TakeWork::Step(Step::Transcribe)),
            WorkPhase::Cleaning => Some(TakeWork::Step(Step::Refine)),
            WorkPhase::Delivering => Some(TakeWork::Delivering {
                refined: self.take_refined,
            }),
        }
    }

    /// The presented result is the outcome of the take this instrument watched.
    fn outcome_of_take(&self, result: &ResultView) -> bool {
        result.event_id.is_some()
            && self
                .snapshot
                .as_ref()
                .and_then(|status| status.outcome.as_ref())
                .is_some_and(|outcome| {
                    result.event_id == Some(outcome.event_id)
                        && outcome.operation_id == self.operation
                })
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
            if self.work.take().is_some() {
                self.lost_active = true;
                // An outcome received during a cosmetic handoff must never
                // reappear as a fresh success after connection recovery.
                self.result = None;
            }
            if self
                .result
                .as_ref()
                .is_some_and(|result| result.waiting_for_settle)
            {
                self.result = None;
            }
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
        if let Some(result) = &mut self.result {
            if result.presented
                && result.waiting_for_settle
                && self.kind == Some(Kind::Resolved)
                && now >= self.track.since + self.track.duration
                && self
                    .track
                    .presented
                    .columns
                    .iter()
                    .all(TrackColumn::fully_lit)
            {
                result.waiting_for_settle = false;
                result.visibility = Visibility(
                    now + SUCCESS_HOLD
                        + if self.reduced_motion {
                            Duration::ZERO
                        } else {
                            RESULT_FADE
                        },
                );
            }
        }
        if self.result.as_ref().is_some_and(|result| {
            result.presented
                && !result.waiting_for_settle
                && result.visibility.alpha(now, self.reduced_motion).is_none()
        }) {
            self.result = None;
        }
        self.sync_header();
        if let Some(since) = self.lost_since {
            if self.lost_active && now.duration_since(since) >= DISCONNECT_DELAY {
                self.set_composition(
                    Some(Kind::Attention),
                    Caption {
                        title: "Cantrip connection lost".to_owned(),
                        detail: "Recording and saved-audio status unknown.".to_owned(),
                        action: deliberately!("Check setup").to_owned(),
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
        let presenting = self.present_work(now);
        // Presenting the work may have advanced or ended its stage.
        self.sync_header();
        if presenting {
            return;
        }
        let Some(status) = &self.snapshot else {
            return;
        };
        let phase_age = now
            .duration_since(self.phase_since)
            .max(Duration::from_secs(status.elapsed));
        match &status.state {
            StateKind::Recording => {
                // The owner's silence guidance names the input and the route to
                // Settings without stopping capture; Stop and Cancel stay as they are.
                let (kind, caption) = if let Some(guidance) = &self.signal.silence {
                    (Kind::Attention, guidance_caption(guidance))
                } else if status.signal.is_none() {
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
                                action: deliberately!("Settings").to_owned(),
                            }
                        } else if status.hud.labels {
                            Caption::title("Starting microphone…")
                        } else {
                            Caption::default()
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
                let phase = WorkPhase::from_stage(stage);
                let caption = match stage {
                    Some(Stage::RequestingMicrophone) => match status.microphone.as_ref() {
                        Some(access) => Caption {
                            title: format!(
                                "Allow microphone access for {} to start recording…",
                                access.grantee.name()
                            ),
                            detail: access.grantee.disclosure().unwrap_or_default(),
                            ..Caption::default()
                        },
                        None => Caption::title("Allow microphone access to start recording…"),
                    },
                    Some(Stage::Cancelling) => {
                        Caption::title("Cancelling… Waiting for current work.")
                    }
                    Some(Stage::RemovingRecording) => Caption::title("Removing saved audio…"),
                    _ if status.hud.labels => phase.map_or_else(
                        || Caption::title("Working…"),
                        |phase| {
                            phase.caption(
                                progress,
                                status.operation_kind == Some(ipc::OperationKind::Recovery),
                            )
                        },
                    ),
                    _ => Caption::default(),
                };
                let kind = phase.map_or(Kind::Working, WorkPhase::kind);
                self.set_composition(Some(kind), caption, progress, None, now);
            }
            StateKind::Idle => {
                if let Some(result) = &mut self.result {
                    if !result.presented {
                        result.presented = true;
                        result.visibility = Visibility(now + result.dwell);
                    }
                    let kind = result.kind;
                    let caption = result.caption.clone();
                    self.set_composition(Some(kind), caption, None, None, now);
                } else if let Some(caption) = status.keychain.as_ref().and_then(keychain_waiting) {
                    // The menu closed on its choice; until the operator answers
                    // the native prompt, say what its buttons mean for takes.
                    self.set_composition(Some(Kind::Neutral), caption, None, None, now);
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
                        action: deliberately!("Check setup").to_owned(),
                    },
                    None,
                    None,
                    now,
                );
            }
        }
    }

    fn observe_work(&mut self, status: &StatusSnapshot, now: Instant) {
        let phase = (status.state == StateKind::Processing)
            .then(|| WorkPhase::from_stage(status.stage.as_ref()))
            .flatten();
        let successful = status.state == StateKind::Idle
            && self.result.is_some()
            && status.outcome.as_ref().is_some_and(|outcome| {
                !outcome.dismissed
                    && outcome.operation_id == self.operation
                    && outcome.completeness == Completeness::Complete
                    && matches!(
                        outcome.delivery,
                        Delivery::Typed | Delivery::Pasted | Delivery::Copied | Delivery::HandedOff
                    )
                    && !matches!(outcome.cleanup, Cleanup::Failed | Cleanup::KeychainPending)
            });
        if self.reduced_motion || phase.is_none() && !successful {
            self.work = None;
            return;
        }
        if let Some(phase) = phase {
            let progress = status.stage.as_ref().and_then(Stage::measured_progress);
            let reset = self.work.as_ref().is_some_and(|work| {
                phase < work.latest
                    || phase == WorkPhase::Transcribing
                        && work.latest == phase
                        && match (work.progress, progress) {
                            (Some((old, total)), Some((new, next_total))) => {
                                total != next_total || new < old
                            }
                            (Some(_), None) => true,
                            _ => false,
                        }
            });
            if reset {
                // A new/reset pass cannot inherit completed pixels or pending
                // later phases from the old pass, even within one operation.
                self.work = None;
                self.track.reset(now);
                self.kind = None;
                self.progress = None;
            }
            let work = self.work.get_or_insert_with(|| WorkPresentation {
                phase,
                latest: phase,
                pending: 0,
                progress,
                recovery: status.operation_kind == Some(ipc::OperationKind::Recovery),
                ready_at: now + PHASE_DWELL,
            });
            if work.phase == WorkPhase::Finalizing && phase == WorkPhase::Transcribing {
                // The centered working band already represents transcription
                // preparation; measured work does not pay for a second hold.
                work.phase = phase;
            } else if phase > work.phase {
                work.pending |= phase.bit();
            }
            work.latest = phase;
            if phase == WorkPhase::Transcribing {
                work.progress = progress;
            }
        }
        if successful {
            if let Some(work) = &mut self.work {
                if let Some((_, total)) = work.progress {
                    // Cleanup may also run on partial text. Only the matching
                    // complete outcome proves an unreported last chunk finished.
                    work.progress = Some((total, total));
                }
            }
        }
    }

    fn present_work(&mut self, now: Instant) -> bool {
        let Some(work) = &self.work else {
            return false;
        };
        let phase = work.phase;
        let progress = (phase == WorkPhase::Transcribing)
            .then_some(work.progress)
            .flatten();
        let caption = if self
            .snapshot
            .as_ref()
            .is_some_and(|status| status.hud.labels)
        {
            phase.caption(progress, work.recovery)
        } else {
            Caption::default()
        };
        self.set_composition(Some(phase.kind()), caption, progress, None, now);
        let work = self.work.as_mut().expect("presenting work");
        let settled = self.track.since + self.track.duration;
        let revealed = self
            .track
            .fill
            .as_ref()
            .map_or(settled, |fill| fill.until());
        work.ready_at = work.ready_at.max(settled.max(revealed) + PHASE_DWELL);
        if now >= work.ready_at {
            if let Some(next) = WorkPhase::next(work.pending) {
                if next.kind() != work.phase.kind() {
                    work.ready_at = now + PHASE_DWELL;
                }
                work.phase = next;
                work.pending &= !next.bit();
                return self.present_work(now);
            }
            if self
                .snapshot
                .as_ref()
                .is_some_and(|status| status.state == StateKind::Idle)
            {
                self.work = None;
                return false;
            }
        }
        true
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
        let progress_changed = progress != self.progress;
        let reset_progress = !changed
            && progress_changed
            && match (self.progress, progress) {
                (Some((old, total)), Some((new, next_total))) => total != next_total || new < old,
                (Some(_), None) => true,
                _ => false,
            };
        if changed || progress_changed {
            self.track.advance(
                progress,
                now,
                changed || reset_progress,
                self.reduced_motion,
            );
        }
        if changed || reset_progress || waveform != self.waveform {
            let target = match kind {
                Some(Kind::Recording) => recording_frame(waveform),
                Some(kind @ (Kind::Working | Kind::Finishing)) => activity_frame(
                    kind,
                    now.saturating_duration_since(self.activity_since),
                    self.track.front(now, self.reduced_motion),
                    self.reduced_motion,
                ),
                Some(Kind::Resolved) => TrackFrame {
                    columns: [TrackColumn::filled(); CELLS],
                },
                Some(Kind::Attention) => TrackFrame {
                    columns: [TrackColumn::rest().with_center(0.55); CELLS],
                },
                _ => TrackFrame {
                    columns: [TrackColumn::rest().with_center(0.35); CELLS],
                },
            };
            self.track.target(
                target,
                now,
                if reset_progress {
                    Duration::ZERO
                } else if changed {
                    if self
                        .snapshot
                        .as_ref()
                        .is_some_and(|status| status.state == StateKind::Recording)
                    {
                        LISTENING_ONSET
                    } else {
                        SETTLE
                    }
                } else {
                    WAVEFORM_EASE
                },
                changed || reset_progress,
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
                self.track.front(now, self.reduced_motion),
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
            .and_then(|result| {
                if !result.presented || result.waiting_for_settle {
                    Some(1.0)
                } else {
                    result.visibility.alpha(now, self.reduced_motion)
                }
            })
            .unwrap_or(1.0)
    }

    fn render_key(
        &self,
        frame: &TrackFrame,
        size: (u32, u32, u32),
        palette: Palette,
        alpha: f32,
    ) -> RenderKey {
        RenderKey {
            kind: self.kind,
            caption_revision: self.caption_revision,
            header_revision: self.header_revision,
            interaction_event: self.interaction.as_ref().and(self.notice_event),
            opacities: std::array::from_fn(|index| {
                frame.columns[index]
                    .opacities
                    .map(|opacity| (opacity * 255.0).round() as u8)
            }),
            alpha: (alpha * 255.0).round() as u8,
            size,
            palette,
        }
    }

    fn animate(&self, now: Instant) -> bool {
        // Keep polling responsive between samples; unchanged frames do not repaint.
        !self.reduced_motion
            && self.kind.is_some()
            && (self.lost_since.is_none()
                && (matches!(
                    self.kind,
                    Some(Kind::Recording | Kind::Working | Kind::Finishing)
                ) || self.result.is_some())
                || (self.lost_since.is_none() || self.kind == Some(Kind::Attention))
                    && self.track.moving(now))
    }
}

/// The pipeline a running dictation, or the recovery of a saved take, started
/// with. Removing audio or replaying saved text runs no pipeline, whatever
/// default its snapshot reports.
fn take_pipeline(status: &StatusSnapshot) -> Option<&PipelineDisplay> {
    status.pipeline.as_ref().filter(|_| {
        matches!(
            status.operation_kind,
            Some(ipc::OperationKind::Dictation | ipc::OperationKind::Recovery)
        )
    })
}

fn attention_outcome(outcome: &TerminalOutcome) -> bool {
    !matches!(
        outcome.completeness,
        Completeness::Cancelled | Completeness::Empty
    ) && (matches!(
        outcome.completeness,
        Completeness::Failed | Completeness::Partial
    ) || matches!(
        outcome.delivery,
        Delivery::Failed | Delivery::Uncertain | Delivery::Deferred
    ) || outcome.cleanup == Cleanup::KeychainPending)
}

/// The owner's capture guidance in HUD words: its sentence naming the input,
/// and where the operator deliberately fixes it. The panel takes no clicks, so
/// the route names the menu or window that carries the action.
fn guidance_caption(guidance: &CaptureGuidance) -> Caption {
    Caption {
        title: guidance.message(),
        detail: String::new(),
        action: match guidance.kind {
            CaptureIssue::MissingMicrophone => {
                missing_microphone_action(guidance.default_input_name.as_deref())
            }
            CaptureIssue::SilentInput | CaptureIssue::NoAudio => SETTINGS_ACTION.to_owned(),
        },
    }
}

/// What a take kept when cleanup did not run: the alias-corrected text, or
/// the original when no alias changed it, copied or sent.
fn kept_text(outcome: &TerminalOutcome) -> &'static str {
    match (
        outcome.delivery == Delivery::Copied,
        outcome.alias_substitutions > 0,
    ) {
        (true, true) => "Alias-corrected text copied.",
        (false, true) => "Alias-corrected text sent.",
        (true, false) => "Original text copied.",
        (false, false) => "Original text sent.",
    }
}

/// Keychain access said once it settles needing the operator: why takes skip
/// cleanup, and the menu item that fixes it. Checking and Ready say nothing,
/// and the wait for an open prompt is live instead.
fn keychain_notice(access: &KeychainAccess) -> Option<Caption> {
    let (title, detail) = match access.authorization {
        KeychainAuthorization::NeedsAuthorization => (
            "Keychain access not yet allowed".to_owned(),
            "Takes skip cleanup until you allow it.",
        ),
        KeychainAuthorization::Unavailable => (
            keychain_unavailable(&access.key_id),
            "Takes skip cleanup until it’s available.",
        ),
        KeychainAuthorization::Checking
        | KeychainAuthorization::Authorizing
        | KeychainAuthorization::Ready => return None,
    };
    Some(Caption {
        title,
        detail: detail.to_owned(),
        action: keychain_route(access).unwrap_or_default(),
    })
}

/// While the owner waits on the native prompt the operator chose from the
/// menu, which has closed by then: what the prompt's buttons mean for takes.
fn keychain_waiting(access: &KeychainAccess) -> Option<Caption> {
    (access.authorization == KeychainAuthorization::Authorizing).then(|| Caption {
        title: KEYCHAIN_WAITING.to_owned(),
        detail: KEYCHAIN_BUTTONS.to_owned(),
        action: String::new(),
    })
}

fn present_outcome(
    outcome: &TerminalOutcome,
    status: &StatusSnapshot,
    now: Instant,
    reduced_motion: bool,
) -> ResultView {
    let attention = attention_outcome(outcome);
    let delivered = matches!(
        outcome.delivery,
        Delivery::Typed | Delivery::Pasted | Delivery::HandedOff
    );
    let copied = outcome.delivery == Delivery::Copied;
    // Cleanup that waited for Keychain approval needs the operator even when
    // its text arrived, so it never settles as success.
    let keychain_pending = outcome.cleanup == Cleanup::KeychainPending
        && outcome.completeness != Completeness::Cancelled;
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
            Delivery::Uncertain => {
                caption.title = if cfg!(target_os = "macos") && outcome.handoff.is_none() {
                    "Copy uncertain. Check clipboard before pasting."
                } else {
                    "Delivery uncertain. Check the app."
                }
                .to_owned();
            }
            Delivery::Deferred => {
                caption.title = if cfg!(target_os = "macos") && outcome.handoff.is_none() {
                    "Keyboard delivery unavailable. Choose Clipboard in Settings.".to_owned()
                } else {
                    "Delivery paused. Text was not inserted.".to_owned()
                };
            }
            Delivery::Failed => caption.title = "Delivery failed.".to_owned(),
            Delivery::Cancelled => caption.title = "Cancelled".to_owned(),
            Delivery::Copied => {
                kind = Kind::Resolved;
                caption.title = outcome.message.clone();
            }
            Delivery::HandedOff => {
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
        caption.title = format!("{} Cleanup unavailable.", kept_text(outcome));
    }
    if keychain_pending && outcome.completeness == Completeness::Complete && (delivered || copied) {
        kind = Kind::Attention;
        caption.title = kept_text(outcome).to_owned();
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
    if let Some(cause) = outcome
        .cloud_failure
        .filter(|_| outcome.completeness != Completeness::Cancelled)
    {
        caption.title = cause.to_string();
        let result = match (outcome.completeness, outcome.delivery) {
            (Completeness::Complete, Delivery::Uncertain) => {
                "Local transcript complete; delivery uncertain."
            }
            (Completeness::Complete, Delivery::Failed | Delivery::Deferred) => {
                "Local transcript complete; not delivered."
            }
            (Completeness::Complete, _) => "Transcript complete; recovered locally.",
            (Completeness::Partial, Delivery::Copied) => {
                "Partial text copied; local recovery incomplete."
            }
            (Completeness::Partial, Delivery::Uncertain) => "Partial transcript; copy uncertain.",
            (Completeness::Partial, _) => "Partial transcript; not delivered.",
            _ => "Local recovery produced no complete transcript.",
        };
        let retained = if outcome.error.as_deref() == Some("storage-failed") {
            "Storage incomplete; retention unconfirmed."
        } else {
            match (outcome.artifacts.audio, outcome.artifacts.text) {
                (true, true) => "Audio and text saved.",
                (true, false) => "Audio saved.",
                (false, true) => "Text saved; no saved audio.",
                (false, false) => "No saved audio or text.",
            }
        };
        caption.detail = format!("{result} {retained}");
    }
    // Actions are descriptions of the deliberately opened menu, not fake controls.
    // Require the matching artifact and capability; never retarget a changing last take.
    let named = outcome.artifacts.take_id.is_some();
    let audio = named && outcome.artifacts.audio;
    let text = named && outcome.artifacts.text;
    if !delivered && !(copied && outcome.completeness == Completeness::Complete) {
        caption.action =
            if text && status.capabilities.copy && outcome.completeness == Completeness::Complete {
                deliberately!("Copy this transcript")
            } else if audio && status.capabilities.recover && status.capabilities.local_model {
                deliberately!("Recover locally to clipboard")
            } else if audio && status.capabilities.recover && status.capabilities.remote_configured
            {
                deliberately!("Recover with configured provider to clipboard")
            } else if audio && !status.capabilities.local_model {
                deliberately!("Install local model")
            } else if text && status.capabilities.copy {
                deliberately!("Copy this transcript")
            } else if attention {
                deliberately!("Check setup")
            } else {
                ""
            }
            .to_owned();
    }
    if let Some(cause) = outcome
        .cloud_failure
        .filter(|_| outcome.completeness != Completeness::Cancelled)
    {
        if outcome.completeness == Completeness::Complete && (delivered || copied) {
            caption.action = if copied {
                "Copied. Paste when ready; no retry needed."
            } else {
                "Sent. No retry needed."
            }
            .to_owned();
        }
        if outcome.delivery == Delivery::Uncertain {
            caption.action = "Check the destination before trying again.".to_owned();
        } else if copied && outcome.completeness == Completeness::Partial {
            caption.action.insert_str(0, "Review before pasting.\n");
        }
        if cause.is_payment_or_budget() {
            caption.action.push('\n');
            caption.action.push_str(cause.advice());
        }
    }
    // Idle guidance describes this outcome: a refused press or a take without
    // audio needs a deliberate input choice, not a recovered recording.
    if let Some(guidance) = &status.capture_guidance {
        let guided = guidance_caption(guidance);
        caption.title = guided.title;
        caption.action = guided.action;
    }
    // The specific reason cleanup was skipped, and the menu item that allows the
    // key while the owner still needs it; never the generic cleanup failure.
    if keychain_pending {
        if !caption.detail.is_empty() {
            caption.detail.push('\n');
        }
        caption.detail.push_str(KEYCHAIN_PENDING_REASON);
        if let Some(route) = status.keychain.as_ref().and_then(keychain_route) {
            if !caption.action.is_empty() {
                caption.action.push('\n');
            }
            caption.action.push_str(&route);
        }
    }
    if attention && status.capabilities.dismiss {
        if !caption.action.is_empty() {
            caption.action.push('\n');
        }
        caption
            .action
            .push_str("Dismiss outcome keeps saved recordings.");
    }
    let dwell = if attention {
        ATTENTION_HOLD
            + if reduced_motion {
                Duration::ZERO
            } else {
                RESULT_FADE
            }
    } else if kind == Kind::Resolved && delivered && outcome.cleanup != Cleanup::Failed {
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
        visibility: Visibility(now + dwell),
        dwell,
        presented: false,
        waiting_for_settle: kind == Kind::Resolved
            && delivered
            && outcome.cleanup != Cleanup::Failed,
        route: outcome.handoff.clone(),
        keychain: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrackColumn {
    // Active light per row above the persistent resting field. Every value is
    // at least FIELD_REST so the full container is always visible.
    opacities: [f32; ROWS],
}

impl TrackColumn {
    const fn rest() -> Self {
        Self {
            opacities: [FIELD_REST; ROWS],
        }
    }

    const fn filled() -> Self {
        Self {
            opacities: [1.0; ROWS],
        }
    }

    fn with_center(mut self, opacity: f32) -> Self {
        let center = ROWS / 2;
        self.opacities[center] = self.opacities[center].max(opacity);
        self
    }

    fn fully_lit(&self) -> bool {
        // Match render-key precision: the final floating-point interpolation can
        // be visually unchanged and therefore never attach another buffer.
        self.opacities
            .iter()
            .all(|opacity| (opacity * 255.0).round() == 255.0)
    }

    fn interpolate(self, to: Self, amount: f32) -> Self {
        Self {
            opacities: std::array::from_fn(|row| {
                self.opacities[row] + (to.opacities[row] - self.opacities[row]) * amount
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TrackFrame {
    columns: [TrackColumn; CELLS],
}

struct FillMotion {
    from: f32,
    to: f32,
    since: Instant,
    duration: Duration,
}

impl FillMotion {
    fn at(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return self.to;
        }
        let t = (now.saturating_duration_since(self.since).as_secs_f32()
            / self.duration.as_secs_f32())
        .min(1.0);
        self.from + (self.to - self.from) * t * t * (3.0 - 2.0 * t)
    }

    fn until(&self) -> Instant {
        self.since + self.duration
    }
}

struct TrackMotion {
    from: TrackFrame,
    to: TrackFrame,
    presented: TrackFrame,
    since: Instant,
    duration: Duration,
    signal_easing: bool,
    fill: Option<FillMotion>,
}

impl TrackMotion {
    fn new(now: Instant) -> Self {
        let quiet = TrackFrame {
            columns: [TrackColumn::rest(); CELLS],
        };
        Self {
            from: quiet,
            to: quiet,
            presented: quiet,
            since: now,
            duration: Duration::ZERO,
            signal_easing: false,
            fill: None,
        }
    }

    fn reset(&mut self, now: Instant) {
        *self = Self::new(now);
    }

    fn advance(&mut self, progress: Option<(u32, u32)>, now: Instant, reset: bool, reduced: bool) {
        self.fill = progress.map(|(completed, total)| FillMotion {
            from: if reset {
                0.0
            } else {
                self.fill.as_ref().map_or(0.0, |fill| fill.at(now))
            },
            to: CELLS as f32 * completed as f32 / total as f32,
            since: now,
            duration: if reduced {
                Duration::ZERO
            } else {
                PROGRESS_REVEAL
            },
        });
    }

    fn front(&self, now: Instant, reduced: bool) -> Option<f32> {
        self.fill
            .as_ref()
            .map(|fill| if reduced { fill.to } else { fill.at(now) })
    }

    fn target(
        &mut self,
        to: TrackFrame,
        now: Instant,
        duration: Duration,
        state_change: bool,
        reduced: bool,
    ) {
        // A state transition begins at the actual attached cell light, even if
        // the prior phase was still moving.
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
            return TrackFrame {
                columns: std::array::from_fn(|index| TrackColumn {
                    opacities: std::array::from_fn(|row| {
                        damp(
                            self.from.columns[index].opacities[row],
                            to.columns[index].opacities[row],
                            1.0 / 255.0,
                        )
                    }),
                }),
            };
        }
        if elapsed >= self.duration {
            return to;
        }
        let t = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        let eased = t * t * (3.0 - 2.0 * t);
        TrackFrame {
            columns: std::array::from_fn(|index| {
                self.from.columns[index].interpolate(to.columns[index], eased)
            }),
        }
    }

    fn moving(&self, now: Instant) -> bool {
        self.from != self.to && self.frame(now) != self.to
    }
}

fn recording_frame(waveform: Option<AudioWaveform>) -> TrackFrame {
    // Listening sensitivity. Normal speech should light most of the field
    // without pinning ordinary peaks at full height; the silence floor and the
    // square-root response are unchanged.
    const WAVEFORM_GAIN: f32 = 3.2;

    fn extent(magnitude: u16) -> f32 {
        let amplitude = if magnitude <= 32 {
            0.0
        } else {
            (WAVEFORM_GAIN * f32::from(magnitude) / 32767.0)
                .clamp(0.0, 1.0)
                .sqrt()
        };
        1.0 + (GRID_HEIGHT / 2.0 - 1.0) * amplitude
    }

    let columns = waveform
        .unwrap_or([[0; 2]; AUDIO_WAVEFORM_BINS])
        .map(|[minimum, maximum]| {
            let upper = extent(maximum.max(0) as u16);
            let lower = extent(minimum.min(0).unsigned_abs());
            let amplitude = (upper.max(lower) - 1.0) / (GRID_HEIGHT / 2.0 - 1.0);
            let active = if amplitude <= 0.0 {
                FIELD_REST
            } else {
                0.3 + 0.7 * amplitude
            };
            // The sparse field is always present. The signed extent only raises
            // the fixed cells it reaches, fading an edge cell by its coverage.
            let opacities = std::array::from_fn(|row| {
                let offset = (row as f32 - (ROWS / 2) as f32) * CELL_PITCH;
                let top = offset + CELL_SIZE / 2.0;
                let bottom = offset - CELL_SIZE / 2.0;
                let covered =
                    ((top.min(upper) - bottom.max(-lower)).max(0.0) / CELL_SIZE).clamp(0.0, 1.0);
                FIELD_REST + (active - FIELD_REST) * covered
            });
            TrackColumn { opacities }
        });
    TrackFrame { columns }
}

fn activity_frame(kind: Kind, age: Duration, front: Option<f32>, reduced: bool) -> TrackFrame {
    const NOISE_NANOS: u128 = 900_000_000;
    const PACKET_NANOS: u128 = 2_400_000_000;
    const PACKET_RADIUS: f32 = 4.0;

    let age = if reduced { Duration::ZERO } else { age };
    let tick = (age.as_nanos() / NOISE_NANOS) as u32;
    let fraction = (age.as_nanos() % NOISE_NANOS) as f32 / NOISE_NANOS as f32;
    let blend = fraction * fraction * (3.0 - 2.0 * fraction);
    let noise = |cell| {
        if reduced {
            return 0.5;
        }
        let from = pixel_noise(cell, tick);
        let to = pixel_noise(cell, tick.wrapping_add(1));
        from + (to - from) * blend
    };
    let packet_center = if kind == Kind::Working && front.is_none() {
        if reduced {
            (CELLS - 1) as f32 / 2.0
        } else {
            (age.as_nanos() % PACKET_NANOS) as f32 / PACKET_NANOS as f32
                * (CELLS as f32 + 2.0 * PACKET_RADIUS)
                - PACKET_RADIUS
        }
    } else {
        0.0
    };
    // Both working patterns span the full field height: the measured front
    // fills whole columns up to acknowledged work, cleanup pulses every cell.
    let finishing = |cell| 0.12 + 0.86 * noise(cell);
    let columns = std::array::from_fn(|column| {
        let opacities = if kind == Kind::Finishing {
            // Independent smooth pulses span near-rest to near-full opacity;
            // all 420 cells remain visible, without a synchronized flash.
            std::array::from_fn(|row| finishing(column * ROWS + row))
        } else if let Some(front) = front {
            // Move one fractional spatial boundary, not the opacity of an entire
            // reported chunk. The filled meter never creeps past acknowledged
            // work; a decaying cleanup-style flicker may light a short band
            // beyond it.
            let coverage = (front - column as f32).clamp(0.0, 1.0);
            let frontier = if reduced {
                0.0
            } else {
                let beyond = (column as f32 - front).max(0.0);
                let t = (beyond / FRONTIER_RADIUS).min(1.0);
                (1.0 - t) * (1.0 - t) * (1.0 - coverage)
            };
            std::array::from_fn(|row| {
                let cell = column * ROWS + row;
                let filled = FIELD_REST + (0.64 + 0.32 * noise(cell) - FIELD_REST) * coverage;
                if frontier <= 0.0 {
                    filled
                } else {
                    (filled + (finishing(cell) - FIELD_REST) * frontier).min(1.0)
                }
            })
        } else {
            let packet =
                (1.0 - (column as f32 - packet_center).abs() / PACKET_RADIUS).clamp(0.0, 1.0);
            let packet = packet * packet * (3.0 - 2.0 * packet);
            // A bounded packet leaves no completed trail and resets
            // outside the grid, where both ends are already dim.
            std::array::from_fn(|row| {
                let active = 0.48 + 0.2 * noise(column * ROWS + row);
                FIELD_REST + (active - FIELD_REST) * packet
            })
        };
        TrackColumn { opacities }
    });
    TrackFrame { columns }
}

fn pixel_noise(cell: usize, tick: u32) -> f32 {
    let mut value = (cell as u32).wrapping_mul(0x9e37_79b9) ^ tick.wrapping_mul(0x85eb_ca6b);
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    (value >> 8) as f32 / 0x00ff_ffff as f32
}

pub(crate) fn format_elapsed(seconds: u64) -> String {
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
                let mut reduced_motion = surface::desktop_reduced_motion();
                let mut preference_at = Instant::now();
                while !stopped.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    if started.duration_since(preference_at) >= PREFERENCE_INTERVAL {
                        palette = theme::load();
                        reduced_motion = surface::desktop_reduced_motion();
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
                    // stalls the native surface or waveform settling.
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

/// Run the native HUD surface. Screenshot mode skips IPC and the instance lock
/// and renders a composed scenario, including deliberately aged transitions,
/// for a take in `mode` when one is given.
pub fn run(
    screenshot: Option<PathBuf>,
    state: Option<ScreenshotState>,
    handoff: Option<String>,
    mode: Option<Mode>,
) -> Result<()> {
    let visual_proof = screenshot.is_some();
    let result = surface::run_native(screenshot, state, handoff, mode);
    if visual_proof {
        return result;
    }
    if let Err(error) = result {
        tracing::warn!("[HUD] unavailable: {error:#}");
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct RenderKey {
    kind: Option<Kind>,
    caption_revision: u64,
    header_revision: u64,
    interaction_event: Option<u64>,
    opacities: [[u8; ROWS]; CELLS],
    alpha: u8,
    size: (u32, u32, u32),
    palette: Palette,
}

/// The platform-neutral HUD: one status model, its painter, and the memo of the
/// last presented frame. Native surfaces only size, place and present its pixels.
struct Instrument {
    model: Model,
    font: FontRef<'static>,
    palette: Palette,
    last_render: Option<RenderKey>,
    /// Whether the last presented frame showed the instrument.
    visible: bool,
    last_refresh: Instant,
    last_status: Instant,
}

/// The next frame to show: painted with [`Instrument::paint`], then recorded with
/// [`Instrument::presented`] once the native surface holds those pixels.
struct Presentation {
    frame: TrackFrame,
    key: RenderKey,
    alpha: f32,
    shown: bool,
    container_width: f32,
}

impl Instrument {
    fn with_model(model: Model, palette: Palette, now: Instant) -> Result<Self> {
        Ok(Self {
            model,
            font: FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR)
                .context("loading HUD typeface")?,
            palette,
            last_render: None,
            visible: false,
            last_refresh: now,
            last_status: now,
        })
    }

    fn live(now: Instant) -> Result<Self> {
        Self::with_model(Model::new(now), theme::load(), now)
    }

    /// A screenshot scenario. A preview handoff is the first target in the live
    /// theme, like a real single configured target.
    fn preview(
        state: ScreenshotState,
        handoff: Option<String>,
        mode: Option<Mode>,
        now: Instant,
    ) -> Result<Self> {
        let palette = theme::load();
        let handoff = handoff.map(|label| ipc::Handoff {
            name: label.to_lowercase(),
            color: palette.target(0),
            label,
        });
        let model = screenshot_model(state, now, handoff.as_ref(), mode);
        Self::with_model(model, palette, now)
    }

    /// Fold queued status readings and the stale-status clock into the model.
    fn poll(&mut self, poller: &Poller, now: Instant) {
        while let Ok(update) = poller.receiver.try_recv() {
            self.palette = update.palette;
            if let Some(reduced) = update.reduced_motion {
                self.model.desktop_reduced_motion = reduced;
            }
            match update.status {
                Ok(status) => self.model.apply(status, now),
                Err(()) => self.model.disconnected(now),
            }
            self.last_refresh = now;
            self.last_status = now;
        }
        if now.duration_since(self.last_refresh) >= POLL_INTERVAL {
            self.model.refresh_connection(self.last_status, now);
            self.last_refresh = now;
        }
    }

    /// Logical surface height for a container `container_width` pixels wide.
    fn height(&self, container_width: f32) -> u32 {
        layout_height(&self.model, &self.font, container_width)
    }

    fn alpha(&self, now: Instant) -> f32 {
        self.model.alpha(now)
    }

    fn animate(&self, now: Instant) -> bool {
        self.model.animate(now)
    }

    /// The next frame for a `size` logical surface at an integral buffer `scale`,
    /// unless the presented pixels already match. `force` always paints.
    fn prepare(
        &self,
        model_now: Instant,
        alpha: f32,
        size: (u32, u32),
        scale: u32,
        container_width: f32,
        force: bool,
    ) -> Option<Presentation> {
        let shown = self.model.kind.is_some();
        let frame = self.model.frame(model_now);
        let key = self
            .model
            .render_key(&frame, (size.0, size.1, scale), self.palette, alpha);
        if !force && self.last_render.as_ref() == Some(&key) && shown == self.visible {
            return None;
        }
        Some(Presentation {
            frame,
            key,
            alpha,
            shown,
            container_width,
        })
    }

    /// Paint into a premultiplied BGRA buffer of `width` × `height` whole pixels.
    fn paint(
        &self,
        presentation: &Presentation,
        bytes: &mut [u8],
        width: u32,
        height: u32,
        scale: u32,
    ) {
        let mut canvas = Canvas {
            bytes,
            width,
            height,
            scale: scale as f32,
            alpha: 1.0,
        };
        canvas.paint_hud(
            &self.model,
            &self.font,
            self.palette,
            &presentation.frame,
            presentation.container_width,
        );
        canvas.fade(presentation.alpha);
    }

    /// The native surface now presents this frame; the next transition starts here.
    fn presented(&mut self, presentation: Presentation) {
        self.model.track.presented = presentation.frame;
        self.visible = presentation.shown;
        self.last_render = Some(presentation.key);
    }

    /// Scale, transform or configured size changed: paint the next frame.
    #[cfg(target_os = "linux")]
    fn invalidate(&mut self) {
        self.last_render = None;
    }

    /// The surface is gone; its replacement starts hidden and freshly painted.
    #[cfg(target_os = "linux")]
    fn detached(&mut self) {
        self.last_render = None;
        self.visible = false;
    }
}

struct Canvas<'a> {
    bytes: &'a mut [u8],
    width: u32,
    height: u32,
    scale: f32,
    alpha: f32,
}

impl Canvas<'_> {
    fn paint_hud(
        &mut self,
        model: &Model,
        font: &FontRef<'_>,
        palette: Palette,
        frame: &TrackFrame,
        container_width: f32,
    ) {
        self.bytes.fill(0);
        let Some(kind) = model.kind else {
            return;
        };
        let logical_width = self.width as f32 / self.scale;
        let [left, top, _, body_height] = container_rect(
            logical_width,
            self.height as f32 / self.scale,
            container_width,
        );
        // A non-default target owns the border, a surface wash and the active field;
        // the default flow keeps the theme's own colors. Text and the field read on
        // the washed surface as the theme's own colors read on its surface.
        let header = model.header.as_ref();
        let handoff = header.and_then(|header| header.handoff.as_ref());
        let (palette, border, active) = match handoff {
            Some(handoff) => {
                let color = handoff.color;
                let palette = palette.on_surface(mix(palette.surface, color, ROUTE_WASH));
                (palette, color, palette.legible(color))
            }
            None => (palette, palette.border, palette.accent),
        };
        self.rect(left, top, container_width, body_height, border, 1.0);
        self.rect(
            left + 1.0,
            top + 1.0,
            container_width - 2.0,
            body_height - 2.0,
            palette.surface,
            1.0,
        );
        if kind == Kind::Attention {
            self.rect(left, top, 2.0, body_height, palette.attention, 1.0);
        }
        let rgb = match kind {
            Kind::Recording | Kind::Working | Kind::Finishing | Kind::Resolved => active,
            Kind::Attention => palette.attention,
            Kind::Neutral => palette.foreground,
        };
        // The target in its own color, then the line naming the take.
        let track_top = match header {
            Some(header) => {
                let width = header_width(container_width);
                let mut y = top + 5.0;
                if let Some(handoff) = handoff {
                    let ink = mix(handoff.color, palette.foreground, ROUTE_LABEL_LIFT);
                    y += self.text(
                        font,
                        &header.route,
                        left + 10.0,
                        y,
                        width,
                        ROUTE_LABEL_SIZE,
                        1,
                        palette.legible(ink),
                    );
                }
                if let Some(line) = &header.line {
                    self.header_line(font, line, left + 10.0, y, width, palette, active);
                }
                top + header.height()
            }
            None => top,
        };
        // The field always fills the container at full width; state changes only
        // raise cell light above FIELD_REST, never the field's geometry.
        let field_width = (container_width - 12.0).max(0.0);
        let slot_width = field_width / CELLS as f32;
        let cell_width = (slot_width * CELL_SIZE / CELL_PITCH).min(CELL_SIZE);
        let track_left = (logical_width - field_width) / 2.0;
        for (index, column) in frame.columns.iter().copied().enumerate() {
            self.pixel_column(
                track_left + index as f32 * slot_width + (slot_width - cell_width) / 2.0,
                track_top + TRACK_HEIGHT / 2.0,
                cell_width,
                column,
                rgb,
            );
        }
        let text_width = (container_width - 32.0).max(16.0);
        let mut y = track_top + TRACK_HEIGHT;
        y += self.text(
            font,
            &model.caption.title,
            left + 16.0,
            y,
            text_width,
            12.0,
            3,
            palette.foreground,
        );
        if !model.caption.detail.is_empty() {
            y += 3.0;
            y += self.text(
                font,
                &model.caption.detail,
                left + 16.0,
                y,
                text_width,
                11.0,
                3,
                palette.foreground,
            );
        }
        if !model.caption.action.is_empty() {
            y += 7.0;
            y += self.text(
                font,
                &model.caption.action,
                left + 16.0,
                y,
                text_width,
                10.0,
                4,
                if kind == Kind::Attention {
                    palette.attention
                } else {
                    palette.foreground
                },
            );
        }
        if let Some((interaction, _)) = &model.interaction {
            y += 7.0;
            self.rect(left + 16.0, y, text_width, 1.0, palette.border, 1.0);
            y += 6.0;
            self.text(
                font,
                interaction,
                left + 16.0,
                y,
                text_width,
                11.0,
                2,
                palette.foreground,
            );
        }
    }

    fn fade(&mut self, alpha: f32) {
        // Fade the composed premultiplied frame once, not each overlapping
        // primitive; the resting surface is always fully opaque.
        if alpha < 1.0 {
            for byte in self.bytes.iter_mut() {
                *byte = (f32::from(*byte) * alpha).round() as u8;
            }
        }
    }

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
        // Every row is a fixed 3×3 logical cell; the field never changes geometry.
        for (row, alpha) in column.opacities.into_iter().enumerate() {
            if alpha <= 0.0 {
                continue;
            }
            let offset = row as i32 - (ROWS / 2) as i32;
            let row_top = center + offset as f32 * CELL_PITCH - CELL_SIZE / 2.0;
            self.aligned_rect(x, row_top, width, CELL_SIZE, rgb, alpha);
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
            let characters = line.chars().take(limit).chain(truncated.then_some('…'));
            self.run(
                font,
                characters,
                x,
                y + row as f32 * (size + 5.0),
                size,
                rgb,
            );
            rows += 1;
        }
        rows as f32 * (size + 5.0)
    }

    /// Draw `characters` one advance apart from `x` on the line whose top is
    /// `y`, without wrapping; return the logical width they take.
    fn run(
        &mut self,
        font: &FontRef<'_>,
        characters: impl IntoIterator<Item = char>,
        x: f32,
        y: f32,
        size: f32,
        rgb: [u8; 3],
    ) -> f32 {
        lay_out(font, characters, [x, y], size, self.scale, |outlined| {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 {
                    self.pixel(px as u32, py as u32, rgb, coverage);
                }
            });
        })
    }

    /// [`Canvas::run`] in `rgb` as `palette` fits it for its painted pixels:
    /// anti-aliasing never paints the full ink where strokes fall between
    /// pixels, so the run's most covered pixel, not the ink, must read at
    /// 4.5:1 on the surface.
    #[allow(clippy::too_many_arguments)] // Raster text geometry, font and colors are explicit.
    fn legible_run(
        &mut self,
        font: &FontRef<'_>,
        characters: impl IntoIterator<Item = char> + Clone,
        x: f32,
        y: f32,
        size: f32,
        rgb: [u8; 3],
        palette: Palette,
    ) -> f32 {
        let ink = palette.painted(rgb, || {
            let mut peak: f32 = 0.0;
            lay_out(
                font,
                characters.clone(),
                [x, y],
                size,
                self.scale,
                |outlined| {
                    outlined.draw(|_, _, coverage| peak = peak.max(coverage));
                },
            );
            peak.min(1.0)
        });
        self.run(font, characters, x, y, size, ink)
    }

    /// `mode`'s glyph from its top-left corner, each cell on whole pixels.
    fn glyph(&mut self, mode: Mode, x: f32, y: f32, rgb: [u8; 3]) {
        let scale = self.scale;
        let snap = |value: f32| (value * scale).round() / scale;
        let (x, y) = (snap(x), snap(y));
        for (column, row) in modes::glyph_cells(mode) {
            self.aligned_rect(
                x + column as f32 * GLYPH_CELL,
                y + row as f32 * GLYPH_CELL,
                GLYPH_CELL,
                GLYPH_CELL,
                rgb,
                1.0,
            );
        }
    }

    /// The line naming a take, from `x` across `width` points of the row whose
    /// top is `y`: the glyph and name, the promise or the stage at work, and
    /// where the take goes against the right edge.
    #[allow(clippy::too_many_arguments)] // Raster geometry, font and colors are explicit.
    fn header_line(
        &mut self,
        font: &FontRef<'_>,
        line: &HeaderLine,
        x: f32,
        y: f32,
        width: f32,
        palette: Palette,
        active: [u8; 3],
    ) {
        let advance = font.as_scaled(LINE_SIZE).h_advance(font.glyph_id('M'));
        let fit = LineFit::new(line, advance, width);
        let middle = y + cap_middle(font, LINE_SIZE);
        let muted = palette.muted();
        self.glyph(line.mode, x, middle - GLYPH_EXTENT / 2.0, active);
        let mut cursor = x + GLYPH_EXTENT + advance;
        let name = shortened(line.name, fit.name);
        cursor += self.run(font, name, cursor, y, LINE_SIZE, palette.foreground);
        match fit.middle {
            Middle::Nothing => {}
            Middle::Promise => {
                let promise = line.promise.chars();
                self.legible_run(font, promise, cursor, y, LINE_SIZE, muted, palette);
            }
            Middle::Working { lights } => {
                cursor += advance;
                if lights {
                    for (index, light) in line.lights.iter().enumerate() {
                        let alpha = if *light == Light::Waiting {
                            LIGHT_REST
                        } else {
                            1.0
                        };
                        self.aligned_rect(
                            cursor + index as f32 * CELL_PITCH,
                            middle - CELL_SIZE / 2.0,
                            CELL_SIZE,
                            CELL_SIZE,
                            active,
                            alpha,
                        );
                    }
                    cursor += lights_width(line.lights.len());
                }
                let working = line.working.unwrap_or_default();
                self.legible_run(font, working.chars(), cursor, y, LINE_SIZE, active, palette);
            }
        }
        let length: usize = line
            .privacy
            .iter()
            .map(|(text, _)| text.chars().count())
            .sum();
        let shortened = fit.privacy < length;
        let start = x + width - fit.privacy as f32 * advance;
        // Each span's shown characters, where it starts, and whether it is lit.
        let spans = || {
            let mut budget = fit.privacy.saturating_sub(usize::from(shortened));
            let mut at = start;
            line.privacy.iter().map(move |(text, lit)| {
                let shown = text.chars().count().min(budget);
                budget -= shown;
                let span = (text.chars().take(shown), at, *lit);
                at += shown as f32 * advance;
                span
            })
        };
        // One color per ink across the cue, fitted to the most covered pixel of
        // every span drawn in it, so the arrow and its provider match.
        let scale = self.scale;
        let peak_coverage = |lit: bool| {
            let mut peak: f32 = 0.0;
            for (characters, at, _) in spans().filter(|span| span.2 == lit) {
                lay_out(font, characters, [at, y], LINE_SIZE, scale, |outlined| {
                    outlined.draw(|_, _, coverage| peak = peak.max(coverage));
                });
            }
            peak.min(1.0)
        };
        let lit_ink = palette.painted(active, || peak_coverage(true));
        let muted_ink = palette.painted(muted, || peak_coverage(false));
        let mut end = start;
        for (characters, at, lit) in spans() {
            let ink = if lit { lit_ink } else { muted_ink };
            end = at + self.run(font, characters, at, y, LINE_SIZE, ink);
        }
        if shortened && fit.privacy > 0 {
            self.legible_run(font, ['…'], end, y, LINE_SIZE, muted, palette);
        }
    }
}

fn line_capacity(font: &FontRef<'_>, width: f32, size: f32) -> usize {
    (width / font.as_scaled(size).h_advance(font.glyph_id('M')))
        .floor()
        .max(1.0) as usize
}

/// Lay `characters` out one advance apart from `x` on the line whose top is
/// `y`, at `scale`, handing each outlined glyph to `each`; return the logical
/// width they take. Painting and measuring a run share this layout.
fn lay_out(
    font: &FontRef<'_>,
    characters: impl IntoIterator<Item = char>,
    [x, y]: [f32; 2],
    size: f32,
    scale: f32,
    mut each: impl FnMut(OutlinedGlyph),
) -> f32 {
    let font_size = size * scale;
    let scaled = font.as_scaled(font_size);
    let advance = scaled.h_advance(font.glyph_id('M'));
    let baseline = y * scale + scaled.ascent();
    let start = x * scale;
    let mut cursor = start;
    for character in characters {
        let glyph = font
            .glyph_id(character)
            .with_scale_and_position(font_size, point(cursor, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            each(outlined);
        }
        cursor += advance;
    }
    (cursor - start) / scale
}

/// How far below a line's top the middle of its capitals sits.
fn cap_middle(font: &FontRef<'_>, size: f32) -> f32 {
    let scaled = font.as_scaled(size);
    // Unscaled outline bounds run from the cap top (min.y) down to the baseline.
    let capital = font.outline(font.glyph_id('H')).map_or(0.0, |outline| {
        (outline.bounds.min.y + outline.bounds.max.y) / 2.0
    });
    scaled.ascent() - capital * scaled.v_scale_factor()
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

/// The container's logical `[left, top, width, height]` in a `width` × `height`
/// surface: centered, 6 points inside the surface's top and bottom edges.
fn container_rect(width: f32, height: f32, container_width: f32) -> [f32; 4] {
    [
        (width - container_width) / 2.0,
        6.0,
        container_width,
        height - 12.0,
    ]
}

fn layout_height(model: &Model, font: &FontRef<'_>, container_width: f32) -> u32 {
    let text_width = (container_width - 32.0).max(16.0);
    let interaction = model.interaction.as_ref().map(|(text, _)| text.as_str());
    SURFACE_HEIGHT
        + header_height(model.header.as_ref()) as u32
        + caption_height(font, &model.caption, interaction, text_width)
}

fn header_height(header: Option<&Header>) -> f32 {
    header.map_or(0.0, Header::height)
}

fn header_width(container_width: f32) -> f32 {
    (container_width - 20.0).max(16.0)
}

/// The rows `text` takes at `size` within `width`, at most `max_lines`, as
/// [`Canvas::text`] lays it out.
fn text_rows(font: &FontRef<'_>, text: &str, width: f32, size: f32, max_lines: usize) -> usize {
    let capacity = line_capacity(font, width, size);
    let mut remaining = text.trim();
    let mut rows = 0;
    while rows < max_lines && !remaining.is_empty() {
        remaining = wrap_line(remaining, capacity).1;
        rows += 1;
    }
    rows
}

fn caption_height(
    font: &FontRef<'_>,
    caption: &Caption,
    interaction: Option<&str>,
    width: f32,
) -> u32 {
    let measure = |text: &str, size: f32, max_lines: usize| {
        text_rows(font, text, width, size, max_lines) as f32 * (size + 5.0)
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
    MissingMicrophone,
    Working,
    RequestingMicrophone,
    RequestingMicrophoneLauncher,
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
    KeychainPending,
    Empty,
    EmptySaved,
    Failed,
    StorageFailed,
    Partial,
    PartialSaved,
    CloudRecovered,
    CloudPartial,
    CloudPayment,
    CloudBudget,
    CloudConnection,
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

/// The input that screenshot guidance names.
const PREVIEW_INPUT: &str = "USB Microphone";

pub(crate) fn preview_snapshot(state: StateKind) -> StatusSnapshot {
    let mode = if state == StateKind::Idle {
        Mode::Raw
    } else {
        Mode::Elevate
    };
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
        attention: false,
        capabilities: ipc::Capabilities {
            stop: true,
            cancel: true,
            recover: true,
            copy: true,
            dismiss: true,
            local_model: true,
            remote_configured: false,
        },
        hud: cantrip_engine::config::HudConfig::default(),
        handoff: None,
        // An idle engine reports its configured default; a take, what it started with.
        pipeline: Some(preview_pipeline(mode, None)),
        microphone: None,
        keychain: None,
        capture_guidance: None,
    }
}

pub(crate) fn preview_outcome(completeness: Completeness, delivery: Delivery) -> TerminalOutcome {
    TerminalOutcome {
        event_id: 1,
        operation_id: Some("preview-take".to_owned()),
        message: "Transcription failed.".to_owned(),
        completeness,
        delivery,
        cleanup: Cleanup::Off,
        alias_substitutions: 0,
        error: None,
        cloud_failure: None,
        artifacts: ipc::Artifacts::default(),
        dismissed: false,
        handoff: None,
    }
}

/// A pipeline as the engine reports it: local Parakeet, or a cloud recognizer at
/// OpenAI by model; two vocabulary spellings; then the mode's model at OpenRouter.
fn preview_pipeline(mode: Mode, cloud_stt: Option<&str>) -> PipelineDisplay {
    PipelineDisplay {
        mode,
        stt_model: cloud_stt
            .unwrap_or(cantrip_engine::models::PARAKEET_V3_INT8.dir_name)
            .to_owned(),
        stt_local: cloud_stt.is_none(),
        stt_host: cloud_stt.map(|_| "api.openai.com".to_owned()),
        llm_model: (mode != Mode::Raw).then(|| "openai/gpt-6.1-sol".to_owned()),
        llm_hosts: if mode == Mode::Raw {
            Vec::new()
        } else {
            vec!["openrouter.ai".to_owned()]
        },
        spellings: 2,
    }
}

/// What each composed take started with: `mode` when given, else Elevate,
/// except the Clean a recovered take recorded, Clean behind cloud recognition
/// or while cleanup waits for Keychain approval, and Raw for a plain copy.
/// Cloud scenarios keep their cloud recognizer.
fn screenshot_pipeline(state: ScreenshotState, mode: Option<Mode>) -> PipelineDisplay {
    let cloud = matches!(
        state,
        ScreenshotState::CloudRecovered
            | ScreenshotState::CloudPartial
            | ScreenshotState::CloudPayment
            | ScreenshotState::CloudBudget
            | ScreenshotState::CloudConnection
    );
    let fixture = match state {
        ScreenshotState::Copied => Mode::Raw,
        ScreenshotState::Recovery | ScreenshotState::KeychainPending => Mode::Clean,
        _ if cloud => Mode::Clean,
        _ => Mode::Elevate,
    };
    preview_pipeline(
        mode.unwrap_or(fixture),
        cloud.then_some("gpt-4o-mini-transcribe"),
    )
}

/// `handoff` composes the same scenario for a take headed to that target, and
/// `mode` for a take in that mode.
fn screenshot_model(
    state: ScreenshotState,
    now: Instant,
    handoff: Option<&ipc::Handoff>,
    mode: Option<Mode>,
) -> Model {
    let start = now - Duration::from_secs(3);
    let mut model = Model::new(start);
    model.apply(preview_snapshot(StateKind::Idle), start);
    if state == ScreenshotState::Idle {
        return model;
    }
    if state == ScreenshotState::MissingMicrophone {
        // The press is refused before capture starts: an idle failure that
        // carries the owner's guidance, with no recording before it.
        let mut refused = preview_snapshot(StateKind::Idle);
        let mut outcome = preview_outcome(Completeness::Failed, Delivery::None);
        outcome.message =
            "Configured microphone isn’t connected. Open Settings in the Cantrip menu.".to_owned();
        outcome.error = Some("capture-source-missing".to_owned());
        refused.outcome = Some(outcome);
        refused.attention = true;
        refused.capture_guidance = Some(CaptureGuidance {
            kind: CaptureIssue::MissingMicrophone,
            source: Some("preview-headset".to_owned()),
            device_name: "Configured microphone".to_owned(),
            default_input_name: Some(PREVIEW_INPUT.to_owned()),
        });
        let refused_at = now - (SETTLE + Duration::from_millis(200));
        model.apply(refused, refused_at);
        let mut at = refused_at;
        while at < now {
            model.refresh(at);
            model.track.presented = model.frame(at);
            at += FRAME_INTERVAL;
        }
        model.refresh(now);
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
            recording.capture_guidance = Some(CaptureGuidance {
                kind: CaptureIssue::SilentInput,
                source: None,
                device_name: PREVIEW_INPUT.to_owned(),
                default_input_name: None,
            });
        }
        _ => {}
    }
    recording.handoff = handoff.cloned();
    recording.pipeline = Some(screenshot_pipeline(state, mode));
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
    if matches!(
        state,
        ScreenshotState::RequestingMicrophone | ScreenshotState::RequestingMicrophoneLauncher
    ) {
        processing.microphone = Some(ipc::MicrophoneAccess {
            authorization: ipc::MicrophoneAuthorization::Requesting,
            grantee: if state == ScreenshotState::RequestingMicrophoneLauncher {
                ipc::MicrophoneGrantee::Launcher(Some("Terminal".to_owned()))
            } else {
                ipc::MicrophoneGrantee::Cantrip
            },
        });
    }
    processing.stage = Some(match state {
        ScreenshotState::RequestingMicrophone | ScreenshotState::RequestingMicrophoneLauncher => {
            Stage::RequestingMicrophone
        }
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
            | ScreenshotState::CloudRecovered
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
    processing.handoff = handoff.cloned();
    processing.pipeline = Some(screenshot_pipeline(state, mode));
    model.apply(processing, processing_at);
    if state == ScreenshotState::Settling {
        model.track.presented = model.frame(now);
    }
    if matches!(
        state,
        ScreenshotState::Working
            | ScreenshotState::RequestingMicrophone
            | ScreenshotState::RequestingMicrophoneLauncher
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
        ScreenshotState::CloudPartial => (Completeness::Partial, Delivery::Deferred),
        ScreenshotState::CloudPayment
        | ScreenshotState::CloudBudget
        | ScreenshotState::CloudConnection => (Completeness::Failed, Delivery::None),
        ScreenshotState::DeliveryFailed => (Completeness::Complete, Delivery::Failed),
        ScreenshotState::DeliveryUncertain => (Completeness::Complete, Delivery::Uncertain),
        ScreenshotState::Deferred => (Completeness::Complete, Delivery::Deferred),
        ScreenshotState::Cancelled => (Completeness::Cancelled, Delivery::Cancelled),
        ScreenshotState::Copied
        | ScreenshotState::CopiedInstead
        | ScreenshotState::KeychainPending => (Completeness::Complete, Delivery::Copied),
        _ if handoff.is_some() => (Completeness::Complete, Delivery::HandedOff),
        _ => (Completeness::Complete, Delivery::Pasted),
    };
    let mut outcome = preview_outcome(completeness, delivery);
    outcome.message = match state {
        ScreenshotState::Copied | ScreenshotState::KeychainPending => "Copied. Paste when ready.",
        ScreenshotState::CopiedInstead => "Copied instead. Paste when ready.",
        ScreenshotState::SetupMissing => "Local model unavailable — run: cantrip models pull",
        ScreenshotState::StorageFailed => "Transcription failed. Audio could not be saved.",
        _ => "Transcription failed.",
    }
    .to_owned();
    if let Some(handoff) = handoff {
        // The daemon's handoff delivery messages.
        match outcome.delivery {
            Delivery::HandedOff => outcome.message = format!("Sent to {}.", handoff.label),
            Delivery::Failed => outcome.message = "Handoff failed.".to_owned(),
            _ => {}
        }
    }
    outcome.handoff = handoff.cloned();
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
    use cantrip_engine::stt::CloudFailure;
    outcome.cloud_failure = match state {
        ScreenshotState::CloudRecovered | ScreenshotState::CloudPartial => {
            Some(CloudFailure::Timeout)
        }
        ScreenshotState::CloudPayment => Some(CloudFailure::Http { status: 402 }),
        ScreenshotState::CloudBudget => Some(CloudFailure::BudgetExhausted),
        ScreenshotState::CloudConnection => Some(CloudFailure::Connection),
        _ => None,
    };
    if outcome.cloud_failure.is_some() {
        outcome.artifacts.audio = true;
        outcome.artifacts.text = matches!(
            state,
            ScreenshotState::CloudRecovered | ScreenshotState::CloudPartial
        );
    }
    if state == ScreenshotState::CleanupFallback {
        outcome.cleanup = Cleanup::Failed;
    }
    if state == ScreenshotState::KeychainPending {
        // The daemon's message for alias-corrected text copied while cleanup
        // waited for Keychain approval.
        outcome.cleanup = Cleanup::KeychainPending;
        outcome.alias_substitutions = 1;
        outcome.error = Some("cleanup-keychain-pending".to_owned());
        outcome.message = format!(
            "{} {KEYCHAIN_PENDING_REASON} Alias-corrected text was used.",
            outcome.message
        );
    }
    if state == ScreenshotState::Dismissed {
        outcome.dismissed = true;
    }
    let mut idle = preview_snapshot(StateKind::Idle);
    idle.hud.reduced_motion = Some(reduced);
    idle.pending_recordings = usize::from(outcome.artifacts.audio || outcome.artifacts.text);
    idle.outcome = Some(outcome);
    if state == ScreenshotState::KeychainPending {
        idle.keychain = Some(KeychainAccess {
            key_id: "openrouter".to_owned(),
            authorization: KeychainAuthorization::NeedsAuthorization,
        });
    }
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
        next.handoff = handoff.cloned();
        next.pipeline = Some(screenshot_pipeline(state, mode));
        model.apply(next, now - Duration::from_millis(180));
    }
    let mut at = if state == ScreenshotState::Interrupted {
        now - Duration::from_millis(180)
    } else {
        result_at
    };
    while at < now {
        model.refresh(at);
        model.track.presented = model.frame(at);
        at += FRAME_INTERVAL;
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

    fn guidance(kind: CaptureIssue) -> CaptureGuidance {
        CaptureGuidance {
            kind,
            source: Some("usb".to_owned()),
            device_name: PREVIEW_INPUT.to_owned(),
            default_input_name: None,
        }
    }

    #[test]
    fn microphone_prompt_stays_explicit_with_ordinary_captions_disabled() {
        for grantee in [
            ipc::MicrophoneGrantee::Cantrip,
            ipc::MicrophoneGrantee::Launcher(Some("Terminal".to_owned())),
            ipc::MicrophoneGrantee::Launcher(None),
        ] {
            let now = Instant::now();
            let mut model = Model::new(now);
            let mut status = preview_snapshot(StateKind::Processing);
            status.stage = Some(Stage::RequestingMicrophone);
            status.microphone = Some(ipc::MicrophoneAccess {
                authorization: ipc::MicrophoneAuthorization::Requesting,
                grantee: grantee.clone(),
            });
            status.hud.labels = false;
            model.apply(status, now);
            model.refresh(now + Duration::from_secs(1));
            assert_eq!(model.kind, Some(Kind::Working));
            assert!(model.caption.title.contains(grantee.name()));
            assert!(model.caption.title.contains("recording"));
            match grantee.disclosure() {
                Some(disclosure) => {
                    assert_eq!(model.caption.detail, disclosure);
                    assert!(model.caption.detail.contains("everything it runs"));
                }
                None => assert!(model.caption.detail.is_empty()),
            }
        }
    }

    #[test]
    fn cloud_recovery_preserves_cause_without_turning_success_into_failure() {
        let now = Instant::now();
        let status = preview_snapshot(StateKind::Idle);
        let mut outcome = preview_outcome(Completeness::Complete, Delivery::Pasted);
        outcome.cloud_failure = Some(cantrip_engine::stt::CloudFailure::Timeout);
        outcome.artifacts = ipc::Artifacts {
            take_id: Some("synthetic-take".to_owned()),
            audio: true,
            text: true,
        };
        let complete = present_outcome(&outcome, &status, now, true);
        assert_eq!(complete.kind, Kind::Resolved);
        assert!(complete.caption.title.contains("timed out"));
        assert!(!complete.caption.title.contains("budget"));
        assert!(complete.caption.detail.contains("complete"));
        assert!(complete.caption.action.contains("No retry"));
        outcome.completeness = Completeness::Partial;
        outcome.delivery = Delivery::Deferred;
        let partial = present_outcome(&outcome, &status, now, true);
        assert_eq!(partial.kind, Kind::Attention);
        assert!(partial.caption.title.contains("timed out"));
        assert!(partial.caption.detail.contains("Partial"));
        assert!(!partial.caption.action.contains("No retry"));
        assert!(partial.caption.action.contains("Recover locally"));
        outcome.delivery = Delivery::Copied;
        let copied_partial = present_outcome(&outcome, &status, now, true);
        assert!(copied_partial.caption.detail.contains("copied"));
        assert!(copied_partial
            .caption
            .action
            .contains("Review before pasting"));
        outcome.completeness = Completeness::Complete;
        outcome.delivery = Delivery::Uncertain;
        let uncertain_delivery = present_outcome(&outcome, &status, now, true);
        assert!(uncertain_delivery.caption.detail.contains("uncertain"));
        assert!(!uncertain_delivery.caption.action.contains("Copy this"));
        assert!(!uncertain_delivery.caption.action.contains("No retry"));
        outcome.error = Some("storage-failed".to_owned());
        let uncertain_storage = present_outcome(&outcome, &status, now, true);
        assert!(uncertain_storage.caption.detail.contains("unconfirmed"));
        assert!(!uncertain_storage.caption.detail.contains("saved"));
    }

    #[test]
    fn silent_recording_stays_at_rest_from_its_first_frame() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(false))), now);
        let quiet = [TrackColumn::rest(); CELLS];
        assert_eq!(model.track.frame(now).columns, quiet);
        assert_eq!(model.track.frame(now + SETTLE / 2).columns, quiet);
        model.apply(
            recording(1, Some(signal(false))),
            now + Duration::from_secs(1),
        );
        assert_eq!(
            model.track.frame(now + Duration::from_secs(1)).columns,
            quiet
        );
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

    fn raster_rows(column: TrackColumn) -> [u8; ROWS] {
        let bytes = raster_column(column, 1);
        std::array::from_fn(|row| {
            let y = 22 + (row as i32 - (ROWS / 2) as i32) * CELL_PITCH as i32;
            bytes[(y as usize * 8 + 3) * 4 + 3]
        })
    }

    fn meter_column(rows: &[u8; ROWS]) -> bool {
        let min = *rows.iter().min().unwrap();
        let max = *rows.iter().max().unwrap();
        min > 160 && max - min < 90
    }

    fn rest_column(rows: &[u8; ROWS]) -> bool {
        *rows == [(FIELD_REST * 255.0).round() as u8; ROWS]
    }

    fn first_rest_column(front: f32) -> usize {
        ((front + FRONTIER_RADIUS).ceil() as usize).min(CELLS)
    }

    fn raster_hud(model: &Model, now: Instant, width: u32, scale: f32) -> (Vec<u8>, u32) {
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let container_width = CONTAINER_WIDTH.min(width as f32 - 12.0);
        let height = (layout_height(model, &font, container_width) as f32 * scale).round() as u32;
        let width = (width as f32 * scale).round() as u32;
        let mut bytes = vec![0; (width * height * 4) as usize];
        Canvas {
            bytes: &mut bytes,
            width,
            height,
            scale,
            alpha: 1.0,
        }
        .paint_hud(
            model,
            &font,
            Palette {
                surface: [0; 3],
                border: [0, 255, 0],
                accent: [255; 3],
                foreground: [0, 0, 255],
                ..Palette::default()
            },
            &model.frame(now),
            container_width,
        );
        (bytes, width)
    }

    #[test]
    fn only_handoff_takes_are_tinted_and_labelled_and_the_tint_ends_with_its_take() {
        let now = Instant::now();
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let kaylee = ipc::Handoff {
            name: "kaylee".to_owned(),
            label: "Kaylee".to_owned(),
            color: [0xe6, 0x8b, 0xd5],
        };
        // Capsule top border at scale 1 (BGRA), right of any attention rail.
        let border = |model: &Model| {
            let (bytes, width) = raster_hud(model, now, SURFACE_WIDTH, 1.0);
            let x = (SURFACE_WIDTH - CONTAINER_WIDTH as u32) / 2 + 10;
            let index = ((6 * width + x) * 4) as usize;
            [bytes[index + 2], bytes[index + 1], bytes[index]]
        };
        for state in [
            ScreenshotState::Recording,
            ScreenshotState::Progress,
            ScreenshotState::Sent,
            ScreenshotState::DeliveryFailed,
            ScreenshotState::Cancelled,
        ] {
            let default = screenshot_model(state, now, None, None);
            let routed = screenshot_model(state, now, Some(&kaylee), None);
            let target = |model: &Model| {
                model
                    .header
                    .as_ref()
                    .and_then(|header| header.handoff.clone())
            };
            assert_eq!(target(&default), None, "{state:?}");
            assert_eq!(target(&routed), Some(kaylee.clone()), "{state:?}");
            assert_eq!(
                routed.header.as_ref().map(|header| header.route.as_str()),
                Some("to Kaylee"),
                "{state:?}"
            );
            // The target's row sits above the same line.
            assert_eq!(
                layout_height(&routed, &font, CONTAINER_WIDTH),
                layout_height(&default, &font, CONTAINER_WIDTH) + ROUTE_ROW as u32,
                "{state:?}"
            );
            assert_eq!(border(&default), [0, 255, 0], "{state:?}");
            assert_eq!(border(&routed), kaylee.color, "{state:?}");
        }
        // An expiring Kaylee result must not leave its tint or label on later idle feedback.
        let mut model = screenshot_model(ScreenshotState::Cancelled, now, Some(&kaylee), None);
        model.interaction = Some(("Nothing is recording.".to_owned(), now + NOTICE_HOLD * 3));
        model.refresh(now + NOTICE_HOLD * 2);
        assert!(model.result.is_none());
        assert!(model.header.is_none());
        // A Kaylee failure must not tint the next default take.
        let mut model = screenshot_model(ScreenshotState::DeliveryFailed, now, Some(&kaylee), None);
        let mut next = recording(0, Some(signal(true)));
        next.operation_id = Some("next-default-take".to_owned());
        model.apply(next, now + Duration::from_secs(1));
        assert!(model
            .header
            .as_ref()
            .is_some_and(|header| header.handoff.is_none()));
        assert_eq!(border(&model), [0, 255, 0]);
    }

    #[test]
    fn a_take_is_named_by_the_pipeline_it_started_with_never_the_default() {
        let now = Instant::now();
        let named = |model: &Model| {
            model
                .header
                .as_ref()
                .and_then(|header| header.line.as_ref())
                .map(|line| line.name)
        };
        // Recording, processing and the take's own outcome name the pipeline it
        // started with, while the idle engine already reports a Raw default.
        for state in [
            ScreenshotState::Recording,
            ScreenshotState::Cleaning,
            ScreenshotState::Sent,
            ScreenshotState::DeliveryFailed,
        ] {
            assert_eq!(
                named(&screenshot_model(state, now, None, None)),
                Some("Elevate"),
                "{state:?}"
            );
        }
        // A recovery runs the mode its recording was made with.
        assert_eq!(
            named(&screenshot_model(
                ScreenshotState::Recovery,
                now,
                None,
                None
            )),
            Some("Clean")
        );
        // Idle and removing saved audio run no pipeline.
        for state in [ScreenshotState::Idle, ScreenshotState::RemovingRecording] {
            assert_eq!(
                named(&screenshot_model(state, now, None, None)),
                None,
                "{state:?}"
            );
        }
        // An engine that reports no pipeline is never given one.
        let mut model = Model::new(now);
        let mut take = recording(1, Some(signal(true)));
        take.pipeline = None;
        model.apply(take, now);
        assert!(model.header.is_none());
        // The name leaves with the take's outcome.
        let mut model = screenshot_model(ScreenshotState::Cancelled, now, None, None);
        assert_eq!(named(&model), Some("Elevate"));
        model.refresh(now + NOTICE_HOLD * 2);
        assert!(model.result.is_none() && model.header.is_none());
    }

    #[test]
    fn the_line_lights_each_stage_the_engine_reports_and_where_it_sends_the_take() {
        let now = Instant::now();
        let line = |model: &Model| {
            let line = model
                .header
                .as_ref()
                .and_then(|header| header.line.as_ref());
            let line = line.expect("the take is named");
            let lit: Vec<_> = line
                .privacy
                .iter()
                .filter(|(_, lit)| *lit)
                .map(|(text, _)| text.as_str())
                .collect();
            (line.lights.clone(), line.working, lit.join(""))
        };
        let take = |state: StateKind, stage: Option<Stage>, mode: Mode| {
            let mut status = preview_snapshot(state);
            status.pipeline = Some(preview_pipeline(mode, None));
            status.stage = stage;
            // Without presentation pacing the line follows each report at once.
            status.hud.reduced_motion = Some(true);
            status
        };
        let transcribing = Stage::Transcribing {
            completed: 0,
            total: 1,
        };
        let mut model = Model::new(now);
        model.apply(take(StateKind::Recording, None, Mode::Elevate), now);
        let at_rest = model
            .header
            .as_ref()
            .and_then(|header| header.line.as_ref());
        assert_eq!(
            at_rest.map(|line| (line.promise.as_str(), line.privacy.clone())),
            Some((
                " · expert rewrite",
                vec![("↗ ".to_owned(), false), ("OpenRouter".to_owned(), false)]
            ))
        );
        assert_eq!(line(&model), (Vec::new(), None, String::new()));
        use Light::{AtWork, Done, Waiting};
        for (stage, expected) in [
            (
                transcribing.clone(),
                (vec![AtWork, Waiting, Waiting], Some("transcribing…"), ""),
            ),
            (
                Stage::CleaningUp,
                (vec![Done, Done, AtWork], Some("refining…"), "↗ OpenRouter"),
            ),
            (
                Stage::Delivering,
                (vec![Done, Done, Done], Some("delivering…"), ""),
            ),
        ] {
            model.apply(
                take(StateKind::Processing, Some(stage.clone()), Mode::Elevate),
                now,
            );
            let (lights, working, lit) = expected;
            assert_eq!(line(&model), (lights, working, lit.to_owned()), "{stage:?}");
        }
        // A take whose refinement never ran does not show it finished.
        let mut model = Model::new(now);
        let mut next = take(StateKind::Processing, Some(transcribing), Mode::Clean);
        next.operation_id = Some("short-take".to_owned());
        model.apply(next.clone(), now);
        next.stage = Some(Stage::Delivering);
        model.apply(next, now);
        assert_eq!(
            line(&model),
            (
                vec![Done, Done, Waiting],
                Some("delivering…"),
                String::new()
            )
        );
        // Once delivered, the take's line rests while its result shows.
        let sent = screenshot_model(ScreenshotState::Sent, now, None, None);
        let rest = sent.header.as_ref().and_then(|header| header.line.as_ref());
        assert_eq!(
            rest.map(|line| (line.lights.is_empty(), line.working)),
            Some((true, None))
        );
        // Raw is a single stage, and nothing leaves the device.
        let mut model = Model::new(now);
        model.apply(
            take(StateKind::Processing, Some(Stage::CleaningUp), Mode::Raw),
            now,
        );
        let raw = model
            .header
            .as_ref()
            .and_then(|header| header.line.as_ref());
        assert_eq!(
            raw.map(|line| line.privacy.clone()),
            Some(vec![(format!("on {}", modes::THIS_DEVICE), false)])
        );
    }

    #[test]
    fn the_line_keeps_the_mode_and_where_it_goes_when_space_runs_short() {
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let advance = font.as_scaled(LINE_SIZE).h_advance(font.glyph_id('M'));
        let pipeline = preview_pipeline(Mode::Elevate, None);
        let resting = HeaderLine::new(&pipeline, None);
        let working = HeaderLine::new(&pipeline, Some(TakeWork::Step(Step::Refine)));
        let privacy = "↗ OpenRouter".chars().count();
        // A width of the glyph, its space, then whole characters and a half.
        let width = |characters: f32| GLYPH_EXTENT + advance * (characters + 0.5);
        let fit = |line: &HeaderLine, width: f32| LineFit::new(line, advance, width);
        let full = header_width(CONTAINER_WIDTH);
        let everything = |middle| LineFit {
            name: 7,
            middle,
            privacy,
        };
        assert_eq!(fit(&resting, full), everything(Middle::Promise));
        assert_eq!(
            fit(&working, full),
            everything(Middle::Working { lights: true })
        );
        // Narrower, the promise and then the stage lights give way first: the
        // space, "Elevate", a gap of two, where it goes and eleven to spare.
        let narrow = width(1.0 + 7.0 + 2.0 + privacy as f32 + 11.0);
        assert_eq!(fit(&resting, narrow), everything(Middle::Nothing));
        assert_eq!(
            fit(&working, narrow),
            everything(Middle::Working { lights: false })
        );
        // Narrower still, the destination shortens but the mode stays whole.
        assert_eq!(
            fit(&working, width(1.0 + 7.0 + 2.0 + 5.0)),
            LineFit {
                name: 7,
                middle: Middle::Nothing,
                privacy: 5
            }
        );
        // Only a surface too narrow for the name shortens it.
        assert_eq!(fit(&working, width(1.0 + 4.0)).name, 4);
        assert_eq!(shortened("Elevate", 4).collect::<String>(), "Ele…");
        assert_eq!(shortened("Elevate", 7).collect::<String>(), "Elevate");
    }

    /// AppKit's Light and Dark palettes, resolved as the panel resolves its
    /// view's appearance, each with the page of its own polarity: white and
    /// black.
    #[cfg(target_os = "macos")]
    fn appkit_palettes() -> [(Palette, [u8; 3]); 2] {
        use objc2_app_kit::{NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua};
        // SAFETY: framework constants, valid for the process lifetime.
        let names = unsafe { [NSAppearanceNameAqua, NSAppearanceNameDarkAqua] };
        let [light, dark] = names.map(|name| {
            theme::resolve(&NSAppearance::appearanceNamed(name).expect("a system appearance"))
        });
        // A failed resolution falls back to Tokyo Night.
        assert!(light != Palette::default() && dark != Palette::default());
        assert_ne!(light.surface, dark.surface);
        [(light, [255; 3]), (dark, [0; 3])]
    }

    /// `model` painted at 1x with `palette`, as BGRA bytes, and its container's
    /// `[left, top, width, height]`.
    #[cfg(target_os = "macos")]
    fn painted(model: &Model, palette: Palette, now: Instant) -> (Vec<u8>, [u32; 4]) {
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let height = layout_height(model, &font, CONTAINER_WIDTH);
        let mut bytes = vec![0; (SURFACE_WIDTH * height * 4) as usize];
        Canvas {
            bytes: &mut bytes,
            width: SURFACE_WIDTH,
            height,
            scale: 1.0,
            alpha: 1.0,
        }
        .paint_hud(model, &font, palette, &model.frame(now), CONTAINER_WIDTH);
        let container = container_rect(SURFACE_WIDTH as f32, height as f32, CONTAINER_WIDTH)
            .map(|value| value as u32);
        (bytes, container)
    }

    #[cfg(target_os = "macos")]
    fn rgb_at(bytes: &[u8], x: u32, y: u32) -> [u8; 3] {
        let index = ((y * SURFACE_WIDTH + x) * 4) as usize;
        [bytes[index + 2], bytes[index + 1], bytes[index]]
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_take_line_reads_at_4_5_to_1_as_painted() {
        use crate::theme::contrast_ratio;
        let now = Instant::now();
        let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).unwrap();
        let advance = font.as_scaled(LINE_SIZE).h_advance(font.glyph_id('M'));
        let width = header_width(CONTAINER_WIDTH);
        for (palette, _) in appkit_palettes() {
            let surface = palette.surface;
            for (state, mode) in [
                (ScreenshotState::Recording, Mode::Raw),
                (ScreenshotState::Recording, Mode::Elevate),
                (ScreenshotState::Transcribing, Mode::Clean),
                (ScreenshotState::Cleaning, Mode::Clean),
                (ScreenshotState::Cleaning, Mode::Elevate),
            ] {
                let model = screenshot_model(state, now, None, Some(mode));
                let header = model.header.as_ref();
                let line = header
                    .and_then(|header| header.line.as_ref())
                    .expect("a take line");
                let (bytes, [left, top, _, _]) = painted(&model, palette, now);
                // Anti-aliasing paints the full ink only where a stroke covers
                // whole pixels: each run's most contrasting pixel must read.
                let rows = top + 5..top + header_height(header) as u32;
                let strongest = |from: f32, to: f32| {
                    (from.floor() as u32..to.ceil() as u32)
                        .flat_map(|x| rows.clone().map(move |y| (x, y)))
                        .map(|(x, y)| rgb_at(&bytes, x, y))
                        .max_by(|a, b| {
                            contrast_ratio(*a, surface).total_cmp(&contrast_ratio(*b, surface))
                        })
                        .unwrap()
                };
                // Each run's place, as the line lays them out.
                let fit = LineFit::new(line, advance, width);
                let x = left as f32 + 10.0;
                let name_end = x + GLYPH_EXTENT + advance + fit.name as f32 * advance;
                let span = |text: &str| text.chars().count() as f32 * advance;
                let middle = match fit.middle {
                    Middle::Promise => name_end..name_end + span(&line.promise),
                    Middle::Working { lights } => {
                        let lights = if lights {
                            lights_width(line.lights.len())
                        } else {
                            0.0
                        };
                        let start = name_end + advance + lights;
                        start..start + span(line.working.unwrap_or_default())
                    }
                    Middle::Nothing => panic!("{state:?} {mode:?} shows no promise or stage"),
                };
                let destination = x + width - fit.privacy as f32 * advance..x + width;
                // The destination is lit while the provider receives the text.
                let lit = line.privacy.iter().any(|(_, lit)| *lit);
                assert_eq!(
                    lit,
                    state == ScreenshotState::Cleaning,
                    "{state:?} {mode:?}"
                );
                for (part, range) in [("middle", middle), ("destination", destination)] {
                    let drawn = strongest(range.start, range.end);
                    let ratio = contrast_ratio(drawn, surface);
                    assert!(
                        ratio >= 4.5,
                        "{state:?} {mode:?} {part}: {drawn:?} on {surface:?} reads {ratio}:1"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_default_frame_reaches_3_to_1_on_its_surface_and_the_page() {
        use crate::theme::contrast_ratio;
        let now = Instant::now();
        for (palette, page) in appkit_palettes() {
            for state in [ScreenshotState::Recording, ScreenshotState::DeliveryFailed] {
                let model = screenshot_model(state, now, None, None);
                let (bytes, [left, top, width, body_height]) = painted(&model, palette, now);
                // The top edge right of any attention rail, and the surface beside the captions.
                let frame = rgb_at(&bytes, left + 10, top);
                let surface = rgb_at(&bytes, left + width - 4, top + body_height - 4);
                assert_eq!(surface, palette.surface, "{state:?}");
                for behind in [surface, page] {
                    let ratio = contrast_ratio(frame, behind);
                    assert!(
                        ratio >= 3.0,
                        "{state:?}: {frame:?} on {behind:?} reads {ratio}:1"
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_handoff_take_reads_at_4_5_to_1_on_its_washed_surface() {
        use crate::theme::contrast_ratio;
        let now = Instant::now();
        for (palette, _) in appkit_palettes() {
            for slot in 0..24 {
                let color = palette.target(slot);
                let washed = mix(palette.surface, color, ROUTE_WASH);
                let handoff = ipc::Handoff {
                    name: "kaylee".to_owned(),
                    label: "Kaylee".to_owned(),
                    color,
                };
                // Recording lights cells of the field's middle row fully; a failed
                // handoff draws the attention rail.
                for state in [ScreenshotState::Recording, ScreenshotState::DeliveryFailed] {
                    let model = screenshot_model(state, now, Some(&handoff), None);
                    let (bytes, [left, top, width, body_height]) = painted(&model, palette, now);
                    let pixel = |x: u32, y: u32| rgb_at(&bytes, x, y);
                    // The wash fills the container beside the captions.
                    assert_eq!(
                        pixel(left + width - 4, top + body_height - 4),
                        washed,
                        "slot {slot} {state:?}"
                    );
                    let drawn = if state == ScreenshotState::Recording {
                        let lit = |column: &TrackColumn| {
                            (column.opacities[ROWS / 2] * 255.0).round() == 255.0
                        };
                        assert!(model.frame(now).columns.iter().any(lit));
                        // The middle row's most contrasting pixel inside the edge: a lit cell.
                        let header = header_height(model.header.as_ref());
                        let middle = top + (header + TRACK_HEIGHT / 2.0) as u32;
                        (left + 1..left + width - 1)
                            .map(|x| pixel(x, middle))
                            .max_by(|a, b| {
                                contrast_ratio(*a, washed).total_cmp(&contrast_ratio(*b, washed))
                            })
                            .unwrap()
                    } else {
                        pixel(left + 1, top + body_height / 2)
                    };
                    let ratio = contrast_ratio(drawn, washed);
                    assert!(
                        ratio >= 4.5,
                        "slot {slot} {state:?}: {drawn:?} on {washed:?} reads {ratio}:1"
                    );
                }
                // The label and captions take their colors from the same fitted palette,
                // and an interaction notice's divider takes its frame color.
                let fitted = palette.on_surface(washed);
                let label = fitted.legible(mix(color, fitted.foreground, ROUTE_LABEL_LIFT));
                for (drawn, minimum) in [
                    (fitted.foreground, 4.5),
                    (fitted.muted(), 4.5),
                    (label, 4.5),
                    (fitted.border, 3.0),
                ] {
                    let ratio = contrast_ratio(drawn, washed);
                    assert!(
                        ratio >= minimum,
                        "slot {slot}: {drawn:?} on {washed:?} reads {ratio}:1"
                    );
                }
            }
        }
    }

    /// The lit field's bounds between `track_start` and `track_end` logical
    /// points from the top, below the header naming the take.
    fn grid_bounds(
        bytes: &[u8],
        width: u32,
        scale: f32,
        track_start: f32,
        track_end: f32,
    ) -> [u32; 4] {
        let mut bounds = [width, u32::MAX, 0, 0];
        for (index, pixel) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let x = index as u32 % width;
            let y = index as u32 / width;
            if y as f32 >= track_end * scale {
                break;
            }
            if (y as f32) < track_start * scale {
                continue;
            }
            if pixel[0] > 0 && pixel[0] == pixel[1] && pixel[1] == pixel[2] {
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x + 1);
                bounds[3] = bounds[3].max(y + 1);
            }
        }
        bounds
    }

    #[test]
    fn cleaning_and_success_fill_the_visualization_without_covering_border_or_caption() {
        let now = Instant::now();
        for state in [
            ScreenshotState::Cleaning,
            ScreenshotState::Sent,
            ScreenshotState::ReducedMotionCleaning,
            ScreenshotState::ReducedMotionSent,
        ] {
            let mut model = screenshot_model(state, now, None, None);
            let unlabelled = model.caption.clone();
            for width in [80, 240, SURFACE_WIDTH] {
                for scale in [1.0, 1.25, 2.0] {
                    model.caption = unlabelled.clone();
                    let container = CONTAINER_WIDTH.min(width as f32 - 12.0);
                    // The field's cells start below the line naming the take, whose
                    // descenders reach into the field's top margin.
                    let header = header_height(model.header.as_ref());
                    let (plain, physical_width) = raster_hud(&model, now, width, scale);
                    let bounds = grid_bounds(
                        &plain,
                        physical_width,
                        scale,
                        11.0 + header,
                        6.0 + header + TRACK_HEIGHT,
                    );
                    let left = ((width as f32 - container) / 2.0 + 6.0) * scale;
                    let right = width as f32 * scale - left;
                    assert!(
                        bounds[0] as f32 >= left - 0.5 && bounds[0] as f32 <= left + 2.0 * scale,
                        "{state:?} at {width}px/{scale}x has an unfilled left gutter: {bounds:?}"
                    );
                    assert!(
                        bounds[2] as f32 <= right + 0.5 && bounds[2] as f32 >= right - 2.0 * scale,
                        "{state:?} at {width}px/{scale}x has an unfilled right gutter: {bounds:?}"
                    );
                    assert_eq!(bounds[1], ((11.5 + header) * scale).round() as u32);
                    assert_eq!(bounds[3], ((44.5 + header) * scale).round() as u32);
                    model.caption = Caption::title("Finishing text…");
                    let (labelled, _) = raster_hud(&model, now, width, scale);
                    let track_end =
                        ((45.5 + header) * scale).floor() as usize * physical_width as usize * 4;
                    assert_eq!(
                        &plain[..track_end],
                        &labelled[..track_end],
                        "captions must remain below the visualization"
                    );
                }
            }
        }
    }

    #[test]
    fn persistent_field_is_present_in_every_composition() {
        // Quiet recording keeps every cell of the field visible and no cell active.
        let quiet = recording_frame(None);
        assert!(quiet
            .columns
            .iter()
            .all(|column| column.opacities == [FIELD_REST; ROWS]));
        // Full-scale audio lights every cell rather than changing the field's shape.
        let loud = recording_frame(Some([[i16::MIN, i16::MAX]; AUDIO_WAVEFORM_BINS]));
        assert!(loud.columns.iter().all(TrackColumn::fully_lit));
        // Measured transcription fills whole columns up to the front.
        let working = activity_frame(Kind::Working, Duration::ZERO, Some(CELLS as f32), false);
        assert!(working
            .columns
            .iter()
            .all(|column| column.opacities.iter().all(|opacity| *opacity > FIELD_REST)));
        // Cleanup, success and the resting/attention compositions never clip the field.
        let finishing = activity_frame(Kind::Finishing, Duration::ZERO, None, false);
        assert!(finishing
            .columns
            .iter()
            .all(|column| column.opacities.iter().all(|alpha| *alpha > FIELD_REST)));
        assert!(TrackColumn::filled().fully_lit());
        for column in [
            TrackColumn::rest(),
            TrackColumn::rest().with_center(0.55),
            TrackColumn::rest().with_center(0.35),
        ] {
            assert!(column.opacities.iter().all(|alpha| *alpha >= FIELD_REST));
        }
    }

    #[test]
    fn signed_columns_preserve_polarity_without_touching_neighbors() {
        let mut waveform = [[0; 2]; AUDIO_WAVEFORM_BINS];
        waveform[13] = [512, 8192];
        waveform[14] = [-8192, -512];
        let columns = recording_frame(Some(waveform)).columns;
        // A positive peak raises rows above center; its negative side stays at rest.
        assert!(columns[13].opacities[4] > FIELD_REST);
        assert!(columns[13].opacities[5] > FIELD_REST);
        assert_eq!(columns[13].opacities[2], FIELD_REST);
        // A negative peak mirrors that polarity below center.
        assert!(columns[14].opacities[2] > FIELD_REST);
        assert!(columns[14].opacities[1] > FIELD_REST);
        assert_eq!(columns[14].opacities[5], FIELD_REST);
        for index in [0, 11, 12, 15, 16, 59] {
            assert_eq!(columns[index], TrackColumn::rest(), "column {index}");
        }
    }

    #[test]
    fn raw_signal_floor_and_clipping_do_not_normalize_quiet_audio() {
        let quiet = [TrackColumn::rest(); CELLS];
        assert_eq!(recording_frame(None).columns, quiet);
        assert_eq!(
            recording_frame(Some([[-32, 32]; AUDIO_WAVEFORM_BINS])).columns,
            quiet
        );
        // A sample just above the floor raises the center cell above rest but
        // does not normalize the column to full brightness.
        let above_floor = recording_frame(Some([[-33, 33]; AUDIO_WAVEFORM_BINS])).columns[0];
        assert!(above_floor.opacities[ROWS / 2] > FIELD_REST);
        assert!(above_floor.opacities[ROWS / 2] < 0.5);
        assert_eq!(
            recording_frame(Some([[i16::MIN, i16::MAX]; AUDIO_WAVEFORM_BINS])).columns,
            [TrackColumn::filled(); CELLS]
        );
    }

    #[test]
    fn field_rows_keep_their_fixed_cells_at_every_buffer_scale() {
        for scale in [1, 2] {
            let width = (8 * scale) as usize;
            let x = (3 * scale) as usize;
            let rest = raster_column(TrackColumn::rest(), scale);
            let full = raster_column(TrackColumn::filled(), scale);
            for y in 0..(44 * scale) as usize {
                let alpha = (y * width + x) * 4 + 3;
                let in_cell = [5.5, 10.5, 15.5, 20.5, 25.5, 30.5, 35.5]
                    .into_iter()
                    .any(|top| {
                        ((top * scale as f32).round() as usize
                            ..((top + 3.0) * scale as f32).round() as usize)
                            .contains(&y)
                    });
                assert_eq!(
                    rest[alpha],
                    if in_cell {
                        (FIELD_REST * 255.0).round() as u8
                    } else {
                        0
                    },
                    "resting scale {scale} row at {y}"
                );
                assert_eq!(
                    full[alpha],
                    if in_cell { 255 } else { 0 },
                    "full scale {scale} row at {y}"
                );
            }
        }
    }

    #[test]
    fn quiet_onset_raises_the_center_row_smoothly_from_the_field() {
        for scale in [1, 2] {
            let width = (8 * scale) as usize;
            let x = (3 * scale) as usize;
            let y = (22 * scale) as usize;
            let center_alpha = |amplitude: u16| {
                let amplitude = amplitude as i16;
                let waveform = [[0, 0]; AUDIO_WAVEFORM_BINS].map(|_| [-amplitude, amplitude]);
                let column = recording_frame(Some(waveform)).columns[0];
                raster_column(column, scale)[(y * width + x) * 4 + 3]
            };
            let steps = [0_u16, 64, 128, 256, 512].map(center_alpha);
            assert!(steps.windows(2).all(|pair| pair[0] <= pair[1]));
            assert!(
                steps[1] > steps[0],
                "speech must rise above the resting field"
            );
        }
    }

    #[test]
    fn owner_silence_guidance_names_the_input_and_clears_with_signal() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(false))), now);
        // Silent PCM alone is the owner's to judge: the HUD adds no warning.
        model.apply(
            recording(9, Some(signal(false))),
            now + Duration::from_secs(9),
        );
        assert_eq!(model.kind, Some(Kind::Recording));
        let silent = guidance(CaptureIssue::SilentInput);
        let mut status = recording(9, Some(signal(false)));
        status.capture_guidance = Some(silent.clone());
        model.apply(status, now + Duration::from_millis(9100));
        assert_eq!(model.kind, Some(Kind::Attention));
        assert_eq!(model.caption.title, silent.message());
        assert_eq!(model.caption.action, SETTINGS_ACTION);
        assert!(model.active(), "the notice never ends the take");
        // The monitor fails after measuring: the owner withdraws its sample and
        // its guidance, so the input is unconfirmed rather than silent.
        model.apply(recording(9, None), now + Duration::from_millis(9500));
        assert_eq!(model.kind, Some(Kind::Attention));
        assert!(model.caption.title.contains("unavailable"));
        assert_ne!(model.caption.title, silent.message());
        assert!(model.active(), "an unknown input never ends the take");
        // A fresh measured sample clears both notices.
        model.apply(
            recording(10, Some(signal(true))),
            now + Duration::from_secs(10),
        );
        assert_eq!(model.kind, Some(Kind::Recording));
        assert_eq!(
            model.caption,
            Caption::default(),
            "a cleared notice must not latch words"
        );
    }

    #[test]
    fn idle_guidance_routes_its_failure_to_a_deliberate_input_choice() {
        let now = Instant::now();
        let default_input = "MacBook Pro Microphone";
        for (issue, named) in [
            (CaptureIssue::MissingMicrophone, Some(default_input)),
            (CaptureIssue::MissingMicrophone, None),
            (CaptureIssue::NoAudio, Some(default_input)),
        ] {
            let mut status = preview_snapshot(StateKind::Idle);
            let mut outcome = preview_outcome(Completeness::Failed, Delivery::None);
            if issue == CaptureIssue::NoAudio {
                // A finalized zero-frame take may keep its file; recovering it
                // cannot produce speech.
                outcome.artifacts = ipc::Artifacts {
                    take_id: Some("preview-take".to_owned()),
                    audio: true,
                    text: false,
                };
            }
            status.outcome = Some(outcome.clone());
            let mut guided = guidance(issue);
            guided.default_input_name = named.map(str::to_owned);
            status.capture_guidance = Some(guided.clone());
            let view = present_outcome(&outcome, &status, now, false);
            assert_eq!(view.kind, Kind::Attention);
            assert_eq!(view.caption.title, guided.message());
            let route = view.caption.action.lines().next().unwrap_or_default();
            // Only the macOS menu offers Use, for a missing microphone whose
            // default replacement the owner named; silence never swaps inputs.
            let offers_use = cfg!(target_os = "macos")
                && issue == CaptureIssue::MissingMicrophone
                && named.is_some();
            let use_named = format!("Use {default_input} instead");
            assert_eq!(route.contains(&use_named), offers_use, "{route}");
            assert!(route.contains("Settings"), "{route}");
            assert!(!view.caption.action.contains("Recover"), "{issue:?}");
        }
    }

    #[test]
    fn an_empty_silent_take_names_the_input_it_heard_nothing_from() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(preview_snapshot(StateKind::Idle), now);
        let silent = guidance(CaptureIssue::SilentInput);
        let mut status = recording(4, Some(signal(false)));
        status.capture_guidance = Some(silent.clone());
        model.apply(status, now + Duration::from_secs(4));
        let mut idle = preview_snapshot(StateKind::Idle);
        let mut outcome = preview_outcome(Completeness::Empty, Delivery::None);
        outcome.artifacts = ipc::Artifacts {
            take_id: Some("preview-take".to_owned()),
            audio: true,
            text: false,
        };
        idle.outcome = Some(outcome);
        model.apply(idle, now + Duration::from_secs(5));
        assert_eq!(model.kind, Some(Kind::Neutral));
        assert_eq!(model.caption.title, silent.message());
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
    fn transcription_fills_full_height_columns_up_to_the_measured_front() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let shown = model.frame(now + Duration::from_millis(55));
        model.track.presented = shown;
        let mut processing = preview_snapshot(StateKind::Processing);
        processing.stage = Some(Stage::Transcribing {
            completed: 1,
            total: 5,
        });
        let stop = now + Duration::from_millis(60);
        model.apply(processing, stop);
        assert_eq!(
            model.frame(stop),
            shown,
            "the field begins at the attached cells"
        );
        // Measured progress raises every row of the columns it covers. A
        // decaying cleanup-style flicker may light a short band beyond the
        // front; far columns stay at rest.
        let settled = model.frame(stop + SETTLE + PROGRESS_REVEAL);
        let front = (CELLS / 5) as f32;
        for (index, column) in settled.columns.into_iter().enumerate() {
            if index < CELLS / 5 {
                for (row, opacity) in column.opacities.into_iter().enumerate() {
                    assert!(opacity > 0.60, "meter cell {index}/{row}");
                }
            } else if (index as f32) >= front + FRONTIER_RADIUS {
                for (row, opacity) in column.opacities.into_iter().enumerate() {
                    assert_eq!(opacity, FIELD_REST, "resting cell {index}/{row}");
                }
            } else {
                assert!(
                    column
                        .opacities
                        .iter()
                        .all(|opacity| *opacity >= FIELD_REST),
                    "frontier cell {index} must stay on the field"
                );
            }
        }
    }

    #[test]
    fn transcribing_frontier_flickers_like_cleanup_beyond_the_measured_fill() {
        let front = 12.0;
        let age = Duration::from_millis(450);
        let working = activity_frame(Kind::Working, age, Some(front), false);
        let cleaning = activity_frame(Kind::Finishing, age, None, false);
        let pixels = working.columns.map(raster_rows);
        let edge = front as usize;

        for (column, rows) in pixels.iter().enumerate().take(edge) {
            assert!(
                meter_column(rows),
                "acknowledged work must stay a solid meter at {column}"
            );
        }
        assert_eq!(
            working.columns[edge], cleaning.columns[edge],
            "the first unfilled column is full-strength cleanup flicker"
        );
        for (column, rows) in pixels
            .iter()
            .enumerate()
            .take(first_rest_column(front))
            .skip(edge + 1)
        {
            let beyond = column as f32 - front;
            let t = (beyond / FRONTIER_RADIUS).min(1.0);
            let halo = (1.0 - t) * (1.0 - t);
            for row in 0..ROWS {
                let expected =
                    FIELD_REST + (cleaning.columns[column].opacities[row] - FIELD_REST) * halo;
                let got = working.columns[column].opacities[row];
                assert!(
                    (got - expected).abs() < 1e-6,
                    "column {column} row {row}: {got} vs {expected}"
                );
            }
            assert!(
                !meter_column(rows),
                "frontier {column} must not read as completed fill"
            );
        }
        assert!(pixels[first_rest_column(front)..].iter().all(rest_column));

        let nearer: u32 = pixels[edge].iter().map(|alpha| u32::from(*alpha)).sum();
        let farther: u32 = pixels[edge + 4].iter().map(|alpha| u32::from(*alpha)).sum();
        assert!(
            nearer > farther,
            "flicker must weaken with distance from the fill"
        );

        let reduced = activity_frame(Kind::Working, age, Some(front), true)
            .columns
            .map(raster_rows);
        assert!(reduced[..edge].iter().all(meter_column));
        assert!(reduced[edge..].iter().all(rest_column));
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
        let released = track.frame(gap).columns;
        assert!(
            released[0].opacities[3] > 0.5,
            "a 100 ms pause must retain a visible tail"
        );
        assert!(released[0].opacities[3] < full.columns[0].opacities[3]);
        let mut previous = released[0].opacities[3];
        for step in 1..=10 {
            let at = gap + POLL_INTERVAL * step;
            // Repeated quiet observations cannot restart a slow ease-in.
            track.target(quiet, at, WAVEFORM_EASE, false, false);
            let current = track.frame(at).columns[0].opacities[3];
            assert!(current <= previous);
            previous = current;
        }
        assert_eq!(track.frame(now + Duration::from_secs(2)), quiet);

        track.target(quiet, now, Duration::ZERO, false, true);
        track.target(full, now, WAVEFORM_EASE, false, false);
        assert_eq!(track.frame(now), quiet);
        let attack = track.frame(gap);
        assert!(
            attack.columns[0].opacities[3] > 0.9,
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
    fn measured_front_moves_spatially_without_restarting_or_inventing_stalled_progress() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Processing);
        status.stage = Some(Stage::Transcribing {
            completed: 0,
            total: 3,
        });
        model.apply(status.clone(), now);
        let reported = now + Duration::from_secs(1);
        model.track.presented = model.frame(reported);
        status.stage = Some(Stage::Transcribing {
            completed: 1,
            total: 3,
        });
        model.apply(status.clone(), reported);
        let halfway_at = reported + PROGRESS_REVEAL / 2;
        let halfway = model.frame(halfway_at);
        let pixels = halfway.columns.map(raster_rows);
        assert!(pixels[..10].iter().all(meter_column));
        assert!(!meter_column(&pixels[10]));
        assert!(pixels[first_rest_column(10.0)..].iter().all(rest_column));
        let t = 0.37_f32;
        let fractional_front = 20.0 * t * t * (3.0 - 2.0 * t);
        let fractional = model
            .frame(reported + PROGRESS_REVEAL * 37 / 100)
            .columns
            .map(raster_rows);
        let filled = fractional_front.floor() as usize;
        assert!(fractional[..filled].iter().all(meter_column));
        assert!(!meter_column(&fractional[filled]));
        assert!(fractional[first_rest_column(fractional_front)..]
            .iter()
            .all(rest_column));
        model.apply(status.clone(), halfway_at);
        assert_eq!(
            model.frame(halfway_at),
            halfway,
            "identical polls must not restart the front"
        );

        status.stage = Some(Stage::Transcribing {
            completed: 2,
            total: 3,
        });
        model.apply(status, halfway_at);
        assert_eq!(
            model.frame(halfway_at),
            halfway,
            "an interrupted reveal must continue in place"
        );
        let t = 0.25_f32;
        let advancing_front = 10.0 + 30.0 * t * t * (3.0 - 2.0 * t);
        let advancing = model
            .frame(halfway_at + PROGRESS_REVEAL / 4)
            .columns
            .map(raster_rows);
        let filled = advancing_front.floor() as usize;
        assert!(advancing[..filled].iter().all(meter_column));
        assert!(advancing[first_rest_column(advancing_front)..]
            .iter()
            .all(rest_column));
        let settled_at = halfway_at + PROGRESS_REVEAL;
        let settled = model.frame(settled_at);
        for frame in [settled, model.frame(settled_at + Duration::from_secs(60))] {
            let pixels = frame.columns.map(raster_rows);
            assert!(pixels[..40].iter().all(meter_column));
            assert!(!meter_column(&pixels[40]));
            assert!(pixels[first_rest_column(40.0)..].iter().all(rest_column));
        }
        assert_ne!(settled, model.frame(settled_at + Duration::from_secs(1)));
        model.track.presented = settled;
        model.disconnected(settled_at);
        assert!(!model.animate(settled_at));
        assert_eq!(model.frame(settled_at + POLL_INTERVAL), settled);
    }

    #[test]
    fn reset_and_changed_passes_cannot_inherit_the_previous_front() {
        for (completed, total, filled) in [(0, 3, 0), (1, 5, 12)] {
            let now = Instant::now();
            let mut model = Model::new(now);
            let mut status = preview_snapshot(StateKind::Processing);
            status.stage = Some(Stage::Transcribing {
                completed: 2,
                total: 3,
            });
            model.apply(status.clone(), now);
            let reset = now + Duration::from_secs(1);
            model.track.presented = model.frame(reset);
            status.stage = Some(Stage::Transcribing { completed, total });
            model.apply(status, reset);
            assert!(
                model
                    .frame(reset)
                    .columns
                    .map(raster_rows)
                    .iter()
                    .all(|rows| !meter_column(rows)),
                "a reset pass must not keep the previous filled meter"
            );
            let pixels = model
                .frame(reset + PROGRESS_REVEAL)
                .columns
                .map(raster_rows);
            assert!(pixels[..filled].iter().all(meter_column));
            let front = CELLS as f32 * completed as f32 / total as f32;
            assert!(pixels[first_rest_column(front)..].iter().all(rest_column));
            if filled < CELLS {
                assert!(!meter_column(&pixels[filled]));
            }
        }
    }
    #[test]
    fn unmeasured_transcription_has_a_bounded_packet_and_never_a_completed_trail() {
        for stage in [
            None,
            Some(Stage::Transcribing {
                completed: 4,
                total: 3,
            }),
        ] {
            let now = Instant::now();
            let mut model = Model::new(now);
            let mut status = preview_snapshot(StateKind::Processing);
            status.stage = stage;
            model.apply(status, now);
            let first = model
                .frame(now + Duration::from_secs(1))
                .columns
                .map(raster_rows);
            let later = model
                .frame(now + Duration::from_secs(61))
                .columns
                .map(raster_rows);
            for frame in [first, later] {
                let pending = frame.iter().map(|rows| rows[3]).min().unwrap();
                let active = frame.iter().filter(|rows| rows[3] > pending).count();
                assert!(
                    active <= 8,
                    "indeterminate activity must leave no completed trail"
                );
            }
            assert_ne!(first, later);
        }
    }

    #[test]
    fn single_chunk_waits_at_the_frontier_then_fills_the_meter() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Processing);
        status.stage = Some(Stage::Transcribing {
            completed: 0,
            total: 1,
        });
        model.apply(status.clone(), now);
        let waiting = model.frame(now + SETTLE).columns.map(raster_rows);
        assert!(waiting.iter().all(|rows| !meter_column(rows)));
        assert!(!rest_column(&waiting[0]));
        assert!(waiting[first_rest_column(0.0)..].iter().all(rest_column));

        let reported = now + Duration::from_secs(1);
        model.track.presented = model.frame(reported);
        status.stage = Some(Stage::Transcribing {
            completed: 1,
            total: 1,
        });
        model.apply(status, reported);
        let filled = model
            .frame(reported + PROGRESS_REVEAL)
            .columns
            .map(raster_rows);
        assert!(
            filled.iter().all(meter_column),
            "a completed single chunk must fill the meter"
        );
    }

    #[test]
    fn cleanup_pulses_individual_cells_but_reduced_motion_stays_fully_active_and_static() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Processing);
        status.stage = Some(Stage::CleaningUp);
        model.apply(status.clone(), now);
        let first_at = now + Duration::from_millis(900);
        let first = model.frame(first_at).columns.map(raster_rows);
        let second = model
            .frame(now + SETTLE + Duration::from_secs(1))
            .columns
            .map(raster_rows);
        assert!(first
            .iter()
            .flatten()
            .chain(second.iter().flatten())
            .all(|alpha| *alpha > 0));
        assert!(
            first
                .iter()
                .all(|column| column.windows(2).any(|rows| rows[0] != rows[1])),
            "rows must pulse independently rather than sharing a column opacity"
        );
        assert_ne!(first, second, "cleanup should remain visibly active");
        let opacities: Vec<_> = first.iter().flatten().copied().collect();
        assert!(*opacities.iter().min().unwrap() < 50);
        assert!(*opacities.iter().max().unwrap() > 230);
        let adjacent = model
            .frame(first_at + FRAME_INTERVAL)
            .columns
            .map(raster_rows);
        assert!(
            first
                .iter()
                .flatten()
                .zip(adjacent.iter().flatten())
                .all(|(from, to)| from.abs_diff(*to) <= 2),
            "independent pulses must remain smooth"
        );

        status.hud.reduced_motion = Some(true);
        let reduced_at = now + Duration::from_secs(2);
        model.apply(status, reduced_at);
        let steady = model.frame(reduced_at).columns.map(raster_rows);
        assert!(steady.iter().flatten().all(|alpha| *alpha > 0));
        assert_eq!(
            steady,
            model
                .frame(reduced_at + Duration::from_secs(60))
                .columns
                .map(raster_rows),
            "reduced motion must freeze every cell without extinguishing any"
        );
        assert!(!model.animate(reduced_at));
    }

    fn rapid_handoff(now: Instant) -> (Model, StatusSnapshot, Instant) {
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let stopped = now + Duration::from_secs(1);
        model.track.presented = model.frame(stopped);
        let mut status = preview_snapshot(StateKind::Processing);
        status.hud.labels = true;
        for (offset, stage) in [
            (0, Stage::FinalizingAudio),
            (
                20,
                Stage::Transcribing {
                    completed: 0,
                    total: 3,
                },
            ),
            (
                40,
                Stage::Transcribing {
                    completed: 1,
                    total: 3,
                },
            ),
            (
                60,
                Stage::Transcribing {
                    completed: 2,
                    total: 3,
                },
            ),
            (80, Stage::CleaningUp),
            (100, Stage::Delivering),
        ] {
            let at = stopped + Duration::from_millis(offset);
            status.stage = Some(stage);
            model.apply(status.clone(), at);
            model.track.presented = model.frame(at);
        }
        let received = stopped + Duration::from_millis(120);
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.hud.labels = true;
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle.clone(), received);
        (model, idle, received)
    }

    #[test]
    fn rapid_handoff_finishes_the_measured_front_and_cleanup_before_presenting_success() {
        let now = Instant::now();
        let (mut model, idle, received) = rapid_handoff(now);
        assert_eq!(model.snapshot.as_ref().unwrap().state, StateKind::Idle);
        assert_eq!(model.kind, Some(Kind::Working));
        assert!(model.caption.title.starts_with("Transcribing"));
        let mut completed_track = false;
        let mut cleaning_at = None;
        let mut resolved_at = None;
        let mut settled_at = None;
        let mut painted = model.track.presented.columns.map(raster_rows);
        let mut at = received;
        while at <= received + MAX_PRESENTATION_LAG + SETTLE {
            model.apply(idle.clone(), at);
            let frame = model.frame(at);
            let pixels = frame.columns.map(raster_rows);
            match model.kind {
                Some(Kind::Working) => {
                    completed_track |= pixels
                        .iter()
                        .all(|rows| rows.iter().all(|alpha| *alpha > 128));
                    assert!(model.caption.title.starts_with("Transcribing"));
                }
                Some(Kind::Finishing) => {
                    cleaning_at.get_or_insert(at);
                    assert!(!model.caption.title.starts_with("Transcribing"));
                }
                Some(Kind::Resolved) => {
                    let started = *resolved_at.get_or_insert(at);
                    if at >= started + SETTLE && pixels.iter().all(|rows| *rows == [255; ROWS]) {
                        settled_at = Some(at);
                    }
                }
                other => panic!("unexpected handoff phase: {other:?}"),
            }
            if pixels != painted {
                // Like the real renderer, do not attach equivalent pixel buffers.
                model.track.presented = frame;
                painted = pixels;
            }
            if settled_at.is_some() {
                break;
            }
            at += FRAME_INTERVAL;
        }
        assert!(
            completed_track,
            "a known complete outcome must let the final measured front finish"
        );
        let cleaning_at = cleaning_at.expect("observed cleanup must survive fast delivery");
        let resolved_at = resolved_at.expect("the bounded handoff must reach success");
        assert!(resolved_at - cleaning_at >= SETTLE + PHASE_DWELL);
        assert!(resolved_at - received <= MAX_PRESENTATION_LAG);
        let settled_at = settled_at.expect("success must reach a fully illuminated grid");
        // Hold starts from the attached settled frame, not receipt of the outcome.
        model.refresh(settled_at);
        let hold_end = settled_at + SUCCESS_HOLD;
        let mut at = settled_at;
        while at < hold_end {
            model.apply(idle.clone(), at);
            assert_eq!(model.kind, Some(Kind::Resolved));
            assert_eq!(model.alpha(at), 1.0);
            assert!(model
                .frame(at)
                .columns
                .map(raster_rows)
                .iter()
                .all(|rows| *rows == [255; ROWS]));
            at += POLL_INTERVAL;
        }
        model.refresh(hold_end + RESULT_FADE / 2);
        assert!(model.alpha(hold_end + RESULT_FADE / 2) < 1.0);
        model.apply(idle, hold_end + RESULT_FADE);
        assert!(
            model.kind.is_none(),
            "repeated outcomes must neither rearm nor replay success"
        );
    }

    #[test]
    fn cleanup_failure_caption_describes_the_delivered_alias_corrected_text() {
        let now = Instant::now();
        let status = preview_snapshot(StateKind::Idle);
        for delivery in [
            Delivery::Copied,
            Delivery::Typed,
            Delivery::Pasted,
            Delivery::HandedOff,
        ] {
            for substitutions in [0, 1] {
                let mut outcome = preview_outcome(Completeness::Complete, delivery);
                outcome.cleanup = Cleanup::Failed;
                outcome.alias_substitutions = substitutions;
                let view = present_outcome(&outcome, &status, now, false);
                let expected = match (delivery == Delivery::Copied, substitutions > 0) {
                    (true, true) => "Alias-corrected text copied. Cleanup unavailable.",
                    (false, true) => "Alias-corrected text sent. Cleanup unavailable.",
                    (true, false) => "Original text copied. Cleanup unavailable.",
                    (false, false) => "Original text sent. Cleanup unavailable.",
                };
                assert_eq!(view.caption.title, expected);
            }
        }
    }

    fn keychain_access(authorization: KeychainAuthorization) -> Option<KeychainAccess> {
        Some(KeychainAccess {
            key_id: "openrouter".to_owned(),
            authorization,
        })
    }

    /// A take whose cleanup waited for Keychain approval, as the daemon reports it.
    fn pending_cleanup(delivery: Delivery, substitutions: usize) -> TerminalOutcome {
        let mut outcome = preview_outcome(Completeness::Complete, delivery);
        outcome.cleanup = Cleanup::KeychainPending;
        outcome.alias_substitutions = substitutions;
        outcome.error = Some("cleanup-keychain-pending".to_owned());
        let kept = if substitutions > 0 {
            "Alias-corrected"
        } else {
            "Original"
        };
        outcome.message =
            format!("Copied. Paste when ready. {KEYCHAIN_PENDING_REASON} {kept} text was used.");
        outcome
    }

    #[test]
    fn keychain_pending_says_why_cleanup_skipped_and_which_text_arrived() {
        let now = Instant::now();
        let mut status = preview_snapshot(StateKind::Idle);
        status.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        for delivery in [
            Delivery::Copied,
            Delivery::Typed,
            Delivery::Pasted,
            Delivery::HandedOff,
        ] {
            for substitutions in [0, 1] {
                let outcome = pending_cleanup(delivery, substitutions);
                let view = present_outcome(&outcome, &status, now, false);
                let expected = match (delivery == Delivery::Copied, substitutions > 0) {
                    (true, true) => "Alias-corrected text copied.",
                    (false, true) => "Alias-corrected text sent.",
                    (true, false) => "Original text copied.",
                    (false, false) => "Original text sent.",
                };
                assert_eq!(view.caption.title, expected);
                assert_eq!(view.caption.detail, KEYCHAIN_PENDING_REASON);
                // Text that arrived without its cleanup is never a success.
                assert_eq!(view.kind, Kind::Attention);
                assert!(!view.waiting_for_settle);
                assert_eq!(view.dwell, ATTENTION_HOLD + RESULT_FADE);
                let caption = format!(
                    "{} {} {}",
                    view.caption.title, view.caption.detail, view.caption.action
                );
                assert!(!caption.contains("Cleanup unavailable"), "{caption}");
                assert_eq!(
                    view.caption
                        .action
                        .contains("Cantrip menu → Allow Cantrip to use your OpenRouter key"),
                    cfg!(target_os = "macos"),
                    "{caption}"
                );
            }
        }
    }

    #[test]
    fn a_pending_take_names_the_approval_only_while_the_menu_offers_it() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(preview_snapshot(StateKind::Idle), now);
        let needed = keychain_access(KeychainAuthorization::NeedsAuthorization);
        // The need settles during a take, which keeps the instrument.
        let mut take = recording(1, Some(signal(true)));
        take.keychain = needed.clone();
        model.apply(take, now + Duration::from_secs(1));
        assert_eq!(model.kind, Some(Kind::Recording));
        let mut status = preview_snapshot(StateKind::Idle);
        status.keychain = needed;
        status.outcome = Some(pending_cleanup(Delivery::Copied, 1));
        let finished = now + Duration::from_secs(2);
        model.apply(status.clone(), finished);
        let allow = "Cantrip menu → Allow Cantrip to use your OpenRouter key";
        let retry = "Cantrip menu → Try Keychain access again";
        for (authorization, route) in [
            (KeychainAuthorization::NeedsAuthorization, Some(allow)),
            (KeychainAuthorization::Authorizing, None),
            (KeychainAuthorization::Unavailable, Some(retry)),
            (KeychainAuthorization::Checking, None),
            (KeychainAuthorization::Ready, None),
        ] {
            status.keychain = keychain_access(authorization);
            model.apply(status.clone(), finished + Duration::from_secs(1));
            assert_eq!(model.kind, Some(Kind::Attention), "{authorization:?}");
            assert_eq!(model.caption.title, "Alias-corrected text copied.");
            assert_eq!(model.caption.detail, KEYCHAIN_PENDING_REASON);
            for candidate in [allow, retry] {
                assert_eq!(
                    model.caption.action.contains(candidate),
                    cfg!(target_os = "macos") && route == Some(candidate),
                    "{authorization:?}: {}",
                    model.caption.action
                );
            }
        }
        // The take said why; once it expires, no second Keychain notice follows.
        status.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        model.apply(status.clone(), finished + Duration::from_secs(2));
        model.apply(status, finished + ATTENTION_HOLD + RESULT_FADE);
        assert!(model.kind.is_none());
    }

    #[test]
    fn delivered_text_awaiting_keychain_approval_never_settles_as_success() {
        let now = Instant::now();
        let mut model = Model::new(now);
        model.apply(recording(0, Some(signal(true))), now);
        let cleaning = now + Duration::from_secs(1);
        model.track.presented = model.frame(cleaning);
        let mut processing = preview_snapshot(StateKind::Processing);
        processing.stage = Some(Stage::CleaningUp);
        model.apply(processing, cleaning);
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        idle.outcome = Some(pending_cleanup(Delivery::Copied, 1));
        let received = cleaning + FRAME_INTERVAL;
        let mut at = received;
        while at <= received + MAX_PRESENTATION_LAG + SETTLE {
            model.apply(idle.clone(), at);
            assert_eq!(model.kind, Some(Kind::Attention));
            assert!(model.work.is_none(), "no success hand-off plays out first");
            let frame = model.frame(at);
            assert!(!frame.columns.iter().all(TrackColumn::fully_lit));
            model.track.presented = frame;
            at += FRAME_INTERVAL;
        }
        assert_eq!(model.caption.title, "Alias-corrected text copied.");
    }

    #[test]
    fn keychain_approval_is_said_once_each_time_its_answer_settles() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Idle);
        // Approval guidance stays explicit with ordinary captions disabled.
        status.hud.labels = false;
        // Linux, and modes without a cleanup credential, report none.
        model.apply(status.clone(), now);
        assert!(model.kind.is_none());
        // Launch preflight: checking says nothing; its answer is said once.
        status.keychain = keychain_access(KeychainAuthorization::Checking);
        model.apply(status.clone(), now + FRAME_INTERVAL);
        assert!(model.kind.is_none());
        status.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        let launched = now + Duration::from_millis(100);
        model.apply(status.clone(), launched);
        assert_eq!(model.kind, Some(Kind::Attention));
        assert_eq!(model.caption.title, "Keychain access not yet allowed");
        assert_eq!(
            model.caption.detail,
            "Takes skip cleanup until you allow it."
        );
        assert_eq!(
            model
                .caption
                .action
                .contains("Cantrip menu → Allow Cantrip to use your OpenRouter key"),
            cfg!(target_os = "macos")
        );
        let expired = launched + ATTENTION_HOLD + RESULT_FADE;
        model.apply(status.clone(), expired);
        assert!(model.kind.is_none(), "the notice never pins the HUD");
        // A reload rechecks and reaches the same answer: not said again.
        status.keychain = keychain_access(KeychainAuthorization::Checking);
        model.apply(status.clone(), expired + Duration::from_secs(1));
        status.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        model.apply(status.clone(), expired + Duration::from_secs(2));
        assert!(model.kind.is_none());
        // The native prompt's buttons are explained for as long as it is open.
        status.keychain = keychain_access(KeychainAuthorization::Authorizing);
        let asked = expired + Duration::from_secs(3);
        model.apply(status.clone(), asked);
        assert_eq!(model.kind, Some(Kind::Neutral));
        assert_eq!(model.caption.title, KEYCHAIN_WAITING);
        assert_eq!(model.caption.detail, KEYCHAIN_BUTTONS);
        assert!(model.caption.action.is_empty());
        model.apply(status.clone(), asked + Duration::from_secs(60));
        assert_eq!(model.caption.title, KEYCHAIN_WAITING);
        // A take started meanwhile owns the instrument; the wait returns after it.
        let mut take = recording(1, Some(signal(true)));
        take.keychain = status.keychain.clone();
        model.apply(take, asked + Duration::from_secs(61));
        assert_eq!(model.kind, Some(Kind::Recording));
        model.apply(status.clone(), asked + Duration::from_secs(62));
        assert_eq!(model.caption.title, KEYCHAIN_WAITING);
        // A one-time Allow leaves later takes unable to read the key silently.
        status.keychain = keychain_access(KeychainAuthorization::NeedsAuthorization);
        let reverted = asked + Duration::from_secs(63);
        model.apply(status.clone(), reverted);
        assert_eq!(model.kind, Some(Kind::Attention));
        assert_eq!(model.caption.title, "Keychain access not yet allowed");
        // Always Allow: Ready withdraws the notice at once and says nothing.
        status.keychain = keychain_access(KeychainAuthorization::Ready);
        model.apply(status.clone(), reverted + Duration::from_secs(1));
        assert!(model.kind.is_none());
        // A key Keychain cannot return is said with its retry.
        status.keychain = keychain_access(KeychainAuthorization::Unavailable);
        model.apply(status, reverted + Duration::from_secs(2));
        assert_eq!(model.kind, Some(Kind::Attention));
        assert_eq!(
            model.caption.title,
            "Keychain didn’t return your OpenRouter key"
        );
        assert_eq!(
            model.caption.action.contains("Try Keychain access again"),
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn cleanup_on_partial_text_never_completes_unreported_chunks() {
        for (completeness, delivery) in [
            (Completeness::Partial, Delivery::Copied),
            (Completeness::Failed, Delivery::None),
            (Completeness::Cancelled, Delivery::Cancelled),
            (Completeness::Complete, Delivery::Deferred),
            (Completeness::Complete, Delivery::Uncertain),
        ] {
            let now = Instant::now();
            let mut model = Model::new(now);
            let mut status = preview_snapshot(StateKind::Processing);
            status.stage = Some(Stage::Transcribing {
                completed: 1,
                total: 3,
            });
            model.apply(status.clone(), now);
            let cleaning = now + Duration::from_millis(100);
            model.track.presented = model.frame(cleaning);
            status.stage = Some(Stage::CleaningUp);
            model.apply(status, cleaning);
            let before_handoff = now + PROGRESS_REVEAL + PHASE_DWELL - FRAME_INTERVAL;
            model.refresh(before_handoff);
            assert_eq!(model.kind, Some(Kind::Working));
            let pixels = model.frame(before_handoff).columns.map(raster_rows);
            assert!(pixels[..20].iter().all(meter_column));
            assert!(pixels[20..].iter().all(|rows| !meter_column(rows)));
            let mut idle = preview_snapshot(StateKind::Idle);
            idle.outcome = Some(preview_outcome(completeness, delivery));
            model.apply(idle, before_handoff);
            assert_ne!(model.kind, Some(Kind::Resolved));
            assert!(model.progress.is_none());
            assert!(model
                .frame(before_handoff + MAX_PRESENTATION_LAG)
                .columns
                .map(raster_rows)
                .iter()
                .all(
                    |rows| [rows[0], rows[1], rows[2], rows[4], rows[5], rows[6]]
                        .iter()
                        .all(|alpha| *alpha == (FIELD_REST * 255.0).round() as u8)
                ));
        }
    }

    #[test]
    fn pending_success_yields_to_control_outcomes_and_epoch_changes() {
        let now = Instant::now();
        for interrupt in 0..8 {
            let (mut model, mut idle, received) = rapid_handoff(now);
            let mut status = match interrupt {
                0 => {
                    let mut next = recording(0, Some(signal(true)));
                    next.operation_id = Some("next-take".to_owned());
                    next
                }
                1 => {
                    let mut cancelling = preview_snapshot(StateKind::Processing);
                    cancelling.stage = Some(Stage::Cancelling);
                    cancelling
                }
                2 => {
                    idle.outcome.as_mut().unwrap().dismissed = true;
                    idle
                }
                3 => {
                    idle.epoch = "new-epoch".to_owned();
                    idle
                }
                4 => preview_snapshot(StateKind::Unknown("new-status".to_owned())),
                _ => {
                    let delivery =
                        [Delivery::Failed, Delivery::Deferred, Delivery::Uncertain][interrupt - 5];
                    let mut outcome = preview_outcome(Completeness::Complete, delivery);
                    outcome.event_id = 2;
                    idle.outcome = Some(outcome);
                    idle
                }
            };
            status.hud.labels = true;
            let at = received + FRAME_INTERVAL;
            model.apply(status.clone(), at);
            match interrupt {
                0 => assert_eq!(model.kind, Some(Kind::Recording)),
                1 => assert!(model.caption.title.starts_with("Cancelling")),
                2 | 3 => assert!(model.kind.is_none()),
                _ => assert_eq!(model.kind, Some(Kind::Attention)),
            }
            model.apply(status, at + MAX_PRESENTATION_LAG + SETTLE);
            assert_ne!(
                model.kind,
                Some(Kind::Resolved),
                "discarded success must not reappear"
            );
        }
    }

    #[test]
    fn disconnected_or_stale_status_discards_unpresented_success() {
        let now = Instant::now();
        for stale in [false, true] {
            let (mut model, idle, received) = rapid_handoff(now);
            let lost = if stale {
                received + STATUS_STALE_AFTER
            } else {
                received
            };
            let frozen = model.track.presented;
            if stale {
                model.refresh_connection(received, lost);
            } else {
                model.disconnected(lost);
            }
            assert_eq!(model.frame(lost), frozen);
            assert!(!model.animate(lost));
            model.refresh(lost + DISCONNECT_DELAY);
            assert_eq!(model.kind, Some(Kind::Attention));
            model.apply(idle, lost + MAX_PRESENTATION_LAG);
            model.refresh(lost + MAX_PRESENTATION_LAG + SETTLE);
            assert_ne!(model.kind, Some(Kind::Resolved));
        }
    }

    #[test]
    fn reduced_motion_cuts_directly_through_pending_work_and_success() {
        let now = Instant::now();
        let (mut model, mut idle, received) = rapid_handoff(now);
        idle.hud.reduced_motion = Some(true);
        model.apply(idle, received);
        assert_eq!(model.kind, Some(Kind::Resolved));
        let frame = model.frame(received);
        assert!(frame
            .columns
            .map(raster_rows)
            .iter()
            .all(|rows| *rows == [255; ROWS]));
        assert_eq!(model.frame(received + SUCCESS_HOLD / 2), frame);
        assert!(!model.animate(received));
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
    fn failure_dismisses_without_replaying_after_a_new_take() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut failed = preview_snapshot(StateKind::Idle);
        failed.outcome = Some(preview_outcome(Completeness::Failed, Delivery::None));
        model.apply(failed.clone(), now);
        model.refresh(now + Duration::from_secs(1));
        assert_eq!(model.kind, Some(Kind::Attention));
        failed.outcome.as_mut().expect("outcome").dismissed = true;
        model.apply(failed.clone(), now + Duration::from_secs(2));
        assert!(model.kind.is_none());
        let mut next = recording(0, Some(signal(true)));
        next.operation_id = Some("other-take".to_owned());
        model.apply(next, now + Duration::from_secs(3));
        failed.outcome.as_mut().expect("outcome").dismissed = false;
        model.apply(failed, now + Duration::from_secs(4));
        assert!(
            model.kind.is_none(),
            "cached old event is not completion of a new take"
        );
    }

    #[test]
    fn attention_outcomes_expire_without_acknowledging_or_replaying() {
        for (completeness, delivery) in [
            (Completeness::Failed, Delivery::None),
            (Completeness::Partial, Delivery::None),
            (Completeness::Complete, Delivery::Failed),
            (Completeness::Complete, Delivery::Deferred),
            (Completeness::Complete, Delivery::Uncertain),
        ] {
            for reduced in [false, true] {
                let now = Instant::now();
                let mut model = Model::new(now);
                model.apply(preview_snapshot(StateKind::Idle), now);
                let shown = now + Duration::from_secs(1);
                let mut status = preview_snapshot(StateKind::Idle);
                status.hud.reduced_motion = Some(reduced);
                status.outcome = Some(preview_outcome(completeness, delivery));
                if completeness == Completeness::Failed {
                    // Capture-start failures need not have an operation identity.
                    status.outcome.as_mut().unwrap().operation_id = None;
                }
                model.apply(status.clone(), shown);
                assert_eq!(model.kind, Some(Kind::Attention));
                let hold_end = shown + Duration::from_secs(8);
                status.capabilities.dismiss = !status.capabilities.dismiss;
                model.apply(status.clone(), hold_end - FRAME_INTERVAL);
                assert_eq!(model.kind, Some(Kind::Attention));
                assert_eq!(model.alpha(hold_end - FRAME_INTERVAL), 1.0);
                if !reduced {
                    let fading = hold_end + RESULT_FADE / 2;
                    model.refresh(fading);
                    assert!(model.alpha(fading) > 0.0 && model.alpha(fading) < 1.0);
                }
                let hidden = hold_end + if reduced { Duration::ZERO } else { RESULT_FADE };
                model.apply(status.clone(), hidden);
                assert!(
                    model.kind.is_none(),
                    "{completeness:?}/{delivery:?}, reduced={reduced}"
                );
                model.apply(status.clone(), hidden + Duration::from_secs(60));
                assert!(
                    model.kind.is_none(),
                    "polling cannot replay an expired event"
                );
                let retained = model.snapshot.as_ref().unwrap().outcome.as_ref().unwrap();
                assert!(
                    !retained.dismissed,
                    "HUD expiry must not acknowledge the outcome"
                );
                assert_eq!(
                    retained.artifacts,
                    status.outcome.as_ref().unwrap().artifacts
                );
                status.outcome.as_mut().unwrap().event_id += 1;
                model.apply(status, hidden + Duration::from_secs(61));
                assert_eq!(
                    model.kind,
                    Some(Kind::Attention),
                    "a newer event presents again"
                );
            }
        }
    }

    #[test]
    fn attaching_or_changing_epoch_presents_cached_attention_once() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Idle);
        status.outcome = Some(preview_outcome(Completeness::Failed, Delivery::None));
        for shown in [now, now + Duration::from_secs(60)] {
            model.apply(status.clone(), shown);
            assert_eq!(model.kind, Some(Kind::Attention));
            model.apply(
                status.clone(),
                shown + Duration::from_secs(8) - FRAME_INTERVAL,
            );
            assert_eq!(model.kind, Some(Kind::Attention));
            model.apply(status.clone(), shown + Duration::from_secs(8) + RESULT_FADE);
            assert!(model.kind.is_none());
            model.apply(status.clone(), shown + Duration::from_secs(30));
            assert!(
                model.kind.is_none(),
                "the cached failure is presented only once per epoch"
            );
            status.epoch = "new-daemon-epoch".to_owned();
        }
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
            assert!(view.visibility.alpha(now + NOTICE_HOLD, false).is_none());
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
            assert!(
                held_frame
                    .columns
                    .into_iter()
                    .all(|column| raster_rows(column) == [255; ROWS]),
                "success must illuminate the entire grid at full accent opacity"
            );
            model.track.presented = held_frame;
            model.refresh(settled);
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
    fn a_deduplicated_final_success_frame_still_starts_and_expires_its_hold() {
        let now = Instant::now();
        let mut model = Model::new(now);
        let mut status = preview_snapshot(StateKind::Processing);
        status.stage = Some(Stage::CleaningUp);
        model.apply(status, now);
        let delivered = now + Duration::from_secs(1);
        model.track.presented = model.frame(delivered);
        let mut idle = preview_snapshot(StateKind::Idle);
        idle.outcome = Some(preview_outcome(Completeness::Complete, Delivery::Pasted));
        model.apply(idle.clone(), delivered);
        let settled = delivered + SETTLE;
        let attached = model.frame(settled - Duration::from_millis(4));
        let final_frame = model.frame(settled);
        assert_eq!(
            attached.columns.map(raster_rows),
            final_frame.columns.map(raster_rows)
        );
        assert_ne!(attached, final_frame);
        // The raster is already identical, so neither native nor gallery render
        // caching needs to attach the mathematically exact interpolation target.
        model.track.presented = attached;
        model.refresh(settled);
        model.apply(idle.clone(), settled + SUCCESS_HOLD - FRAME_INTERVAL);
        assert_eq!(model.alpha(settled + SUCCESS_HOLD - FRAME_INTERVAL), 1.0);
        let fading = settled + SUCCESS_HOLD + RESULT_FADE / 2;
        model.refresh(fading);
        assert!(model.alpha(fading) > 0.0 && model.alpha(fading) < 1.0);
        model.apply(idle, settled + SUCCESS_HOLD + RESULT_FADE);
        assert!(model.kind.is_none());
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
