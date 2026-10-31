//! The state behind both Settings windows: the configuration form with its
//! save, apply and repair lifecycle, and the past-transcript list with its
//! Copy requests. Linux binds it to egui and macOS to AppKit; no toolkit type
//! appears here. Each window renders this state, runs the effects it returns
//! off its UI thread and hands their results back, so both validate, save,
//! apply and copy alike.

use super::store::{self, EditableConfigLoad};
use crate::actions;
use crate::inject::{self, DeliveryGuard};
use anyhow::{anyhow, Result};
use cantrip_engine::config::{
    Config, HandoffTarget, HudConfig, PostprocConfig, SttConfig, TelemetryConfig,
};
use cantrip_engine::delivery::{InjectionFailureKind, InjectionMode};
use cantrip_engine::{ipc, recovery};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Every delivery mode, in the order the windows list them.
pub(crate) const INJECTION_CHOICES: [(InjectionMode, &str); 4] = [
    (InjectionMode::Auto, "Auto"),
    (InjectionMode::Paste, "Paste"),
    (InjectionMode::Type, "Type"),
    (InjectionMode::Clipboard, "Clipboard"),
];

/// The HUD motion preference: follow the desktop, or override it.
pub(crate) const MOTION_CHOICES: [(Option<bool>, &str); 3] = [
    (None, "Follow system"),
    (Some(true), "Reduced"),
    (Some(false), "Normal"),
];

/// A flat, editable view of the config, bound directly to text fields.
pub(crate) struct Editable {
    pub(crate) injection: InjectionMode,
    pub(crate) keep_warm: bool,
    /// macOS: a CoreAudio device UID. Linux: a PipeWire source name.
    pub(crate) audio_source: String,
    /// The macOS global shortcut; empty uses the default. Linux shortcuts belong
    /// to the compositor, so the value is only carried through saves there.
    pub(crate) hotkey: String,
    pub(crate) vocabulary: String,
    pub(crate) stt_model: String,
    pub(crate) stt_endpoint: String,
    pub(crate) stt_key: String,
    pub(crate) pp_enabled: bool,
    pub(crate) pp_endpoint: String,
    pub(crate) pp_model: String,
    pub(crate) pp_key: String,
    /// Not editable in the window; carried through saves so a
    /// config-file reasoning effort survives a settings write.
    pp_effort: Option<String>,
    pub(crate) pp_timeout: u64,
    pub(crate) pp_passes: u8,
    pub(crate) pp_min_chars: usize,
    pub(crate) pp_instructions: String,
    pp_decision_model: Option<String>,
    pp_decision_endpoint: Option<String>,
    pp_decision_key: Option<String>,
    /// Not editable in the window; carried through saves so enabling
    /// telemetry in the config file survives a settings write.
    telemetry: TelemetryConfig,
    pub(crate) hud: HudConfig,
    /// Local handoff commands are file-only settings; preserve them across GUI saves.
    handoff: BTreeMap<String, HandoffTarget>,
}

impl Editable {
    pub(crate) fn from_config(cfg: &Config) -> Self {
        Self {
            injection: cfg.injection,
            keep_warm: cfg.keep_warm,
            audio_source: cfg.audio_source.clone().unwrap_or_default(),
            hotkey: cfg.hotkey.clone().unwrap_or_default(),
            vocabulary: cfg.vocabulary.join(", "),
            stt_model: cfg.stt.model.clone(),
            stt_endpoint: cfg.stt.endpoint.clone().unwrap_or_default(),
            stt_key: cfg.stt.api_key_id.clone().unwrap_or_default(),
            pp_enabled: cfg.postproc.enabled,
            pp_endpoint: cfg.postproc.endpoint.clone(),
            pp_model: cfg.postproc.model.clone(),
            pp_key: cfg.postproc.api_key_id.clone().unwrap_or_default(),
            pp_effort: cfg.postproc.reasoning_effort.clone(),
            pp_timeout: cfg.postproc.timeout_ms,
            pp_passes: cfg.postproc.passes,
            pp_min_chars: cfg.postproc.min_chars,
            pp_instructions: cfg.postproc.instructions.clone(),
            pp_decision_model: cfg.postproc.decision_model.clone(),
            pp_decision_endpoint: cfg.postproc.decision_endpoint.clone(),
            pp_decision_key: cfg.postproc.decision_api_key_id.clone(),
            telemetry: cfg.telemetry.clone(),
            hud: cfg.hud,
            handoff: cfg.handoff.clone(),
        }
    }

