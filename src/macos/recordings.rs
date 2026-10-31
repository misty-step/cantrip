//! The macOS Recordings and Recovery window: the shared [`Recordings`] state in
//! standard AppKit controls, inside the menu-bar app.
//!
//! A segmented control in the toolbar switches between Recordings and Setup.
//! Recordings lists saved takes in a table, only those waiting for recovery
//! unless all are asked for, with the selected take's details and actions
//! below. Copy and recovery ask the engine to put text on the clipboard; Forget
//! asks in a standard alert sheet first. Setup installs the local model,
//! starts or reloads Cantrip, runs Check Setup and saves an API key from a
//! sheet. Observations and actions run on worker threads and the app's tick
//! folds their results in, so the AppKit thread never waits on IPC, the disk,
//! a download or the keychain.

use super::settings::Live;
use super::ui::{self, Message};
use crate::actions::effects::{self, Workers};
use crate::actions::model::{self, Action, Job, ListNotice, Observed, Recordings};
use crate::actions::{take_duration, take_time};
use anyhow::{Context, Result};
use block2::RcBlock;
use cantrip_engine::ipc;
use cantrip_engine::recovery::Take;
use objc2::{
    define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject},
    sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message as _,
};
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication,
    NSApplicationActivationPolicy, NSBackingStoreType, NSBorderType, NSBox, NSBoxType, NSButton,
    NSColor, NSControlSize, NSControlTextEditingDelegate, NSFont, NSFontWeightRegular,
    NSGridCellPlacement, NSGridRowAlignment, NSGridView, NSImageView, NSModalResponse,
    NSModalResponseCancel, NSProgressIndicator, NSProgressIndicatorStyle, NSScrollView,
    NSSecureTextField, NSSegmentSwitchTracking, NSSegmentedControl, NSStackView,
    NSStackViewGravity, NSTableCellView, NSTableColumn, NSTableView, NSTableViewDataSource,
    NSTableViewDelegate, NSTableViewStyle, NSTextField, NSTextFieldDelegate, NSTextView,
    NSTitlePosition, NSToolbar, NSToolbarDelegate, NSToolbarDisplayMode,
    NSToolbarFlexibleSpaceItemIdentifier, NSToolbarItem, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask, NSWindowToolbarStyle,
};
use objc2_foundation::{
    NSArray, NSEdgeInsets, NSIndexSet, NSInteger, NSNotification, NSPoint, NSRect, NSSize, NSString,
};
use std::cell::{Cell, OnceCell, Ref, RefCell, RefMut};
use std::path::Path;
use std::time::Instant;

const WIDTH: f64 = 640.0;
const INSET: f64 = 20.0;
/// The width inside a pane's margins.
const BODY: f64 = WIDTH - 2.0 * INSET;
const ROW_HEIGHT: f64 = 24.0;
const HEADER_HEIGHT: f64 = 28.0;
/// Complete recording rows visible at the window's size.
const VISIBLE_ROWS: f64 = 8.0;
/// The action buttons beside a selected recording share one width.
const ACTION: f64 = 200.0;
const REPORT_HEIGHT: f64 = 240.0;

const PANES: &str = "panes";
const REFRESH: &str = "refresh";

/// The recording table's columns: identifier, title, width.
const COLUMNS: [(&str, &str, f64); 5] = [
    ("recorded", "Recorded", 190.0),
    ("duration", "Duration", 80.0),
    ("transcript", "Transcript", 150.0),
    ("audio", "Audio", 90.0),
    ("waiting", "Recovery", 70.0),
];

/// Where the window opens when it is asked to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Open {
    Recordings,
    Setup,
    /// The setup pane, running Check Setup.
    CheckSetup,
}

/// Something only the app can do for this window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppRequest {
    OpenSettings,
    StartEngine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Recordings,
    Setup,
}

impl Pane {
    fn index(self) -> usize {
        match self {
            Self::Recordings => 0,
            Self::Setup => 1,
        }
    }
}

struct State {
    recordings: Recordings,
    workers: Workers,
    pane: Pane,
    live: String,
    online: bool,
    /// The outcome and notice the window shows; Dismiss names exactly these.
    outcome: Option<u64>,
    notice: Option<u64>,
    requests: Vec<AppRequest>,
    /// The Forget confirmation while it is shown.
    alert: Option<Retained<NSAlert>>,
    /// The app is quitting: close once setup work ends, whatever its result.
    quitting: bool,
}

/// An outcome or notice above the recordings, with its own Dismiss.
struct Banner {
    view: Retained<NSBox>,
    icon: Option<Retained<NSImageView>>,
    text: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    dismiss: Retained<NSButton>,
}

/// The sheet that saves an API key in the keychain.
struct KeySheet {
    window: Retained<NSWindow>,
    id: Retained<NSTextField>,
    secret: Retained<NSSecureTextField>,
    save: Retained<NSButton>,
}

struct Views {
    window: Retained<NSWindow>,
    toolbar: Retained<NSToolbar>,
    root: Retained<NSStackView>,
    panes: [Retained<NSView>; 2],
    // Recordings
    offline: Message,
    outcome: Banner,
    notice: Banner,
    filter: Retained<NSSegmentedControl>,
    table: Retained<NSTableView>,
    list_message: Message,
    retained: Retained<NSTextField>,
    detail: Retained<NSView>,
    title: Retained<NSTextField>,
    facts: Retained<NSTextField>,
    identifier: Retained<NSTextField>,
    copy: Retained<NSButton>,
    partial: Message,
    local: Retained<NSButton>,
    local_note: Retained<NSTextField>,
    provider: Retained<NSButton>,
    provider_note: Retained<NSTextField>,
    forget: Retained<NSButton>,
    missing: Message,
    // Setup
    setup_spinner: Retained<NSProgressIndicator>,
    setup_busy: Retained<NSTextField>,
    setup_status: Message,
    model_state: Retained<NSTextField>,
    install: Retained<NSButton>,
    cancel_install: Retained<NSButton>,
    model_note: Message,
    config_state: Retained<NSTextField>,
    config_note: Message,
    engine_state: Retained<NSTextField>,
    start: Retained<NSButton>,
    reload: Retained<NSButton>,
    store_key: Retained<NSButton>,
    check: Retained<NSButton>,
    report_scroll: Retained<NSScrollView>,
    report: Retained<NSTextView>,
    // Footer
    feedback: Message,
    live_dot: Option<Retained<NSImageView>>,
    live: Retained<NSTextField>,
    spinner: Retained<NSProgressIndicator>,
    busy: Retained<NSTextField>,
    stop: Retained<NSButton>,
    cancel: Retained<NSButton>,
    key_sheet: KeySheet,
}

/// The toolbar's items, made before the toolbar first asks for them.
struct ToolbarItems {
    panes: Retained<NSToolbarItem>,
    control: Retained<NSSegmentedControl>,
    refresh: Retained<NSToolbarItem>,
}

