//! The macOS Settings window: the shared [`SettingsForm`] and
//! [`TranscriptHistory`] in standard AppKit controls, inside the menu-bar app.
//!
//! A preference-style toolbar switches between Transcripts, General,
//! Transcription, Modes and HUD. Configuration edits stay in the window until
//! Save & Apply validates them, writes the file with its comments preserved
//! and asks the running Cantrip to apply it, exactly as on Linux. The cleanup
//! provider's API key is the exception: Modes saves it to the keychain at once,
//! under the ID the form names, and never shows a saved key. Live state and
//! microphone access come from the app's status reader, which is the capture
//! owner's own snapshot; Allow asks that owner. Effects run on worker threads
//! and the app's tick hands their results back, so the AppKit thread never
//! waits on IPC, history, the keychain or the clipboard.

use super::mark;
use super::ui::{self, Message};
use crate::inject::{self, DeliveryAvailability};
use crate::modes::ModeView;
use crate::settings::credential::Credential;
use crate::settings::model::{
    self, CopyFailure, CopyPhase, Microphones, PastTranscript, SettingsForm, TranscriptHistory,
    INJECTION_CHOICES, MOTION_CHOICES,
};
use anyhow::{Context, Result};
use block2::RcBlock;
use cantrip_engine::{
    config::Mode,
    ipc::{self, KeychainAuthorization},
    paths,
};
use clap::ValueEnum;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message as _,
};
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSBackingStoreType, NSBorderType, NSButton, NSButtonType,
    NSCellImagePosition, NSColor, NSControl, NSControlTextEditingDelegate, NSFont,
    NSFontWeightRegular, NSGridView, NSImageView, NSLayoutConstraintOrientation, NSMenu,
    NSMenuDelegate, NSModalResponse, NSModalResponseCancel, NSPopUpButton, NSScrollView,
    NSSecureTextField, NSStackView, NSStackViewGravity, NSTableColumn,
    NSTableColumnResizingOptions, NSTableView, NSTableViewDataSource, NSTableViewDelegate,
    NSTableViewStyle, NSTextField, NSTextFieldDelegate, NSTextView, NSToolbar, NSToolbarDelegate,
    NSToolbarItem, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSInteger, NSNotification, NSPoint, NSRect, NSSize, NSString,
};
use std::cell::{Cell, OnceCell, Ref, RefCell, RefMut};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const WIDTH: f64 = 600.0;
const INSET: f64 = 24.0;
/// The width inside a pane's margins.
const BODY: f64 = WIDTH - 2.0 * INSET;
const CONTROL: f64 = 340.0;
const NUMBER: f64 = 90.0;
/// A mode's reasoning effort, beside its model.
const EFFORT: f64 = 110.0;
/// A mode's model, sharing the control column with its effort.
const MODEL: f64 = CONTROL - EFFORT - 8.0;
/// From a radio button's edge to its title.
const RADIO_TITLE: f64 = 20.0;
/// A mode's explanation under its radio button's title.
const EXPLANATION: f64 = 440.0;
/// A mode's choice, from its radio button to its explanation's edge; the
/// cleanup provider's key below the modes shares it.
const CHOICE: f64 = RADIO_TITLE + EXPLANATION;
const ROW_HEIGHT: f64 = 44.0;
/// Complete transcript rows visible at the window's size.
const VISIBLE_ROWS: f64 = 5.0;

/// One pane of the Settings window, in toolbar order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Pane {
    #[default]
    Transcripts,
    General,
    Transcription,
    Modes,
    Hud,
}

impl Pane {
    const ALL: [Self; 5] = [
        Self::Transcripts,
        Self::General,
        Self::Transcription,
        Self::Modes,
        Self::Hud,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::General => "General",
            Self::Transcription => "Transcription",
            Self::Modes => "Modes",
            Self::Hud => "HUD",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Transcripts => "clock.arrow.circlepath",
            Self::General => "gearshape",
            Self::Transcription => "waveform",
            Self::Modes => "wand.and.stars",
            Self::Hud => "rectangle.bottomhalf.inset.filled",
        }
    }

    fn identifier(self) -> &'static str {
        match self {
            Self::Transcripts => "transcripts",
            Self::General => "general",
            Self::Transcription => "transcription",
            Self::Modes => "modes",
            Self::Hud => "hud",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|pane| *pane == self).unwrap_or(0)
    }
}

/// What the app's status reader last saw, handed to the window each tick.
pub(crate) struct Live<'a> {
    /// The menu's own first line, such as "Ready" or "Cantrip is not running".
    pub(crate) line: &'a str,
    pub(crate) online: bool,
    pub(crate) microphone: Option<&'a ipc::MicrophoneAccess>,
}

struct State {
    form: SettingsForm,
    history: TranscriptHistory,
    microphones: Microphones,
    /// The stored UID behind each microphone pop-up item.
    microphone_uids: Vec<String>,
    delivery: DeliveryAvailability,
    pane: Pane,
    online: bool,
    microphone: Option<ipc::MicrophoneAccess>,
    live: String,
    apply: Option<Receiver<Result<ipc::CommandReply>>>,
    allow: Option<Receiver<Result<ipc::CommandReply>>>,
    access_problem: Option<String>,
    load: Option<Receiver<Result<Vec<PastTranscript>>>>,
    copy: Option<Receiver<Result<(), CopyFailure>>>,
    /// The cleanup provider's API key in the keychain, under the form's ID.
    credential: Credential,
    /// The key's last answer, whose controls stay in place, disabled, while
    /// the next is awaited.
    settled: Option<KeychainAuthorization>,
    /// Replace was chosen: the masked field shows over the stored key.
    replacing: bool,
    /// A new key went to the keychain and its answer has not arrived.
    saving: bool,
    /// The Remove confirmation while it is shown.
    remove_alert: Option<Retained<NSAlert>>,
}

/// What the key area shows for the credential as it stands.
struct KeyView {
    name: String,
    status: &'static str,
    /// The keychain withholds the key, or a read failed.
    attention: bool,
    /// The masked field and Save; Cancel too while `replacing`.
    entry: bool,
    replacing: bool,
    /// A key may be stored: its approval or retry, Replace and Remove.
    stored: bool,
    action: Option<String>,
    /// Nothing is running, so the controls can act.
    enabled: bool,
    note: Option<&'static str>,
    problem: Option<&'static str>,
}

impl State {
    /// The key area for the credential as it stands. While an answer is
    /// awaited, the last answer's controls stay in place, disabled, so the
    /// pane keeps its size.
    fn key_view(&mut self) -> KeyView {
        let credential = &self.credential;
        let id = credential.id();
        let access = credential.access();
        let enabled = !credential.busy();
        if enabled {
            self.settled = Some(access);
        }
        let shown = if enabled { Some(access) } else { self.settled };
        let named = !id.is_empty();
        let entry = named && (self.replacing || shown == Some(KeychainAuthorization::NotSet));
        let stored = named && !entry && shown.is_some();
        let problem = credential.problem();
        KeyView {
            name: key_name(id),
            status: if named {
                credential.label()
            } else {
                "Not used"
            },
            attention: access != KeychainAuthorization::NotSet && access.action().is_some(),
            entry,
            replacing: entry && self.replacing,
            stored,
            action: shown.filter(|_| stored).and_then(action_title),
            enabled,
            note: if entry || !named || problem.is_some() {
                None
            } else {
                access.note()
            },
            problem,
        }
    }
}

/// The repair sheet: the file's original text, editable, over the window.
struct RepairSheet {
    window: Retained<NSWindow>,
    text: Retained<NSTextView>,
    message: Message,
}

/// The cleanup provider's API key in the Modes pane, outside Details.
struct KeyArea {
    view: Retained<NSStackView>,
    name: Retained<NSTextField>,
    attention: Option<Retained<NSImageView>>,
    status: Retained<NSTextField>,
    /// The masked field that takes only a new key, Save and Cancel.
    entry: Retained<NSStackView>,
    secret: Retained<NSSecureTextField>,
    save: Retained<NSButton>,
    cancel: Retained<NSButton>,
    /// Approval or a retry, Replace and Remove, for a key that may be stored.
    actions: Retained<NSStackView>,
    action: Retained<NSButton>,
    replace: Retained<NSButton>,
    remove: Retained<NSButton>,
    note: Message,
    problem: Message,
}

