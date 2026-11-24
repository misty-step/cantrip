//! The facade the UI drives. The UI is a pure view: each frame it reads one
//! [`UiSnapshot`] and turns user input into [`Command`]s. It never holds policy.

use crate::shortcut::Chord;
use crate::speech::Mode;
use crate::surface::{Hud, RecoveryAction, RecoveryRow};
use std::path::PathBuf;

/// State of the OpenRouter key as Settings may show it (never the key itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyState {
    Saved,
    NotSet,
    /// The secret store failed; this is not "no key".
    Error(String),
}

/// Everything the popover, Settings window and HUD render.
#[derive(Clone, Debug, PartialEq)]
pub struct UiSnapshot {
    // Popover
    pub dictating: bool,
    pub meeting_active: bool,
    pub mode: Mode,
    pub post_processing: bool,
    pub has_last_dictation: bool,
    /// The contextual recovery row, `None` when nothing is staged.
    pub recovery: Option<RecoveryRow>,
    /// One-word status shown in the popover header ("Idle", "Listening", …).
    pub status: String,
    // HUD
    pub hud: Hud,
    /// Live input level 0..1 while listening.
    pub level: f32,
    // Settings
    pub shortcut: Chord,
    pub microphone: Option<String>,
    /// (id, name) of available inputs.
    pub microphones: Vec<(String, String)>,
    pub key: KeyState,
    pub vocabulary: Vec<String>,
    pub meeting_folder: PathBuf,
    /// Inline rejection under the Meeting folder row, if the last choice failed.
    pub folder_error: Option<String>,
    pub open_at_login: bool,
    /// Inline error under Open at login, if toggling failed.
    pub login_error: Option<String>,
    /// Inline error under the key row, if saving/removing failed.
    pub key_error: Option<String>,
}

/// Every action a user can take from the closed control surface.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    ToggleDictation,
    ToggleMeeting,
    SetMode(Mode),
    SetPostProcessing(bool),
    CopyLastDictation,
    OpenSettings,
    Quit,
    /// Recovery row action on the item `item_id`. `Discard` arrives only after
    /// the UI's confirmation.
    Recovery {
        item_id: String,
        action: RecoveryAction,
    },
    // Settings pane
    SetShortcut(Chord),
    SetMicrophone(Option<String>),
    SaveKey(String),
    RemoveKey,
    AddTerm(String),
    RemoveTerm(String),
    ChooseMeetingFolder(PathBuf),
    SetOpenAtLogin(bool),
}

/// Implemented by the running app; a fake implements it for UI galleries.
pub trait Control: Send + Sync {
    fn snapshot(&self) -> UiSnapshot;
    fn send(&self, cmd: Command);
}

// ───────────────────────────── the running app ─────────────────────────────

use crate::dictation::{Dictation, DictationDeps, TakeConfig};
use crate::meeting::{self, MeetingDeps, Meetings};
use crate::ports::{LoginItem, Recorder, SecretError, SecretStore};
use crate::settings::Settings;
use crate::shortcut::{KeyEvent, PressTracker, ShortcutAction};
use crate::staging::Staging;
use crate::surface::RecoveryKind;
use crate::vocabulary;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// HUD notice when sleep stopped a meeting.
pub const MEETING_STOPPED_SLEEP: &str = "Meeting stopped: the computer went to sleep";
/// Prefix of the outcome notice of a meeting that sleep stopped.
pub const MEETING_STOPPED_BY_SLEEP_PREFIX: &str = "Stopped by sleep.";

/// Everything [`AppCore`] is built from.
pub struct AppDeps {
    pub dictation: DictationDeps,
    pub meeting: MeetingDeps,
    pub recorder: Arc<dyn Recorder>,
    pub secrets: Arc<dyn SecretStore>,
    pub login: Arc<dyn LoginItem>,
    pub settings_path: PathBuf,
    pub home: PathBuf,
}