pub(crate) struct Ivars {
    state: RefCell<State>,
    views: OnceCell<Views>,
    toolbar_items: OnceCell<ToolbarItems>,
    closed: Cell<bool>,
    /// The window itself is changing the table's selection.
    syncing: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; `RecordingsWindow` has no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripRecordingsWindow"]
    #[ivars = Ivars]
    pub(crate) struct RecordingsWindow;

    // SAFETY: NSObjectProtocol has no safety requirements.
    unsafe impl NSObjectProtocol for RecordingsWindow {}

    // SAFETY: the signatures match NSWindowDelegate.
    unsafe impl NSWindowDelegate for RecordingsWindow {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            let close = self.state_mut().recordings.request_close();
            if !close {
                self.render(None);
            }
            close
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            self.ivars().closed.set(true);
        }
    }

    // SAFETY: the signatures match NSToolbarDelegate.
    unsafe impl NSToolbarDelegate for RecordingsWindow {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(
            &self,
            _toolbar: &NSToolbar,
            identifier: &NSString,
            _inserted: bool,
        ) -> Option<Retained<NSToolbarItem>> {
            let items = self.ivars().toolbar_items.get();
            match (items, identifier.to_string().as_str()) {
                (Some(items), PANES) => Some(items.panes.clone()),
                (Some(items), REFRESH) => Some(items.refresh.clone()),
                _ => None,
            }
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn default_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            toolbar_identifiers()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn allowed_items(&self, _toolbar: &NSToolbar) -> Retained<NSArray<NSString>> {
            toolbar_identifiers()
        }
    }

    // SAFETY: the signature matches NSTableViewDataSource.
    unsafe impl NSTableViewDataSource for RecordingsWindow {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.state().recordings.row_count() as NSInteger
        }
    }

    // SAFETY: the signatures match NSTableViewDelegate.
    unsafe impl NSTableViewDelegate for RecordingsWindow {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn table_cell(
            &self,
            _table: &NSTableView,
            column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            let column = column.map(|column| column.identifier().to_string());
            match (column, usize::try_from(row)) {
                (Some(column), Ok(row)) => self.recording_cell(&column, row),
                _ => None,
            }
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_did_change(&self, _notification: &NSNotification) {
            if self.ivars().syncing.get() {
                return;
            }
            let row = usize::try_from(self.views().table.selectedRow()).ok();
            {
                let mut state = self.state_mut();
                let id = row
                    .and_then(|row| state.recordings.row(row))
                    .map(|take| take.id.clone());
                state.recordings.select(id);
            }
            self.render(None);
        }
    }

    // SAFETY: the signature matches NSControlTextEditingDelegate.
    unsafe impl NSControlTextEditingDelegate for RecordingsWindow {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, _notification: &NSNotification) {
            // Only the API-key sheet's fields report edits: Save follows them.
            self.show_key_sheet_state();
        }
    }

    // SAFETY: NSTextFieldDelegate's methods are all optional.
    unsafe impl NSTextFieldDelegate for RecordingsWindow {}

    // SAFETY: each action takes its sender and returns nothing.
    impl RecordingsWindow {
        #[unsafe(method(selectPane:))]
        fn select_pane(&self, sender: &NSSegmentedControl) {
            let pane = if sender.selectedSegment() == 1 {
                Pane::Setup
            } else {
                Pane::Recordings
            };
            self.show_pane(pane, true);
        }

        #[unsafe(method(refresh:))]
        fn refresh(&self, _sender: Option<&AnyObject>) {
            self.state_mut().recordings.refresh();
        }

        #[unsafe(method(filter:))]
        fn filter(&self, sender: &NSSegmentedControl) {
            let all = sender.selectedSegment() == 1;
            self.state_mut().recordings.show_all(all);
            self.reload_table();
            self.render(None);
        }

        #[unsafe(method(stop:))]
        fn stop(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.stop();
            self.run(job);
        }

        #[unsafe(method(cancel:))]
        fn cancel(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.cancel();
            self.run(job);
        }

        #[unsafe(method(dismissOutcome:))]
        fn dismiss_outcome(&self, _sender: Option<&AnyObject>) {
            let job = {
                let mut state = self.state_mut();
                state.outcome.and_then(|event| state.recordings.dismiss(event))
            };
            self.run(job);
        }

        #[unsafe(method(dismissNotice:))]
        fn dismiss_notice(&self, _sender: Option<&AnyObject>) {
            let job = {
                let mut state = self.state_mut();
                state.notice.and_then(|event| state.recordings.dismiss(event))
            };
            self.run(job);
        }

        #[unsafe(method(copyTranscript:))]
        fn copy_transcript(&self, _sender: Option<&AnyObject>) {
            let job = self.with_selected(|recordings, id| recordings.copy(id));
            self.run(job);
        }

        /// Copy (⌘C) with the recording list in front copies the selected
        /// take's saved transcript, as its Copy button does.
        #[unsafe(method(copy:))]
        fn copy_selected(&self, _sender: Option<&AnyObject>) {
            if self.state().pane == Pane::Recordings {
                let job = self.with_selected(|recordings, id| recordings.copy(id));
                self.run(job);
            }
        }

        #[unsafe(method(recoverLocally:))]
        fn recover_locally(&self, _sender: Option<&AnyObject>) {
            let job = self.with_selected(|recordings, id| recordings.recover(id, true));
            self.run(job);
        }

        #[unsafe(method(recoverWithProvider:))]
        fn recover_with_provider(&self, _sender: Option<&AnyObject>) {
            let job = self.with_selected(|recordings, id| recordings.recover(id, false));
            self.run(job);
        }

        #[unsafe(method(forget:))]
        fn forget(&self, _sender: Option<&AnyObject>) {
            self.ask_forget();
        }

        #[unsafe(method(installModel:))]
        fn install_model(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.install_model();
            self.run(job);
        }

        #[unsafe(method(cancelInstall:))]
        fn cancel_install(&self, _sender: Option<&AnyObject>) {
            self.state_mut().recordings.cancel_install();
            self.render(None);
        }

        #[unsafe(method(startCantrip:))]
        fn start_cantrip(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.start_daemon();
            self.run(job);
        }

        #[unsafe(method(reloadConfiguration:))]
        fn reload_configuration(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.reload();
            self.run(job);
        }

        #[unsafe(method(checkSetup:))]
        fn check_setup(&self, _sender: Option<&AnyObject>) {
            let job = self.state_mut().recordings.check_setup();
            self.run(job);
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            self.state_mut().requests.push(AppRequest::OpenSettings);
        }

        #[unsafe(method(storeKey:))]
        fn store_key(&self, _sender: Option<&AnyObject>) {
            self.begin_key_sheet();
        }

        #[unsafe(method(saveKey:))]
        fn save_key(&self, _sender: Option<&AnyObject>) {
            self.finish_key_sheet(true);
        }

        #[unsafe(method(cancelKey:))]
        fn cancel_key(&self, _sender: Option<&AnyObject>) {
            self.finish_key_sheet(false);
        }
    }
);