struct Views {
    window: Retained<NSWindow>,
    toolbar: Retained<NSToolbar>,
    root: Retained<NSStackView>,
    panes: Vec<Retained<NSView>>,
    separator: Retained<NSView>,
    footer: Retained<NSView>,
    // General
    microphone: Retained<NSPopUpButton>,
    microphone_problem: Message,
    access: Retained<NSTextField>,
    /// Allow or Open Privacy Settings, on their own row so a long app name fits.
    access_actions: Retained<NSStackView>,
    allow: Retained<NSButton>,
    privacy: Retained<NSButton>,
    access_note: Message,
    access_problem: Message,
    shortcut: Retained<NSTextField>,
    shortcut_hint: Message,
    delivery: Retained<NSPopUpButton>,
    delivery_note: Message,
    vocabulary: Retained<NSTextField>,
    keep_warm: Retained<NSButton>,
    // Transcription
    stt_model: Retained<NSTextField>,
    stt_endpoint: Retained<NSTextField>,
    stt_key: Retained<NSTextField>,
    // Modes
    /// One radio button per mode, in `Mode::ALL` order.
    modes: Vec<Retained<NSButton>>,
    /// Each mode's promise, what it does to the text, and what leaves this
    /// Mac and its model, in `Mode::ALL` order.
    promises: Vec<Retained<NSTextField>>,
    effects: Vec<Retained<NSTextField>>,
    summaries: Vec<Retained<NSTextField>>,
    /// The cleanup provider's API key, between the modes and Details.
    key: KeyArea,
    /// The disclosure that shows `details`: the raw ids and their settings.
    details_toggle: Retained<NSButton>,
    details: Retained<NSGridView>,
    clean_model: Retained<NSTextField>,
    clean_effort: Retained<NSTextField>,
    elevate_model: Retained<NSTextField>,
    elevate_effort: Retained<NSTextField>,
    pp_endpoint: Retained<NSTextField>,
    pp_key: Retained<NSTextField>,
    pp_timeout: Retained<NSTextField>,
    pp_passes: Retained<NSTextField>,
    pp_min_chars: Retained<NSTextField>,
    pp_instructions: Retained<NSTextView>,
    // HUD
    labels: Retained<NSButton>,
    motion: Retained<NSPopUpButton>,
    // Transcripts
    summary: Retained<NSTextField>,
    refresh: Retained<NSButton>,
    table: Retained<NSTableView>,
    history_message: Message,
    copy_message: Message,
    // Footer
    notice: Message,
    live_dot: Option<Retained<NSImageView>>,
    live: Retained<NSTextField>,
    repair: Retained<NSButton>,
    reload: Retained<NSButton>,
    save: Retained<NSButton>,
    repair_sheet: RepairSheet,
}

pub(crate) struct Ivars {
    state: RefCell<State>,
    views: OnceCell<Views>,
    closed: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `SettingsWindow` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripSettingsWindow"]
    #[ivars = Ivars]
    pub(crate) struct SettingsWindow;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for SettingsWindow {}

    // SAFETY: the signature matches NSWindowDelegate.
    unsafe impl NSWindowDelegate for SettingsWindow {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.ivars().closed.set(true);
            // A key typed but not saved goes with the window.
            self.drop_new_key();
        }
    }

    // SAFETY: the signatures match NSToolbarDelegate.
    unsafe impl NSToolbarDelegate for SettingsWindow {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            self.toolbar_item_for(identifier)
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }

        #[unsafe(method_id(toolbarSelectableItemIdentifiers:))]
        fn selectable_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            pane_identifiers()
        }
    }

    // SAFETY: the signatures match NSControlTextEditingDelegate.
    unsafe impl NSControlTextEditingDelegate for SettingsWindow {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            // A new key never joins the form: its field only enables Save.
            if notification.object().is_some_and(|field| self.is_new_key(&field)) {
                self.show_key();
                return;
            }
            let _ = self.collect();
            self.show_shortcut_hint();
            self.show_form_state();
        }

        /// Return in the masked field saves the key, not the form; Escape there
        /// abandons a replacement.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn text_command(&self, control: &NSControl, _editor: &NSTextView, command: Sel) -> bool {
            if !self.is_new_key(control) {
                false
            } else if command == sel!(insertNewline:) {
                self.store_key();
                true
            } else if command == sel!(cancelOperation:) && self.state().replacing {
                self.end_replacing();
                true
            } else {
                false
            }
        }
    }

    // SAFETY: NSTextFieldDelegate's methods are all optional.
    unsafe impl NSTextFieldDelegate for SettingsWindow {}

    // SAFETY: the signature matches NSTableViewDataSource.
    unsafe impl NSTableViewDataSource for SettingsWindow {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.state().history.rows().len() as NSInteger
        }
    }

    // SAFETY: the signature matches NSTableViewDelegate.
    unsafe impl NSTableViewDelegate for SettingsWindow {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn table_row(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            usize::try_from(row).ok().and_then(|row| self.transcript_row(row))
        }
    }

    // SAFETY: the signature matches NSMenuDelegate.
    unsafe impl NSMenuDelegate for SettingsWindow {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, _menu: &NSMenu) {
            // The microphone list is read again each time it opens.
            self.refresh_microphones();
        }
    }

    // SAFETY: each action takes its sender and returns nothing.
    impl SettingsWindow {
        #[unsafe(method(selectPane:))]
        fn select_pane(&self, item: &NSToolbarItem) {
            let identifier = item.itemIdentifier().to_string();
            if let Some(pane) = Pane::ALL
                .into_iter()
                .find(|pane| pane.identifier() == identifier)
            {
                self.show_pane(pane, true);
            }
        }

        #[unsafe(method(save:))]
        fn save(&self, _sender: Option<&AnyObject>) {
            self.save_form();
        }

        #[unsafe(method(reloadFromDisk:))]
        fn reload_from_disk(&self, _sender: Option<&AnyObject>) {
            self.state_mut().form.reload_from_disk();
            self.populate();
        }

        #[unsafe(method(repairConfiguration:))]
        fn repair_configuration(&self, _sender: Option<&AnyObject>) {
            self.begin_repair();
        }

        #[unsafe(method(saveRepair:))]
        fn save_repair(&self, _sender: Option<&AnyObject>) {
            self.finish_repair();
        }

        #[unsafe(method(cancelRepair:))]
        fn cancel_repair(&self, _sender: Option<&AnyObject>) {
            self.state_mut().form.cancel_repair();
            self.end_repair_sheet();
            self.show_form_state();
        }

        #[unsafe(method(allowMicrophone:))]
        fn allow_microphone(&self, _sender: Option<&AnyObject>) {
            {
                let mut state = self.state_mut();
                state.allow = Some(send(ipc::Command::AllowMicrophone));
                state.access_problem = None;
            }
            self.show_access();
        }

        #[unsafe(method(openPrivacySettings:))]
        fn open_privacy_settings(&self, _sender: Option<&AnyObject>) {
            let problem = crate::macos::open_microphone_settings()
                .err()
                .map(|error| format!("{error:#}"));
            self.state_mut().access_problem = problem;
            self.show_access();
        }

        #[unsafe(method(refreshTranscripts:))]
        fn refresh_transcripts(&self, _sender: Option<&AnyObject>) {
            self.load_history();
        }

        #[unsafe(method(copyTranscript:))]
        fn copy_transcript(&self, sender: &NSView) {
            let row = self.views().table.rowForView(sender);
            if let Ok(row) = usize::try_from(row) {
                self.start_copy(row);
            }
        }

        /// Copy (⌘C) with the transcript list in front copies its selected row.
        #[unsafe(method(copy:))]
        fn copy_selected(&self, _sender: Option<&AnyObject>) {
            if self.state().pane != Pane::Transcripts {
                return;
            }
            if let Ok(row) = usize::try_from(self.views().table.selectedRow()) {
                self.start_copy(row);
            }
        }

        #[unsafe(method(formChanged:))]
        fn form_changed(&self, _sender: Option<&AnyObject>) {
            let _ = self.collect();
            self.show_general();
            self.show_form_state();
        }

        #[unsafe(method(selectMode:))]
        fn select_mode(&self, sender: &NSButton) {
            let chosen = usize::try_from(sender.tag())
                .ok()
                .and_then(|index| Mode::ALL.get(index).copied());
            if let Some(mode) = chosen {
                let _ = self.collect();
                self.state_mut().form.edit.mode = mode;
            }
            self.show_modes();
            self.show_form_state();
        }

        /// The Details disclosure.
        #[unsafe(method(toggleDetails:))]
        fn toggle_details(&self, _sender: &NSButton) {
            let views = self.views();
            ui::show_in_form(&views.details, ui::checked(&views.details_toggle));
            self.fit(true);
        }

        #[unsafe(method(saveKey:))]
        fn save_key(&self, _sender: Option<&AnyObject>) {
            self.store_key();
        }

        #[unsafe(method(replaceKey:))]
        fn replace_key(&self, _sender: Option<&AnyObject>) {
            self.begin_replacing();
        }

        #[unsafe(method(cancelKey:))]
        fn cancel_key(&self, _sender: Option<&AnyObject>) {
            self.end_replacing();
        }

        #[unsafe(method(removeKey:))]
        fn remove_key(&self, _sender: Option<&AnyObject>) {
            self.ask_remove_key();
        }

        /// Approve Keychain access, or Retry keyring access.
        #[unsafe(method(keyAction:))]
        fn key_action(&self, _sender: Option<&AnyObject>) {
            self.run_key_action();
        }
    }
);

/// The pane toolbar's identifiers, in order.
fn pane_identifiers() -> Retained<NSArray<NSString>> {
    let identifiers: Vec<Retained<NSString>> = Pane::ALL
        .into_iter()
        .map(|pane| ui::ns(pane.identifier()))
        .collect();
    NSArray::from_retained_slice(&identifiers)
}

/// Send one command to the running Cantrip off the AppKit thread.
fn send(command: ipc::Command) -> Receiver<Result<ipc::CommandReply>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(ipc::command(command));
    });
    receiver
}

