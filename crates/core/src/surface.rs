//! The closed control surface and the HUD's exact vocabulary. The UI renders
//! *only* what this module lists, so the 7 + 1 + 6 contract is enforced in one
//! place and checked by tests.

use crate::speech::Mode;

/// The popover's 7 standing controls, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandingControl {
    Dictation,
    Meeting,
    Mode,
    PostProcessing,
    CopyLastDictation,
    Settings,
    Quit,
}

pub const STANDING_CONTROLS: [StandingControl; 7] = [
    StandingControl::Dictation,
    StandingControl::Meeting,
    StandingControl::Mode,
    StandingControl::PostProcessing,
    StandingControl::CopyLastDictation,
    StandingControl::Settings,
    StandingControl::Quit,
];

impl StandingControl {
    /// Accessible name / visible label given the current state.
    pub fn label(self, dictating: bool, meeting: bool) -> &'static str {
        match self {
            StandingControl::Dictation if dictating => "Stop dictation",
            StandingControl::Dictation => "Start dictation",
            StandingControl::Meeting if meeting => "Stop meeting",
            StandingControl::Meeting => "Start meeting",
            StandingControl::Mode => "Mode",
            StandingControl::PostProcessing => "Post-Processing",
            StandingControl::CopyLastDictation => "Copy last dictation",
            StandingControl::Settings => "Settings…",
            StandingControl::Quit => "Quit",
        }
    }
}

pub const MODES: [Mode; 3] = [Mode::Raw, Mode::Clean, Mode::Elevated];

/// The Settings pane's 6 controls, one pane, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsControl {
    Shortcut,
    Microphone,
    OpenRouterKey,
    Vocabulary,
    MeetingFolder,
    OpenAtLogin,
}

pub const SETTINGS_CONTROLS: [SettingsControl; 6] = [
    SettingsControl::Shortcut,
    SettingsControl::Microphone,
    SettingsControl::OpenRouterKey,
    SettingsControl::Vocabulary,
    SettingsControl::MeetingFolder,
    SettingsControl::OpenAtLogin,
];

impl SettingsControl {
    pub fn label(self) -> &'static str {
        match self {
            SettingsControl::Shortcut => "Shortcut",
            SettingsControl::Microphone => "Microphone",
            SettingsControl::OpenRouterKey => "OpenRouter key",
            SettingsControl::Vocabulary => "Vocabulary",
            SettingsControl::MeetingFolder => "Meeting folder",
            SettingsControl::OpenAtLogin => "Open at login",
        }
    }
}

/// Actions the contextual recovery row may offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    Retry,
    SavePartialNotes,
    Discard,
}

impl RecoveryAction {
    pub fn label(self) -> &'static str {
        match self {
            RecoveryAction::Retry => "Retry",
            RecoveryAction::SavePartialNotes => "Save partial notes",
            RecoveryAction::Discard => "Discard…",
        }
    }
}

/// What kind of staged item heads the recovery queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryKind {
    DictationFailed,
    DictationNotDelivered,
    MeetingInterrupted,
    MeetingPartsFailed { failed_parts: usize },
    MeetingPendingArchive,
}

/// The single contextual row: shown only while a staged item exists, for the
/// newest one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryRow {
    /// Stable id of the staged item the actions apply to.
    pub item_id: String,
    /// e.g. `⚠ Dictation 10:42 failed`.
    pub text: String,
    pub actions: Vec<RecoveryAction>,
    /// The confirmation question asked before Discard.
    pub discard_warning: String,
}

impl RecoveryRow {
    /// Build the row for one item. `hhmm` is the local start time.
    pub fn for_item(item_id: &str, kind: &RecoveryKind, hhmm: &str) -> RecoveryRow {
        use RecoveryAction::*;
        let (text, actions, warn) = match kind {
            RecoveryKind::DictationFailed => (
                format!("⚠ Dictation {hhmm} failed"),
                vec![Retry, Discard],
                "Discard this dictation's recording? It cannot be recovered.".to_string(),
            ),
            RecoveryKind::DictationNotDelivered => (
                format!("⚠ Dictation {hhmm} not delivered"),
                vec![Retry, Discard],
                "Discard this dictation's recording? It cannot be recovered.".to_string(),
            ),
            RecoveryKind::MeetingInterrupted => (
                format!("⚠ Meeting {hhmm} interrupted"),
                vec![Retry, Discard],
                "Discard this meeting's recording? Nothing in the Meeting folder changes.".to_string(),
            ),
            RecoveryKind::MeetingPartsFailed { failed_parts } => (
                format!(
                    "⚠ Meeting {hhmm}: {failed_parts} part{} failed",
                    if *failed_parts == 1 { "" } else { "s" }
                ),
                vec![Retry, SavePartialNotes, Discard],
                "Discard this meeting's audio and unsaved notes? Nothing in the Meeting folder changes."
                    .to_string(),
            ),
            RecoveryKind::MeetingPendingArchive => (
                format!("⚠ Meeting {hhmm} not saved: folder not writable"),
                vec![Retry, Discard],
                "Discard this meeting? Its notes and audio go; nothing in the Meeting folder changes."
                    .to_string(),
            ),
        };
        RecoveryRow {
            item_id: item_id.to_string(),
            text,
            actions,
            discard_warning: warn,
        }
    }
}