/// The window title on the leading edge; the pane switch and Refresh trail it.
fn toolbar_identifiers() -> Retained<NSArray<NSString>> {
    // SAFETY: a framework constant, valid for the process lifetime.
    let flexible = unsafe { NSToolbarFlexibleSpaceItemIdentifier };
    NSArray::from_slice(&[flexible, &*ui::ns(PANES), &*ui::ns(REFRESH)])
}

impl RecordingsWindow {
    /// A Recordings and Recovery window, built but not yet shown.
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let state = State {
            recordings: Recordings::default(),
            workers: Workers::default(),
            pane: Pane::Recordings,
            live: String::new(),
            online: false,
            outcome: None,
            notice: None,
            requests: Vec::new(),
            alert: None,
            quitting: false,
        };
        let this = Self::alloc(mtm).set_ivars(Ivars {
            state: RefCell::new(state),
            views: OnceCell::new(),
            toolbar_items: OnceCell::new(),
            closed: Cell::new(false),
            syncing: Cell::new(false),
        });
        // SAFETY: `init` is NSObject's designated initializer.
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let views = this.build(mtm);
        let _ = this.ivars().views.set(views);
        this.show_pane(Pane::Recordings, false);
        this.render(None);
        this.views().window.center();
        this
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
            .expect("the Recordings window builds its views when created")
    }

    /// Show the window where `open` asks, bringing it in front.
    pub(crate) fn open(&self, open: Open) {
        self.show_pane(
            if open == Open::Recordings {
                Pane::Recordings
            } else {
                Pane::Setup
            },
            false,
        );
        if open == Open::CheckSetup {
            let job = self.state_mut().recordings.check_setup();
            self.run(job);
        }
        ui::present(&self.views().window);
    }

    /// The window was closed; the app drops it at its next tick.
    pub(crate) fn closed(&self) -> bool {
        self.ivars().closed.get()
    }

    /// The app is quitting. Open sheets are abandoned, keeping every recording.
    /// True once the window has closed; a download first cleans up and a
    /// bounded start finishes, after which the window closes by itself.
    pub(crate) fn close_for_quit(&self) -> bool {
        self.end_sheets();
        let close = {
            let mut state = self.state_mut();
            state.quitting = true;
            state.recordings.request_close() || state.recordings.setup_free()
        };
        if close {
            self.views().window.close();
        } else {
            self.render(None);
        }
        self.closed()
    }

    /// Report a problem the app met while acting for this window.
    pub(crate) fn show_problem(&self, text: String) {
        self.state_mut().recordings.show_problem(text);
        self.render(None);
    }

    /// Detach every AppKit callback before the window is released.
    pub(crate) fn teardown(&self) {
        self.end_sheets();
        let views = self.views();
        views.window.setDelegate(None);
        views.toolbar.setDelegate(None);
        // SAFETY: clearing a delegate or data source has no preconditions.
        unsafe {
            views.table.setDataSource(None);
            views.table.setDelegate(None);
            views.key_sheet.id.setDelegate(None);
            views.key_sheet.secret.setDelegate(None);
        }
    }

    /// Fold in live status and finished background work, start the next
    /// observation, and hand back what only the app can do.
    pub(crate) fn tick(&self, live: &Live) -> Vec<AppRequest> {
        let now = Instant::now();
        let (collected, line, close) = {
            let mut state = self.state_mut();
            let state = &mut *state;
            let collected = state.workers.tick(&mut state.recordings, now, || {});
            let line = (state.live != live.line || state.online != live.online).then(|| {
                state.live = live.line.to_owned();
                state.online = live.online;
            });
            let close = collected.close || state.quitting && state.recordings.setup_free();
            (collected, line, close)
        };
        if collected.changed || line.is_some() {
            self.render(collected.observed);
        }
        if close && !self.closed() {
            self.views().window.close();
        }
        std::mem::take(&mut self.state_mut().requests)
    }

    /// Run a job the shared state handed out. Starting Cantrip also asks the
    /// app to start its engine, which the job then waits for.
    fn run(&self, job: Option<Job>) {
        if let Some(job) = job {
            let mut state = self.state_mut();
            if matches!(job.action, Action::StartDaemon) {
                state.requests.push(AppRequest::StartEngine);
            }
            state.workers.start(job, || {});
        }
        self.render(None);
    }

    /// Apply `act` to the selected take's ID, if a take is selected.
    fn with_selected(&self, act: impl FnOnce(&mut Recordings, &str) -> Option<Job>) -> Option<Job> {
        let mut state = self.state_mut();
        let id = state.recordings.selected_id()?.to_owned();
        act(&mut state.recordings, &id)
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
        if let Some(items) = self.ivars().toolbar_items.get() {
            items.control.setSelectedSegment(pane.index() as NSInteger);
        }
        self.fit(animate);
        let first: &NSView = match pane {
            Pane::Recordings => &views.table,
            Pane::Setup => &views.check,
        };
        views.window.makeFirstResponder(Some(first));
    }

    /// Size the window to its pane, keeping its top edge where it is.
    fn fit(&self, animate: bool) {
        let views = self.views();
        views.root.layoutSubtreeIfNeeded();
        let size = views.root.fittingSize();
        let window = &views.window;
        let current = window.contentRectForFrameRect(window.frame());
        if (current.size.height - size.height).abs() < 0.5 {
            return;
        }
        let top = current.origin.y + current.size.height;
        let content = NSRect::new(
            NSPoint::new(current.origin.x, top - size.height),
            NSSize::new(WIDTH, size.height),
        );
        window.setFrame_display_animate(window.frameRectForContentRect(content), true, animate);
    }

    /// Reread the table, then select the selected take wherever it now is, or
    /// nothing: a row never stands in for a take that is gone.
    fn reload_table(&self) {
        let views = self.views();
        let (row, all) = {
            let state = self.state();
            (
                state.recordings.selected_row(),
                state.recordings.showing_all(),
            )
        };
        if let Some(column) = views.table.tableColumnWithIdentifier(&ui::ns("waiting")) {
            column.setHidden(!all);
        }
        self.ivars().syncing.set(true);
        views.table.reloadData();
        let rows = match row {
            Some(row) => NSIndexSet::indexSetWithIndex(row),
            None => NSIndexSet::new(),
        };
        views
            .table
            .selectRowIndexes_byExtendingSelection(&rows, false);
        self.ivars().syncing.set(false);
        if let Some(row) = row {
            views.table.scrollRowToVisible(row as NSInteger);
        }
    }

    /// Show everything the state says, after any change.
    fn render(&self, observed: Option<Observed>) {
        if let Some(observed) = observed {
            if observed.history {
                self.reload_table();
            }
            if observed.forget_withdrawn {
                self.end_alert();
            }
            if observed.first_selection && self.state().pane == Pane::Recordings {
                self.views()
                    .window
                    .makeFirstResponder(Some(&self.views().table));
            }
        }
        self.show_status();
        self.show_list();
        self.show_detail();
        self.show_setup();
        self.show_alert_state();
        self.fit(false);
    }

    /// The live line, the outcome and notice, and the latest command's result.
    fn show_status(&self) {
        let views = self.views();
        let mut state = self.state_mut();
        let state = &mut *state;
        let recordings = &state.recordings;
        let actions = recordings.live_actions();
        let capabilities = recordings
            .status()
            .map(|status| status.capabilities)
            .unwrap_or_default();
        ui::set_text(&views.live, &state.live);
        let handoff = recordings
            .status()
            .and_then(|status| status.handoff.as_ref());
        let color = match handoff {
            Some(handoff) => rgb(handoff.color),
            None => NSColor::secondaryLabelColor(),
        };
        views.live.setTextColor(Some(&color));
        if let Some(dot) = &views.live_dot {
            let color = if state.online {
                NSColor::systemGreenColor()
            } else {
                NSColor::tertiaryLabelColor()
            };
            dot.setContentTintColor(Some(&color));
        }
        views.stop.setHidden(!capabilities.stop);
        views.stop.setEnabled(actions.stop);
        views.cancel.setHidden(!capabilities.cancel);
        views.cancel.setEnabled(actions.cancel);
        let busy = recordings
            .command_busy()
            .or(recordings.cancelling().then_some(model::CANCELLING));
        show_busy(&views.spinner, &views.busy, busy);
        views.feedback.show(
            recordings.message().map(|message| message.text.as_str()),
            recordings
                .message()
                .is_some_and(|message| message.attention),
        );
        views.offline.show(
            (recordings.observed_once() && !recordings.online()).then_some(model::OFFLINE),
            false,
        );

        state.outcome = recordings.outcome().map(|outcome| outcome.event_id);
        match recordings.outcome() {
            Some(outcome) => views.outcome.show(
                &model::outcome_text(outcome),
                outcome.error.as_deref(),
                outcome.needs_attention(),
                outcome.handoff.as_ref().map(|handoff| handoff.color),
                actions.dismiss,
            ),
            None => views.outcome.hide(),
        }
        state.notice = recordings.notice().map(|notice| notice.event_id);
        match recordings.notice() {
            Some(notice) => views
                .notice
                .show(&notice.message, None, true, None, actions.dismiss),
            None => views.notice.hide(),
        }
    }

    fn show_list(&self) {
        let views = self.views();
        let state = self.state();
        let recordings = &state.recordings;
        views
            .filter
            .setSelectedSegment(NSInteger::from(recordings.showing_all()));
        let notice = recordings.list_notice().map(|notice| match notice {
            ListNotice::Loading => model::LOADING,
            ListNotice::Stale => model::STALE,
            ListNotice::Empty => model::EMPTY,
            ListNotice::NothingWaiting => {
                "Nothing is waiting to recover. Choose All Recordings to see saved transcripts."
            }
        });
        views
            .list_message
            .show(notice, recordings.list_notice() == Some(ListNotice::Stale));
        let summary = recordings.retained_summary();
        views.retained.setHidden(summary.is_none());
        if let Some(summary) = summary {
            ui::set_text(&views.retained, &summary);
        }
    }

    /// The selected take's details and the actions available for it.
    fn show_detail(&self) {
        let views = self.views();
        let state = self.state();
        let recordings = &state.recordings;
        let take = recordings.selected();
        views.detail.setHidden(take.is_none());
        views.missing.show(
            recordings.selection_missing().then_some(model::MISSING),
            false,
        );
        let Some(take) = take else {
            return;
        };
        let actions = recordings.take_actions(take);
        ui::set_text(&views.title, &take_time(take.created_at_unix_ms));
        ui::set_text(
            &views.facts,
            &format!(
                "{} · {} · audio {}",
                take_duration(take.duration_ms),
                model::completeness(take),
                model::audio(take)
            ),
        );
        ui::set_text(&views.identifier, &format!("Recording ID: {}", take.id));
        views.copy.setEnabled(actions.copy);
        views.partial.show(
            (take.partial && take.text_available).then_some(model::PARTIAL),
            true,
        );
        views.local.setEnabled(actions.recover_locally);
        views.provider.setEnabled(actions.recover_with_provider);
        ui::set_text(
            &views.local_note,
            if take.audio_available {
                model::LOCAL
            } else {
                model::NO_AUDIO
            },
        );
        views.provider_note.setHidden(!take.audio_available);
        views.forget.setEnabled(actions.forget);
    }

    fn show_setup(&self) {
        let views = self.views();
        let state = self.state();
        let recordings = &state.recordings;
        show_busy(
            &views.setup_spinner,
            &views.setup_busy,
            recordings.setup_busy(),
        );
        views.setup_status.show(
            recordings.setup_status().map(|status| status.text.as_str()),
            recordings
                .setup_status()
                .is_some_and(|status| status.attention),
        );
        let observed = recordings.observed_once();
        let free = recordings.setup_free();
        let installed = recordings.local_model_installed();
        ui::set_text(
            &views.model_state,
            match (observed, installed) {
                (false, _) => "Checking…",
                (true, true) => "Installed",
                (true, false) => "Not installed",
            },
        );
        let installing = recordings.installing();
        views.install.setHidden(installed || installing.is_some());
        views.install.setEnabled(free && observed);
        views.cancel_install.setHidden(installing.is_none());
        views.cancel_install.setEnabled(installing.unwrap_or(false));
        views.model_note.show(
            (observed && !installed).then_some(model::MODEL_MISSING),
            false,
        );
        let attention = recordings.config_needs_attention();
        ui::set_text(
            &views.config_state,
            match (observed, attention) {
                (false, _) => "Checking…",
                (true, false) => "Valid",
                (true, true) => "Needs attention",
            },
        );
        views
            .config_note
            .show(attention.then_some(model::CONFIG_ATTENTION), true);
        let online = recordings.online();
        ui::set_text(
            &views.engine_state,
            match (observed, online) {
                (false, _) => "Checking…",
                (true, true) => "Running",
                (true, false) => "Not running",
            },
        );
        views.start.setHidden(!observed || online);
        views.start.setEnabled(free);
        views.reload.setHidden(!online);
        views.reload.setEnabled(recordings.can_reload());
        views.store_key.setEnabled(free);
        views.check.setEnabled(free);
        let diagnosis = recordings.diagnosis();
        views.report_scroll.setHidden(diagnosis.is_none());
        if let Some(diagnosis) = diagnosis {
            if views.report.string().to_string() != diagnosis {
                views.report.setString(&ui::ns(diagnosis));
            }
        }
    }

    /// One recording table cell, in the system font; the cell view hands the
    /// row's selection highlight to its text.
    fn recording_cell(&self, column: &str, row: usize) -> Option<Retained<NSView>> {
        let mtm = self.mtm();
        let state = self.state();
        let take = state.recordings.row(row)?;
        let text = match column {
            "recorded" => take_time(take.created_at_unix_ms),
            "duration" => take_duration(take.duration_ms),
            "transcript" => sentence(model::completeness(take)),
            "audio" => if take.audio_available {
                "Saved"
            } else {
                "Not retained"
            }
            .to_owned(),
            "waiting" => if take.unresolved { "Waiting" } else { "" }.to_owned(),
            _ => return None,
        };
        let label = ui::truncating_label(mtm, &text);
        if column != "recorded" {
            label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        label.setTranslatesAutoresizingMaskIntoConstraints(false);
        let cell = NSTableCellView::new(mtm);
        cell.addSubview(&label);
        // SAFETY: the label is the cell's subview, so it outlives this
        // unretained reference.
        unsafe { cell.setTextField(Some(&label)) };
        label
            .leadingAnchor()
            .constraintEqualToAnchor_constant(&cell.leadingAnchor(), 4.0)
            .setActive(true);
        label
            .trailingAnchor()
            .constraintLessThanOrEqualToAnchor_constant(&cell.trailingAnchor(), -4.0)
            .setActive(true);
        label
            .centerYAnchor()
            .constraintEqualToAnchor(&cell.centerYAnchor())
            .setActive(true);
        Some(Retained::into_super(cell))
    }

    /// Ask, in a standard alert sheet, before forgetting the selected take.
    /// Return never forgets; Escape keeps the recording.
    fn ask_forget(&self) {
        let take = {
            let mut state = self.state_mut();
            let Some(id) = state.recordings.selected_id().map(str::to_owned) else {
                return;
            };
            if !state.recordings.ask_forget(&id) {
                return;
            }
            state.recordings.forget_confirmation().cloned()
        };
        let Some(take) = take else {
            return;
        };
        let alert = NSAlert::new(self.mtm());
        alert.setAlertStyle(NSAlertStyle::Warning);
        alert.setMessageText(&ui::ns("Forget this recording?"));
        alert.setInformativeText(&ui::ns(&forget_details(&take)));
        let forget = alert.addButtonWithTitle(&ui::ns("Forget Recording"));
        forget.setHasDestructiveAction(true);
        forget.setKeyEquivalent(&ui::ns(""));
        let keep = alert.addButtonWithTitle(&ui::ns("Cancel"));
        let this = self.retain();
        let answered = RcBlock::new(move |response: NSModalResponse| {
            this.forget_answered(response == NSAlertFirstButtonReturn);
        });
        self.state_mut().alert = Some(alert.clone());
        self.show_alert_state();
        alert.beginSheetModalForWindow_completionHandler(&self.views().window, Some(&answered));
        alert.window().makeFirstResponder(Some(&keep));
    }

    fn forget_answered(&self, confirmed: bool) {
        let job = {
            let mut state = self.state_mut();
            state.alert = None;
            let job = if confirmed {
                state.recordings.confirm_forget()
            } else {
                None
            };
            // A confirmation that can no longer proceed closes with nothing deleted.
            if job.is_none() {
                state.recordings.keep();
            }
            job
        };
        self.run(job);
    }

    /// Forget is enabled in the open alert only while it is allowed now.
    fn show_alert_state(&self) {
        let state = self.state();
        if let Some(alert) = &state.alert {
            if let Some(forget) = alert.buttons().firstObject() {
                forget.setEnabled(state.recordings.can_confirm_forget());
            }
        }
    }

    /// Withdraw the Forget confirmation: its take changed since it opened.
    fn end_alert(&self) {
        let alert = self.state().alert.clone();
        if let Some(alert) = alert {
            self.views()
                .window
                .endSheet_returnCode(&alert.window(), NSModalResponseCancel);
        }
    }

    /// Close any open sheet without acting.
    fn end_sheets(&self) {
        self.end_alert();
        if self.views().window.attachedSheet().is_some() {
            self.finish_key_sheet(false);
        }
    }

    fn begin_key_sheet(&self) {
        let key_id = self
            .state()
            .recordings
            .key_id()
            .unwrap_or_default()
            .to_owned();
        let views = self.views();
        let sheet = &views.key_sheet;
        ui::set_text(&sheet.id, &key_id);
        sheet.secret.setStringValue(&NSString::new());
        self.show_key_sheet_state();
        views
            .window
            .beginSheet_completionHandler(&sheet.window, None);
        sheet.window.makeFirstResponder(Some(&sheet.id));
    }

    fn show_key_sheet_state(&self) {
        let sheet = &self.views().key_sheet;
        let id = ui::text_of(&sheet.id);
        let entered = sheet.secret.stringValue().length() > 0;
        let state = self.state();
        // The secret's own text is never copied out to decide this.
        let allowed = state
            .recordings
            .can_save_key(&id, if entered { "set" } else { "" });
        sheet.save.setEnabled(allowed);
    }

    /// Close the key sheet, saving the key when asked and allowed. The field
    /// is cleared either way; the value lives only in the saving worker.
    fn finish_key_sheet(&self, save: bool) {
        let views = self.views();
        let sheet = &views.key_sheet;
        let id = ui::text_of(&sheet.id);
        let secret = save.then(|| ui::text_of(&sheet.secret));
        sheet.secret.setStringValue(&NSString::new());
        if views.window.attachedSheet().is_some() {
            views.window.endSheet(&sheet.window);
        }
        sheet.window.orderOut(None);
        if let Some(secret) = secret {
            let job = self.state_mut().recordings.save_key(id, secret);
            self.run(job);
        }
    }

    fn build(&self, mtm: MainThreadMarker) -> Views {
        let target: &AnyObject = self;

        // Toolbar items, made before the window asks for them.
        let control = {
            let labels = [ui::ns("Recordings"), ui::ns("Setup")];
            // SAFETY: `selectPane:` is defined above.
            unsafe {
                NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                    &NSArray::from_retained_slice(&labels),
                    NSSegmentSwitchTracking::SelectOne,
                    Some(target),
                    Some(sel!(selectPane:)),
                    mtm,
                )
            }
        };
        control.setSelectedSegment(0);
        control.setAccessibilityLabel(Some(&ui::ns("Show")));
        let panes_item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), &ui::ns(PANES));
        panes_item.setView(Some(&control));
        panes_item.setLabel(&ui::ns("Show"));
        panes_item.setPaletteLabel(&ui::ns("Show"));
        let refresh_item =
            NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), &ui::ns(REFRESH));
        refresh_item.setLabel(&ui::ns("Refresh"));
        refresh_item.setPaletteLabel(&ui::ns("Refresh"));
        refresh_item.setToolTip(Some(&ui::ns("Read recordings and setup again")));
        refresh_item.setImage(ui::symbol("arrow.clockwise", "Refresh").as_deref());
        refresh_item.setBordered(true);
        // SAFETY: `refresh:` is defined above; the window outlives its toolbar items.
        unsafe {
            refresh_item.setTarget(Some(target));
            refresh_item.setAction(Some(sel!(refresh:)));
        }
        let _ = self.ivars().toolbar_items.set(ToolbarItems {
            panes: panes_item,
            control,
            refresh: refresh_item,
        });

        // Recordings
        let offline = Message::new(mtm, BODY);
        let outcome = banner(mtm, target, sel!(dismissOutcome:), "Dismiss Outcome");
        let notice = banner(mtm, target, sel!(dismissNotice:), "Dismiss Notice");
        let filter = {
            let labels = [ui::ns("Waiting to Recover"), ui::ns("All Recordings")];
            // SAFETY: `filter:` is defined above.
            unsafe {
                NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                    &NSArray::from_retained_slice(&labels),
                    NSSegmentSwitchTracking::SelectOne,
                    Some(target),
                    Some(sel!(filter:)),
                    mtm,
                )
            }
        };
        filter.setAccessibilityLabel(Some(&ui::ns("Recordings shown")));
        let table = NSTableView::initWithFrame(NSTableView::alloc(mtm), NSRect::ZERO);
        for (identifier, title, width) in COLUMNS {
            let column =
                NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ui::ns(identifier));
            column.setTitle(&ui::ns(title));
            column.setWidth(width);
            column.setMinWidth(width * 0.6);
            table.addTableColumn(&column);
        }
        table.setRowHeight(ROW_HEIGHT);
        table.setStyle(NSTableViewStyle::FullWidth);
        table.setUsesAlternatingRowBackgroundColors(true);
        table.setAllowsColumnReordering(false);
        table.setAllowsEmptySelection(true);
        // SAFETY: the window clears these before it is released.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(self)));
            table.setDelegate(Some(ProtocolObject::from_ref(self)));
        }
        table.setAccessibilityLabel(Some(&ui::ns("Saved recordings")));
        let list = NSScrollView::new(mtm);
        list.setDocumentView(Some(&table));
        list.setHasVerticalScroller(true);
        list.setBorderType(NSBorderType::BezelBorder);
        ui::fixed_width(&list, BODY);
        ui::fixed_height(&list, HEADER_HEIGHT + ROW_HEIGHT * VISIBLE_ROWS + 2.0);
        let list_message = Message::new(mtm, BODY);
        let retained = ui::note(mtm, "", BODY);
        let explanation = ui::note(mtm, model::METADATA_ONLY, BODY);

        let title = ui::label(mtm, "");
        title.setFont(Some(
            &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
        ));
        let facts = ui::label(mtm, "");
        facts.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let identifier = ui::label(mtm, "");
        identifier.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        identifier.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        identifier.setSelectable(true);
        let copy = action_button(mtm, "Copy Transcript", target, sel!(copyTranscript:));
        let partial = Message::new(mtm, BODY - ACTION - 12.0);
        let local = action_button(mtm, "Recover Locally", target, sel!(recoverLocally:));
        let local_note = ui::note(mtm, model::LOCAL, BODY - ACTION - 12.0);
        let provider = action_button(
            mtm,
            "Recover with Provider",
            target,
            sel!(recoverWithProvider:),
        );
        let provider_note = ui::note(mtm, model::PROVIDER, BODY - ACTION - 12.0);
        let forget = action_button(mtm, "Forget…", target, sel!(forget:));
        forget.setHasDestructiveAction(true);
        let empty = NSView::new(mtm);
        let actions = action_grid(
            mtm,
            &[
                // Wrapped, so hiding the warning keeps Copy's row.
                (&copy, &ui::column(mtm, &[&partial.view], 0.0)),
                (&local, &local_note),
                (&provider, &provider_note),
                (&forget, &empty),
            ],
        );
        let detail = ui::column(mtm, &[&title, &facts, &identifier, &actions], 4.0);
        detail.setCustomSpacing_afterView(12.0, &identifier);
        let missing = Message::new(mtm, BODY);
        let recordings_pane = pane(
            mtm,
            &[
                &offline.view,
                &outcome.view,
                &notice.view,
                &filter,
                &list,
                &list_message.view,
                &retained,
                &explanation,
                &ui::separator(mtm),
                &detail,
                &missing.view,
            ],
        );

        // Setup
        let setup_spinner = spinner(mtm);
        let setup_busy = ui::label(mtm, "");
        let setup_activity = ui::row(mtm, &[&setup_spinner, &setup_busy], 6.0);
        let setup_status = Message::new(mtm, BODY);
        let model_state = ui::label(mtm, "");
        let install = ui::button(mtm, "Install Local Model", target, sel!(installModel:));
        let cancel_install = ui::button(mtm, "Cancel Installation", target, sel!(cancelInstall:));
        let model_row = ui::row(mtm, &[&model_state, &install, &cancel_install], 8.0);
        let model_note = Message::new(mtm, 420.0);
        let config_state = ui::label(mtm, "");
        let open_settings = ui::button(mtm, "Open Settings…", target, sel!(openSettings:));
        let config_row = ui::row(mtm, &[&config_state, &open_settings], 8.0);
        let config_note = Message::new(mtm, 420.0);
        let engine_state = ui::label(mtm, "");
        let start = ui::button(mtm, "Start Cantrip", target, sel!(startCantrip:));
        let reload = ui::button(
            mtm,
            "Reload Configuration",
            target,
            sel!(reloadConfiguration:),
        );
        let engine_row = ui::row(mtm, &[&engine_state, &start, &reload], 8.0);
        let store_key = ui::button(mtm, "Save an API Key…", target, sel!(storeKey:));
        let key_note = ui::note(
            mtm,
            "Stores a provider key in your keychain under the ID that Settings uses.",
            420.0,
        );
        let setup_form = ui::form(
            mtm,
            &[
                (Some("Local model:"), &model_row),
                (None, &model_note.view),
                (Some("Configuration:"), &config_row),
                (None, &config_note.view),
                (Some("Cantrip:"), &engine_row),
                (Some("API key:"), &store_key),
                (None, &key_note),
            ],
        );
        let check = ui::button(mtm, "Check Setup", target, sel!(checkSetup:));
        let check_note = ui::note(
            mtm,
            "Reads configuration, microphone, speech, cleanup and delivery readiness. It changes nothing and downloads nothing.",
            BODY - 140.0,
        );
        let check_row = ui::row(mtm, &[&check, &check_note], 12.0);
        check_row.setAlignment(objc2_app_kit::NSLayoutAttribute::FirstBaseline);
        let (report_scroll, report) = report_view(mtm);
        let setup_pane = pane(
            mtm,
            &[
                &setup_activity,
                &setup_status.view,
                &setup_form,
                &ui::separator(mtm),
                &check_row,
                &report_scroll,
            ],
        );

        // Footer
        let feedback = Message::new(mtm, BODY);
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
        let busy_spinner = spinner(mtm);
        let busy = ui::label(mtm, "");
        busy.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        let stop = ui::button(mtm, "Stop Recording", target, sel!(stop:));
        let cancel = ui::button(mtm, "Cancel Without Delivery", target, sel!(cancel:));
        let status_row = NSStackView::new(mtm);
        status_row.setSpacing(6.0);
        if let Some(dot) = &live_dot {
            status_row.addView_inGravity(dot, NSStackViewGravity::Leading);
        }
        status_row.addView_inGravity(&live, NSStackViewGravity::Leading);
        status_row.addView_inGravity(&busy_spinner, NSStackViewGravity::Leading);
        status_row.addView_inGravity(&busy, NSStackViewGravity::Leading);
        status_row.addView_inGravity(&stop, NSStackViewGravity::Trailing);
        status_row.addView_inGravity(&cancel, NSStackViewGravity::Trailing);
        ui::fixed_width(&status_row, BODY);
        let footer = ui::column(mtm, &[&feedback.view, &status_row], 8.0);
        footer.setEdgeInsets(NSEdgeInsets {
            top: 10.0,
            left: INSET,
            bottom: 14.0,
            right: INSET,
        });
        let separator = ui::separator(mtm);
        ui::fixed_width(&separator, WIDTH);
        let root = ui::column(mtm, &[&recordings_pane, &separator, &footer], 0.0);

        // SAFETY: created on the main thread; `Views` owns the only reference,
        // so AppKit must not also release it on close.
        let window = unsafe {
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::ZERO, NSSize::new(WIDTH, 600.0)),
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Miniaturizable,
                NSBackingStoreType::Buffered,
                true,
            );
            window.setReleasedWhenClosed(false);
            window
        };
        window.setTitle(&ui::ns("Recordings and Recovery"));
        window.setContentView(Some(&root));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let toolbar =
            NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &ui::ns("CantripRecordings"));
        toolbar.setDelegate(Some(ProtocolObject::from_ref(self)));
        toolbar.setAllowsUserCustomization(false);
        toolbar.setDisplayMode(NSToolbarDisplayMode::IconOnly);
        window.setToolbar(Some(&toolbar));
        window.setToolbarStyle(NSWindowToolbarStyle::Unified);

        Views {
            key_sheet: key_sheet(mtm, self),
            window,
            toolbar,
            root,
            panes: [
                Retained::into_super(recordings_pane),
                Retained::into_super(setup_pane),
            ],
            offline,
            outcome,
            notice,
            filter,
            table,
            list_message,
            retained,
            detail: Retained::into_super(detail),
            title,
            facts,
            identifier,
            copy,
            partial,
            local,
            local_note,
            provider,
            provider_note,
            forget,
            missing,
            setup_spinner,
            setup_busy,
            setup_status,
            model_state,
            install,
            cancel_install,
            model_note,
            config_state,
            config_note,
            engine_state,
            start,
            reload,
            store_key,
            check,
            report_scroll,
            report,
            feedback,
            live_dot,
            live,
            spinner: busy_spinner,
            busy,
            stop,
            cancel,
        }
    }
}

