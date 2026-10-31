//! The state behind both Recordings and Recovery windows: what the latest
//! observation saw, the saved takes and the one selected by its stable ID, the
//! three independent action lanes, and the Forget confirmation. Linux binds it
//! to egui and macOS to AppKit; no toolkit type appears here. Each window runs
//! the observations and jobs this state asks for on worker threads
//! ([`super::effects`]) and hands their results back, so both windows offer
//! the same actions under the same conditions. Availability here only decides
//! what a window asks for; the engine stays the authority on what happens.

use anyhow::Result;
use cantrip_engine::ipc::{Command, InteractionNotice, StateKind, StatusSnapshot, TerminalOutcome};
use cantrip_engine::recovery::Take;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a window observes live status and saved history.
pub(crate) const POLL: Duration = Duration::from_secs(2);

pub(crate) const METADATA_ONLY: &str = "Only recording details are shown here. Copying replaces your clipboard; nothing is typed into another app.";
pub(crate) const LOADING: &str = "Loading saved recordings…";
pub(crate) const STALE: &str = "Saved history could not be refreshed. Displayed details may be out of date. Refresh recordings or check setup.";
pub(crate) const EMPTY: &str = "No saved recordings yet.";
pub(crate) const MISSING: &str =
    "This recording is no longer available. Choose another recording explicitly.";
pub(crate) const PARTIAL: &str =
    "Saved text is partial. Recover the whole take before replacing text in your document.";
pub(crate) const LOCAL: &str = "Uses installed Parakeet. Audio stays here; no cloud cleanup.";
pub(crate) const PROVIDER: &str = "Sends audio to your configured STT endpoint; configured cleanup may also run. No automatic cloud fallback.";
pub(crate) const NO_AUDIO: &str = "No audio is available to transcribe again.";
pub(crate) const FORGET: &str = "Permanently delete this take’s retained audio and any incomplete transcript, and clear its pending recovery marker. Complete archived transcript text is kept. This cannot be undone.";
pub(crate) const OFFLINE: &str = "Live recording and delivery status are unknown. Saved metadata below is read from disk; actions need a daemon connection.";
pub(crate) const MODEL_MISSING: &str = "Local recovery needs the Parakeet model. Installing it downloads model files, not your recordings.";
pub(crate) const CONFIG_ATTENTION: &str = "Configuration needs attention. Open Settings to correct it; retained recordings are unchanged.";
/// Explains the API-key dialog: only the OS keyring stores the value.
pub(crate) const STORE_HELP: &str = concat!(
    "Store a provider key in the OS keyring. Use the same key ID in Settings. ",
    "Saving replaces an existing key with this ID."
);
pub(crate) const CANCELLING: &str = "Requesting cancellation…";

/// What one background observation saw: live status, saved history when its
/// revision changed or a reread was asked for, and the local setup facts.
pub(crate) struct Observation {
    pub(crate) status: Option<StatusSnapshot>,
    pub(crate) history: Option<Vec<Take>>,
    pub(crate) retained_audio_bytes: Option<u64>,
    pub(crate) revision: HistoryRevision,
    pub(crate) history_unavailable: bool,
    pub(crate) local_model: bool,
    pub(crate) config_ok: bool,
    /// The key ID the API-key dialog offers: STT's, otherwise cleanup's.
    pub(crate) key_id: Option<String>,
}

/// Saved history can only have changed when one of these did: another engine,
/// another outcome or another number of pending takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRevision {
    pub(crate) epoch: String,
    pub(crate) event: Option<u64>,
    pub(crate) pending: usize,
}

/// One effect a window runs off its UI thread.
pub(crate) enum Action {
    Command(Command),
    InstallModel { cancel: Arc<AtomicBool> },
    Doctor,
    StoreKey { id: String, secret: String },
    StartDaemon,
}

impl Action {
    /// Cancellation never waits behind another command, and setup work never
    /// blocks either of them.
    fn lane(&self) -> Lane {
        match self {
            Self::Command(Command::Cancel) => Lane::Cancel,
            Self::Command(_) => Lane::Command,
            _ => Lane::Setup,
        }
    }