#[derive(Default)]
struct Extras {
    key: Option<KeyState>,
    folder_error: Option<String>,
    login_error: Option<String>,
    key_error: Option<String>,
    /// A transient HUD notice (refusals, meeting outcomes) and when it began.
    notice: Option<(Hud, Instant)>,
    microphones: Vec<(String, String)>,
    shortcut_changed: Option<Chord>,
    quit: bool,
    meeting_was_processing: bool,
    /// Sleep stopped the meeting now processing: its outcome notice says so.
    meeting_stopped_by_sleep: bool,
}

/// The running app: owns the settings, both controllers and the shortcut
/// state machine, and implements [`Control`] for the UI. All policy for the
/// control surface lives here, so it is testable without a display.
pub struct AppCore {
    settings_path: PathBuf,
    home: PathBuf,
    settings: Mutex<Settings>,
    dictation: Dictation,
    meetings: Meetings,
    staging: Staging,
    recorder: Arc<dyn Recorder>,
    secrets: Arc<dyn SecretStore>,
    login: Arc<dyn LoginItem>,
    tracker: Mutex<PressTracker>,
    extras: Arc<Mutex<Extras>>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl AppCore {
    /// Load settings (a corrupt file is an error, never a silent reset) and
    /// build the controllers. Reads nothing that could prompt.
    pub fn new(deps: AppDeps) -> std::io::Result<AppCore> {
        let mut settings = Settings::load(&deps.settings_path)?;
        settings.open_at_login = deps.login.enabled();
        let notify = deps.dictation.notify.clone();
        let staging = deps.dictation.staging.clone();
        Ok(AppCore {
            settings_path: deps.settings_path,
            home: deps.home,
            settings: Mutex::new(settings),
            dictation: Dictation::new(deps.dictation),
            meetings: Meetings::new(deps.meeting),
            staging,
            recorder: deps.recorder,
            secrets: deps.secrets,
            login: deps.login,
            tracker: Mutex::new(PressTracker::new()),
            extras: Arc::new(Mutex::new(Extras::default())),
            notify,
        })
    }

    pub fn dictation(&self) -> &Dictation {
        &self.dictation
    }

    pub fn meetings(&self) -> &Meetings {
        &self.meetings
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Feed one shortcut event (from the platform hotkey backend).
    pub fn on_key(&self, ev: KeyEvent) {
        // Escape cancels the active take in any phase (recording or
        // transcribing) and discards it.
        if ev == KeyEvent::Escape {
            if self.dictation.busy() {
                self.tracker.lock().unwrap().sync_active(false);
                self.dictation.cancel();
            }
            return;
        }
        let action = {
            let mut t = self.tracker.lock().unwrap();
            // Keep the machine in step with takes started or ended elsewhere
            // (popover, failures); Escape still cancels while transcribing.
            let recording = self.dictation.recording();
            if !self.dictation.busy() || recording != t.active() {
                t.sync_active(recording);
            }
            t.on_event(ev)
        };
        match action {
            ShortcutAction::Start => self.start_dictation(),
            ShortcutAction::Stop => self.dictation.stop(),
            ShortcutAction::Cancel => self.dictation.cancel(),
            ShortcutAction::None => {}
        }
    }

    /// The system is going to sleep (logind `PrepareForSleep`, macOS
    /// `NSWorkspaceWillSleepNotification`): an active meeting stops cleanly —
    /// its tracks are finalized and staged for processing — with a message
    /// (DESIGN.md › Meeting pipeline: "sleep anyway … stops the meeting with
    /// a message"). Called from the platform's sleep watcher thread; returns
    /// once the audio is finalized, so the watcher can then let sleep go on.
    pub fn on_system_sleep(&self) {
        if !self.meetings.active() {
            return;
        }
        self.meetings.stop();
        self.extras().meeting_stopped_by_sleep = true;
        self.notice(Hud::Notice(MEETING_STOPPED_SLEEP.into()));
    }

    /// Whether Escape should be grabbed now (a take is active).
    pub fn wants_escape(&self) -> bool {
        self.dictation.busy()
    }

    /// A new shortcut the platform must register (taken once).
    pub fn take_shortcut_change(&self) -> Option<Chord> {
        self.extras().shortcut_changed.take()
    }

    /// The previous shortcut could not be registered: restore it.
    pub fn revert_shortcut(&self, previous: Chord, why: &str) {
        self.update_settings(|s| s.shortcut = previous);
        self.notice(Hud::Notice(format!("Shortcut unavailable: {why}")));
    }

    pub fn quit_requested(&self) -> bool {
        self.extras().quit
    }

    /// Read the key state (may touch the secret store) and device list.
    /// Blocking: call off the UI thread (see [`AppCore::refresh_async`]).
    pub fn refresh(&self) {
        let key = read_key_state(self.secrets.as_ref());
        let mics = self.recorder.devices();
        {
            let mut x = self.extras();
            x.key = Some(key);
            x.microphones = mics;
        }
        (self.notify)();
    }

    /// Run a secret-store change on a worker thread (a keyring may block on
    /// an unlock prompt), then publish the new key state.
    fn key_change_async(
        &self,
        change: impl FnOnce(&dyn SecretStore) -> Result<(), String> + Send + 'static,
    ) {
        let secrets = self.secrets.clone();
        let extras = self.extras.clone();
        let notify = self.notify.clone();
        std::thread::spawn(move || {
            let result = change(secrets.as_ref());
            let key = read_key_state(secrets.as_ref());
            {
                let mut x = extras.lock().unwrap();
                x.key_error = result.err();
                x.key = Some(key);
            }
            notify();
        });
    }

    fn take_config(&self) -> TakeConfig {
        let s = self.settings.lock().unwrap();
        TakeConfig {
            mode: s.mode,
            post_processing: s.post_processing,
            vocabulary: s.vocabulary.clone(),
            microphone: s.microphone.clone(),
        }
    }

    fn start_dictation(&self) {
        self.extras().notice = None;
        self.dictation.start(self.take_config());
    }

    fn meeting_folder(&self) -> PathBuf {
        self.settings
            .lock()
            .unwrap()
            .effective_meeting_folder(&self.home)
    }

    fn extras(&self) -> MutexGuard<'_, Extras> {
        self.extras.lock().unwrap()
    }

    fn notice(&self, hud: Hud) {
        self.extras().notice = Some((hud, Instant::now()));
        (self.notify)();
    }

    fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let snapshot = {
            let mut s = self.settings.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        if let Err(e) = snapshot.save(&self.settings_path) {
            self.notice(Hud::Notice(format!("Settings not saved ({})", e.kind())));
        }
        (self.notify)();
    }

    /// Newest staged item across dictations and meetings.
    fn recovery_row(&self) -> Option<RecoveryRow> {
        let dictation = self
            .staging
            .recoverable_dictations()
            .into_iter()
            .next()
            .map(|d| {
                let kind = match d.state {
                    crate::staging::DictationState::NotDelivered => {
                        RecoveryKind::DictationNotDelivered
                    }
                    _ => RecoveryKind::DictationFailed,
                };
                (d.started_unix, d.id, kind)
            });
        let meeting = self
            .meetings
            .recoverable()
            .into_iter()
            .next()
            .map(|m| (m.started_unix, m.id, m.kind));
        let newest = match (dictation, meeting) {
            (Some(d), Some(m)) => Some(if m.0 > d.0 { m } else { d }),
            (d, m) => d.or(m),
        };
        newest.map(|(t, id, kind)| RecoveryRow::for_item(&id, &kind, &local_hhmm(t)))
    }

    /// A meeting finished processing since the last snapshot: say how.
    fn meeting_outcome_notice(&self) {
        let processing = self.meetings.processing();
        let finished = {
            let mut x = self.extras();
            let was = std::mem::replace(&mut x.meeting_was_processing, processing);
            was && !processing
        };
        if !finished {
            return;
        }
        use crate::meeting::process::Finish;
        let text = match self.meetings.last_finish() {
            Some(Finish::Archived(_)) => "Meeting notes saved".to_string(),
            Some(Finish::PartialSaved(_)) => "Partial notes saved".to_string(),
            Some(Finish::PartsFailed(n)) => {
                format!(
                    "Meeting: {n} part{} failed. Retry from the menu",
                    if n == 1 { "" } else { "s" }
                )
            }
            Some(Finish::PendingArchive(_)) | Some(Finish::PartialBlocked(_)) => {
                "Meeting not saved: folder not writable".to_string()
            }
            Some(Finish::Error(_)) => "Meeting needs Retry from the menu".to_string(),
            None => return,
        };
        // Keep the reason the meeting ended in view after the outcome lands.
        let text = if std::mem::take(&mut self.extras().meeting_stopped_by_sleep) {
            format!("{MEETING_STOPPED_BY_SLEEP_PREFIX} {text}")
        } else {
            text
        };
        self.notice(Hud::Notice(text));
    }

    fn hud(&self) -> Hud {
        let dictation = self.dictation.hud();
        if dictation != Hud::Hidden {
            return dictation;
        }
        if let Some(elapsed) = self.meetings.elapsed() {
            return Hud::Meeting {
                elapsed_secs: elapsed.as_secs(),
            };
        }
        let mut x = self.extras();
        if let Some((hud, since)) = &x.notice {
            let dwell = Duration::from_millis(hud.dwell_ms().unwrap_or(2000));
            if since.elapsed() < dwell {
                return hud.clone();
            }
            x.notice = None;
        }
        drop(x);
        if self.meetings.processing() {
            return Hud::Transcribing;
        }
        Hud::Hidden
    }
}

impl Control for AppCore {
    fn snapshot(&self) -> UiSnapshot {
        self.meeting_outcome_notice();
        let hud = self.hud();
        let s = self.settings();
        let dictating = self.dictation.busy();
        let meeting_active = self.meetings.active();
        let status = if self.dictation.recording() {
            "Listening"
        } else if dictating {
            "Transcribing"
        } else if meeting_active {
            "Meeting"
        } else if self.meetings.processing() {
            "Processing"
        } else {
            "Idle"
        };
        let x = self.extras();
        UiSnapshot {
            dictating,
            meeting_active,
            mode: s.mode,
            post_processing: s.post_processing,
            has_last_dictation: self.dictation.last_text().is_some(),
            recovery: None,
            status: status.into(),
            hud,
            level: self.dictation.level(),
            shortcut: s.shortcut.clone(),
            microphone: s.microphone.clone(),
            microphones: x.microphones.clone(),
            key: x.key.clone().unwrap_or(KeyState::NotSet),
            vocabulary: s.vocabulary.clone(),
            meeting_folder: s.effective_meeting_folder(&self.home),
            folder_error: x.folder_error.clone(),
            open_at_login: s.open_at_login,
            login_error: x.login_error.clone(),
            key_error: x.key_error.clone(),
        }
        .with_recovery(self.recovery_row_after(x))
    }