impl Banner {
    fn show(
        &self,
        text: &str,
        detail: Option<&str>,
        attention: bool,
        color: Option<[u8; 3]>,
        dismiss: bool,
    ) {
        self.view.setHidden(false);
        ui::set_text(&self.text, text);
        let color = match color {
            Some(color) => rgb(color),
            None => NSColor::labelColor(),
        };
        self.text.setTextColor(Some(&color));
        self.detail.setHidden(detail.is_none());
        ui::set_text(&self.detail, detail.unwrap_or_default());
        if let Some(icon) = &self.icon {
            icon.setHidden(!attention);
        }
        self.dismiss.setEnabled(dismiss);
    }

    fn hide(&self) {
        self.view.setHidden(true);
    }
}

/// An outcome or notice: its sentence and cause beside its own Dismiss.
fn banner(
    mtm: MainThreadMarker,
    target: &AnyObject,
    action: objc2::runtime::Sel,
    title: &str,
) -> Banner {
    let width = BODY - 180.0;
    let text = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
    text.setPreferredMaxLayoutWidth(width);
    ui::fixed_width(&text, width);
    let detail = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
    detail.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    detail.setTextColor(Some(&NSColor::secondaryLabelColor()));
    detail.setPreferredMaxLayoutWidth(width);
    ui::fixed_width(&detail, width);
    let words = ui::column(mtm, &[&text, &detail], 2.0);
    let icon = ui::symbol("exclamationmark.triangle.fill", "Needs attention").map(|image| {
        let icon = NSImageView::imageViewWithImage(&image, mtm);
        icon.setContentTintColor(Some(&NSColor::systemOrangeColor()));
        icon
    });
    let dismiss = ui::button(mtm, title, target, action);
    let line = NSStackView::new(mtm);
    line.setSpacing(8.0);
    line.setAlignment(objc2_app_kit::NSLayoutAttribute::CenterY);
    if let Some(icon) = &icon {
        line.addView_inGravity(icon, NSStackViewGravity::Leading);
    }
    line.addView_inGravity(&words, NSStackViewGravity::Leading);
    line.addView_inGravity(&dismiss, NSStackViewGravity::Trailing);
    ui::fixed_width(&line, BODY - 24.0);
    let view = NSBox::new(mtm);
    view.setBoxType(NSBoxType::Primary);
    view.setTitlePosition(NSTitlePosition::NoTitle);
    view.setContentViewMargins(NSSize::new(10.0, 8.0));
    view.setContentView(Some(&line));
    ui::fixed_width(&view, BODY);
    view.setHidden(true);
    Banner {
        view,
        icon,
        text,
        detail,
        dismiss,
    }
}