impl SettingsWindow {
    /// A Settings window on `pane`, built but not yet shown.
    pub(crate) fn new(mtm: MainThreadMarker, pane: Pane) -> Result<Retained<Self>> {
        let path = paths::config_file().context("locating the configuration file")?;
        let state = State {
            form: SettingsForm::load(path),
            history: TranscriptHistory::default(),
            microphones: Microphones::list(),
            microphone_uids: Vec::new(),
            delivery: inject::delivery_availability(),
            pane,
            online: false,
            microphone: None,
            live: String::new(),
            apply: None,
            allow: None,
            access_problem: None,
            load: None,
            copy: None,
            credential: Credential::default(),
            settled: None,
            replacing: false,
            saving: false,
            remove_alert: None,
        };
        let this = Self::alloc(mtm).set_ivars(Ivars {
            state: RefCell::new(state),
            views: OnceCell::new(),
            closed: Cell::new(false),
        });
        // SAFETY: `init` is NSObject's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let views = this.build(mtm);
        let _ = this.ivars().views.set(views);
        this.populate();
        this.show_pane(pane, false);
        this.views().window.center();
        Ok(this)
    }

    fn state(&self) -> Ref<'_, State> {
        self.ivars().state.borrow()
    }

    fn state_mut(&self) -> RefMut<'_, State> {
        self.ivars().state.borrow_mut()
    }

    fn views(&self) -> &Views {
        self.ivars()
            .views
            .get()
            .expect("the Settings window builds its views when created")
    }

    /// Bring the window forward, loading the transcript list when it opens.
    pub(crate) fn show(&self) {
        if self.state().load.is_none() && !self.state().history.loading() {
            self.load_history();
        }
        ui::present(&self.views().window);
    }

    /// Turn the window to `pane`, as an action that names it asks, such as
    /// Open Settings beside capture guidance turning to the microphone.
    pub(crate) fn open_pane(&self, pane: Pane) {
        self.show_pane(pane, true);
    }

    /// The window was closed; the app drops it at its next tick.
    pub(crate) fn closed(&self) -> bool {
        self.ivars().closed.get()
    }

    /// Close the window, as quitting Cantrip does. An open repair is
    /// abandoned, leaving the file as it is, and an unanswered Remove keeps
    /// the key.
    pub(crate) fn close(&self) {
        if self.state().form.repairing() {
            self.state_mut().form.cancel_repair();
            self.end_repair_sheet();
        }
        self.end_remove_alert();
        self.views().window.close();
    }

    /// Detach every AppKit callback before the window is released.
    pub(crate) fn teardown(&self) {
        // An unanswered Remove keeps the key and lets go of this window.
        self.end_remove_alert();
        let views = self.views();
        views.window.setDelegate(None);
        views.toolbar.setDelegate(None);
        if let Some(menu) = views.microphone.menu() {
            menu.setDelegate(None);
        }
        // SAFETY: clearing a delegate or data source has no preconditions.
        unsafe {
            for field in [
                &views.shortcut,
                &views.stt_model,
                &views.stt_endpoint,
                &views.clean_model,
                &views.elevate_model,
                &views.pp_endpoint,
                &views.pp_key,
            ] {
                field.setDelegate(None);
            }
            views.key.secret.setDelegate(None);
            views.table.setDataSource(None);
            views.table.setDelegate(None);
        }
    }

    /// Show a default mode the menu just saved, keeping unsaved edits.
    pub(crate) fn follow_saved_mode(&self) {
        self.state_mut().form.follow_saved_mode();
        self.show_modes();
        self.show_form_state();
    }

    /// Fold in live status and any finished background work.
    pub(crate) fn tick(&self, live: &Live) {
        let mut access = false;
        let mut form = false;
        let mut history = false;
        let mut key = false;
        let line = {
            let mut state = self.state_mut();
            if state.online != live.online || state.microphone.as_ref() != live.microphone {
                state.online = live.online;
                state.microphone = live.microphone.cloned();
                access = true;
            }
            if let Some(reply) = model::completed(state.apply.as_ref()) {
                state.apply = None;
                state.form.applied(reply);
                form = true;
            }
            if let Some(reply) = model::completed(state.allow.as_ref()) {
                state.allow = None;
                state.access_problem = model::allow_problem(reply);
                access = true;
            }
            if let Some(rows) = model::completed(state.load.as_ref()) {
                state.load = None;
                state.history.loaded(rows);
                history = true;
            }
            let copied = match state.copy.as_ref().map(Receiver::try_recv) {
                Some(Ok(result)) => Some(result),
                Some(Err(TryRecvError::Disconnected)) => Some(Err(TranscriptHistory::copy_lost())),
                _ => None,
            };
            if let Some(result) = copied {
                state.copy = None;
                state.history.copied(result);
                history = true;
            }
            if state.credential.poll() {
                // A key the keychain refused leaves its field open for another.
                if std::mem::take(&mut state.saving) && state.credential.problem().is_some() {
                    state.replacing = true;
                }
                key = true;
            }
            (state.live != live.line).then(|| {
                state.live = live.line.to_owned();
                (live.line.to_owned(), live.online)
            })
        };
        if let Some((line, online)) = line {
            self.show_live(&line, online);
        }
        if access {
            self.show_access();
        }
        if form {
            self.show_form_state();
        }
        if history {
            self.show_history();
        }
        if key {
            self.show_key();
        }
    }

    fn load_history(&self) {
        let mut state = self.state_mut();
        if !state.history.start_load() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(model::load_transcripts());
        });
        state.load = Some(receiver);
        drop(state);
        self.show_history();
    }

    fn start_copy(&self, row: usize) {
        let mut state = self.state_mut();
        let Some(id) = state.history.start_copy(row) else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(model::copy_transcript(&id));
        });
        state.copy = Some(receiver);
        drop(state);
        self.show_history();
    }

    fn save_form(&self) {
        let read = self.collect();
        let mut state = self.state_mut();
        if let Err(problem) = read {
            state.form.refuse(&problem);
        } else if let Some(_apply) = state.form.save() {
            state.apply = Some(send(ipc::Command::Reload));
        }
        drop(state);
        // Saving may adopt a default mode chosen from the menu or `cantrip mode`.
        self.show_modes();
        self.show_form_state();
    }

    fn begin_repair(&self) {
        let original = {
            let mut state = self.state_mut();
            state.form.start_repair();
            state.form.repair_text().map(|text| text.clone())
        };
        let Some(original) = original else {
            self.show_form_state();
            return;
        };
        let views = self.views();
        let sheet = &views.repair_sheet;
        sheet.text.setString(&ui::ns(&original));
        sheet.message.show(None, false);
        views
            .window
            .beginSheet_completionHandler(&sheet.window, None);
        sheet.window.makeFirstResponder(Some(&sheet.text));
    }

    fn finish_repair(&self) {
        let views = self.views();
        let edited = views.repair_sheet.text.string().to_string();
        let saved = {
            let mut state = self.state_mut();
            if let Some(text) = state.form.repair_text() {
                *text = edited;
            }
            match state.form.save_repair() {
                Some(_apply) => {
                    state.apply = Some(send(ipc::Command::Reload));
                    Ok(())
                }
                None => Err(state.form.notice.as_ref().map(|notice| notice.text.clone())),
            }
        };
        match saved {
            Ok(()) => {
                self.end_repair_sheet();
                self.populate();
            }
            Err(problem) => views.repair_sheet.message.show(problem.as_deref(), true),
        }
    }

    fn end_repair_sheet(&self) {
        let views = self.views();
        if views.window.attachedSheet().is_some() {
            views.window.endSheet(&views.repair_sheet.window);
        }
        views.repair_sheet.window.orderOut(None);
    }

    /// Re-read the microphones, keeping the selected one.
    fn refresh_microphones(&self) {
        let _ = self.collect();
        self.state_mut().microphones = Microphones::list();
        self.fill_microphones();
        self.show_general();
    }

    /// Rebuild the microphone pop-up from the listed devices. A configured
    /// device that is not listed stays selected under its own disabled item.
    fn fill_microphones(&self) {
        let (items, selected) = {
            let mut state = self.state_mut();
            let selected_uid = state.form.edit.audio_source.trim().to_owned();
            let mut choices = state.microphones.choices();
            if !choices.iter().any(|(uid, _)| *uid == selected_uid) {
                choices.push((selected_uid.clone(), state.microphones.name(&selected_uid)));
            }
            let listed = state.microphones.choices().len();
            let selected = choices
                .iter()
                .position(|(uid, _)| *uid == selected_uid)
                .unwrap_or(0);
            let items: Vec<(String, bool)> = choices
                .iter()
                .enumerate()
                .map(|(index, (_, label))| (label.clone(), index < listed))
                .collect();
            state.microphone_uids = choices.into_iter().map(|(uid, _)| uid).collect();
            (items, selected)
        };
        ui::fill_popup(&self.views().microphone, &items, selected);
    }

    /// Show the form's values in every control, after loading or reloading.
    fn populate(&self) {
        self.fill_microphones();
        let views = self.views();
        {
            let state = self.state();
            let edit = &state.form.edit;
            let modes: Vec<(&str, bool)> = INJECTION_CHOICES
                .iter()
                .map(|(mode, label)| (*label, model::offered(*mode, state.delivery)))
                .collect();
            let mode = INJECTION_CHOICES
                .iter()
                .position(|(mode, _)| *mode == edit.injection)
                .unwrap_or(0);
            ui::fill_popup(&views.delivery, &modes, mode);
            ui::set_text(&views.shortcut, &edit.hotkey);
            ui::set_text(&views.vocabulary, &edit.vocabulary);
            ui::set_checked(&views.keep_warm, edit.keep_warm);
            ui::set_text(&views.stt_model, &edit.stt_model);
            ui::set_text(&views.stt_endpoint, &edit.stt_endpoint);
            ui::set_text(&views.stt_key, &edit.stt_key);
            ui::set_text(&views.clean_model, &edit.clean_model);
            ui::set_text(&views.clean_effort, &edit.clean_effort);
            ui::set_text(&views.elevate_model, &edit.elevate_model);
            ui::set_text(&views.elevate_effort, &edit.elevate_effort);
            ui::set_text(&views.pp_endpoint, &edit.pp_endpoint);
            ui::set_text(&views.pp_key, &edit.pp_key);
            ui::set_text(&views.pp_timeout, &edit.pp_timeout.to_string());
            ui::set_text(&views.pp_passes, &edit.pp_passes.to_string());
            ui::set_text(&views.pp_min_chars, &edit.pp_min_chars.to_string());
            views
                .pp_instructions
                .setString(&ui::ns(&edit.pp_instructions));
            ui::set_checked(&views.labels, edit.hud.labels);
            let motion = MOTION_CHOICES
                .iter()
                .position(|(choice, _)| *choice == edit.hud.reduced_motion)
                .unwrap_or(0);
            let motions: Vec<(&str, bool)> = MOTION_CHOICES
                .iter()
                .map(|(_, label)| (*label, true))
                .collect();
            ui::fill_popup(&views.motion, &motions, motion);
        }
        self.follow_key_id();
        self.show_modes();
        self.show_general();
        self.show_key();
        self.show_form_state();
    }

    /// Read every control into the form. Text that is not a number keeps the
    /// form's previous value and is reported, so Save never guesses one.
    fn collect(&self) -> Result<(), String> {
        // Read AppKit first: no control is asked while the state is borrowed.
        let views = self.views();
        let microphone = ui::selected(&views.microphone);
        let mode = ui::selected(&views.delivery);
        let motion = ui::selected(&views.motion);
        let timeout = whole(&views.pp_timeout, "Timeout");
        let passes = whole(&views.pp_passes, "Clean passes");
        let min_chars = whole(&views.pp_min_chars, "Minimum length");
        let hotkey = ui::text_of(&views.shortcut);
        let vocabulary = ui::text_of(&views.vocabulary);
        let keep_warm = ui::checked(&views.keep_warm);
        let stt_model = ui::text_of(&views.stt_model);
        let stt_endpoint = ui::text_of(&views.stt_endpoint);
        let stt_key = ui::text_of(&views.stt_key);
        let clean_model = ui::text_of(&views.clean_model);
        let clean_effort = ui::text_of(&views.clean_effort);
        let elevate_model = ui::text_of(&views.elevate_model);
        let elevate_effort = ui::text_of(&views.elevate_effort);
        let pp_endpoint = ui::text_of(&views.pp_endpoint);
        let pp_key = ui::text_of(&views.pp_key);
        let instructions = views.pp_instructions.string().to_string();
        let labels = ui::checked(&views.labels);

        let mut guard = self.state_mut();
        let state = &mut *guard;
        let edit = &mut state.form.edit;
        if let Some(uid) = microphone.and_then(|index| state.microphone_uids.get(index)) {
            edit.audio_source = uid.clone();
        }
        if let Some((mode, _)) = mode.and_then(|index| INJECTION_CHOICES.get(index)) {
            edit.injection = *mode;
        }
        edit.hotkey = hotkey;
        edit.vocabulary = vocabulary;
        edit.keep_warm = keep_warm;
        edit.stt_model = stt_model;
        edit.stt_endpoint = stt_endpoint;
        edit.stt_key = stt_key;
        edit.clean_model = clean_model;
        edit.clean_effort = clean_effort;
        edit.elevate_model = elevate_model;
        edit.elevate_effort = elevate_effort;
        edit.pp_endpoint = pp_endpoint;
        edit.pp_key = pp_key;
        edit.pp_instructions = instructions;
        edit.hud.labels = labels;
        if let Some((choice, _)) = motion.and_then(|index| MOTION_CHOICES.get(index)) {
            edit.hud.reduced_motion = *choice;
        }
        let mut problems = Vec::new();
        match timeout {
            Ok(value) => edit.pp_timeout = value,
            Err(problem) => problems.push(problem),
        }
        match passes {
            Ok(value) => edit.pp_passes = value,
            Err(problem) => problems.push(problem),
        }
        match min_chars {
            Ok(value) => edit.pp_min_chars = value,
            Err(problem) => problems.push(problem),
        }
        drop(guard);
        // The key area names and checks the key the ID now points to.
        self.follow_key_id();
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join(" "))
        }
    }

    fn show_pane(&self, pane: Pane, animate: bool) {
        if pane != Pane::Modes {
            // A key typed in Modes is not carried to another pane.
            self.drop_new_key();
        } else if self.state().pane != Pane::Modes {
            // The key may have changed elsewhere since Modes last showed it.
            self.state_mut().credential.refresh();
            self.show_key();
        }
        self.state_mut().pane = pane;
        let views = self.views();
        let current = views.root.arrangedSubviews().firstObject();
        let next = &views.panes[pane.index()];
        if current.as_deref() != Some(&**next) {
            if let Some(current) = current {
                views.root.removeArrangedSubview(&current);
                current.removeFromSuperview();
            }
            views.root.insertArrangedSubview_atIndex(next, 0);
            // Tab follows the controls of the pane now shown.
            views.window.recalculateKeyViewLoop();
        }
        let settings = pane != Pane::Transcripts;
        views.separator.setHidden(!settings);
        views.footer.setHidden(!settings);
        views.window.setTitle(&ui::ns(pane.title()));
        views
            .toolbar
            .setSelectedItemIdentifier(Some(&ui::ns(pane.identifier())));
        self.fit(animate);
        let first: Option<&NSView> = match pane {
            Pane::Transcripts => Some(&views.table),
            Pane::General => Some(&views.microphone),
            Pane::Transcription => Some(&views.stt_model),
            Pane::Modes => Some(&views.modes[0]),
            Pane::Hud => Some(&views.labels),
        };
        views.window.makeFirstResponder(first.map(|view| &**view));
    }

    /// Size the pane, preserving its top edge unless that would hide the footer.
    fn fit(&self, animate: bool) {
        let views = self.views();
        views.root.layoutSubtreeIfNeeded();
        let size = views.root.fittingSize();
        let window = &views.window;
        let current = window.contentRectForFrameRect(window.frame());
        let top = current.origin.y + current.size.height;
        let content = NSRect::new(
            NSPoint::new(current.origin.x, top - size.height),
            NSSize::new(WIDTH, size.height),
        );
        let mut frame = window.frameRectForContentRect(content);
        if let Some(screen) = window.screen() {
            // AppKit constrains the title bar, not the footer above the Dock.
            let visible = screen.visibleFrame();
            frame.origin.y = frame
                .origin
                .y
                .max(visible.origin.y)
                .min(visible.origin.y + visible.size.height - frame.size.height);
        }
        window.setFrame_display_animate(frame, true, animate);
    }

    /// Microphone, access, shortcut and delivery feedback from the form.
    fn show_general(&self) {
        let views = self.views();
        {
            let state = self.state();
            let uid = state.form.edit.audio_source.trim();
            views
                .microphone_problem
                .show(state.microphones.problem(uid).as_deref(), true);
            let note = model::delivery_note(state.form.edit.injection, state.delivery);
            views.delivery_note.show(Some(&note.text), !note.ok);
        }
        self.show_access();
        self.show_shortcut_hint();
    }

    fn show_shortcut_hint(&self) {
        let views = self.views();
        match model::shortcut_hint(&ui::text_of(&views.shortcut)) {
            Ok(label) => views.shortcut_hint.show(Some(&label), false),
            Err(problem) => views.shortcut_hint.show(Some(&problem), true),
        }
    }

    /// The default mode's radio button; the window keeps the group exclusive.
    fn show_modes(&self) {
        let views = self.views();
        let mode = self.state().form.edit.mode;
        for (radio, choice) in views.modes.iter().zip(Mode::ALL) {
            ui::set_checked(radio, choice == mode);
        }
    }

    fn show_access(&self) {
        let views = self.views();
        let state = self.state();
        let view = model::access_view(
            state.online,
            state.microphone.as_ref(),
            state.allow.is_some(),
        );
        ui::set_text(&views.access, &view.text);
        if let Some(title) = &view.allow {
            views.allow.setTitle(&ui::ns(title));
        }
        views.allow.setHidden(view.allow.is_none());
        views.privacy.setHidden(!view.privacy);
        ui::show_in_form(&views.access_actions, view.allow.is_some() || view.privacy);
        views.access_note.show(view.note.as_deref(), true);
        views
            .access_problem
            .show(state.access_problem.as_deref(), true);
    }

    /// Point the key area at the form's API key ID. Another ID starts a check
    /// of its own key and drops anything typed for the last one.
    fn follow_key_id(&self) {
        let changed = {
            let mut state = self.state_mut();
            let state = &mut *state;
            let changed = state.credential.id() != state.form.edit.pp_key.trim();
            if changed {
                state.credential.select(&state.form.edit.pp_key);
                state.replacing = false;
                state.saving = false;
            }
            changed
        };
        if changed {
            self.views().key.secret.setStringValue(&NSString::new());
            self.show_key();
        }
    }

    /// The cleanup provider's key: its name and state, and the controls that
    /// can act on it now. The masked field empties whenever it hides, so a
    /// new key never waits out of sight.
    fn show_key(&self) {
        let view = self.state_mut().key_view();
        let key = &self.views().key;
        let height = key.view.fittingSize().height;
        ui::set_text(&key.name, &view.name);
        ui::set_text(&key.status, view.status);
        if let Some(icon) = &key.attention {
            icon.setHidden(!view.attention);
        }
        if !view.entry {
            key.secret.setStringValue(&NSString::new());
        }
        let typed = key.secret.stringValue().length() > 0;
        key.entry.setHidden(!view.entry);
        key.secret.setEnabled(view.enabled);
        key.save.setEnabled(view.enabled && typed);
        key.cancel.setHidden(!view.replacing);
        key.actions.setHidden(!view.stored);
        if let Some(title) = &view.action {
            key.action.setTitle(&ui::ns(title));
        }
        key.action.setHidden(view.action.is_none());
        for button in [&key.cancel, &key.action, &key.replace, &key.remove] {
            button.setEnabled(view.enabled);
        }
        key.secret.setAccessibilityLabel(Some(&ui::ns(&view.name)));
        for (button, verb) in [
            (&key.save, "Save"),
            (&key.replace, "Replace"),
            (&key.remove, "Remove"),
        ] {
            button.setAccessibilityLabel(Some(&ui::ns(&format!("{verb} {}", view.name))));
        }
        key.note.show(view.note, false);
        key.problem.show(view.problem, true);
        // Tab reaches whichever of the key's controls show now.
        self.views().window.recalculateKeyViewLoop();
        // Only a new height resizes the window, so an answer that changes
        // nothing never cuts a pane's animation short.
        key.view.layoutSubtreeIfNeeded();
        let resized = (key.view.fittingSize().height - height).abs() > 0.5;
        if resized && self.state().pane == Pane::Modes {
            self.fit(false);
        }
    }

    /// Whether `object` is the masked field for a new key.
    fn is_new_key(&self, object: &AnyObject) -> bool {
        let field: &AnyObject = &self.views().key.secret;
        std::ptr::eq(object, field)
    }

    /// Save beside the masked field, or Return in it: the key goes to the
    /// keychain worker and the field empties at once. Whitespace around a
    /// pasted key, which a key never has, is dropped.
    fn store_key(&self) {
        let view = self.state_mut().key_view();
        let views = self.views();
        let field = &views.key.secret;
        let typed = field.stringValue().length() > 0;
        if !(view.entry && view.enabled && typed) {
            return;
        }
        let mut secret = ui::text_of(field);
        trim_in_place(&mut secret);
        // Focus leaves the field before it empties.
        views.window.makeFirstResponder(None);
        field.setStringValue(&NSString::new());
        if !secret.is_empty() {
            let mut state = self.state_mut();
            state.credential.save(secret);
            state.replacing = false;
            state.saving = true;
        }
        self.show_key();
    }

    /// Replace: the masked field opens for a new key; the stored one stays
    /// until that is saved.
    fn begin_replacing(&self) {
        let view = self.state_mut().key_view();
        if !(view.stored && view.enabled) {
            return;
        }
        self.state_mut().replacing = true;
        self.show_key();
        let views = self.views();
        views.window.makeFirstResponder(Some(&views.key.secret));
    }

    /// Cancel a replacement, or Escape in its field: the typed key is dropped
    /// and the stored key's controls return.
    fn end_replacing(&self) {
        let views = self.views();
        views.window.makeFirstResponder(None);
        self.drop_new_key();
        self.show_key();
        // With keyboard navigation on, focus goes back to Replace.
        views.window.makeFirstResponder(Some(&views.key.replace));
    }

    /// Forget a key typed but not saved, and any replacement under way.
    fn drop_new_key(&self) {
        self.views().key.secret.setStringValue(&NSString::new());
        self.state_mut().replacing = false;
    }

    /// Approve Keychain access to a key it withholds, or read a keyring that
    /// failed once more.
    fn run_key_action(&self) {
        let view = self.state_mut().key_view();
        if !(view.action.is_some() && view.enabled) {
            return;
        }
        {
            let mut state = self.state_mut();
            if needs_approval(state.credential.access()) {
                state.credential.approve();
            } else {
                state.credential.refresh();
            }
        }
        self.show_key();
    }

    /// Ask, in a standard alert sheet, before removing the stored key. Return
    /// never removes; Escape keeps the key.
    fn ask_remove_key(&self) {
        let view = self.state_mut().key_view();
        let id = {
            let state = self.state();
            if !(view.stored && view.enabled) || state.remove_alert.is_some() {
                return;
            }
            state.credential.id().to_owned()
        };
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&ui::ns(&format!("Remove the {}?", view.name)));
        alert.setInformativeText(&ui::ns(
            "This removes the key from your Keychain. Takes will skip cleanup without it.",
        ));
        let remove = alert.addButtonWithTitle(&ui::ns("Remove Key"));
        remove.setHasDestructiveAction(true);
        remove.setKeyEquivalent(&ui::ns(""));
        let keep = alert.addButtonWithTitle(&ui::ns("Cancel"));
        let this = self.retain();
        let answered = RcBlock::new(move |response: NSModalResponse| {
            this.remove_answered(&id, response == NSAlertFirstButtonReturn);
        });
        self.state_mut().remove_alert = Some(alert.clone());
        alert.beginSheetModalForWindow_completionHandler(&self.views().window, Some(&answered));
        alert.window().makeFirstResponder(Some(&keep));
    }

    /// Remove only when confirmed, and only the key the question named.
    fn remove_answered(&self, id: &str, confirmed: bool) {
        {
            let mut state = self.state_mut();
            state.remove_alert = None;
            if confirmed && state.credential.id() == id {
                state.credential.remove();
            }
        }
        self.show_key();
    }

    /// Withdraw the Remove confirmation, keeping the key.
    fn end_remove_alert(&self) {
        let alert = self.state().remove_alert.clone();
        if let Some(alert) = alert {
            self.views()
                .window
                .endSheet_returnCode(&alert.window(), NSModalResponseCancel);
        }
    }

    /// The save notice, which footer actions are available, and what each mode
    /// would do with the values in the form.
    fn show_form_state(&self) {
        let views = self.views();
        let state = self.state();
        let form = &state.form;
        for (index, mode) in Mode::ALL.into_iter().enumerate() {
            let pipeline = form.edit.pipeline(mode);
            let view = ModeView::new(&pipeline);
            ui::set_text(&views.promises[index], view.headline());
            ui::set_text(&views.effects[index], &view.effect());
            ui::set_text(&views.summaries[index], &view.summary());
        }
        views.notice.show(
            form.notice.as_ref().map(|notice| notice.text.as_str()),
            form.notice.as_ref().is_some_and(|notice| !notice.ok),
        );
        views.save.setEnabled(form.can_save());
        views.reload.setEnabled(!form.applying());
        views.repair.setHidden(!form.needs_repair());
        drop(state);
        if self.state().pane != Pane::Transcripts {
            self.fit(false);
        }
    }

    fn show_history(&self) {
        let views = self.views();
        {
            let state = self.state();
            let history = &state.history;
            ui::set_text(&views.summary, history.summary());
            views.refresh.setEnabled(!history.loading());
            match history.error() {
                Some(error) => views.history_message.show(Some(error), true),
                None => views.history_message.show(history.empty_hint(), false),
            }
            let status = history.copy_status();
            views.copy_message.show(
                status.map(|status| status.message.as_str()),
                status.is_some_and(|status| {
                    matches!(status.phase, CopyPhase::NotCopied | CopyPhase::Uncertain)
                }),
            );
        }
        views.table.reloadData();
        if self.state().pane == Pane::Transcripts {
            self.fit(false);
        }
    }

    fn show_live(&self, line: &str, online: bool) {
        let views = self.views();
        ui::set_text(&views.live, line);
        if let Some(dot) = &views.live_dot {
            let color = if online {
                NSColor::systemGreenColor()
            } else {
                NSColor::tertiaryLabelColor()
            };
            dot.setContentTintColor(Some(&color));
        }
    }

    fn toolbar_item_for(&self, identifier: &NSString) -> Option<Retained<NSToolbarItem>> {
        let identifier_text = identifier.to_string();
        let pane = Pane::ALL
            .into_iter()
            .find(|pane| pane.identifier() == identifier_text)?;
        let item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(self.mtm()), identifier);
        item.setLabel(&ui::ns(pane.title()));
        item.setPaletteLabel(&ui::ns(pane.title()));
        item.setImage(ui::symbol(pane.symbol(), pane.title()).as_deref());
        // SAFETY: `selectPane:` is defined above; the window outlives its toolbar items.
        unsafe {
            item.setTarget(Some(self));
            item.setAction(Some(sel!(selectPane:)));
        }
        Some(item)
    }

    /// One past transcript: when and how long, its opening words, the latest
    /// Copy result for it, and its own Copy button.
    fn transcript_row(&self, index: usize) -> Option<Retained<NSView>> {
        let mtm = self.mtm();
        let state = self.state();
        let history = &state.history;
        let row = history.rows().get(index)?;
        let meta = ui::truncating_label(mtm, &row.meta);
        meta.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        meta.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let preview = if row.partial {
            let preview = ui::truncating_label(mtm, &format!("Partial: {}", row.preview));
            preview.setToolTip(Some(&ui::ns(
                "Saved text may not cover the whole recording.",
            )));
            preview
        } else {
            ui::truncating_label(mtm, &row.preview)
        };
        let text = ui::column(mtm, &[&meta, &preview], 2.0);
        text.setContentCompressionResistancePriority_forOrientation(
            1.0,
            NSLayoutConstraintOrientation::Horizontal,
        );
        let copy = ui::small_button(mtm, "Copy", self, sel!(copyTranscript:));
        copy.setEnabled(history.can_copy());
        copy.setAccessibilityLabel(Some(&ui::ns(&format!("Copy transcript from {}", row.time))));
        let tag = history.phase(&row.id).map(|phase| {
            let tag = ui::label(
                mtm,
                match phase {
                    CopyPhase::Copying => "copying…",
                    CopyPhase::Copied => "copied",
                    CopyPhase::NotCopied => "not copied",
                    CopyPhase::Uncertain => "uncertain",
                },
            );
            tag.setFont(Some(&NSFont::systemFontOfSize(
                NSFont::smallSystemFontSize(),
            )));
            tag.setTextColor(Some(&NSColor::secondaryLabelColor()));
            tag
        });
        // Text on the leading edge, the result and Copy on the trailing edge.
        let line = NSStackView::new(mtm);
        line.setSpacing(8.0);
        line.addView_inGravity(&text, NSStackViewGravity::Leading);
        if let Some(tag) = &tag {
            line.addView_inGravity(tag, NSStackViewGravity::Trailing);
        }
        line.addView_inGravity(&copy, NSStackViewGravity::Trailing);
        line.setEdgeInsets(NSEdgeInsets {
            top: 0.0,
            left: 8.0,
            bottom: 0.0,
            right: 8.0,
        });
        Some(Retained::into_super(line))
    }

    fn build(&self, mtm: MainThreadMarker) -> Views {
        let target: &AnyObject = self;
        let changed = sel!(formChanged:);

        // General
        let microphone = ui::popup(mtm, target, changed);
        if let Some(menu) = microphone.menu() {
            menu.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        ui::fixed_width(&microphone, CONTROL);
        let microphone_problem = Message::new(mtm, CONTROL);
        let access_note = Message::new(mtm, CONTROL);
        // Wraps rather than widening the form when it names another app.
        let access = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
        access.setPreferredMaxLayoutWidth(CONTROL + 60.0);
        let allow = ui::button(
            mtm,
            "Allow Microphone Access…",
            target,
            sel!(allowMicrophone:),
        );
        let privacy = ui::button(
            mtm,
            "Open Privacy Settings…",
            target,
            sel!(openPrivacySettings:),
        );
        let access_actions = ui::row(mtm, &[&allow, &privacy], 8.0);
        let access_problem = Message::new(mtm, CONTROL);
        let shortcut = ui::field(mtm, crate::macos::shortcut::DEFAULT, CONTROL);
        // SAFETY: the window clears this delegate before it is released.
        unsafe { shortcut.setDelegate(Some(ProtocolObject::from_ref(self))) };
        let shortcut_hint = Message::new(mtm, CONTROL);
        let delivery = ui::popup(mtm, target, changed);
        let delivery_note = Message::new(mtm, CONTROL);
        let vocabulary = ui::field(mtm, "PipeWire, Parakeet", CONTROL);
        let vocabulary_note = ui::note(
            mtm,
            "Comma-separated terms. File-defined aliases are kept separately.",
            CONTROL,
        );
        let keep_warm = ui::checkbox(
            mtm,
            "Keep the speech model loaded for faster dictation",
            target,
            changed,
        );
        let path = {
            let state = self.state();
            format!("Saved in {}", home_relative(state.form.path()))
        };
        let general_form = ui::form(
            mtm,
            &[
                (Some("Microphone:"), &microphone),
                (None, &microphone_problem.view),
                (Some("Microphone access:"), &access),
                (None, &access_actions),
                (None, &access_note.view),
                (None, &access_problem.view),
                (Some("Dictation shortcut:"), &shortcut),
                (None, &shortcut_hint.view),
                (Some("Delivery:"), &delivery),
                (None, &delivery_note.view),
                (Some("Vocabulary:"), &vocabulary),
                (None, &vocabulary_note),
                (None, &keep_warm),
            ],
        );
        let path_note = ui::note(mtm, &path, BODY);
        let general = pane(mtm, &[&general_form, &path_note]);

        // Transcription
        let stt_model = ui::field(mtm, "", CONTROL);
        let stt_endpoint = ui::field(mtm, "https://api.openai.com/v1", CONTROL);
        // The Modes pane names the recognizer, so it follows these as they are typed.
        // SAFETY: teardown clears these delegates before the views are released.
        unsafe {
            stt_model.setDelegate(Some(ProtocolObject::from_ref(self)));
            stt_endpoint.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        let endpoint_note = ui::note(
            mtm,
            "Leave empty to transcribe on this Mac with the local model.",
            CONTROL,
        );
        let stt_key = ui::field(mtm, "openai", CONTROL);
        let key_note = ui::note(
            mtm,
            "The name of a key saved in your keychain, not the key itself.",
            CONTROL,
        );
        let transcription_form = ui::form(
            mtm,
            &[
                (Some("Model:"), &stt_model),
                (Some("Endpoint:"), &stt_endpoint),
                (None, &endpoint_note),
                (Some("API key ID:"), &stt_key),
                (None, &key_note),
            ],
        );
        let transcription = pane(mtm, &[&transcription_form]);

        // Modes: each mode's name, glyph and promise, what it does to the text,
        // and what leaves this Mac and its model; the cleanup provider's key;
        // the raw ids under Details.
        let modes: Vec<Retained<NSButton>> = Mode::ALL
            .iter()
            .enumerate()
            .map(|(index, mode)| {
                let radio = ui::radio(mtm, mode.label(), index, target, sel!(selectMode:));
                radio.setFont(Some(
                    &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
                ));
                radio
            })
            .collect();
        let promises: Vec<Retained<NSTextField>> =
            Mode::ALL.iter().map(|_| ui::label(mtm, "")).collect();
        let effects: Vec<Retained<NSTextField>> = Mode::ALL
            .iter()
            .map(|_| ui::small(mtm, "", EXPLANATION))
            .collect();
        let summaries: Vec<Retained<NSTextField>> = Mode::ALL
            .iter()
            .map(|_| ui::small(mtm, "", EXPLANATION))
            .collect();
        let mut choices: Vec<Retained<NSStackView>> = Vec::with_capacity(2 * modes.len());
        for (index, mode) in Mode::ALL.into_iter().enumerate() {
            let glyph = NSImageView::imageViewWithImage(&mark::mode_glyph(mode), mtm);
            glyph.setContentTintColor(Some(&NSColor::labelColor()));
            choices.push(ui::row(
                mtm,
                &[&modes[index], &glyph, &promises[index]],
                6.0,
            ));
            // What it does and what leaves sit under the radio button's title.
            let lines = ui::column(mtm, &[&effects[index], &summaries[index]], 2.0);
            lines.setEdgeInsets(NSEdgeInsets {
                top: 0.0,
                left: RADIO_TITLE,
                bottom: 0.0,
                right: 0.0,
            });
            choices.push(lines);
        }
        // The window keeps the radio buttons exclusive: each explanation close
        // under its title, the next choice set apart.
        let views: Vec<&NSView> = choices.iter().map(|view| &***view).collect();
        let mode_group = ui::column(mtm, &views, 3.0);
        for lines in choices.iter().skip(1).step_by(2).take(Mode::ALL.len() - 1) {
            mode_group.setCustomSpacing_afterView(8.0, lines);
        }
        // Outside Details, where a missing key is found at once.
        let key = key_area(mtm, self);
        // A borderless toggle in the label color: AppKit's disclosure triangle
        // and borderless titles draw below 4.5:1 in Light.
        let details_toggle = ui::button(mtm, "Details", target, sel!(toggleDetails:));
        details_toggle.setBordered(false);
        details_toggle.setButtonType(NSButtonType::Toggle);
        details_toggle.setImage(ui::symbol("chevron.right", "").as_deref());
        details_toggle.setAlternateImage(ui::symbol("chevron.down", "").as_deref());
        details_toggle.setImagePosition(NSCellImagePosition::ImageLeading);
        details_toggle.setContentTintColor(Some(&NSColor::labelColor()));
        let columns = ui::row(
            mtm,
            &[
                &ui::note(mtm, "Model", MODEL),
                &ui::note(mtm, "Reasoning effort", EFFORT),
            ],
            8.0,
        );
        let model_field = |placeholder: &str, width: f64, name: &str| {
            let field = ui::field(mtm, placeholder, width);
            field.setAccessibilityLabel(Some(&ui::ns(name)));
            field
        };
        let clean_model = model_field("none — spelling fixes only", MODEL, "Clean model");
        let clean_effort = model_field("not sent", EFFORT, "Clean reasoning effort");
        let elevate_model = model_field("none — spelling fixes only", MODEL, "Elevate model");
        let elevate_effort = model_field("not sent", EFFORT, "Elevate reasoning effort");
        // Each mode's explanation names its model as it is typed.
        // SAFETY: teardown clears these delegates before the views are released.
        unsafe {
            clean_model.setDelegate(Some(ProtocolObject::from_ref(self)));
            elevate_model.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        let clean_row = ui::row(mtm, &[&clean_model, &clean_effort], 8.0);
        let elevate_row = ui::row(mtm, &[&elevate_model, &elevate_effort], 8.0);
        let pp_endpoint = ui::field(mtm, "", CONTROL);
        // Where Clean and Elevate send the text follows the endpoint as it is typed.
        // SAFETY: teardown clears this delegate before the view is released.
        unsafe { pp_endpoint.setDelegate(Some(ProtocolObject::from_ref(self))) };
        let pp_key = ui::field(mtm, "none", CONTROL);
        // The key area above Details follows the ID as it is typed.
        // SAFETY: teardown clears this delegate before the view is released.
        unsafe { pp_key.setDelegate(Some(ProtocolObject::from_ref(self))) };
        pp_key.setToolTip(Some(&ui::ns(
            "Shared by Clean and Elevate; names a keychain item, not a key.",
        )));
        let pp_timeout = ui::field(mtm, "", NUMBER);
        let milliseconds = ui::label(mtm, "ms");
        let timeout_row = ui::row(mtm, &[&pp_timeout, &milliseconds], 6.0);
        let pp_passes = ui::field(mtm, "", NUMBER);
        pp_passes.setToolTip(Some(&ui::ns(
            "2 adds a proofread pass. Elevate always makes one pass.",
        )));
        let pp_min_chars = ui::field(mtm, "", NUMBER);
        let characters = ui::label(mtm, "characters");
        let min_chars_row = ui::row(mtm, &[&pp_min_chars, &characters], 6.0);
        pp_min_chars.setToolTip(Some(&ui::ns(
            "Shorter transcripts skip Clean's model; 0 never skips.",
        )));
        let (instructions_scroll, pp_instructions) = text_area(mtm, CONTROL, 50.0, false);
        let details = ui::form(
            mtm,
            &[
                (None, &columns),
                (Some("Clean:"), &clean_row),
                (Some("Elevate:"), &elevate_row),
                (Some("Endpoint:"), &pp_endpoint),
                (Some("API key ID:"), &pp_key),
                (Some("Timeout:"), &timeout_row),
                (Some("Clean passes:"), &pp_passes),
                (Some("Minimum length:"), &min_chars_row),
                (Some("Clean instructions:"), &instructions_scroll),
            ],
        );
        // A little denser than the form above it, so Save & Apply stays above
        // the Dock at 1440×900 with Details open.
        details.setRowSpacing(8.0);
        let modes_form = ui::form(
            mtm,
            &[
                (Some("Mode:"), &mode_group),
                (None, &key.view),
                (None, &details_toggle),
                (None, &details),
            ],
        );
        // The key sits apart from the choices above it.
        modes_form.rowAtIndex(1).setTopPadding(6.0);
        ui::show_in_form(&details, false);
        let modes_pane = pane(mtm, &[&modes_form]);

        // HUD
        let labels = ui::checkbox(mtm, "Always show state labels", target, changed);
        let motion = ui::popup(mtm, target, changed);
        let motion_note = ui::note(mtm, "Important exceptions are always labelled.", CONTROL);
        let hud_form = ui::form(
            mtm,
            &[
                (None, &labels),
                (Some("Motion:"), &motion),
                (None, &motion_note),
            ],
        );
        let hud = pane(mtm, &[&hud_form]);

        // Transcripts
        let summary = ui::label(mtm, "");
        let refresh = ui::button(mtm, "Refresh", target, sel!(refreshTranscripts:));
        let header = NSStackView::new(mtm);
        header.addView_inGravity(&summary, NSStackViewGravity::Leading);
        header.addView_inGravity(&refresh, NSStackViewGravity::Trailing);
        ui::fixed_width(&header, BODY);
        let explanation = ui::note(
            mtm,
            "Saved on this Mac, newest first. Copy replaces your clipboard.",
            BODY,
        );
        let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), NSRect::ZERO);
        let column =
            NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ui::ns("transcript"));
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setRowHeight(ROW_HEIGHT);
        table.setStyle(NSTableViewStyle::FullWidth);
        table.setUsesAlternatingRowBackgroundColors(true);
        // SAFETY: the window clears these before it is released.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        table.setAccessibilityLabel(Some(&ui::ns("Past transcripts")));
        let list = NSScrollView::new(mtm);
        list.setDocumentView(Some(&table));
        list.setHasVerticalScroller(true);
        list.setBorderType(NSBorderType::BezelBorder);
        ui::fixed_width(&list, BODY);
        ui::fixed_height(&list, ROW_HEIGHT * VISIBLE_ROWS + 2.0);
        let history_message = Message::new(mtm, BODY);
        let copy_message = Message::new(mtm, BODY);
        let transcripts = pane(
            mtm,
            &[
                &header,
                &explanation,
                &list,
                &history_message.view,
                &copy_message.view,
            ],
        );

        // Footer
        let notice = Message::new(mtm, BODY);
        let live_dot = ui::symbol("circle.fill", "Cantrip status").map(|image| {
            let dot = NSImageView::imageViewWithImage(&image, mtm);
            dot.setContentTintColor(Some(&NSColor::tertiaryLabelColor()));
            dot
        });
        let live = ui::label(mtm, "");
        live.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        live.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let repair = ui::button(
            mtm,
            "Repair Configuration…",
            target,
            sel!(repairConfiguration:),
        );
        let reload = ui::button(mtm, "Reload from Disk", target, sel!(reloadFromDisk:));
        let save = ui::button(mtm, "Save & Apply", target, sel!(save:));
        save.setKeyEquivalent(&ui::ns("\r"));
        let actions = NSStackView::new(mtm);
        if let Some(dot) = &live_dot {
            actions.addView_inGravity(dot, NSStackViewGravity::Leading);
        }
        actions.addView_inGravity(&live, NSStackViewGravity::Leading);
        for button in [&repair, &reload, &save] {
            actions.addView_inGravity(button, NSStackViewGravity::Trailing);
        }
        ui::fixed_width(&actions, BODY);
        let footer = ui::column(mtm, &[&notice.view, &actions], 10.0);
        footer.setEdgeInsets(NSEdgeInsets {
            top: 12.0,
            left: INSET,
            bottom: 16.0,
            right: INSET,
        });

        let separator = ui::separator(mtm);
        ui::fixed_width(&separator, WIDTH);
        let root = ui::column(mtm, &[&transcripts, &separator, &footer], 0.0);

        // SAFETY: created on the main thread; `Views` owns the only reference,
        // so AppKit must not also release it on close.
        let window = unsafe {
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, 400.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                true,
            );
            window.setReleasedWhenClosed(false);
            window
        };
        window.setContentView(Some(&root));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let toolbar =
            NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &ui::ns("CantripSettings"));
        toolbar.setDelegate(Some(ProtocolObject::from_ref(self)));
        toolbar.setAllowsUserCustomization(false);
        window.setToolbar(Some(&toolbar));
        window.setToolbarStyle(NSWindowToolbarStyle::Preference);

        Views {
            repair_sheet: repair_sheet(mtm, target),
            window,
            toolbar,
            root,
            panes: vec![
                Retained::into_super(transcripts),
                Retained::into_super(general),
                Retained::into_super(transcription),
                Retained::into_super(modes_pane),
                Retained::into_super(hud),
            ],
            separator: Retained::into_super(separator),
            footer: Retained::into_super(footer),
            microphone,
            microphone_problem,
            access,
            access_actions,
            allow,
            privacy,
            access_note,
            access_problem,
            shortcut,
            shortcut_hint,
            delivery,
            delivery_note,
            vocabulary,
            keep_warm,
            stt_model,
            stt_endpoint,
            stt_key,
            modes,
            promises,
            effects,
            summaries,
            key,
            details_toggle,
            details,
            clean_model,
            clean_effort,
            elevate_model,
            elevate_effort,
            pp_endpoint,
            pp_key,
            pp_timeout,
            pp_passes,
            pp_min_chars,
            pp_instructions,
            labels,
            motion,
            summary,
            refresh,
            table,
            history_message,
            copy_message,
            notice,
            live_dot,
            live,
            repair,
            reload,
            save,
        }
    }
}