    pub(crate) fn to_config(&self) -> Config {
        Config {
            injection: self.injection,
            keep_warm: self.keep_warm,
            audio_source: non_empty(self.audio_source.trim()),
            hotkey: non_empty(self.hotkey.trim()),
            vocabulary: self
                .vocabulary
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
            stt: SttConfig {
                model: self.stt_model.trim().to_owned(),
                endpoint: non_empty(self.stt_endpoint.trim()),
                api_key_id: non_empty(self.stt_key.trim()),
            },
            postproc: PostprocConfig {
                enabled: self.pp_enabled,
                endpoint: self.pp_endpoint.trim().to_owned(),
                model: self.pp_model.trim().to_owned(),
                api_key_id: non_empty(self.pp_key.trim()),
                reasoning_effort: self.pp_effort.clone(),
                timeout_ms: self.pp_timeout,
                passes: self.pp_passes,
                min_chars: self.pp_min_chars,
                instructions: self.pp_instructions.clone(),
                decision_model: self.pp_decision_model.clone(),
                decision_endpoint: self.pp_decision_endpoint.clone(),
                decision_api_key_id: self.pp_decision_key.clone(),
            },
            telemetry: self.telemetry.clone(),
            hud: self.hud,
            handoff: self.handoff.clone(),
            selected_handoff: None,
        }
    }
}

/// `Some(text)` for non-empty input, `None` for empty (an absent optional key).
pub(crate) fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// A line under the form: what the latest load, save or request did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) text: String,
    /// False when the operator needs to act on it.
    pub(crate) ok: bool,
}

impl Notice {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: true,
        }
    }

    fn attention(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: false,
        }
    }
}

/// Ask the running Cantrip to re-read the saved file: the window sends
/// `Command::Reload` off its UI thread and hands the reply to
/// [`SettingsForm::applied`].
#[must_use]
pub(crate) struct Apply;

/// The original text of a file that did not parse, and its replacement.
struct Repair {
    original: String,
    edited: String,
}

/// The configuration file being edited and what saving it has done so far.
pub(crate) struct SettingsForm {
    pub(crate) edit: Editable,
    path: PathBuf,
    /// False when the file is unreadable or malformed; structured saving is
    /// disabled so defaults can never overwrite configuration we could not parse.
    loaded_ok: bool,
    /// Raw file text as loaded, used to refuse clobbering concurrent edits.
    loaded_text: String,
    pub(crate) notice: Option<Notice>,
    /// The saved file is being applied by the running Cantrip.
    applying: bool,
    repair: Option<Repair>,
}

impl SettingsForm {
    pub(crate) fn load(path: PathBuf) -> Self {
        match store::load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => Self {
                edit: Editable::from_config(&config),
                path,
                loaded_ok: true,
                loaded_text: text,
                notice: warning.map(Notice::attention),
                applying: false,
                repair: None,
            },
            EditableConfigLoad::Blocked { message } => Self {
                edit: Editable::from_config(&Config::default()),
                path,
                loaded_ok: false,
                loaded_text: String::new(),
                notice: Some(Notice::attention(message)),
                applying: false,
                repair: None,
            },
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Save is offered: the file parsed and no earlier save is still applying.
    pub(crate) fn can_save(&self) -> bool {
        self.loaded_ok && !self.applying
    }

    pub(crate) fn applying(&self) -> bool {
        self.applying
    }

    /// The file could not be read or parsed, so only repairing its original
    /// text, or reloading it once fixed elsewhere, can save it.
    pub(crate) fn needs_repair(&self) -> bool {
        !self.loaded_ok && self.repair.is_none()
    }

    /// Replace the form with the file as it is now.
    pub(crate) fn reload_from_disk(&mut self) {
        match store::load_editable_config(&self.path) {
            EditableConfigLoad::Ready {
                config,
                text,
                warning,
            } => {
                self.edit = Editable::from_config(&config);
                self.loaded_ok = true;
                self.loaded_text = text;
                self.notice = Some(
                    warning.map_or_else(|| Notice::ok("Reloaded from disk"), Notice::attention),
                );
            }
            EditableConfigLoad::Blocked { message } => {
                self.loaded_ok = false;
                self.notice = Some(Notice::attention(message));
            }
        }
    }

    /// Validate the form and write it into the file, keeping its comments.
    /// Returns the request to apply it once the file holds the new values.
    pub(crate) fn save(&mut self) -> Option<Apply> {
        if self.applying {
            return None;
        }
        if !self.loaded_ok {
            self.notice = Some(Notice::attention(
                "Not saved — the config could not be loaded; fix it and reload first",
            ));
            return None;
        }
        let config = self.edit.to_config();
        if let Err(problem) = check(&config) {
            self.notice = Some(Notice::attention(format!("Not saved — {problem}")));
            return None;
        }
        match store::save_config_preserving(&self.path, &config, &self.loaded_text) {
            Ok(text) => self.loaded_text = text,
            Err(error) => {
                self.notice = Some(Notice::attention(format!("Save failed: {error:#}")));
                return None;
            }
        }
        Some(self.apply())
    }

    /// Refuse to save because the window could not read a value it shows,
    /// such as text in a number field.
    #[cfg(target_os = "macos")]
    pub(crate) fn refuse(&mut self, problem: &str) {
        self.notice = Some(Notice::attention(format!("Not saved — {problem}")));
    }

    fn apply(&mut self) -> Apply {
        self.applying = true;
        self.notice = Some(Notice::ok("Saved; applying…"));
        Apply
    }

    /// The running Cantrip's answer to [`Apply`]. The saved file stays as
    /// written whatever the answer.
    pub(crate) fn applied(&mut self, reply: Result<ipc::CommandReply>) {
        self.applying = false;
        self.notice = Some(match reply {
            Ok(reply) if reply.ok => Notice::ok("Saved and applied"),
            Ok(reply) => Notice::attention(format!(
                "Saved to disk. {}",
                reply
                    .error
                    .or(reply.message)
                    .unwrap_or_else(|| "Cantrip did not apply it.".to_owned())
            )),
            // Linux recovery lives in Cantrip actions (Start Cantrip, setup).
            Err(_) => Notice::ok(if cfg!(target_os = "macos") {
                "Saved to disk; Cantrip unreachable. Open the Cantrip menu to check setup."
            } else {
                "Saved to disk; Cantrip unreachable. Open Cantrip actions for setup."
            }),
        });
    }

    /// Open the file's original text for repair.
    pub(crate) fn start_repair(&mut self) {
        if !self.needs_repair() {
            return;
        }
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                self.repair = Some(Repair {
                    original: text.clone(),
                    edited: text,
                })
            }
            Err(_) => {
                self.notice = Some(Notice::attention(
                    "Cannot read this file. Check its ownership and permissions before editing.",
                ))
            }
        }
    }

    /// The replacement text while repairing.
    pub(crate) fn repair_text(&mut self) -> Option<&mut String> {
        self.repair.as_mut().map(|repair| &mut repair.edited)
    }

    pub(crate) fn repairing(&self) -> bool {
        self.repair.is_some()
    }

    /// Replace the file with the repaired text once it parses and validates.
    pub(crate) fn save_repair(&mut self) -> Option<Apply> {
        if self.applying {
            return None;
        }
        let repair = self.repair.as_ref()?;
        match store::save_repaired(&self.path, &repair.original, &repair.edited) {
            Ok(config) => {
                let Repair { edited, .. } = self.repair.take()?;
                self.loaded_text = edited;
                self.edit = Editable::from_config(&config);
                self.loaded_ok = true;
                Some(self.apply())
            }
            Err(error) => {
                self.notice = Some(Notice::attention(format!("Not saved: {error:#}")));
                None
            }
        }
    }

    pub(crate) fn cancel_repair(&mut self) {
        self.repair = None;
    }
}