/// A push button of the shared action width.
fn action_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: objc2::runtime::Sel,
) -> Retained<NSButton> {
    let button = ui::button(mtm, title, target, action);
    ui::fixed_width(&button, ACTION);
    button
}

/// Action buttons on the leading edge, each explained beside it.
fn action_grid(mtm: MainThreadMarker, rows: &[(&NSView, &NSView)]) -> Retained<NSGridView> {
    let rows: Vec<Retained<NSArray<NSView>>> = rows
        .iter()
        .map(|(button, note)| NSArray::from_slice(&[*button, *note]))
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    grid.setColumnSpacing(12.0);
    grid.setRowSpacing(8.0);
    grid.columnAtIndex(0)
        .setXPlacement(NSGridCellPlacement::Leading);
    grid.columnAtIndex(1)
        .setXPlacement(NSGridCellPlacement::Leading);
    grid
}

/// A pane's content: its views stacked in the window's margins.
fn pane(mtm: MainThreadMarker, views: &[&NSView]) -> Retained<NSStackView> {
    let pane = ui::column(mtm, views, 10.0);
    pane.setEdgeInsets(NSEdgeInsets {
        top: 16.0,
        left: INSET,
        bottom: 16.0,
        right: INSET,
    });
    ui::fixed_width(&pane, WIDTH);
    pane
}

