//! The egui UI: popover, single-pane Settings and the HUD pill, one codebase
//! for macOS and Linux. A pure view over `cantrip_core::app::Control`: each
//! frame reads one snapshot and turns input into commands.
//!
//! Window model: the root viewport *is* the HUD pill (transparent, undecorated,
//! click-through, never focused, hidden while `Hud::Hidden`). The popover and
//! the Settings window are immediate child viewports opened by tray / launcher
//! requests. While the root is hidden eframe runs only `App::logic`; opening a
//! child asks for a painted pass with `RequestPaintWhileHidden`.

pub mod gallery;
pub mod hud;
pub mod popover;
pub mod settings;
pub mod theme;
pub mod widgets;

use crate::platform::raw_window::Raw;
use crate::platform::{UiPlatform, UiRequest};
use cantrip_core::app::{Command, Control, UiSnapshot};
use cantrip_core::surface::Hud;
use eframe::egui::{self, Pos2, Vec2, ViewportBuilder, ViewportCommand, ViewportId};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

static UI_CTX: OnceLock<egui::Context> = OnceLock::new();

/// Wake the UI from any thread (wire into the core's `notify`), so state
/// changes such as the HUD appearing repaint at once instead of on the next
/// idle poll.
pub fn wake() {
    if let Some(ctx) = UI_CTX.get() {
        ctx.request_repaint();
    }
}

/// Idle poll interval for tray / launcher requests when nothing is shown.
const IDLE_POLL: Duration = Duration::from_millis(200);
/// Repaint cadence while the HUD is animating (meter, timer, spinner).
const LIVE_POLL: Duration = Duration::from_millis(50);
const THEME_POLL: Duration = Duration::from_secs(2);
/// Distance of the HUD's bottom edge above the anchor (display bottom-centre).
const HUD_BOTTOM_MARGIN: f32 = 72.0;

/// Called once per UI pass on the main thread (hotkey re-registration,
/// Escape grab, tray state — platform work that must stay on the main thread).
pub type Tick = Box<dyn FnMut()>;

