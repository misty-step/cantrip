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