    fn send(&self, cmd: Command) {
        match cmd {
            Command::ToggleDictation => {
                if self.dictation.busy() {
                    if self.dictation.recording() {
                        self.dictation.stop();
                    }
                } else {
                    self.start_dictation();
                }
                self.tracker
                    .lock()
                    .unwrap()
                    .sync_active(self.dictation.recording());
            }
            Command::ToggleMeeting => {
                if self.meetings.active() {
                    self.meetings.stop();
                } else {
                    // A new meeting never inherits an earlier sleep stop.
                    self.extras().meeting_stopped_by_sleep = false;
                    let s = self.settings();
                    let folder = s.effective_meeting_folder(&self.home);
                    if let Err(msg) =
                        self.meetings
                            .start(s.microphone.as_deref(), s.vocabulary, folder)
                    {
                        self.notice(Hud::Notice(msg));
                    }
                }
                (self.notify)();
            }
            Command::SetMode(m) => self.update_settings(|s| s.mode = m),
            Command::SetPostProcessing(on) => self.update_settings(|s| s.post_processing = on),
            Command::CopyLastDictation => {
                if !self.dictation.copy_last() && self.dictation.last_text().is_some() {
                    self.notice(Hud::NotDelivered);
                }
            }
            Command::OpenSettings => {}
            Command::Quit => {
                if self.meetings.active() {
                    self.meetings.stop();
                }
                if self.dictation.recording() {
                    self.dictation.cancel();
                }
                self.extras().quit = true;
            }
            Command::Recovery { item_id, action } => self.recover(&item_id, action),
            Command::SetShortcut(chord) => {
                if chord.is_valid() {
                    self.update_settings(|s| s.shortcut = chord.clone());
                    self.extras().shortcut_changed = Some(chord);
                }
            }
            Command::SetMicrophone(m) => self.update_settings(|s| s.microphone = m),
            Command::SaveKey(key) => {
                let key = key.trim().to_string();
                if key.is_empty() {
                    self.extras().key_error = Some("Enter a key".into());
                    (self.notify)();
                } else {
                    self.key_change_async(move |s| {
                        s.set_key(&key).map_err(|SecretError::Unavailable(m)| {
                            format!("The system keyring refused the key ({m})")
                        })
                    });
                }
            }
            Command::RemoveKey => self.key_change_async(|s| {
                s.delete_key().map_err(|SecretError::Unavailable(m)| {
                    format!("The system keyring could not remove the key ({m})")
                })
            }),
            Command::AddTerm(t) => self.update_settings(|s| {
                vocabulary::add_term(&mut s.vocabulary, &t);
            }),
            Command::RemoveTerm(t) => self.update_settings(|s| {
                vocabulary::remove_term(&mut s.vocabulary, &t);
            }),
            Command::ChooseMeetingFolder(folder) => {
                match meeting::check_folder_choice(&folder, &self.staging) {
                    Ok(()) => {
                        self.extras().folder_error = None;
                        self.update_settings(|s| s.meeting_folder = Some(folder));
                    }
                    Err(msg) => {
                        self.extras().folder_error = Some(msg);
                        (self.notify)();
                    }
                }
            }
            Command::SetOpenAtLogin(on) => {
                let result = self.login.set_enabled(on);
                let now = self.login.enabled();
                self.extras().login_error = result.err();
                self.update_settings(|s| s.open_at_login = now);
            }
        }
    }
}

impl AppCore {
    fn recovery_row_after(&self, extras: MutexGuard<'_, Extras>) -> Option<RecoveryRow> {
        drop(extras);
        self.recovery_row()
    }