// ───────────────────────────── HUD ─────────────────────────────

/// Clipboard claim shown under `Posted — unconfirmed` and `Copied — …`.
pub const CLAIM_ON_CLIPBOARD: &str = "Text is on the clipboard";
pub const CLAIM_USE_COPY_LAST: &str = "Use Copy last dictation";

/// Why a dictation was copied instead of delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyReason {
    FocusChanged,
    TargetUncertain,
    PasteUnsupported,
    /// Missing permission, e.g. "Accessibility".
    Allow(String),
}

impl CopyReason {
    pub fn text(&self) -> String {
        match self {
            CopyReason::FocusChanged => "Copied — focus changed".into(),
            CopyReason::TargetUncertain => "Copied — target uncertain".into(),
            CopyReason::PasteUnsupported => "Copied — paste unsupported on this session".into(),
            CopyReason::Allow(p) => format!("Copied — allow {p}"),
        }
    }
}

/// Visual tone of a HUD state. Colour means state only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Red dot: recording.
    Recording,
    /// Neutral progress.
    Working,
    /// Green check: confirmed.
    Confirmed,
    /// Neutral information.
    Neutral,
    /// Amber: needs attention.
    Attention,
}

/// Every state the floating HUD pill can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hud {
    Hidden,
    Listening,
    Transcribing,
    Inserted,
    SentUnconfirmed,
    /// Tier-2 paste posted; carries the clipboard claim.
    PostedUnconfirmed {
        on_clipboard: bool,
    },
    Copied {
        reason: CopyReason,
        on_clipboard: bool,
    },
    RawCleanupSkipped,
    OnDevice,
    /// `Not transcribed. …` with the fix.
    NotTranscribed(String),
    NotDelivered,
    MicrophoneBlocked,
    /// A one-line refusal such as "Meetings need an OpenRouter key".
    Notice(String),
    /// Meeting in progress: red dot + tabular `MM:SS`.
    Meeting {
        elapsed_secs: u64,
    },
}

impl Hud {
    /// Main text exactly as DESIGN.md writes it.
    pub fn text(&self) -> String {
        match self {
            Hud::Hidden => String::new(),
            Hud::Listening => "Listening".into(),
            Hud::Transcribing => "Transcribing".into(),
            Hud::Inserted => "✓ Inserted".into(),
            Hud::SentUnconfirmed => "Sent — unconfirmed".into(),
            Hud::PostedUnconfirmed { .. } => "Posted — unconfirmed".into(),
            Hud::Copied { reason, .. } => reason.text(),
            Hud::RawCleanupSkipped => "Raw (cleanup skipped)".into(),
            Hud::OnDevice => "On-device".into(),
            Hud::NotTranscribed(fix) => format!("Not transcribed. {fix}"),
            Hud::NotDelivered => "Not delivered. Use Copy last dictation".into(),
            Hud::MicrophoneBlocked => "Microphone blocked. Open Settings".into(),
            Hud::Notice(s) => s.clone(),
            Hud::Meeting { elapsed_secs } => format!("{} Meeting", mm_ss(*elapsed_secs)),
        }
    }

