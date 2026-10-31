//! AppKit building blocks for the menu-bar app's windows: standard controls in
//! the system font and semantic colors, two-column forms, inline messages,
//! presenting a window from an agent app, the hidden menu that carries the
//! standard editing shortcuts, and offscreen PNG rendering for verification.

use anyhow::{Context, Result};
use objc2::runtime::Sel;
use objc2::{rc::Retained, runtime::AnyObject, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBitmapImageFileType, NSBox, NSBoxType, NSButton, NSColor, NSControlSize,
    NSFont, NSGridCell, NSGridCellPlacement, NSGridRowAlignment, NSGridView, NSImage, NSImageView,
    NSLayoutAttribute, NSLayoutConstraintOrientation, NSLineBreakMode, NSMenu, NSMenuItem,
    NSPopUpButton, NSStackView, NSTextField, NSUserInterfaceLayoutOrientation, NSView, NSWindow,
};
use objc2_foundation::{NSArray, NSDictionary, NSRect, NSString};
use std::{fs, path::Path};

/// Space between a form's label column and its controls, and between rows.
const FORM_COLUMN_SPACING: f64 = 8.0;
const FORM_ROW_SPACING: f64 = 10.0;

pub(crate) fn ns(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

/// A one-line label in the system font.
pub(crate) fn label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    NSTextField::labelWithString(&ns(text), mtm)
}

/// A label that truncates at its end instead of growing its container.
pub(crate) fn truncating_label(mtm: MainThreadMarker, text: &str) -> Retained<NSTextField> {
    let label = label(mtm, text);
    label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    label.setMaximumNumberOfLines(1);
    label.setContentCompressionResistancePriority_forOrientation(
        1.0,
        NSLayoutConstraintOrientation::Horizontal,
    );
    label
}

/// Secondary explanatory text in the small system font, wrapping at `width`.
pub(crate) fn note(mtm: MainThreadMarker, text: &str, width: f64) -> Retained<NSTextField> {
    let note = NSTextField::wrappingLabelWithString(&ns(text), mtm);
    note.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    note.setPreferredMaxLayoutWidth(width);
    fixed_width(&note, width);
    note
}

/// An editable single-line text field `width` points wide.
pub(crate) fn field(mtm: MainThreadMarker, placeholder: &str, width: f64) -> Retained<NSTextField> {
    let field = NSTextField::textFieldWithString(&NSString::new(), mtm);
    if !placeholder.is_empty() {
        field.setPlaceholderString(Some(&ns(placeholder)));
    }
    fixed_width(&field, width);
    field
}

pub(crate) fn button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSButton> {
    // SAFETY: every action this module is given is a method its target defines.
    unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(action), mtm) }
}

/// A push button in the small control size, for actions inside a list row.
pub(crate) fn small_button(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSButton> {
    let button = button(mtm, title, target, action);
    button.setControlSize(NSControlSize::Small);
    button.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    button
}

pub(crate) fn checkbox(
    mtm: MainThreadMarker,
    title: &str,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSButton> {
    // SAFETY: as for `button`.
    unsafe {
        NSButton::checkboxWithTitle_target_action(&ns(title), Some(target), Some(action), mtm)
    }
}

/// A pop-up menu whose items this window enables itself.
pub(crate) fn popup(
    mtm: MainThreadMarker,
    target: &AnyObject,
    action: Sel,
) -> Retained<NSPopUpButton> {
    let popup =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
    popup.setAutoenablesItems(false);
    // SAFETY: as for `button`.
    unsafe {
        popup.setTarget(Some(target));
        popup.setAction(Some(action));
    }
    popup
}

/// Replace a pop-up's items with `items` (title, enabled) and select `selected`.
pub(crate) fn fill_popup<T: AsRef<str>>(
    popup: &NSPopUpButton,
    items: &[(T, bool)],
    selected: usize,
) {
    popup.removeAllItems();
    for (index, (title, enabled)) in items.iter().enumerate() {
        popup.addItemWithTitle(&ns(title.as_ref()));
        if let Some(item) = popup.itemAtIndex(index as isize) {
            item.setEnabled(*enabled);
        }
    }
    popup.selectItemAtIndex(selected as isize);
}

/// The selected item's index, if any.
pub(crate) fn selected(popup: &NSPopUpButton) -> Option<usize> {
    usize::try_from(popup.indexOfSelectedItem()).ok()
}

pub(crate) fn text_of(field: &NSTextField) -> String {
    field.stringValue().to_string()
}

pub(crate) fn set_text(field: &NSTextField, text: &str) {
    field.setStringValue(&ns(text));
}

pub(crate) fn set_checked(button: &NSButton, checked: bool) {
    button.setState(isize::from(checked));
}

pub(crate) fn checked(button: &NSButton) -> bool {
    button.state() != 0
}

/// Views stacked top to bottom, aligned at their leading edges.
pub(crate) fn column(
    mtm: MainThreadMarker,
    views: &[&NSView],
    spacing: f64,
) -> Retained<NSStackView> {
    let stack = stack(mtm, views, spacing);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack
}

/// Views side by side, centred on one line.
pub(crate) fn row(mtm: MainThreadMarker, views: &[&NSView], spacing: f64) -> Retained<NSStackView> {
    let stack = stack(mtm, views, spacing);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Horizontal);
    stack.setAlignment(NSLayoutAttribute::CenterY);
    stack
}

fn stack(mtm: MainThreadMarker, views: &[&NSView], spacing: f64) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    stack.setSpacing(spacing);
    stack.setDetachesHiddenViews(true);
    stack
}