fn spinner(mtm: MainThreadMarker) -> Retained<NSProgressIndicator> {
    let spinner = NSProgressIndicator::new(mtm);
    spinner.setStyle(NSProgressIndicatorStyle::Spinning);
    spinner.setControlSize(NSControlSize::Small);
    spinner.setIndeterminate(true);
    spinner.setDisplayedWhenStopped(false);
    spinner.sizeToFit();
    spinner
}

/// A spinner and its sentence while work runs; nothing otherwise.
fn show_busy(spinner: &NSProgressIndicator, label: &NSTextField, busy: Option<&str>) {
    label.setHidden(busy.is_none());
    ui::set_text(label, busy.unwrap_or_default());
    // SAFETY: animation changes need no sender.
    unsafe {
        if busy.is_some() {
            spinner.startAnimation(None);
        } else {
            spinner.stopAnimation(None);
        }
    }
}

/// The selectable, monospaced Check Setup report.
fn report_view(mtm: MainThreadMarker) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = NSTextView::scrollableTextView(mtm);
    let report = scroll
        .documentView()
        .and_then(|view| view.downcast::<NSTextView>().ok())
        .expect("a scrollable text view contains a text view");
    report.setEditable(false);
    report.setSelectable(true);
    report.setRichText(false);
    // SAFETY: a framework constant, valid for the process lifetime.
    report.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(
        NSFont::smallSystemFontSize(),
        unsafe { NSFontWeightRegular },
    )));
    report.setAccessibilityLabel(Some(&ui::ns("Setup report")));
    scroll.setBorderType(NSBorderType::BezelBorder);
    ui::fixed_width(&scroll, BODY);
    ui::fixed_height(&scroll, REPORT_HEIGHT);
    scroll.setHidden(true);
    (scroll, report)
}