/// Run the UI event loop on the main thread until Quit.
/// `requests` carries tray / launcher / IPC activations.
pub fn run(
    control: Arc<dyn Control>,
    platform: Arc<dyn UiPlatform>,
    requests: Receiver<UiRequest>,
    tick: Tick,
) -> anyhow::Result<()> {
    let hud_viewport = ViewportBuilder::default()
        .with_title("Cantrip HUD")
        .with_app_id("cantrip")
        .with_inner_size(Vec2::new(160.0, hud::PILL_H))
        .with_decorations(false)
        .with_transparent(true)
        .with_resizable(false)
        .with_always_on_top()
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_active(false)
        .with_has_shadow(false)
        .with_window_type(egui::X11WindowType::Notification)
        .with_override_redirect(true)
        .with_visible(false);
    let mut options = eframe::NativeOptions {
        viewport: hud_viewport,
        renderer: eframe::Renderer::Glow,
        centered: false,
        persist_window: false,
        ..Default::default()
    };
    crate::platform::event_loop_tweaks(&mut options);
    eframe::run_native(
        "cantrip",
        options,
        Box::new(move |cc| {
            let _ = UI_CTX.set(cc.egui_ctx.clone());
            let (sans, mono) = platform.system_fonts();
            theme::install_fonts(&cc.egui_ctx, sans, mono);
            let dark = platform.prefers_dark();
            theme::apply(&cc.egui_ctx, &theme::palette(dark));
            let raw = raw_of(cc.window_handle().ok().map(|h| h.as_raw()));
            platform.configure_hud(raw);
            Ok(Box::new(Shell::new(
                control, platform, requests, raw, dark, tick,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("UI event loop failed: {e}"))
}

#[allow(clippy::unnecessary_cast)] // Xlib window is c_ulong
fn raw_of(h: Option<RawWindowHandle>) -> Raw {
    match h {
        Some(RawWindowHandle::Xlib(h)) => Raw::X11(h.window as u64),
        Some(RawWindowHandle::Xcb(h)) => Raw::X11(h.window.get() as u64),
        Some(RawWindowHandle::Wayland(h)) => Raw::Wayland(h.surface.as_ptr() as usize),
        Some(RawWindowHandle::AppKit(h)) => Raw::AppKit(h.ns_view.as_ptr() as usize),
        _ => Raw::Unknown,
    }
}

struct Shell {
    control: Arc<dyn Control>,
    platform: Arc<dyn UiPlatform>,
    requests: Receiver<UiRequest>,
    raw: Raw,
    dark: bool,
    theme_checked: Instant,
    snap: Option<UiSnapshot>,
    /// What the HUD window currently shows (`Hidden` = unmapped).
    hud_shown: Hud,
    hud_size: Vec2,
    popover: Child<popover::PopoverState>,
    settings: Child<settings::SettingsState>,
    quitting: bool,
    tick: Tick,
}

/// One child window's open/focus bookkeeping.
#[derive(Default)]
struct Child<S> {
    open: bool,
    just_opened: bool,
    focused_once: bool,
    height: f32,
    state: S,
}

impl<S: Default> Child<S> {
    fn open(&mut self) {
        if !self.open {
            self.open = true;
            self.just_opened = true;
            self.focused_once = false;
        } else {
            self.just_opened = true; // re-raise
        }
    }
    fn close(&mut self) {
        self.open = false;
        self.state = S::default();
    }
}

impl Shell {
    fn new(
        control: Arc<dyn Control>,
        platform: Arc<dyn UiPlatform>,
        requests: Receiver<UiRequest>,
        raw: Raw,
        dark: bool,
        tick: Tick,
    ) -> Self {
        Shell {
            control,
            platform,
            requests,
            raw,
            dark,
            theme_checked: Instant::now(),
            snap: None,
            hud_shown: Hud::Hidden,
            hud_size: Vec2::new(160.0, hud::PILL_H),
            popover: Child {
                height: 300.0,
                ..Default::default()
            },
            settings: Child {
                height: 520.0,
                ..Default::default()
            },
            quitting: false,
            tick,
        }
    }

    fn palette(&self) -> theme::Palette {
        theme::palette(self.dark)
    }

    fn handle(&mut self, ctx: &egui::Context, cmd: Command) {
        let quit = cmd == Command::Quit;
        self.control.send(cmd);
        if quit {
            self.quitting = true;
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
        }
    }

    /// Show, resize, move or hide the HUD window to match `hud`.
    fn sync_hud(&mut self, ctx: &egui::Context, hud: &Hud) {
        if *hud == Hud::Hidden {
            if self.hud_shown != Hud::Hidden {
                ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
                self.platform.show_hud(self.raw, false);
                self.hud_shown = Hud::Hidden;
            }
            return;
        }
        let size = hud::pill_size(ctx, hud);
        let was_hidden = self.hud_shown == Hud::Hidden;
        if was_hidden || size != self.hud_size {
            self.hud_size = size;
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::InnerSize(size));
            let (ax, ay) = self.platform.hud_anchor().unwrap_or_else(|| {
                let m = ctx
                    .input(|i| i.viewport().monitor_size)
                    .unwrap_or(Vec2::new(1280.0, 800.0));
                (m.x / 2.0, m.y)
            });
            let pos = Pos2::new(
                (ax - size.x / 2.0).round(),
                (ay - HUD_BOTTOM_MARGIN - size.y).round(),
            );
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::OuterPosition(pos));
        }
        if was_hidden {
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
            self.platform.show_hud(self.raw, true);
        }
        self.hud_shown = hud.clone();
    }

    fn show_popover(&mut self, ctx: &egui::Context, snap: &UiSnapshot) {
        let p = self.palette();
        let id = ViewportId::from_hash_of("cantrip-popover");
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or(Vec2::new(1280.0, 800.0));
        let size = Vec2::new(popover::WIDTH, self.popover.height);
        let builder = ViewportBuilder::default()
            .with_title("Cantrip")
            .with_app_id("cantrip")
            .with_inner_size(size)
            .with_position(Pos2::new(monitor.x - popover::WIDTH - 12.0, 36.0))
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(false);
        let just_opened = std::mem::take(&mut self.popover.just_opened);
        let mut actions = Vec::new();
        let mut measured = self.popover.height;
        let mut lost_focus = false;
        let mut focused = None;
        let mut close_req = false;
        let st = &mut self.popover.state;
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            if just_opened {
                ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
            }
            close_req = ui.input(|i| i.viewport().close_requested());
            focused = ui.input(|i| i.viewport().focused);
            lost_focus = focused == Some(false);
            egui::Frame::new()
                .fill(p.bg)
                .stroke(egui::Stroke::new(1.0, p.hairline))
                .inner_margin(egui::Margin::symmetric(6, 8))
                .show(ui, |ui| {
                    ui.set_width(popover::WIDTH - 14.0);
                    actions = popover::show(ui, &p, snap, st);
                    measured = ui.min_rect().height() + 16.0 + 2.0;
                });
            if (measured - size.y).abs() > 0.5 {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
                        popover::WIDTH,
                        measured,
                    )));
            }
        });
        self.popover.height = measured;
        if focused == Some(true) {
            self.popover.focused_once = true;
        }
        for a in actions {
            match a {
                popover::Action::Send(cmd) => self.handle(ctx, cmd),
                popover::Action::OpenSettings => self.settings.open(),
                popover::Action::Close => self.popover.close(),
            }
        }
        if close_req || (lost_focus && self.popover.focused_once) {
            self.popover.close();
        }
    }

    fn show_settings(&mut self, ctx: &egui::Context, snap: &UiSnapshot) {
        let p = self.palette();
        let id = ViewportId::from_hash_of("cantrip-settings");
        let size = Vec2::new(settings::WIDTH, self.settings.height);
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or(Vec2::new(1280.0, 800.0));
        let builder = ViewportBuilder::default()
            .with_title("Cantrip Settings")
            .with_position(Pos2::new(
                ((monitor.x - settings::WIDTH) / 2.0).max(0.0).round(),
                ((monitor.y - self.settings.height) / 3.0).max(0.0).round(),
            ))
            .with_app_id("cantrip")
            .with_inner_size(size)
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false);
        let just_opened = std::mem::take(&mut self.settings.just_opened);
        let platform = self.platform.clone();
        let mut cmds = Vec::new();
        let mut measured = self.settings.height;
        let mut close_req = false;
        let st = &mut self.settings.state;
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            if just_opened {
                ui.ctx().send_viewport_cmd(ViewportCommand::Focus);
            }
            close_req = ui.input(|i| i.viewport().close_requested());
            egui::Frame::new().fill(p.bg).show(ui, |ui| {
                ui.set_width(settings::WIDTH);
                cmds = settings::show(ui, &p, snap, st, platform.as_ref());
                measured = ui.min_rect().height();
            });
            if (measured - size.y).abs() > 0.5 {
                ui.ctx()
                    .send_viewport_cmd(ViewportCommand::InnerSize(Vec2::new(
                        settings::WIDTH,
                        measured,
                    )));
            }
        });
        self.settings.height = measured;
        for c in cmds {
            self.handle(ctx, c);
        }
        if close_req {
            self.settings.close();
        }
    }
}

