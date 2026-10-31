//! The macOS Settings window: the shared [`SettingsForm`] and
//! [`TranscriptHistory`] in standard AppKit controls, inside the menu-bar app.
//!
//! A preference-style toolbar switches between Transcripts, General,
//! Transcription, Cleanup and HUD. Configuration edits stay in the window until
//! Save & Apply validates them, writes the file with its comments preserved
//! and asks the running Cantrip to apply it, exactly as on Linux. Live state
//! and microphone access come from the app's status reader, which is the
//! capture owner's own snapshot; Allow asks that owner. Effects run on worker
//! threads and the app's tick hands their results back, so the AppKit thread
//! never waits on IPC, history or the clipboard.

use super::ui::{self, Message};
use crate::inject::{self, DeliveryAvailability};
use crate::settings::model::{
    self, CopyFailure, CopyPhase, Microphones, PastTranscript, SettingsForm, TranscriptHistory,
    INJECTION_CHOICES, MOTION_CHOICES,
};
use anyhow::{Context, Result};
use cantrip_engine::{ipc, paths};
use clap::ValueEnum;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAccessibility, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType,
    NSBorderType, NSButton, NSColor, NSControlTextEditingDelegate, NSFont, NSFontWeightRegular,
    NSImageView, NSLayoutConstraintOrientation, NSMenu, NSMenuDelegate, NSPopUpButton,
    NSScrollView, NSStackView, NSStackViewGravity, NSTableColumn, NSTableColumnResizingOptions,
    NSTableView, NSTableViewDataSource, NSTableViewDelegate, NSTableViewStyle, NSTextField,
    NSTextFieldDelegate, NSTextView, NSToolbar, NSToolbarDelegate, NSToolbarItem, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSInteger, NSNotification, NSPoint, NSRect, NSSize, NSString,
};
use std::cell::{Cell, OnceCell, Ref, RefCell, RefMut};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

const WIDTH: f64 = 600.0;
const INSET: f64 = 24.0;
/// The width inside a pane's margins.
const BODY: f64 = WIDTH - 2.0 * INSET;
const CONTROL: f64 = 340.0;
const NUMBER: f64 = 90.0;
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
    Cleanup,
    Hud,
}

impl Pane {
    const ALL: [Self; 5] = [
        Self::Transcripts,
        Self::General,
        Self::Transcription,
        Self::Cleanup,
        Self::Hud,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::General => "General",
            Self::Transcription => "Transcription",
            Self::Cleanup => "Cleanup",
            Self::Hud => "HUD",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Transcripts => "clock.arrow.circlepath",
            Self::General => "gearshape",
            Self::Transcription => "waveform",
            Self::Cleanup => "wand.and.stars",
            Self::Hud => "rectangle.bottomhalf.inset.filled",
        }
    }

    fn identifier(self) -> &'static str {
        match self {
            Self::Transcripts => "transcripts",
            Self::General => "general",
            Self::Transcription => "transcription",
            Self::Cleanup => "cleanup",
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
}

/// The repair sheet: the file's original text, editable, over the window.
struct RepairSheet {
    window: Retained<NSWindow>,
    text: Retained<NSTextView>,
    message: Message,
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
    // Cleanup
    cleanup: Retained<NSButton>,
    pp_endpoint: Retained<NSTextField>,
    pp_model: Retained<NSTextField>,
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

    // SAFETY: the signature matches NSControlTextEditingDelegate.
    unsafe impl NSControlTextEditingDelegate for SettingsWindow {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            // Only the shortcut field reports its edits: its hint follows each keystroke.
            self.show_shortcut_hint();
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

    /// The window was closed; the app drops it at its next tick.
    pub(crate) fn closed(&self) -> bool {
        self.ivars().closed.get()
    }

    /// Close the window, as quitting Cantrip does. An open repair is
    /// abandoned, leaving the file as it is.
    pub(crate) fn close(&self) {
        if self.state().form.repairing() {
            self.state_mut().form.cancel_repair();
            self.end_repair_sheet();
        }
        self.views().window.close();
    }

    /// Detach every AppKit callback before the window is released.
    pub(crate) fn teardown(&self) {
        let views = self.views();
        views.window.setDelegate(None);
        views.toolbar.setDelegate(None);
        if let Some(menu) = views.microphone.menu() {
            menu.setDelegate(None);
        }
        // SAFETY: clearing a delegate or data source has no preconditions.
        unsafe {
            views.shortcut.setDelegate(None);
            views.table.setDataSource(None);
            views.table.setDelegate(None);
        }
    }