/// A pane's content: its views stacked in the window's margins.
fn pane(mtm: MainThreadMarker, views: &[&NSView]) -> Retained<NSStackView> {
    let pane = ui::column(mtm, views, 14.0);
    pane.setEdgeInsets(NSEdgeInsets {
        top: 20.0,
        left: INSET,
        bottom: 20.0,
        right: INSET,
    });
    ui::fixed_width(&pane, WIDTH);
    pane
}

/// A plain-text editor `width` by `height` points that keeps exactly what is
/// typed: no smart quotes, dashes, replacements or spelling corrections.
fn text_area(
    mtm: MainThreadMarker,
    width: f64,
    height: f64,
    code: bool,
) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = NSTextView::scrollableTextView(mtm);
    let text = scroll
        .documentView()
        .and_then(|view| view.downcast::<NSTextView>().ok())
        .expect("a scrollable text view contains a text view");
    text.setRichText(false);
    text.setAllowsUndo(true);
    text.setAutomaticQuoteSubstitutionEnabled(false);
    text.setAutomaticDashSubstitutionEnabled(false);
    text.setAutomaticTextReplacementEnabled(false);
    text.setAutomaticSpellingCorrectionEnabled(false);
    let font = if code {
        // SAFETY: a framework constant, valid for the process lifetime.
        NSFont::monospacedSystemFontOfSize_weight(NSFont::systemFontSize(), unsafe {
            NSFontWeightRegular
        })
    } else {
        NSFont::systemFontOfSize(NSFont::systemFontSize())
    };
    text.setFont(Some(&font));
    scroll.setBorderType(NSBorderType::BezelBorder);
    ui::fixed_width(&scroll, width);
    ui::fixed_height(&scroll, height);
    (scroll, text)
}