    /// What the window says while this runs.
    fn busy(&self) -> &'static str {
        match self {
            Self::Command(Command::Copy { .. } | Command::Recover { .. }) => {
                "Applying recording action…"
            }
            Self::Command(Command::Forget { .. }) => "Forgetting this recording…",
            Self::Command(Command::Reload) => "Reloading configuration…",
            Self::Command(_) => "Applying action…",
            Self::InstallModel { .. } => "Downloading and verifying the local model…",
            Self::Doctor => "Checking setup…",
            Self::StoreKey { .. } => "Saving the key in the OS keyring…",
            Self::StartDaemon => "Starting Cantrip…",
        }
    }
}

pub(crate) enum ActionResult {
    /// The engine accepted a command; later snapshots describe what it did.
    CommandAccepted,
    Message(String),
    Diagnosis(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    Command,
    Setup,
    Cancel,
}

/// An action to run on a worker thread, whose result returns through
/// [`Recordings::finished`] on the same lane.
#[must_use]
pub(crate) struct Job {
    pub(crate) lane: Lane,
    pub(crate) action: Action,
}

/// One line of feedback, and whether it needs the operator's attention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Feedback {
    pub(crate) text: String,
    pub(crate) attention: bool,
}

/// The live-status actions a window may offer now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveActions {
    pub(crate) stop: bool,
    pub(crate) cancel: bool,
    pub(crate) dismiss: bool,
}

/// The actions a window may offer now for one saved take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TakeActions {
    pub(crate) copy: bool,
    pub(crate) recover_locally: bool,
    pub(crate) recover_with_provider: bool,
    pub(crate) forget: bool,
}

/// What an arriving observation changed, for the window to redraw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Observed {
    /// The saved takes were reread.
    pub(crate) history: bool,
    /// The first history load chose the initial selection.
    pub(crate) first_selection: bool,
    /// The take awaiting Forget confirmation changed; the confirmation is gone.
    pub(crate) forget_withdrawn: bool,
}

/// Why the recording list is empty, or may be out of date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ListNotice {
    Loading,
    Stale,
    Empty,
    /// Takes exist, but none waits for recovery and only waiting ones are shown.
    NothingWaiting,
}

/// The Recordings and Recovery window's state.
#[derive(Default)]
pub(crate) struct Recordings {
    observation: Option<Observation>,
    observing: bool,
    /// None while an observation is due now.
    next_poll: Option<Instant>,
    reread_history: bool,
    history_revision: Option<HistoryRevision>,
    takes: Vec<Take>,
    /// Indices of unresolved takes, in list order.
    pending: Vec<usize>,
    retained_audio_bytes: Option<u64>,
    show_all: bool,
    selected: Option<String>,
    selection_initialized: bool,
    /// The exact take shown in the Forget confirmation.
    forget: Option<Take>,
    command: bool,
    cancel: bool,
    setup: Option<&'static str>,
    /// What the command lane is doing. A cancellation also writes it.
    busy: &'static str,
    /// The lane whose result the message line reports.
    latest: Option<Lane>,
    message: Option<Feedback>,
    setup_status: Option<Feedback>,
    diagnosis: Option<String>,
    install: Option<Arc<AtomicBool>>,
    /// Closing waits for this setup work: a download or a bounded start.
    close_guard: bool,
    close_after_setup: bool,
}