    /// Fold in live status and any finished background work.
    pub(crate) fn tick(&self, live: &Live) {
        let mut access = false;
        let mut form = false;
        let mut history = false;
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
            ui::set_checked(&views.cleanup, edit.pp_enabled);
            ui::set_text(&views.pp_endpoint, &edit.pp_endpoint);
            ui::set_text(&views.pp_model, &edit.pp_model);
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
        self.show_general();
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
        let passes = whole(&views.pp_passes, "Cleanup passes");
        let min_chars = whole(&views.pp_min_chars, "Minimum length");
        let hotkey = ui::text_of(&views.shortcut);
        let vocabulary = ui::text_of(&views.vocabulary);
        let keep_warm = ui::checked(&views.keep_warm);
        let stt_model = ui::text_of(&views.stt_model);
        let stt_endpoint = ui::text_of(&views.stt_endpoint);
        let stt_key = ui::text_of(&views.stt_key);
        let pp_enabled = ui::checked(&views.cleanup);
        let pp_endpoint = ui::text_of(&views.pp_endpoint);
        let pp_model = ui::text_of(&views.pp_model);
        let pp_key = ui::text_of(&views.pp_key);
        let instructions = views.pp_instructions.string().to_string();
        let labels = ui::checked(&views.labels);

        let mut state = self.state_mut();
        let state = &mut *state;
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
        edit.pp_enabled = pp_enabled;
        edit.pp_endpoint = pp_endpoint;
        edit.pp_model = pp_model;
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
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join(" "))
        }
    }

    fn show_pane(&self, pane: Pane, animate: bool) {
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
            Pane::Cleanup => Some(&views.cleanup),
            Pane::Hud => Some(&views.labels),
        };
        views.window.makeFirstResponder(first.map(|view| &**view));
    }

    /// Size the window to the pane, keeping its top edge where it is.
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
        window.setFrame_display_animate(window.frameRectForContentRect(content), true, animate);
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
        views.access_note.show(view.note.as_deref(), true);
        views
            .access_problem
            .show(state.access_problem.as_deref(), true);
    }

    /// The save notice and which footer actions are available.
    fn show_form_state(&self) {
        let views = self.views();
        let state = self.state();
        let form = &state.form;
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
        let access = ui::label(mtm, "");
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
        let access_row = ui::row(mtm, &[&access, &allow, &privacy], 8.0);
        let access_problem = Message::new(mtm, CONTROL);
        let shortcut = ui::field(mtm, crate::macos::shortcut::DEFAULT, CONTROL);
        // SAFETY: the window clears this delegate before it is released.
        unsafe { shortcut.setDelegate(Some(ProtocolObject::from_ref(self))) };
        let shortcut_hint = Message::new(mtm, CONTROL);
        let delivery = ui::popup(mtm, target, changed);
        let delivery_note = Message::new(mtm, CONTROL);
        let vocabulary = ui::field(mtm, "PipeWire, Parakeet", CONTROL);
        let vocabulary_note = ui::note(mtm, "Exact spellings, separated by commas.", CONTROL);
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
                (Some("Microphone access:"), &access_row),
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

        // Cleanup
        let cleanup = ui::checkbox(mtm, "Clean up transcripts", target, changed);
        let pp_endpoint = ui::field(mtm, "", CONTROL);
        let pp_model = ui::field(mtm, "", CONTROL);
        let pp_key = ui::field(mtm, "openrouter", CONTROL);
        let pp_timeout = ui::field(mtm, "", NUMBER);
        let milliseconds = ui::label(mtm, "ms");
        let timeout_row = ui::row(mtm, &[&pp_timeout, &milliseconds], 6.0);
        let pp_passes = ui::field(mtm, "", NUMBER);
        let passes_note = ui::note(
            mtm,
            "1 is one cleanup round; 2 adds a proofread pass.",
            CONTROL,
        );
        let pp_min_chars = ui::field(mtm, "", NUMBER);
        let characters = ui::label(mtm, "characters");
        let min_chars_row = ui::row(mtm, &[&pp_min_chars, &characters], 6.0);
        let min_chars_note = ui::note(
            mtm,
            "Shorter transcripts skip cleanup; 0 never skips.",
            CONTROL,
        );
        let (instructions_scroll, pp_instructions) = text_area(mtm, CONTROL, 88.0, false);
        let cleanup_form = ui::form(
            mtm,
            &[
                (None, &cleanup),
                (Some("Endpoint:"), &pp_endpoint),
                (Some("Model:"), &pp_model),
                (Some("API key ID:"), &pp_key),
                (Some("Timeout:"), &timeout_row),
                (Some("Cleanup passes:"), &pp_passes),
                (None, &passes_note),
                (Some("Minimum length:"), &min_chars_row),
                (None, &min_chars_note),
                (Some("Instructions:"), &instructions_scroll),
            ],
        );
        let cleanup_pane = pane(mtm, &[&cleanup_form]);

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
                Retained::into_super(cleanup_pane),
                Retained::into_super(hud),
            ],
            separator: Retained::into_super(separator),
            footer: Retained::into_super(footer),
            microphone,
            microphone_problem,
            access,
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
            cleanup,
            pp_endpoint,
            pp_model,
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
    }
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