/// The sheet that repairs a configuration file Cantrip could not parse.
fn repair_sheet(mtm: MainThreadMarker, target: &AnyObject) -> RepairSheet {
    let title = ui::label(mtm, "Repair Configuration");
    title.setFont(Some(
        &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
    ));
    let explanation = ui::note(
        mtm,
        "Repair the TOML below. The existing file is kept until the replacement parses and validates.",
        BODY,
    );
    let (scroll, text) = text_area(mtm, BODY, 300.0, true);
    let message = Message::new(mtm, BODY);
    let cancel = ui::button(mtm, "Cancel", target, sel!(cancelRepair:));
    cancel.setKeyEquivalent(&ui::ns("\u{1b}"));
    let save = ui::button(
        mtm,
        "Save Repaired Configuration",
        target,
        sel!(saveRepair:),
    );
    let buttons = NSStackView::new(mtm);
    buttons.addView_inGravity(&cancel, NSStackViewGravity::Trailing);
    buttons.addView_inGravity(&save, NSStackViewGravity::Trailing);
    ui::fixed_width(&buttons, BODY);
    let content = ui::column(
        mtm,
        &[&title, &explanation, &scroll, &message.view, &buttons],
        12.0,
    );
    content.setEdgeInsets(NSEdgeInsets {
        top: 20.0,
        left: INSET,
        bottom: 20.0,
        right: INSET,
    });
    // SAFETY: created on the main thread; `RepairSheet` owns the only reference,
    // so AppKit must not also release it on close.
    let window = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, 440.0)),
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            true,
        );
        window.setReleasedWhenClosed(false);
        window
    };
    window.setContentView(Some(&content));
    RepairSheet {
        window,
        text,
        message,
    }
}