    fn recover(&self, id: &str, action: RecoveryAction) {
        let is_meeting = self.meetings.recoverable().iter().any(|m| m.id == id);
        match (is_meeting, action) {
            (false, RecoveryAction::Retry) => self.dictation.retry(id, self.take_config()),
            (false, RecoveryAction::Discard) => self.dictation.discard(id),
            (false, RecoveryAction::SavePartialNotes) => {}
            (true, RecoveryAction::Retry) => self.meetings.retry(id, self.meeting_folder()),
            (true, RecoveryAction::SavePartialNotes) => {
                self.meetings.save_partial(id, self.meeting_folder())
            }
            (true, RecoveryAction::Discard) => self.meetings.discard(id),
        }
        (self.notify)();
    }
}

impl UiSnapshot {
    fn with_recovery(mut self, row: Option<RecoveryRow>) -> UiSnapshot {
        self.recovery = row;
        self
    }
}

fn read_key_state(secrets: &dyn SecretStore) -> KeyState {
    match secrets.get_key() {
        Ok(Some(k)) if !k.trim().is_empty() => KeyState::Saved,
        Ok(_) => KeyState::NotSet,
        Err(SecretError::Unavailable(m)) => {
            KeyState::Error(format!("The system keyring could not be read ({m})"))
        }
    }
}

/// Local `HH:MM` for a Unix time (the recovery row's label).
pub fn local_hhmm(unix: u64) -> String {
    let t = unix as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        let s = unix % 86_400;
        format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
    }
}

/// Default settings file: `$XDG_CONFIG_HOME/cantrip/v2/settings.json` (Linux),
/// `~/Library/Application Support/Cantrip/v2/settings.json` (macOS).
pub fn default_settings_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cantrip/v2/settings.json")
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
            .join("cantrip/v2/settings.json")
    }
}