    /// Secondary line (the clipboard claim), if any.
    pub fn claim(&self) -> Option<&'static str> {
        match self {
            Hud::PostedUnconfirmed { on_clipboard } | Hud::Copied { on_clipboard, .. } => {
                Some(if *on_clipboard {
                    CLAIM_ON_CLIPBOARD
                } else {
                    CLAIM_USE_COPY_LAST
                })
            }
            _ => None,
        }
    }

    /// Drop the clipboard claim (never re-raised once dropped).
    pub fn drop_claim(&mut self) {
        match self {
            Hud::PostedUnconfirmed { on_clipboard } | Hud::Copied { on_clipboard, .. } => {
                *on_clipboard = false
            }
            _ => {}
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            Hud::Listening | Hud::Meeting { .. } => Tone::Recording,
            Hud::Transcribing => Tone::Working,
            Hud::Inserted => Tone::Confirmed,
            Hud::SentUnconfirmed | Hud::PostedUnconfirmed { .. } | Hud::Hidden => Tone::Neutral,
            _ => Tone::Attention,
        }
    }

    /// How long a terminal state stays before fading; `None` = until changed.
    pub fn dwell_ms(&self) -> Option<u64> {
        match self {
            Hud::Inserted | Hud::SentUnconfirmed => Some(800),
            Hud::PostedUnconfirmed { .. } => Some(1500),
            Hud::Copied { .. } | Hud::RawCleanupSkipped | Hud::OnDevice | Hud::Notice(_) => {
                Some(2000)
            }
            Hud::NotTranscribed(_) | Hud::NotDelivered | Hud::MicrophoneBlocked => Some(6000),
            Hud::Hidden | Hud::Listening | Hud::Transcribing | Hud::Meeting { .. } => None,
        }
    }
}

/// `MM:SS` (minutes keep counting past 59).
pub fn mm_ss(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popover_has_exactly_seven_standing_controls_with_exact_names() {
        let names: Vec<_> = STANDING_CONTROLS
            .iter()
            .map(|c| c.label(false, false))
            .collect();
        assert_eq!(
            names,
            [
                "Start dictation",
                "Start meeting",
                "Mode",
                "Post-Processing",
                "Copy last dictation",
                "Settings…",
                "Quit"
            ]
        );
        assert_eq!(
            StandingControl::Dictation.label(true, false),
            "Stop dictation"
        );
        assert_eq!(StandingControl::Meeting.label(false, true), "Stop meeting");
    }

    #[test]
    fn settings_has_exactly_six_controls_in_one_pane() {
        let names: Vec<_> = SETTINGS_CONTROLS.iter().map(|c| c.label()).collect();
        assert_eq!(
            names,
            [
                "Shortcut",
                "Microphone",
                "OpenRouter key",
                "Vocabulary",
                "Meeting folder",
                "Open at login"
            ]
        );
    }

    #[test]
    fn save_partial_notes_appears_only_for_failed_parts() {
        let kinds = [
            RecoveryKind::DictationFailed,
            RecoveryKind::DictationNotDelivered,
            RecoveryKind::MeetingInterrupted,
            RecoveryKind::MeetingPendingArchive,
        ];
        for k in kinds {
            let row = RecoveryRow::for_item("x", &k, "10:42");
            assert_eq!(
                row.actions,
                vec![RecoveryAction::Retry, RecoveryAction::Discard]
            );
        }
        let row = RecoveryRow::for_item(
            "m",
            &RecoveryKind::MeetingPartsFailed { failed_parts: 2 },
            "14:00",
        );
        assert_eq!(row.text, "⚠ Meeting 14:00: 2 parts failed");
        assert!(row.actions.contains(&RecoveryAction::SavePartialNotes));
    }

    #[test]
    fn hud_texts_match_design() {
        assert_eq!(Hud::Inserted.text(), "✓ Inserted");
        assert_eq!(
            Hud::Copied {
                reason: CopyReason::PasteUnsupported,
                on_clipboard: true
            }
            .text(),
            "Copied — paste unsupported on this session"
        );
        assert_eq!(
            Hud::NotDelivered.text(),
            "Not delivered. Use Copy last dictation"
        );
        assert_eq!(Hud::Meeting { elapsed_secs: 724 }.text(), "12:04 Meeting");
        assert_eq!(Hud::RawCleanupSkipped.text(), "Raw (cleanup skipped)");
    }

    #[test]
    fn claim_drops_and_never_returns() {
        let mut h = Hud::Copied {
            reason: CopyReason::FocusChanged,
            on_clipboard: true,
        };
        assert_eq!(h.claim(), Some(CLAIM_ON_CLIPBOARD));
        h.drop_claim();
        assert_eq!(h.claim(), Some(CLAIM_USE_COPY_LAST));
        assert_eq!(Hud::Inserted.claim(), None);
    }
}