/// The sheet that saves an API key: its ID, the masked key, Cancel and Save.
fn key_sheet(mtm: MainThreadMarker, owner: &RecordingsWindow) -> KeySheet {
    let target: &AnyObject = owner;
    let width = 360.0;
    let title = ui::label(mtm, "Save an API Key");
    title.setFont(Some(
        &NSFont::boldSystemFontOfSize(NSFont::systemFontSize()),
    ));
    let explanation = ui::note(mtm, model::STORE_HELP, width + 100.0);
    let id = ui::field(mtm, "openai", width);
    let secret = NSSecureTextField::new(mtm);
    ui::fixed_width(&secret, width);
    // SAFETY: the window clears these delegates before it is released.
    unsafe {
        id.setDelegate(Some(ProtocolObject::from_ref(owner)));
        secret.setDelegate(Some(ProtocolObject::from_ref(owner)));
    }
    let form = ui::form(mtm, &[(Some("Key ID:"), &id), (Some("API key:"), &secret)]);
    let cancel = ui::button(mtm, "Cancel", target, sel!(cancelKey:));
    cancel.setKeyEquivalent(&ui::ns("\u{1b}"));
    let save = ui::button(mtm, "Save in Keychain", target, sel!(saveKey:));
    save.setKeyEquivalent(&ui::ns("\r"));
    let buttons = NSStackView::new(mtm);
    buttons.addView_inGravity(&cancel, NSStackViewGravity::Trailing);
    buttons.addView_inGravity(&save, NSStackViewGravity::Trailing);
    ui::fixed_width(&buttons, width + 100.0);
    let content = ui::column(mtm, &[&title, &explanation, &form, &buttons], 12.0);
    content.setEdgeInsets(NSEdgeInsets {
        top: 20.0,
        left: INSET,
        bottom: 20.0,
        right: INSET,
    });
    // SAFETY: created on the main thread; `KeySheet` owns the only reference,
    // so AppKit must not also release it on close.
    let window = unsafe {
        let window = NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            NSRect::new(
                NSPoint::ZERO,
                NSSize::new(width + 100.0 + 2.0 * INSET, 220.0),
            ),
            NSWindowStyleMask::Titled,
            NSBackingStoreType::Buffered,
            true,
        );
        window.setReleasedWhenClosed(false);
        window
    };
    window.setContentView(Some(&content));
    KeySheet {
        window,
        id,
        secret,
        save,
    }
}