/// The key area: its name and state, a masked field that takes only a new
/// key, and what a stored key allows. `show_key` fills it in.
fn key_area(mtm: MainThreadMarker, owner: &SettingsWindow) -> KeyArea {
    let target: &AnyObject = owner;
    // Bold like the modes' names, so a missing key is found at a glance.
    let name = ui::truncating_label(mtm, "");
    name.setFont(Some(
        &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
    ));
    name.setAllowsExpansionToolTips(true);
    let warning = ui::symbol("exclamationmark.triangle.fill", "Needs attention");
    let attention = warning.map(|image| {
        let icon = NSImageView::imageViewWithImage(&image, mtm);
        icon.setContentTintColor(Some(&NSColor::systemOrangeColor()));
        icon.setHidden(true);
        icon
    });
    let status = ui::label(mtm, "");
    let header = match &attention {
        Some(icon) => {
            let header = ui::row(mtm, &[&name, icon, &status], 8.0);
            header.setCustomSpacing_afterView(4.0, icon);
            header
        }
        None => ui::row(mtm, &[&name, &status], 8.0),
    };
    // A long ID truncates rather than widening the pane.
    ui::fixed_width(&header, CHOICE);
    let secret = NSSecureTextField::new(mtm);
    secret.setPlaceholderString(Some(&ui::ns("Paste key")));
    // One scrolling line, like the fields `ui::field` makes: a long key never
    // wraps or grows the pane.
    if let Some(cell) = secret.cell() {
        cell.setScrollable(true);
        cell.setUsesSingleLineMode(true);
    }
    ui::fixed_width(&secret, CONTROL);
    // SAFETY: the window clears this delegate before it is released.
    unsafe { secret.setDelegate(Some(ProtocolObject::from_ref(owner))) };
    let save = ui::button(mtm, "Save", target, sel!(saveKey:));
    let cancel = ui::button(mtm, "Cancel", target, sel!(cancelKey:));
    let entry = ui::row(mtm, &[&secret, &save, &cancel], 8.0);
    let action = ui::button(mtm, "", target, sel!(keyAction:));
    let replace = ui::button(mtm, "Replace…", target, sel!(replaceKey:));
    let remove = ui::button(mtm, "Remove…", target, sel!(removeKey:));
    remove.setHasDestructiveAction(true);
    let actions = ui::row(mtm, &[&action, &replace, &remove], 8.0);
    let note = Message::new(mtm, CHOICE);
    let problem = Message::new(mtm, CHOICE);
    let view = ui::column(
        mtm,
        &[&header, &entry, &actions, &note.view, &problem.view],
        6.0,
    );
    KeyArea {
        view,
        name,
        attention,
        status,
        entry,
        secret,
        save,
        cancel,
        actions,
        action,
        replace,
        remove,
        note,
        problem,
    }
}