/// What every structured save must satisfy beyond parsing. macOS also refuses
/// a dictation shortcut it could not safely register.
fn check(config: &Config) -> Result<(), String> {
    config.validate().map_err(|error| format!("{error:#}"))?;
    #[cfg(target_os = "macos")]
    crate::macos::shortcut::Shortcut::configured(config.hotkey.as_deref())?;
    Ok(())
}

/// A finished background request, or an error when its worker vanished.
pub(crate) fn completed<T>(receiver: Option<&Receiver<Result<T>>>) -> Option<Result<T>> {
    match receiver?.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => {
            Some(Err(anyhow!("The background request stopped unexpectedly")))
        }
    }
}

/// One saved transcript, formatted once per load so frames only borrow text.
pub(crate) struct PastTranscript {
    pub(crate) id: String,
    pub(crate) time: String,
    /// Capture time and duration, as in the recordings list.
    pub(crate) meta: String,
    pub(crate) preview: String,
    pub(crate) partial: bool,
}

impl PastTranscript {
    fn new(transcript: recovery::Transcript) -> Self {
        let take = transcript.take;
        let time = actions::take_time(take.created_at_unix_ms);
        Self {
            meta: format!("{time}   {}", actions::take_duration(take.duration_ms)),
            time,
            preview: transcript.preview,
            partial: take.partial,
            id: take.id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CopyPhase {
    Copying,
    Copied,
    NotCopied,
    Uncertain,
}

/// Why a Copy did not finish. Causes are fixed messages, never transcript text.
#[derive(Debug)]
pub(crate) enum CopyFailure {
    /// Nothing reached the clipboard.
    NotCopied(String),
    /// The clipboard handoff may have happened.
    Uncertain(String),
}

/// The latest Copy request, bound to the transcript ID it was made for.
pub(crate) struct CopyStatus {
    pub(crate) id: String,
    time: String,
    pub(crate) phase: CopyPhase,
    pub(crate) message: String,
}

/// Past transcripts, loaded and copied off the UI thread. Independent of the
/// config file and the daemon, so history stays usable when either fails.
#[derive(Default)]
pub(crate) struct TranscriptHistory {
    rows: Vec<PastTranscript>,
    /// "N saved transcripts", formatted when a load succeeds.
    count: String,
    /// True once any load has finished; later loads are refreshes.
    loaded: bool,
    error: Option<String>,
    loading: bool,
    copying: bool,
    copy_status: Option<CopyStatus>,
}

impl TranscriptHistory {
    /// Start a load unless one is running; listed rows stay visible meanwhile.
    /// True when the window should start [`load_transcripts`].
    pub(crate) fn start_load(&mut self) -> bool {
        !std::mem::replace(&mut self.loading, true)
    }

    pub(crate) fn loaded(&mut self, result: Result<Vec<PastTranscript>>) {
        self.loading = false;
        self.loaded = true;
        match result {
            Ok(rows) => {
                self.count = match rows.len() {
                    1 => "1 saved transcript".to_owned(),
                    count => format!("{count} saved transcripts"),
                };
                self.rows = rows;
                self.error = None;
            }
            Err(error) if self.rows.is_empty() => {
                self.error = Some(format!("Transcript history could not be read: {error:#}"));
            }
            Err(error) => {
                self.error = Some(format!(
                    "Transcript history could not be refreshed: {error:#}. The list below may be out of date."
                ));
            }
        }
    }

    /// Copy one listed row. Returns the ID for [`copy_transcript`], never its
    /// preview, or None while another Copy runs.
    pub(crate) fn start_copy(&mut self, index: usize) -> Option<String> {
        if self.copying {
            return None;
        }
        let row = self.rows.get(index)?;
        self.copy_status = Some(CopyStatus {
            id: row.id.clone(),
            time: row.time.clone(),
            phase: CopyPhase::Copying,
            message: format!("Copying transcript from {}…", row.time),
        });
        self.copying = true;
        Some(row.id.clone())
    }

    pub(crate) fn copied(&mut self, result: Result<(), CopyFailure>) {
        self.copying = false;
        if let Some(status) = &mut self.copy_status {
            (status.phase, status.message) = match result {
                Ok(()) => (
                    CopyPhase::Copied,
                    format!("Copied transcript from {}. Paste when ready.", status.time),
                ),
                Err(CopyFailure::NotCopied(cause)) => (
                    CopyPhase::NotCopied,
                    format!("Transcript from {} was not copied. {cause}", status.time),
                ),
                Err(CopyFailure::Uncertain(cause)) => (
                    CopyPhase::Uncertain,
                    format!(
                        "Transcript from {} may not have been copied. {cause}",
                        status.time
                    ),
                ),
            };
        }
    }

    /// A Copy worker that vanished may already have reached the clipboard.
    pub(crate) fn copy_lost() -> CopyFailure {
        CopyFailure::Uncertain(
            "The copy stopped unexpectedly. Check the clipboard before pasting.".to_owned(),
        )
    }

    pub(crate) fn rows(&self) -> &[PastTranscript] {
        &self.rows
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn can_copy(&self) -> bool {
        !self.copying
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub(crate) fn copy_status(&self) -> Option<&CopyStatus> {
        self.copy_status.as_ref()
    }

    /// The latest Copy's phase for this row's transcript.
    pub(crate) fn phase(&self, id: &str) -> Option<CopyPhase> {
        self.copy_status
            .as_ref()
            .filter(|status| status.id == id)
            .map(|status| status.phase)
    }

    pub(crate) fn summary(&self) -> &str {
        if self.loading && self.loaded {
            "Refreshing…"
        } else if self.loading {
            "Loading transcripts…"
        } else if !self.rows.is_empty() {
            &self.count
        } else if self.error.is_some() {
            "History unavailable"
        } else {
            "No saved transcripts yet."
        }
    }

    /// What to do when nothing is listed and nothing went wrong.
    pub(crate) fn empty_hint(&self) -> Option<&'static str> {
        (self.error.is_none() && !self.loading && self.rows.is_empty())
            .then_some("Dictate, then choose Refresh to see it here.")
    }
}

/// Read saved history newest first; runs on a worker thread.
pub(crate) fn load_transcripts() -> Result<Vec<PastTranscript>> {
    recovery::transcripts()
        .map(|transcripts| transcripts.into_iter().map(PastTranscript::new).collect())
}

/// Read one transcript fresh by ID and hand it to the existing clipboard lane.
/// The text stays in this worker; failure causes never include it.
pub(crate) fn copy_transcript(id: &str) -> Result<(), CopyFailure> {
    let text = recovery::read_text(id).map_err(|error| {
        CopyFailure::NotCopied(format!(
            "It could not be read from saved history ({error:#}). The clipboard was not changed; choose Refresh to update the list."
        ))
    })?;
    inject::inject(
        &text,
        InjectionMode::Clipboard,
        &DeliveryGuard::capture(),
        &AtomicBool::new(false),
    )
    .map(|_| ())
    .map_err(|failure| match failure.kind {
        InjectionFailureKind::Uncertain => CopyFailure::Uncertain(failure.message),
        _ => CopyFailure::NotCopied(failure.message),
    })
}

/// macOS delivers only by explicit clipboard copy and manual paste; any other
/// configured mode stays selected but is never offered.
#[cfg(target_os = "macos")]
pub(crate) fn offered(mode: InjectionMode, delivery: inject::DeliveryAvailability) -> bool {
    delivery.automatic || mode == InjectionMode::Clipboard && delivery.clipboard
}

/// What the configured delivery mode does on this Mac.
#[cfg(target_os = "macos")]
pub(crate) fn delivery_note(mode: InjectionMode, delivery: inject::DeliveryAvailability) -> Notice {
    if offered(mode, delivery) {
        return Notice::ok(
            "Each finished take is copied; paste it where you want with ⌘V. On macOS Cantrip never types or pastes into other apps.",
        );
    }
    let label = INJECTION_CHOICES
        .iter()
        .find_map(|(choice, label)| (*choice == mode).then_some(*label))
        .unwrap_or_default();
    Notice::attention(format!(
        "{label} cannot deliver on macOS: takes are saved but not sent. Choose Clipboard to copy each take for pasting."
    ))
}

/// The typed dictation shortcut as macOS would register it: its symbol label
/// (the default's when blank), or why it would be refused.
#[cfg(target_os = "macos")]
pub(crate) fn shortcut_hint(typed: &str) -> Result<String, String> {
    use crate::macos::shortcut::Shortcut;
    let typed = typed.trim();
    let shortcut = Shortcut::configured(non_empty(typed).as_deref())?;
    Ok(if typed.is_empty() {
        format!("Default: {}", shortcut.label())
    } else {
        shortcut.label().to_owned()
    })
}

/// The microphones this Mac lists, or why it could not list them. Selection
/// stores the device's stable UID; a missing device is never substituted.
#[cfg(target_os = "macos")]
pub(crate) struct Microphones(std::result::Result<Vec<crate::capture::InputDevice>, String>);

#[cfg(target_os = "macos")]
impl Microphones {
    pub(crate) fn list() -> Self {
        Self(crate::capture::input_devices().map_err(|error| format!("{error:#}")))
    }

    /// What the selected `uid` is called; empty is the system default.
    pub(crate) fn name(&self, uid: &str) -> String {
        if uid.is_empty() {
            return "System default".to_owned();
        }
        match &self.0 {
            Ok(devices) => devices.iter().find(|device| device.uid == uid).map_or_else(
                || "Unavailable microphone".to_owned(),
                |device| device.name.clone(),
            ),
            Err(_) => uid.to_owned(),
        }
    }

    /// Every choice as `(uid, label)`: the system default, then each device.
    pub(crate) fn choices(&self) -> Vec<(String, String)> {
        let mut choices = vec![(String::new(), "System default".to_owned())];
        if let Ok(devices) = &self.0 {
            choices.extend(devices.iter().map(|device| {
                let label = if device.is_default {
                    format!("{} (current default)", device.name)
                } else {
                    device.name.clone()
                };
                (device.uid.clone(), label)
            }));
        }
        choices
    }

    /// Why the selected `uid` cannot record now, if it cannot.
    pub(crate) fn problem(&self, uid: &str) -> Option<String> {
        match &self.0 {
            Err(problem) => Some(format!("Microphones could not be listed: {problem}")),
            Ok(devices)
                if !uid.is_empty() && !devices.iter().any(|device| device.uid == uid) =>
            {
                Some(
                    "This microphone is not connected. Cantrip records only from it, never another one."
                        .to_owned(),
                )
            }
            Ok(_) => None,
        }
    }
}

/// The microphone access row: the running Cantrip's answer, or why it is unknown.
/// Access belongs to the running Cantrip, which records: macOS answers each
/// process separately, so a window reads it from that process's status and
/// asks that process to request it, never consulting its own.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccessRow {
    /// No running Cantrip answered; this window's own answer never stands in.
    Unknown,
    /// The running Cantrip does not report it.
    Unreported,
    Waiting,
    Ask,
    Allowed,
    Denied,
    Restricted,
}

#[cfg(target_os = "macos")]
pub(crate) fn access_row(
    online: bool,
    reported: Option<ipc::MicrophoneAuthorization>,
    sending: bool,
) -> AccessRow {
    use ipc::MicrophoneAuthorization as Access;
    if !online {
        return AccessRow::Unknown;
    }
    match reported {
        _ if sending => AccessRow::Waiting,
        None => AccessRow::Unreported,
        Some(Access::NotDetermined) => AccessRow::Ask,
        Some(Access::Requesting) => AccessRow::Waiting,
        Some(Access::Authorized) => AccessRow::Allowed,
        Some(Access::Denied) => AccessRow::Denied,
        Some(Access::Restricted) => AccessRow::Restricted,
    }
}

/// What the microphone access row shows: the capture owner's answer, the one
/// action it offers, and, when macOS answers for another app (the one that
/// started an outside `cantrip daemon`), that app's name before Allow and in
/// the denied and restricted states, as the menu does.
#[cfg(target_os = "macos")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccessView {
    pub(crate) text: String,
    /// The Allow button's title, when Allow is offered.
    pub(crate) allow: Option<String>,
    /// Offer Open Privacy Settings.
    pub(crate) privacy: bool,
    /// Who else this grant covers, said before the operator answers.
    pub(crate) note: Option<String>,
}

#[cfg(target_os = "macos")]
pub(crate) fn access_view(
    online: bool,
    reported: Option<&ipc::MicrophoneAccess>,
    sending: bool,
) -> AccessView {
    let row = access_row(online, reported.map(|access| access.authorization), sending);
    let reported = reported.filter(|_| online);
    let target = reported
        .filter(|access| access.grantee != ipc::MicrophoneGrantee::Cantrip)
        .map_or_else(String::new, |access| {
            format!(" for {}", access.grantee.name())
        });
    let (text, allow, privacy) = match row {
        AccessRow::Unknown => (
            "Unknown while Cantrip is not running".to_owned(),
            None,
            false,
        ),
        AccessRow::Unreported => (
            "Not reported by the running Cantrip".to_owned(),
            None,
            false,
        ),
        AccessRow::Waiting => ("Waiting for your decision…".to_owned(), None, false),
        AccessRow::Ask => (
            "Not requested yet".to_owned(),
            Some(format!("Allow Microphone Access{target}…")),
            false,
        ),
        AccessRow::Allowed => ("Allowed".to_owned(), None, false),
        AccessRow::Denied => (format!("Denied{target}"), None, true),
        AccessRow::Restricted => (format!("Restricted by macOS policy{target}"), None, false),
    };
    let note = reported
        .filter(|access| access.authorization != ipc::MicrophoneAuthorization::Authorized)
        .and_then(|access| access.grantee.disclosure());
    AccessView {
        text,
        allow,
        privacy,
        note,
    }
}

/// The running Cantrip's reply to Allow: nothing to report, or why it did not ask.
#[cfg(target_os = "macos")]
pub(crate) fn allow_problem(reply: Result<ipc::CommandReply>) -> Option<String> {
    match reply {
        Ok(reply) if reply.ok => None,
        Ok(reply) => Some(
            reply
                .message
                .unwrap_or_else(|| "Cantrip did not request microphone access.".to_owned()),
        ),
        Err(_) => Some("Cantrip did not answer; open Cantrip, then try again.".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::fixture::{sample_config, ConfigFixture};

    fn reply(ok: bool) -> ipc::CommandReply {
        ipc::CommandReply {
            ok,
            state: ipc::StateKind::Idle,
            message: None,
            stage: None,
            outcome: None,
            error: (!ok).then(|| "reload-failed".to_owned()),
        }
    }

    fn row(id: &str) -> PastTranscript {
        PastTranscript {
            id: id.to_owned(),
            time: format!("time of {id}"),
            meta: String::new(),
            preview: String::new(),
            partial: false,
        }
    }

    #[test]
    fn empty_optional_fields_become_absent() {
        let mut edit = Editable::from_config(&sample_config());
        edit.audio_source = String::new();
        edit.hotkey = String::new();
        edit.vocabulary = String::new();
        edit.stt_endpoint = String::new();
        edit.stt_key = "  ".to_owned();
        edit.pp_key = String::new();
        let config = edit.to_config();
        assert_eq!(config.audio_source, None);
        assert_eq!(config.hotkey, None);
        assert_eq!(config.stt.endpoint, None);
        assert_eq!(config.stt.api_key_id, None);
        assert_eq!(config.postproc.api_key_id, None);
        assert!(config.vocabulary.is_empty());
    }

    #[test]
    fn vocabulary_splits_on_commas_and_trims() {
        let mut edit = Editable::from_config(&sample_config());
        edit.vocabulary = "PipeWire, ,Canary,Parakeet".to_owned();
        assert_eq!(
            edit.to_config().vocabulary,
            vec!["PipeWire", "Canary", "Parakeet"]
        );
    }

    #[test]
    fn invalid_cleanup_passes_stay_invalid_until_explicitly_corrected() {
        let mut config = sample_config();
        config.postproc.passes = 0;
        let mut edit = Editable::from_config(&config);
        assert!(edit.to_config().validate().is_err());
        edit.pp_passes = 1;
        assert!(edit.to_config().validate().is_ok());
    }

    #[test]
    fn saving_writes_the_form_then_waits_for_the_running_cantrip() {
        let fixture = ConfigFixture::new();
        fs::write(fixture.path(), "# mine\n[hud]\nlabels = false\n").expect("fixture");
        let mut form = SettingsForm::load(fixture.path());
        assert!(form.can_save() && form.notice.is_none());

        form.edit.hud.labels = true;
        assert!(form.save().is_some());
        let saved = fs::read_to_string(fixture.path()).expect("saved file");
        assert!(toml::from_str::<Config>(&saved).expect("parses").hud.labels);
        assert!(saved.contains("# mine"));
        // One apply at a time: Save waits for the running Cantrip's answer.
        assert!(form.applying() && !form.can_save());
        assert!(form.save().is_none());

        form.applied(Ok(reply(true)));
        assert!(form.can_save());
        assert!(form.notice.as_ref().is_some_and(|notice| notice.ok));
    }

    #[test]
    fn an_unapplied_save_keeps_the_written_file_and_says_so() {
        let fixture = ConfigFixture::new();
        let mut form = SettingsForm::load(fixture.path());
        form.edit.keep_warm = !form.edit.keep_warm;
        let expected = form.edit.keep_warm;
        assert!(form.save().is_some());

        form.applied(Ok(reply(false)));
        assert!(form.notice.as_ref().is_some_and(|notice| !notice.ok));
        assert!(form.can_save());
        let saved = fs::read_to_string(fixture.path()).expect("saved file");
        assert_eq!(
            toml::from_str::<Config>(&saved).expect("parses").keep_warm,
            expected
        );

        // Unreachable is not a refusal: the file is saved for the next start.
        assert!(form.save().is_some());
        form.applied(Err(anyhow!("connection refused")));
        assert!(form.can_save() && form.notice.is_some());
    }

    #[test]
    fn invalid_values_are_refused_without_touching_the_file() {
        let fixture = ConfigFixture::new();
        let original = "[postproc]\npasses = 2\n";
        fs::write(fixture.path(), original).expect("fixture");
        let mut form = SettingsForm::load(fixture.path());
        form.edit.pp_passes = 0;
        assert!(form.save().is_none());
        assert!(form.notice.as_ref().is_some_and(|notice| !notice.ok));
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), original);
        assert!(!form.applying() && form.can_save());

        form.edit.pp_passes = 1;
        assert!(form.save().is_some());
    }

    #[test]
    fn a_concurrent_edit_is_refused_until_reloaded_from_disk() {
        let fixture = ConfigFixture::new();
        fs::write(fixture.path(), "keep_warm = true\n").expect("fixture");
        let mut form = SettingsForm::load(fixture.path());
        let external = "keep_warm = false\n# edited elsewhere\n";
        fs::write(fixture.path(), external).expect("external edit");

        assert!(form.save().is_none());
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), external);

        form.reload_from_disk();
        assert!(!form.edit.keep_warm);
        assert!(form.notice.as_ref().is_some_and(|notice| notice.ok));
        assert!(form.save().is_some());
        assert!(fs::read_to_string(fixture.path())
            .unwrap()
            .contains("# edited elsewhere"));
    }