/// The Forget confirmation's details: which take, and what Forget removes.
fn forget_details(take: &Take) -> String {
    format!(
        "{}\n{} · Recording ID: {}\n\n{}",
        take_time(take.created_at_unix_ms),
        take_duration(take.duration_ms),
        take.id,
        model::FORGET
    )
}

/// `text` with its first letter capitalized, for a table cell.
fn sentence(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// A handoff target's theme color.
fn rgb([red, green, blue]: [u8; 3]) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(red) / 255.0,
        f64::from(green) / 255.0,
        f64::from(blue) / 255.0,
        1.0,
    )
}

/// Render Recordings and Recovery offscreen to a PNG at `path`, with one
/// observation read; with `doctor`, the Setup pane after Check Setup ran. No
/// window is ordered on screen and the process never activates.
pub(crate) fn screenshot(path: &Path, doctor: bool) -> Result<()> {
    let mtm = MainThreadMarker::new().context("Recordings must render on the main thread")?;
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let window = RecordingsWindow::new(mtm);
    let observed = {
        let mut state = window.state_mut();
        let revision = state.recordings.poll(Instant::now()).flatten();
        let observed = state.recordings.observed(effects::observe(revision));
        if doctor {
            if let Some(job) = state.recordings.check_setup() {
                let result = effects::perform(job.action);
                let _ = state.recordings.finished(job.lane, result);
            }
            state.pane = Pane::Setup;
        }
        let status = ipc::status().ok();
        state.live = status
            .as_ref()
            .map_or_else(|| "Cantrip is not running".to_owned(), super::status_line);
        state.online = status.is_some();
        observed
    };
    let pane = window.state().pane;
    window.show_pane(pane, false);
    window.render(Some(observed));
    // The frame view draws the window background and toolbar behind the pane.
    let frame = window
        .views()
        .window
        .contentView()
        // SAFETY: the window retains its frame view for as long as it exists.
        .and_then(|content| unsafe { content.superview() })
        .context("the Recordings window has no frame view")?;
    ui::save_png(&frame, path)?;
    eprintln!("saved screenshot to {}", path.display());
    window.teardown();
    Ok(())
}