/// A form in the macOS layout: trailing-aligned labels ending in a colon, and
/// controls aligned on one leading edge. A row without a label continues the
/// row above it, such as a note under its control.
pub(crate) fn form(
    mtm: MainThreadMarker,
    rows: &[(Option<&str>, &NSView)],
) -> Retained<NSGridView> {
    let empty = NSGridCell::emptyContentView(mtm);
    let labels: Vec<Retained<NSView>> = rows
        .iter()
        .map(|(title, _)| match title {
            Some(title) => Retained::into_super(Retained::into_super(label(mtm, title))),
            None => empty.clone(),
        })
        .collect();
    let rows: Vec<Retained<NSArray<NSView>>> = rows
        .iter()
        .zip(&labels)
        .map(|((_, control), label)| NSArray::from_slice(&[&**label, *control]))
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    grid.setColumnSpacing(FORM_COLUMN_SPACING);
    grid.setRowSpacing(FORM_ROW_SPACING);
    grid.columnAtIndex(0)
        .setXPlacement(NSGridCellPlacement::Trailing);
    grid.columnAtIndex(1)
        .setXPlacement(NSGridCellPlacement::Leading);
    grid
}

/// Show or hide `view`, and its whole row when it sits in a form, so a hidden
/// line leaves no gap.
pub(crate) fn show_in_form(view: &NSView, shown: bool) {
    view.setHidden(!shown);
    // SAFETY: an attached view's superview outlives this call.
    let grid = unsafe { view.superview() }.and_then(|view| view.downcast::<NSGridView>().ok());
    if let Some(row) = grid
        .and_then(|grid| grid.cellForView(view))
        .and_then(|cell| cell.row())
    {
        row.setHidden(!shown);
    }
}

pub(crate) fn fixed_width(view: &NSView, width: f64) {
    view.widthAnchor()
        .constraintEqualToConstant(width)
        .setActive(true);
}

pub(crate) fn fixed_height(view: &NSView, height: f64) {
    view.heightAnchor()
        .constraintEqualToConstant(height)
        .setActive(true);
}

/// A horizontal hairline between regions of a window.
pub(crate) fn separator(mtm: MainThreadMarker) -> Retained<NSBox> {
    let line = NSBox::new(mtm);
    line.setBoxType(NSBoxType::Separator);
    line
}

/// An SF Symbol, when this macOS has it.
pub(crate) fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns(name), Some(&ns(description)))
}

/// One inline status line under a control. Attention adds the system warning
/// symbol, so color never carries the meaning alone; the text keeps the label
/// color for contrast in both appearances.
pub(crate) struct Message {
    pub(crate) view: Retained<NSStackView>,
    icon: Option<Retained<NSImageView>>,
    text: Retained<NSTextField>,
}