    #[test]
    fn a_malformed_file_saves_only_through_repair() {
        let fixture = ConfigFixture::new();
        let malformed = "injection = [not valid TOML";
        fs::write(fixture.path(), malformed).expect("fixture");
        let mut form = SettingsForm::load(fixture.path());
        assert!(form.needs_repair() && !form.can_save());
        assert!(form.save().is_none());
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), malformed);

        form.start_repair();
        assert!(form.repairing() && !form.needs_repair());
        let text = form.repair_text().expect("repairing");
        assert_eq!(text, malformed);
        *text = "injection = \"type\"\n[stt]\nmodel = \"retired-model\"\n".to_owned();
        // A replacement that parses but fails validation is still refused.
        assert!(form.save_repair().is_none());
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), malformed);

        let repaired = "injection = \"type\"\n";
        *form.repair_text().expect("still repairing") = repaired.to_owned();
        assert!(form.save_repair().is_some());
        assert_eq!(fs::read_to_string(fixture.path()).unwrap(), repaired);
        assert!(!form.repairing() && !form.needs_repair());
        assert_eq!(form.edit.injection, InjectionMode::Type);
        let backups = fs::read_dir(fixture.path().parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .map(|entry| fs::read_to_string(entry.path()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(backups, vec![malformed.to_owned()]);
    }

    #[test]
    fn cancelling_repair_keeps_the_file_blocked() {
        let fixture = ConfigFixture::new();
        fs::write(fixture.path(), "[[not toml").expect("fixture");
        let mut form = SettingsForm::load(fixture.path());
        form.start_repair();
        form.cancel_repair();
        assert!(form.needs_repair() && !form.can_save());
    }

    #[test]
    fn history_refresh_keeps_listed_rows_when_it_fails() {
        let mut history = TranscriptHistory::default();
        assert!(history.start_load());
        assert!(!history.start_load(), "one load at a time");
        history.loaded(Ok(vec![row("a"), row("b")]));
        assert!(!history.loading() && history.empty_hint().is_none());

        assert!(history.start_load());
        history.loaded(Err(anyhow!("disk unavailable")));
        assert_eq!(history.rows().len(), 2);
        assert!(history.error().is_some());

        let mut empty = TranscriptHistory::default();
        assert!(empty.start_load());
        empty.loaded(Ok(Vec::new()));
        assert!(empty.empty_hint().is_some() && empty.error().is_none());
    }

    #[test]
    fn copy_is_bound_to_the_chosen_row_and_one_runs_at_a_time() {
        let mut history = TranscriptHistory::default();
        history.start_load();
        history.loaded(Ok(vec![row("a"), row("b")]));

        assert_eq!(history.start_copy(1).as_deref(), Some("b"));
        assert!(!history.can_copy());
        assert_eq!(history.start_copy(0), None);
        assert_eq!(history.phase("b"), Some(CopyPhase::Copying));
        assert_eq!(history.phase("a"), None);

        // A refresh that reorders rows cannot retarget the running copy.
        history.start_load();
        history.loaded(Ok(vec![row("b"), row("a")]));
        history.copied(Ok(()));
        assert_eq!(history.phase("b"), Some(CopyPhase::Copied));
        assert!(history.can_copy());

        assert_eq!(history.start_copy(5), None, "no row, no request");
        assert_eq!(history.start_copy(1).as_deref(), Some("a"));
        history.copied(Err(TranscriptHistory::copy_lost()));
        assert_eq!(history.phase("a"), Some(CopyPhase::Uncertain));
        assert_eq!(history.start_copy(0).as_deref(), Some("b"));
        history.copied(Err(CopyFailure::NotCopied("stale".to_owned())));
        assert_eq!(history.phase("b"), Some(CopyPhase::NotCopied));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn microphone_access_offers_allow_only_for_the_running_cantrips_undetermined_answer() {
        use ipc::MicrophoneAuthorization as Access;
        for reported in [
            None,
            Some(Access::NotDetermined),
            Some(Access::Requesting),
            Some(Access::Authorized),
            Some(Access::Denied),
            Some(Access::Restricted),
        ] {
            for sending in [false, true] {
                // Without a running Cantrip no answer stands in, not even its last one.
                assert_eq!(access_row(false, reported, sending), AccessRow::Unknown);
                let asks = access_row(true, reported, sending) == AccessRow::Ask;
                assert_eq!(asks, reported == Some(Access::NotDetermined) && !sending);
            }
        }
        assert_eq!(allow_problem(Ok(reply(true))), None);
        assert!(allow_problem(Ok(reply(false))).is_some());
        assert!(allow_problem(Err(anyhow!("unreachable"))).is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_outside_daemons_access_row_names_the_app_macos_grants() {
        use ipc::{MicrophoneAccess, MicrophoneAuthorization as Access, MicrophoneGrantee};
        let terminal = MicrophoneGrantee::Launcher(Some("Terminal".to_owned()));
        for grantee in [
            MicrophoneGrantee::Cantrip,
            terminal,
            MicrophoneGrantee::Launcher(None),
        ] {
            let launched = grantee != MicrophoneGrantee::Cantrip;
            for authorization in [
                Access::NotDetermined,
                Access::Requesting,
                Access::Authorized,
                Access::Denied,
                Access::Restricted,
            ] {
                let reported = MicrophoneAccess {
                    authorization,
                    grantee: grantee.clone(),
                };
                let view = access_view(true, Some(&reported), false);
                let shown = format!(
                    "{} {} {}",
                    view.text,
                    view.allow.clone().unwrap_or_default(),
                    view.note.clone().unwrap_or_default()
                );
                if authorization == Access::Authorized {
                    // Granted: nothing to decide, so nothing to disclose.
                    assert_eq!(view.note, None, "{shown}");
                } else {
                    // Before Allow and while denied or restricted, an outside
                    // daemon's row says who macOS grants; Cantrip's own does not.
                    assert_eq!(view.note.is_some(), launched, "{shown}");
                }
                if launched && grantee != MicrophoneGrantee::Launcher(None) {
                    let named = shown.contains("Terminal");
                    assert_eq!(named, authorization != Access::Authorized, "{shown}");
                }
                assert_eq!(view.allow.is_some(), authorization == Access::NotDetermined);
                assert_eq!(view.privacy, authorization == Access::Denied);
                // Without a running Cantrip, nobody's grant is known or named.
                let offline = access_view(false, Some(&reported), false);
                assert_eq!((offline.allow, offline.note), (None, None));
            }
        }
        let own = MicrophoneAccess {
            authorization: Access::Denied,
            grantee: MicrophoneGrantee::Cantrip,
        };
        assert!(!access_view(true, Some(&own), false).text.contains(" for "));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_missing_microphone_stays_selected_and_is_reported() {
        use crate::capture::InputDevice;
        let listed = Microphones(Ok(vec![InputDevice {
            uid: "usb".to_owned(),
            name: "USB".to_owned(),
            is_default: true,
        }]));
        assert_eq!(listed.problem(""), None);
        assert_eq!(listed.problem("usb"), None);
        assert!(listed.problem("unplugged").is_some());
        assert_ne!(listed.name("unplugged"), listed.name("usb"));
        let uids = listed
            .choices()
            .into_iter()
            .map(|(uid, _)| uid)
            .collect::<Vec<_>>();
        assert_eq!(uids, ["", "usb"]);

        let failed = Microphones(Err("CoreAudio unavailable".to_owned()));
        assert!(failed.problem("").is_some());
        assert_eq!(failed.choices().len(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_shortcut_hint_names_what_macos_would_register() {
        assert!(shortcut_hint("").is_ok());
        assert!(shortcut_hint("Command+Shift+D").is_ok());
        assert!(shortcut_hint("D").is_err(), "a bare key captures typing");
        assert!(shortcut_hint("not a shortcut").is_err());
        assert_ne!(shortcut_hint(""), shortcut_hint("Command+Shift+D"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_offers_only_clipboard_and_explains_unsupported_modes() {
        let delivery = inject::DeliveryAvailability {
            keyboard: false,
            clipboard: true,
            automatic: false,
            reason: None,
        };
        for (mode, _) in INJECTION_CHOICES {
            let offered = offered(mode, delivery);
            assert_eq!(offered, mode == InjectionMode::Clipboard);
            assert_eq!(delivery_note(mode, delivery).ok, offered);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saving_refuses_a_shortcut_macos_would_not_register() {
        let fixture = ConfigFixture::new();
        let mut form = SettingsForm::load(fixture.path());
        form.edit.hotkey = "D".to_owned();
        assert!(form.save().is_none());
        assert!(!fixture.path().exists(), "nothing was written");
        form.edit.hotkey = "Command+Shift+D".to_owned();
        assert!(form.save().is_some());
    }
}