/// `path` with the home folder written as `~`, as Finder and Terminal show it.
fn home_relative(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// A whole number typed into `field`, or why it is not one.
fn whole<T: std::str::FromStr>(field: &NSTextField, name: &str) -> Result<T, String> {
    ui::text_of(field)
        .trim()
        .parse()
        .map_err(|_| format!("{name} must be a whole number."))
}

/// The cleanup credential as people know it: OpenRouter's by name, any other
/// by the ID it is stored under.
fn key_name(id: &str) -> String {
    if id.is_empty() {
        "API key".to_owned()
    } else if id.eq_ignore_ascii_case("openrouter") {
        "OpenRouter API key".to_owned()
    } else {
        format!("“{id}” API key")
    }
}

/// Whether the keychain holds a stored key back until access is approved,
/// rather than a read failing that another try may pass.
fn needs_approval(access: KeychainAuthorization) -> bool {
    matches!(
        access,
        KeychainAuthorization::NeedsAuthorization | KeychainAuthorization::Denied
    )
}

/// The button for a key the keyring did not return. Approval opens the macOS
/// prompt, so its title ends in an ellipsis; a retry only reads again.
fn action_title(access: KeychainAuthorization) -> Option<String> {
    let action = access.action()?;
    let title = if needs_approval(access) {
        format!("{action}…")
    } else {
        action.to_owned()
    };
    Some(title)
}

/// `text` without the whitespace around it, trimmed where it lies so a key
/// is not copied again.
fn trim_in_place(text: &mut String) {
    text.truncate(text.trim_end().len());
    let start = text.len() - text.trim_start().len();
    text.replace_range(..start, "");
}

/// Render `pane` of the Settings window offscreen to a PNG at `path`, with the
/// saved history loaded and live status read once. No window is ordered on
/// screen and the process never activates.
pub(crate) fn screenshot(path: &Path, pane: Pane) -> Result<()> {
    let mtm = MainThreadMarker::new().context("Settings must render on the main thread")?;
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let window = SettingsWindow::new(mtm, pane)?;
    {
        let mut state = window.state_mut();
        state.history.start_load();
        state.history.loaded(model::load_transcripts());
        // The key's state settles on its worker, as in the app; the render
        // waits for it as for the history, to show an answer, not a check.
        let settle = Instant::now() + Duration::from_secs(6);
        while state.credential.busy() && Instant::now() < settle {
            std::thread::sleep(Duration::from_millis(10));
            state.credential.poll();
        }
    }
    window.show_key();
    let status = ipc::status().ok();
    let line = status
        .as_ref()
        .map_or_else(|| "Cantrip is not running".to_owned(), super::status_line);
    window.tick(&Live {
        line: &line,
        online: status.is_some(),
        microphone: status
            .as_ref()
            .and_then(|status| status.microphone.as_ref()),
    });
    window.show_history();
    window.fit(false);
    // The frame view draws the window background and toolbar behind the panes.
    let frame = window
        .views()
        .window
        .contentView()
        // SAFETY: the window retains its frame view for as long as it exists.
        .and_then(|content| unsafe { content.superview() })
        .context("the Settings window has no frame view")?;
    ui::save_png(&frame, path)?;
    eprintln!("saved screenshot to {}", path.display());
    window.teardown();
    Ok(())
}