impl Message {
    pub(crate) fn new(mtm: MainThreadMarker, width: f64) -> Self {
        let text = NSTextField::wrappingLabelWithString(&NSString::new(), mtm);
        text.setFont(Some(&NSFont::systemFontOfSize(
            NSFont::smallSystemFontSize(),
        )));
        let icon = symbol("exclamationmark.triangle.fill", "Needs attention")
            .map(|image| NSImageView::imageViewWithImage(&image, mtm));
        let text_width = width - icon.as_ref().map_or(0.0, |_| 20.0);
        text.setPreferredMaxLayoutWidth(text_width);
        fixed_width(&text, text_width);
        let view = match &icon {
            Some(icon) => {
                icon.setContentTintColor(Some(&NSColor::systemOrangeColor()));
                row(mtm, &[icon, &text], 4.0)
            }
            None => row(mtm, &[&text], 4.0),
        };
        view.setAlignment(NSLayoutAttribute::Top);
        view.setHidden(true);
        Self { view, icon, text }
    }

    /// Show `text`, or hide the line when there is none. A line inside a form
    /// hides its whole row, so the form keeps its spacing.
    pub(crate) fn show(&self, text: Option<&str>, attention: bool) {
        show_in_form(&self.view, text.is_some());
        let Some(text) = text else {
            return;
        };
        set_text(&self.text, text);
        let color = if attention {
            NSColor::labelColor()
        } else {
            NSColor::secondaryLabelColor()
        };
        self.text.setTextColor(Some(&color));
        if let Some(icon) = &self.icon {
            icon.setHidden(!attention);
        }
    }
}

/// Bring `window` in front of other apps with keyboard focus. An agent app has
/// no Dock icon to click, so a window the operator asked for must make Cantrip
/// the active app. macOS 14 and later activate cooperatively and ignore a
/// request from the command that asked; the forced activation the operator's
/// request stands for still makes the window key there.
pub(crate) fn present(window: &NSWindow) {
    let app = NSApplication::sharedApplication(window.mtm());
    if objc2::available!(macos = 14.0) {
        app.activate();
    }
    // Deprecated since macOS 14, but the only call that still takes focus from
    // the Terminal that ran `cantrip settings`; cooperative `activate` alone
    // leaves the window in front without keyboard focus.
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.makeKeyAndOrderFront(None);
    window.orderFrontRegardless();
}

/// The menu bar never shows for an agent app, but AppKit still routes key
/// equivalents through its main menu: this gives the windows' text fields
/// Undo, Cut, Copy, Paste and Select All, and their windows Close and Minimize.
pub(crate) fn install_main_menu(mtm: MainThreadMarker) {
    let main = NSMenu::new(mtm);
    let edit = submenu(&main, "Edit");
    item(&edit, "Undo", sel!(undo:), "z");
    item(&edit, "Redo", sel!(redo:), "Z");
    edit.addItem(&NSMenuItem::separatorItem(mtm));
    item(&edit, "Cut", sel!(cut:), "x");
    item(&edit, "Copy", sel!(copy:), "c");
    item(&edit, "Paste", sel!(paste:), "v");
    item(&edit, "Select All", sel!(selectAll:), "a");
    let window = submenu(&main, "Window");
    item(&window, "Close", sel!(performClose:), "w");
    item(&window, "Minimize", sel!(performMiniaturize:), "m");
    NSApplication::sharedApplication(mtm).setMainMenu(Some(&main));
}

fn submenu(main: &NSMenu, title: &str) -> Retained<NSMenu> {
    let mtm = main.mtm();
    let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &ns(title));
    let holder = NSMenuItem::new(mtm);
    holder.setSubmenu(Some(&menu));
    main.addItem(&holder);
    menu
}

fn item(menu: &NSMenu, title: &str, action: Sel, key: &str) {
    // SAFETY: each action is a standard responder-chain message with no target.
    unsafe { menu.addItemWithTitle_action_keyEquivalent(&ns(title), Some(action), &ns(key)) };
}

/// Draw `view` through AppKit into an owned bitmap at its window's backing
/// scale and save it as a PNG. Nothing is read from the display.
pub(crate) fn save_png(view: &NSView, path: &Path) -> Result<()> {
    view.layoutSubtreeIfNeeded();
    let bounds = view.bounds();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .context("allocating the offscreen window bitmap")?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // SAFETY: PNG takes no required properties; an empty dictionary is valid.
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .context("encoding the offscreen window as PNG")?;
    fs::write(path, png.to_vec()).with_context(|| format!("writing {}", path.display()))
}