impl Recordings {
    /// Whether an observation should start now, and the history revision it
    /// compares against; `Some(None)` rereads saved history unconditionally.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Option<HistoryRevision>> {
        if self.observing || self.next_poll.is_some_and(|due| now < due) {
            return None;
        }
        self.observing = true;
        self.next_poll = Some(now + POLL);
        Some(if std::mem::take(&mut self.reread_history) {
            None
        } else {
            self.history_revision.clone()
        })
    }

    /// Observe again at once and reread saved history.
    pub(crate) fn refresh(&mut self) {
        self.reread_history = true;
        self.next_poll = None;
    }

    pub(crate) fn observed(&mut self, mut observation: Observation) -> Observed {
        self.observing = false;
        let mut observed = Observed::default();
        if let Some(history) = observation.history.take() {
            self.pending = history
                .iter()
                .enumerate()
                .filter_map(|(index, take)| take.unresolved.then_some(index))
                .collect();
            self.retained_audio_bytes = observation.retained_audio_bytes;
            self.takes = history;
            observed.history = true;
        }
        if observation.history_unavailable {
            self.reread_history = true;
        } else {
            self.history_revision = Some(observation.revision.clone());
        }
        self.observation = Some(observation);
        if !self.selection_initialized && observed.history {
            self.selected = self
                .takes
                .iter()
                .find(|take| take.unresolved)
                .map(|take| take.id.clone());
            self.selection_initialized = true;
            observed.first_selection = self.selected.is_some();
        }
        // A changed or deleted take requires a new confirmation, never an
        // action on whichever take now occupies its list position.
        if self
            .forget
            .as_ref()
            .is_some_and(|confirmed| !self.takes.contains(confirmed))
        {
            self.forget = None;
            self.message = Some(Feedback {
                text: "That recording changed. Review it before choosing Forget again.".to_owned(),
                attention: true,
            });
            observed.forget_withdrawn = true;
        }
        observed
    }

    /// The observation worker stopped without an answer.
    pub(crate) fn observation_lost(&mut self, now: Instant) {
        self.observing = false;
        self.reread_history = true;
        self.next_poll = Some(now + POLL);
        if let Some(observation) = &mut self.observation {
            observation.status = None;
            observation.history_unavailable = true;
        }
        self.message = Some(Feedback {
            text: "Status refresh stopped unexpectedly. Live state and saved history could not be checked.".to_owned(),
            attention: true,
        });
    }

    /// Hand out `action` unless its lane is busy.
    pub(crate) fn submit(&mut self, action: Action) -> Option<Job> {
        let lane = action.lane();
        let busy = match lane {
            Lane::Command => self.command,
            Lane::Setup => self.setup.is_some(),
            Lane::Cancel => self.cancel,
        };
        if busy {
            return None;
        }
        match lane {
            Lane::Setup => {
                self.setup = Some(action.busy());
                self.setup_status = None;
                self.close_guard =
                    matches!(action, Action::InstallModel { .. } | Action::StartDaemon);
                if let Action::InstallModel { cancel } = &action {
                    self.install = Some(Arc::clone(cancel));
                }
            }
            Lane::Command | Lane::Cancel => {
                if lane == Lane::Command {
                    self.command = true;
                } else {
                    self.cancel = true;
                }
                self.latest = Some(lane);
                self.busy = action.busy();
                self.message = None;
            }
        }
        Some(Job { lane, action })
    }

    /// Fold in a job's result. True when the window should now close: it was
    /// asked to while setup work ran, and that work ended without a failure.
    pub(crate) fn finished(&mut self, lane: Lane, result: Result<ActionResult>) -> bool {
        let mut message = match result {
            // Acceptance is not live state. Only status snapshots describe progress.
            Ok(ActionResult::CommandAccepted) => None,
            Ok(ActionResult::Message(text)) => Some(Feedback {
                text,
                attention: false,
            }),
            Ok(ActionResult::Diagnosis(diagnosis)) => {
                self.diagnosis = Some(diagnosis);
                None
            }
            Err(error) => Some(Feedback {
                text: format!("{error:#}"),
                attention: true,
            }),
        };
        let mut close = false;
        match lane {
            Lane::Setup => {
                self.setup = None;
                self.setup_status = message.take();
                self.install = None;
                self.close_guard = false;
                close = self.close_after_setup
                    && !self
                        .setup_status
                        .as_ref()
                        .is_some_and(|status| status.attention);
                self.close_after_setup = false;
            }
            Lane::Command => self.command = false,
            Lane::Cancel => self.cancel = false,
        }
        if self.latest == Some(lane) {
            self.message = message;
        }
        self.refresh();
        close
    }

    /// The operator asked to close. True to close now; false while a download
    /// cleans up or a bounded start finishes, after which [`Self::finished`]
    /// says when to close.
    pub(crate) fn request_close(&mut self) -> bool {
        if !self.close_guard {
            return true;
        }
        self.close_after_setup = true;
        self.setup = Some(match &self.install {
            Some(cancel) => {
                cancel.store(true, Ordering::Relaxed);
                "Stopping model installation and cleaning temporary files before closing…"
            }
            None => "Finishing the bounded startup check before closing…",
        });
        false
    }

    pub(crate) fn cancel_install(&mut self) {
        if let Some(cancel) = &self.install {
            cancel.store(true, Ordering::Relaxed);
            self.setup = Some("Stopping model installation and cleaning temporary files…");
        }
    }

    pub(crate) fn stop(&mut self) -> Option<Job> {
        if !self.live_actions().stop {
            return None;
        }
        self.submit(Action::Command(Command::Stop))
    }

    pub(crate) fn cancel(&mut self) -> Option<Job> {
        if !self.live_actions().cancel {
            return None;
        }
        self.submit(Action::Command(Command::Cancel))
    }

    /// Dismiss the outcome or notice with this event ID: the one the window
    /// showed, never whichever replaced it since.
    pub(crate) fn dismiss(&mut self, event_id: u64) -> Option<Job> {
        if !self.live_actions().dismiss {
            return None;
        }
        self.submit(Action::Command(Command::Dismiss {
            event_id: Some(event_id),
        }))
    }

    pub(crate) fn copy(&mut self, id: &str) -> Option<Job> {
        if !self
            .take(id)
            .is_some_and(|take| self.take_actions(take).copy)
        {
            return None;
        }
        self.submit(Action::Command(Command::Copy { id: id.to_owned() }))
    }

    /// Transcribe a take's retained audio again and copy the result: locally
    /// with the installed model, or through the configured provider.
    pub(crate) fn recover(&mut self, id: &str, local: bool) -> Option<Job> {
        let allowed = self.take(id).is_some_and(|take| {
            let actions = self.take_actions(take);
            if local {
                actions.recover_locally
            } else {
                actions.recover_with_provider
            }
        });
        if !allowed {
            return None;
        }
        self.submit(Action::Command(Command::Recover {
            id: Some(id.to_owned()),
            local,
            clipboard: true,
        }))
    }

    /// Ask to confirm forgetting this take. True when the confirmation opened.
    pub(crate) fn ask_forget(&mut self, id: &str) -> bool {
        let Some(take) = self
            .take(id)
            .filter(|take| self.take_actions(take).forget)
            .cloned()
        else {
            return false;
        };
        self.forget = Some(take);
        true
    }

    /// Close the confirmation and keep the recording.
    pub(crate) fn keep(&mut self) {
        self.forget = None;
    }

    /// Forget exactly the take the confirmation showed, when allowed now.
    pub(crate) fn confirm_forget(&mut self) -> Option<Job> {
        if !self.can_confirm_forget() {
            return None;
        }
        let take = self.forget.take()?;
        self.submit(Action::Command(Command::Forget { id: take.id }))
    }

    pub(crate) fn install_model(&mut self) -> Option<Job> {
        if self.local_model_installed() {
            return None;
        }
        self.submit(Action::InstallModel {
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn start_daemon(&mut self) -> Option<Job> {
        if self.online() {
            return None;
        }
        self.submit(Action::StartDaemon)
    }

    pub(crate) fn reload(&mut self) -> Option<Job> {
        if !self.can_reload() {
            return None;
        }
        self.submit(Action::Command(Command::Reload))
    }

    pub(crate) fn check_setup(&mut self) -> Option<Job> {
        self.submit(Action::Doctor)
    }

    pub(crate) fn save_key(&mut self, id: String, secret: String) -> Option<Job> {
        if !self.can_save_key(&id, !secret.is_empty()) {
            return None;
        }
        self.submit(Action::StoreKey { id, secret })
    }

    /// The latest live status; None before the first observation or while
    /// Cantrip is unreachable.
    pub(crate) fn status(&self) -> Option<&StatusSnapshot> {
        self.observation
            .as_ref()
            .and_then(|observation| observation.status.as_ref())
    }

    /// True once any observation has arrived.
    pub(crate) fn observed_once(&self) -> bool {
        self.observation.is_some()
    }

    pub(crate) fn online(&self) -> bool {
        self.status().is_some()
    }

    pub(crate) fn live_actions(&self) -> LiveActions {
        let capabilities = self
            .status()
            .map(|status| status.capabilities)
            .unwrap_or_default();
        LiveActions {
            stop: capabilities.stop && !self.command,
            cancel: capabilities.cancel && !self.cancel,
            dismiss: capabilities.dismiss && !self.command,
        }
    }

    /// The latest outcome, until it is dismissed.
    pub(crate) fn outcome(&self) -> Option<&TerminalOutcome> {
        self.status()
            .and_then(|status| status.outcome.as_ref())
            .filter(|outcome| !outcome.dismissed)
    }

    pub(crate) fn notice(&self) -> Option<&InteractionNotice> {
        self.status().and_then(|status| status.notice.as_ref())
    }

    /// The latest command's result: its error or message.
    pub(crate) fn message(&self) -> Option<&Feedback> {
        self.message.as_ref()
    }

    /// Report a problem the window itself met, such as a window that did not open.
    pub(crate) fn show_problem(&mut self, text: String) {
        self.message = Some(Feedback {
            text,
            attention: true,
        });
    }

    /// What the command lane is doing, while it is busy.
    pub(crate) fn command_busy(&self) -> Option<&'static str> {
        self.command.then_some(self.busy)
    }

    pub(crate) fn cancelling(&self) -> bool {
        self.cancel
    }

    pub(crate) fn retained_summary(&self) -> Option<String> {
        self.retained_audio_bytes.filter(|bytes| *bytes > 0).map(|bytes| {
            format!(
                "{:.1} MiB of retained audio. Kept until recovery or explicit Forget; never automatically deleted.",
                bytes as f64 / 1_048_576.0
            )
        })
    }

    pub(crate) fn list_notice(&self) -> Option<ListNotice> {
        let Some(observation) = &self.observation else {
            return Some(ListNotice::Loading);
        };
        if observation.history_unavailable {
            Some(ListNotice::Stale)
        } else if self.takes.is_empty() {
            Some(ListNotice::Empty)
        } else if self.pending.is_empty() && !self.show_all {
            Some(ListNotice::NothingWaiting)
        } else {
            None
        }
    }

    /// True while the list includes completed takes, not only waiting ones.
    pub(crate) fn showing_all(&self) -> bool {
        self.show_all
    }

    pub(crate) fn show_all(&mut self, all: bool) {
        self.show_all = all;
    }

    pub(crate) fn row_count(&self) -> usize {
        if self.show_all {
            self.takes.len()
        } else {
            self.pending.len()
        }
    }

    /// The take listed in this row of the current list.
    pub(crate) fn row(&self, row: usize) -> Option<&Take> {
        let index = if self.show_all {
            row
        } else {
            *self.pending.get(row)?
        };
        self.takes.get(index)
    }

    /// The selected take's row in the current list, if it is listed.
    pub(crate) fn selected_row(&self) -> Option<usize> {
        let id = self.selected.as_deref()?;
        (0..self.row_count()).find(|row| self.row(*row).is_some_and(|take| take.id == id))
    }

    /// Select a take by its stable ID; None clears the selection.
    pub(crate) fn select(&mut self, id: Option<String>) {
        self.selected = id;
    }

    pub(crate) fn selected_id(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected take, wherever it is in saved history.
    pub(crate) fn selected(&self) -> Option<&Take> {
        self.take(self.selected.as_deref()?)
    }

    /// A take was selected and is no longer saved. The window says so instead
    /// of choosing another one.
    pub(crate) fn selection_missing(&self) -> bool {
        self.selected.is_some() && self.selected().is_none()
    }

    fn take(&self, id: &str) -> Option<&Take> {
        self.takes.iter().find(|take| take.id == id)
    }

    /// Saved history was read by the latest observation.
    pub(crate) fn history_fresh(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|observation| !observation.history_unavailable)
    }

    fn idle(&self) -> bool {
        self.status()
            .is_some_and(|status| status.state == StateKind::Idle)
    }

    /// Every take action waits for a free command lane and fresh history.
    pub(crate) fn take_actions(&self, take: &Take) -> TakeActions {
        if self.command || !self.history_fresh() {
            return TakeActions::default();
        }
        let capabilities = self
            .status()
            .map(|status| status.capabilities)
            .unwrap_or_default();
        let audio = take.audio_available;
        TakeActions {
            copy: capabilities.copy && take.text_available,
            recover_locally: capabilities.recover && capabilities.local_model && audio,
            recover_with_provider: capabilities.recover && capabilities.remote_configured && audio,
            forget: self.idle() && (audio || take.unresolved),
        }
    }

    /// The take awaiting Forget confirmation.
    pub(crate) fn forget_confirmation(&self) -> Option<&Take> {
        self.forget.as_ref()
    }

    /// Confirming needs a free command lane, fresh history and an idle engine.
    pub(crate) fn can_confirm_forget(&self) -> bool {
        !self.command && self.history_fresh() && self.idle()
    }

    /// What setup work is running, while the setup lane is busy.
    pub(crate) fn setup_busy(&self) -> Option<&'static str> {
        self.setup
    }

    pub(crate) fn setup_free(&self) -> bool {
        self.setup.is_none()
    }

    /// The latest setup result.
    pub(crate) fn setup_status(&self) -> Option<&Feedback> {
        self.setup_status.as_ref()
    }

    /// While a model installation runs: whether it can still be cancelled.
    pub(crate) fn installing(&self) -> Option<bool> {
        self.install
            .as_ref()
            .map(|cancel| !cancel.load(Ordering::Relaxed))
    }

    /// The local model's files are installed on this computer. Unknown before
    /// the first observation, which counts as not installed.
    pub(crate) fn local_model_installed(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|observation| observation.local_model)
    }

    pub(crate) fn config_needs_attention(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|observation| !observation.config_ok)
    }

    /// Reload goes through the command lane, so it waits for both lanes.
    pub(crate) fn can_reload(&self) -> bool {
        self.online() && !self.command && self.setup_free()
    }

    /// The key ID the API-key dialog starts with.
    pub(crate) fn key_id(&self) -> Option<&str> {
        self.observation
            .as_ref()
            .and_then(|observation| observation.key_id.as_deref())
    }

    /// Saving needs a key ID, an entered key and a free setup lane. The key
    /// itself is never needed to decide this.
    pub(crate) fn can_save_key(&self, id: &str, key_entered: bool) -> bool {
        !id.trim().is_empty() && key_entered && self.setup_free()
    }

    /// The latest setup check's report.
    pub(crate) fn diagnosis(&self) -> Option<&str> {
        self.diagnosis.as_deref()
    }
}