impl eframe::App for Shell {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        (self.tick)();
        while let Ok(req) = self.requests.try_recv() {
            match req {
                UiRequest::ShowPopover => self.popover.open(),
                UiRequest::ShowSettings => self.settings.open(),
                UiRequest::ToggleDictation => self.control.send(Command::ToggleDictation),
            }
        }
        if self.theme_checked.elapsed() >= THEME_POLL {
            self.theme_checked = Instant::now();
            let dark = self.platform.prefers_dark();
            if dark != self.dark {
                self.dark = dark;
                theme::apply(ctx, &self.palette());
            }
        }
        let snap = self.control.snapshot();
        let hud = if self.quitting {
            Hud::Hidden
        } else {
            snap.hud.clone()
        };
        self.sync_hud(ctx, &hud);
        let live = hud != Hud::Hidden;
        self.snap = Some(snap);
        if self.popover.open || self.settings.open {
            // Child windows are painted from the root's pass, which eframe
            // skips while the HUD (root) is hidden unless asked.
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::RequestPaintWhileHidden);
            ctx.request_repaint_after(LIVE_POLL);
        } else {
            ctx.request_repaint_after(if live { LIVE_POLL } else { IDLE_POLL });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let Some(snap) = self.snap.clone() else {
            return;
        };
        let p = self.palette();
        if self.hud_shown != Hud::Hidden {
            let hud = self.hud_shown.clone();
            let rect = hud::paint_pill(ui, &p, ui.max_rect().min, &hud, snap.level);
            let desc = hud::describe(&hud);
            ui.allocate_rect(rect, egui::Sense::hover())
                .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, &desc));
        }
        if self.popover.open {
            self.show_popover(&ctx, &snap);
        }
        if self.settings.open {
            self.show_settings(&ctx, &snap);
        }
    }
}