/// How much of a take's transcript is saved.
pub(crate) fn completeness(take: &Take) -> &'static str {
    if take.partial && take.text_available {
        "partial text saved"
    } else if take.text_available {
        "complete text saved"
    } else {
        "no transcript saved"
    }
}

/// Whether a take's audio is retained for recovery.
pub(crate) fn audio(take: &Take) -> &'static str {
    if take.audio_available {
        "saved on this machine"
    } else {
        "not retained"
    }
}

/// An outcome's sentence, naming a non-default handoff target.
pub(crate) fn outcome_text(outcome: &TerminalOutcome) -> String {
    match &outcome.handoff {
        Some(handoff) => format!("{} · to {}", outcome.message, handoff.label),
        None => outcome.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use cantrip_engine::config::HudConfig;
    use cantrip_engine::ipc::{Capabilities, OperationKind};

    fn take(id: &str, unresolved: bool) -> Take {
        Take {
            id: id.to_owned(),
            created_at_unix_ms: 1_700_000_000_000,
            duration_ms: Some(4_000),
            text_available: true,
            audio_available: true,
            partial: false,
            unresolved,
        }
    }

    fn status(state: StateKind) -> StatusSnapshot {
        StatusSnapshot {
            epoch: "epoch".to_owned(),
            operation_id: None,
            operation_kind: (state != StateKind::Idle).then_some(OperationKind::Dictation),
            state,
            elapsed: 0,
            signal: None,
            stage: None,
            outcome: None,
            notice: None,
            pending_recordings: 0,
            attention: false,
            capabilities: Capabilities {
                stop: false,
                cancel: false,
                recover: true,
                copy: true,
                dismiss: true,
                local_model: true,
                remote_configured: false,
            },
            hud: HudConfig::default(),
            handoff: None,
            microphone: None,
        }
    }

    fn observation(status: Option<StatusSnapshot>, history: Option<Vec<Take>>) -> Observation {
        Observation {
            revision: HistoryRevision {
                epoch: "epoch".to_owned(),
                event: None,
                pending: history.as_ref().map_or(0, |takes| {
                    takes.iter().filter(|take| take.unresolved).count()
                }),
            },
            status,
            history,
            retained_audio_bytes: None,
            history_unavailable: false,
            local_model: true,
            config_ok: true,
            key_id: None,
        }
    }

    fn idle_with(takes: Vec<Take>) -> Recordings {
        let mut recordings = Recordings::default();
        let _ = recordings.poll(Instant::now());
        recordings.observed(observation(Some(status(StateKind::Idle)), Some(takes)));
        recordings
    }

    fn command(job: Option<Job>) -> Command {
        match job.expect("a job").action {
            Action::Command(command) => command,
            _ => panic!("not a command"),
        }
    }

    #[test]
    fn selection_starts_on_the_first_waiting_take_and_follows_its_id() {
        let mut recordings = idle_with(vec![take("a", false), take("b", true), take("c", true)]);
        assert_eq!(recordings.selected().map(|take| &*take.id), Some("b"));
        assert_eq!(recordings.selected_row(), Some(0));
        assert_eq!(recordings.row_count(), 2);

        // A newer take arrives above it: the same take stays selected.
        recordings.refresh();
        let _ = recordings.poll(Instant::now());
        recordings.observed(observation(
            Some(status(StateKind::Idle)),
            Some(vec![take("d", true), take("a", false), take("b", true)]),
        ));
        assert_eq!(recordings.selected().map(|take| &*take.id), Some("b"));
        assert_eq!(recordings.selected_row(), Some(1));

        // Once it is gone, nothing else is chosen for the operator.
        recordings.refresh();
        let _ = recordings.poll(Instant::now());
        recordings.observed(observation(
            Some(status(StateKind::Idle)),
            Some(vec![take("d", true), take("a", false)]),
        ));
        assert!(recordings.selected().is_none());
        assert!(recordings.selection_missing());
        assert_eq!(recordings.selected_row(), None);
    }

    #[test]
    fn completed_takes_are_listed_only_on_request() {
        let mut recordings = idle_with(vec![take("a", false), take("b", false)]);
        assert_eq!(recordings.selected(), None);
        assert_eq!(recordings.list_notice(), Some(ListNotice::NothingWaiting));
        assert_eq!(recordings.row_count(), 0);
        recordings.show_all(true);
        assert_eq!(recordings.list_notice(), None);
        assert_eq!(recordings.row(1).map(|take| &*take.id), Some("b"));
        recordings.select(Some("b".to_owned()));
        // Hiding completed takes keeps the chosen one's details.
        recordings.show_all(false);
        assert_eq!(recordings.selected().map(|take| &*take.id), Some("b"));
        assert_eq!(recordings.selected_row(), None);
    }

    #[test]
    fn take_actions_follow_capabilities_saved_artifacts_and_lanes() {
        let mut silent = take("silent", true);
        silent.text_available = false;
        let mut recordings = idle_with(vec![silent.clone(), take("spoken", true)]);
        let actions = recordings.take_actions(&silent);
        assert!(!actions.copy && actions.recover_locally && actions.forget);
        assert!(!actions.recover_with_provider, "no provider is configured");
        assert!(recordings.copy("silent").is_none());

        assert!(matches!(
            command(recordings.recover("silent", true)),
            Command::Recover { id: Some(id), local: true, clipboard: true } if id == "silent"
        ));
        // The command lane is busy: nothing else on it, but cancellation stays free.
        assert_eq!(recordings.take_actions(&silent), TakeActions::default());
        assert!(recordings.copy("spoken").is_none());
        assert!(!recordings.ask_forget("spoken"));
        assert_eq!(
            recordings.command_busy(),
            Some("Applying recording action…")
        );
        let mut busy = status(StateKind::Recording);
        busy.capabilities.cancel = true;
        recordings.observed(observation(Some(busy), None));
        assert!(matches!(command(recordings.cancel()), Command::Cancel));
        assert!(recordings.cancel().is_none(), "one cancellation at a time");
    }

    #[test]
    fn setup_work_never_blocks_commands_or_cancellation() {
        let mut recordings = Recordings::default();
        let _ = recordings.poll(Instant::now());
        recordings.observed(observation(
            Some(status(StateKind::Idle)),
            Some(vec![take("a", true)]),
        ));
        let mut model_missing = observation(Some(status(StateKind::Idle)), None);
        model_missing.local_model = false;
        recordings.observed(model_missing);
        let install = recordings.install_model().expect("installation starts");
        assert_eq!(install.lane, Lane::Setup);
        assert!(
            recordings.check_setup().is_none(),
            "one setup job at a time"
        );
        assert!(!recordings.can_reload());
        assert!(matches!(
            command(recordings.copy("a")),
            Command::Copy { .. }
        ));
    }

    #[test]
    fn accepted_commands_wait_for_status_and_reread_history() {
        let mut recordings = idle_with(vec![take("a", true)]);
        let now = Instant::now();
        assert_eq!(recordings.poll(now), None, "an observation is not due yet");
        let job = recordings.copy("a").expect("copy is offered");
        assert!(!recordings.finished(job.lane, Ok(ActionResult::CommandAccepted)));
        assert_eq!(recordings.message(), None, "acceptance is not success");
        assert_eq!(
            recordings.poll(now),
            Some(None),
            "history is reread at once"
        );

        let job = recordings.copy("a").expect("copy is offered again");
        recordings.finished(job.lane, Err(anyhow!("Cantrip did not answer")));
        assert_eq!(
            recordings.message(),
            Some(&Feedback {
                text: "Cantrip did not answer".to_owned(),
                attention: true
            })
        );
    }

    #[test]
    fn only_the_latest_command_lane_reports_its_result() {
        let mut recordings = idle_with(vec![take("a", true)]);
        let mut recording = status(StateKind::Recording);
        recording.capabilities.stop = true;
        recording.capabilities.cancel = true;
        recordings.observed(observation(Some(recording), None));
        let stop = recordings.stop().expect("stop is offered");
        let cancel = recordings.cancel().expect("cancel is offered");
        recordings.finished(stop.lane, Err(anyhow!("stop refused")));
        assert_eq!(recordings.message(), None);
        recordings.finished(
            cancel.lane,
            Ok(ActionResult::Message("cancelled".to_owned())),
        );
        assert_eq!(recordings.message().map(|m| &*m.text), Some("cancelled"));
    }

    #[test]
    fn forget_confirms_the_exact_take_and_withdraws_when_it_changes() {
        let mut recordings = idle_with(vec![take("a", true), take("b", true)]);
        assert!(recordings.ask_forget("a"));
        assert!(recordings.can_confirm_forget());
        assert!(matches!(
            command(recordings.confirm_forget()),
            Command::Forget { id } if id == "a"
        ));
        recordings.finished(Lane::Command, Ok(ActionResult::CommandAccepted));

        assert!(recordings.ask_forget("b"));
        let mut changed = take("b", true);
        changed.audio_available = false;
        let _ = recordings.poll(Instant::now());
        let observed = recordings.observed(observation(
            Some(status(StateKind::Idle)),
            Some(vec![take("a", false), changed]),
        ));
        assert!(observed.forget_withdrawn);
        assert!(recordings.forget_confirmation().is_none());
        assert!(recordings.confirm_forget().is_none());
        assert!(recordings
            .message()
            .is_some_and(|message| message.attention));
    }

    #[test]
    fn forget_waits_for_an_idle_engine_and_fresh_history() {
        let mut recordings = idle_with(vec![take("a", true)]);
        assert!(recordings.ask_forget("a"));
        let mut recording = status(StateKind::Recording);
        recording.capabilities.stop = true;
        recordings.observed(observation(Some(recording), None));
        assert!(!recordings.can_confirm_forget());
        assert!(recordings.confirm_forget().is_none());
        assert!(
            recordings.forget_confirmation().is_some(),
            "still confirming"
        );

        recordings.observation_lost(Instant::now());
        assert!(!recordings.can_confirm_forget());
        assert_eq!(recordings.list_notice(), Some(ListNotice::Stale));
        assert_eq!(
            recordings.take_actions(&take("a", true)),
            TakeActions::default(),
            "stale history offers no take actions"
        );
    }

    #[test]
    fn a_lost_observation_retries_after_the_interval_and_rereads_history() {
        let mut recordings = idle_with(vec![take("a", true)]);
        let now = Instant::now();
        recordings.refresh();
        assert!(recordings.poll(now).is_some());
        recordings.observation_lost(now);
        assert!(recordings.status().is_none());
        assert_eq!(recordings.poll(now), None);
        assert_eq!(recordings.poll(now + POLL), Some(None));
    }

    #[test]
    fn closing_during_installation_cancels_it_and_waits_for_cleanup() {
        let mut recordings = Recordings::default();
        let job = recordings.install_model().expect("no model observed yet");
        let Action::InstallModel { cancel } = &job.action else {
            panic!("an installation")
        };
        assert!(!recordings.request_close());
        assert!(
            cancel.load(Ordering::Relaxed),
            "closing cancels the download"
        );
        assert_eq!(recordings.installing(), Some(false));
        let cancelled = "Model installation cancelled; the previous model is unchanged.";
        assert!(recordings.finished(Lane::Setup, Ok(ActionResult::Message(cancelled.to_owned()))));

        // A failed start keeps the window open to explain itself.
        let start = recordings.start_daemon().expect("offline");
        assert!(!recordings.request_close());
        assert!(!recordings.finished(start.lane, Err(anyhow!("did not start"))));
        assert!(recordings.request_close(), "nothing guards a later close");
    }
}
