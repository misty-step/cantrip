//! Evidence gallery: renders the *real* popover, Settings and HUD code with a
//! fixture snapshot in an ordinary window and saves a PNG of the painted
//! frame. The theme override exists only here; the app follows the system.
//!
//! `cantrip gallery --surface popover|settings|hud --theme light|dark --out <png> [--scale 2]`

use super::{hud, popover, settings, theme};
use crate::platform::raw_window::Raw;
use crate::platform::{SurfaceKind, UiPlatform};
use cantrip_core::app::{Command, Control, KeyState, UiSnapshot};
use cantrip_core::shortcut::Chord;
use cantrip_core::speech::Mode;
use cantrip_core::surface::{CopyReason, Hud, RecoveryKind, RecoveryRow};
use cantrip_core::vocabulary;
use eframe::egui::{self, Pos2, Vec2, ViewportBuilder, ViewportCommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A `Control` over an in-memory snapshot; applies simple commands so the
/// gallery (and UI tests) can be driven interactively.
pub struct FakeControl {
    pub snap: Mutex<UiSnapshot>,
    pub sent: Mutex<Vec<Command>>,
}

impl FakeControl {
    pub fn new(snap: UiSnapshot) -> Self {
        FakeControl {
            snap: Mutex::new(snap),
            sent: Mutex::new(Vec::new()),
        }
    }
}

impl Control for FakeControl {
    fn snapshot(&self) -> UiSnapshot {
        self.snap.lock().unwrap().clone()
    }

    fn send(&self, cmd: Command) {
        {
            let mut s = self.snap.lock().unwrap();
            match &cmd {
                Command::SetMode(m) => s.mode = *m,
                Command::SetPostProcessing(on) => s.post_processing = *on,
                Command::AddTerm(t) => {
                    vocabulary::add_term(&mut s.vocabulary, t);
                }
                Command::RemoveTerm(t) => {
                    vocabulary::remove_term(&mut s.vocabulary, t);
                }
                Command::SetShortcut(c) => s.shortcut = c.clone(),
                Command::SetMicrophone(m) => s.microphone = m.clone(),
                Command::SaveKey(_) => s.key = KeyState::Saved,
                Command::RemoveKey => s.key = KeyState::NotSet,
                Command::ChooseMeetingFolder(p) => s.meeting_folder = p.clone(),
                Command::SetOpenAtLogin(on) => s.open_at_login = *on,
                Command::ToggleDictation => s.dictating = !s.dictating,
                Command::ToggleMeeting => s.meeting_active = !s.meeting_active,
                Command::Recovery { .. } => s.recovery = None,
                _ => {}
            }
        }
        self.sent.lock().unwrap().push(cmd);
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/user".into())
}

/// The fixture snapshot used for evidence screenshots.
pub fn fixture() -> UiSnapshot {
    UiSnapshot {
        dictating: false,
        meeting_active: false,
        mode: Mode::Clean,
        post_processing: true,
        has_last_dictation: true,
        recovery: Some(RecoveryRow::for_item(
            "d1",
            &RecoveryKind::DictationNotDelivered,
            "10:42",
        )),
        status: "Idle".into(),
        hud: Hud::Hidden,
        level: 0.0,
        shortcut: Chord::platform_default(),
        microphone: None,
        microphones: vec![
            (
                "alsa_input.usb-headset".into(),
                "USB Headset Microphone".into(),
            ),
            (
                "alsa_input.pci-analog".into(),
                "Built-in Audio Analog Stereo".into(),
            ),
        ],
        key: KeyState::Saved,
        vocabulary: vocabulary::seed(),
        meeting_folder: home().join("supercortex/captures/voice"),
        folder_error: None,
        open_at_login: false,
        login_error: None,
        key_error: None,
    }
}

/// Every HUD state the gallery shows, in order, with the level used.
pub fn hud_states() -> Vec<(Hud, f32)> {
    vec![
        (Hud::Listening, 0.22),
        (Hud::Transcribing, 0.0),
        (Hud::Inserted, 0.0),
        (Hud::SentUnconfirmed, 0.0),
        (Hud::PostedUnconfirmed { on_clipboard: true }, 0.0),
        (
            Hud::Copied {
                reason: CopyReason::FocusChanged,
                on_clipboard: true,
            },
            0.0,
        ),
        (
            Hud::Copied {
                reason: CopyReason::TargetUncertain,
                on_clipboard: false,
            },
            0.0,
        ),
        (
            Hud::Copied {
                reason: CopyReason::PasteUnsupported,
                on_clipboard: true,
            },
            0.0,
        ),
        (Hud::RawCleanupSkipped, 0.0),
        (Hud::OnDevice, 0.0),
        (
            Hud::NotTranscribed(cantrip_core::offline::FIX_NO_ON_DEVICE.to_string()),
            0.0,
        ),
        (Hud::NotDelivered, 0.0),
        (Hud::MicrophoneBlocked, 0.0),
        (Hud::Meeting { elapsed_secs: 724 }, 0.0),
    ]
}

/// A UiPlatform for the gallery: fontconfig fonts, fixed theme, no window
/// patching, no folder picker.
pub struct GalleryPlatform {
    pub dark: bool,
}

/// Resolve a fontconfig pattern to font bytes (Linux); `None` elsewhere.
pub fn fc_font(pattern: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", pattern])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    std::fs::read(path.trim()).ok()
}

impl UiPlatform for GalleryPlatform {
    fn configure_window(&self, _kind: SurfaceKind, _raw: Raw) {}
    fn show_hud(&self, _raw: Raw, _visible: bool) {}
    fn hud_anchor(&self) -> Option<(f32, f32)> {
        None
    }
    fn prefers_dark(&self) -> bool {
        self.dark
    }
    fn system_fonts(&self) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        (fc_font("sans-serif"), fc_font("monospace"))
    }
    fn choose_folder(&self, _start: &Path) -> Result<Option<PathBuf>, String> {
        Err("No folder picker on this session".into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Surface {
    Popover,
    Settings,
    Hud,
}

struct Args {
    surface: Surface,
    dark: bool,
    out: PathBuf,
    scale: f32,
}

fn parse(args: &[String]) -> anyhow::Result<Args> {
    let mut a = Args {
        surface: Surface::Popover,
        dark: false,
        out: "gallery.png".into(),
        scale: 2.0,
    };
    let mut it = args.iter();
    while let Some(k) = it.next() {
        let mut v = || {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{k} needs a value"))
        };
        match k.as_str() {
            "--surface" => {
                a.surface = match v()?.as_str() {
                    "popover" => Surface::Popover,
                    "settings" => Surface::Settings,
                    "hud" => Surface::Hud,
                    s => anyhow::bail!("unknown surface {s}"),
                }
            }
            "--theme" => a.dark = v()? == "dark",
            "--out" => a.out = v()?.into(),
            "--scale" => a.scale = v()?.parse()?,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

/// Run the gallery; blocks until the PNG is written.
///
/// `--surface live` instead runs the real windowed shell ([`super::run`]) with
/// the fixture: the HUD pill listening, then the popover and Settings opened
/// through `UiRequest`s, for display-level capture (`import -window root`).
pub fn run_gallery(args: &[String]) -> anyhow::Result<()> {
    if args
        .windows(2)
        .any(|w| w[0] == "--surface" && w[1] == "live")
    {
        return run_live(args.windows(2).any(|w| w[0] == "--theme" && w[1] == "dark"));
    }
    let args = parse(args)?;
    let platform = GalleryPlatform { dark: args.dark };
    let control = Arc::new(FakeControl::new(fixture()));
    let initial = match args.surface {
        Surface::Popover => Vec2::new(popover::WIDTH + 48.0, 420.0),
        Surface::Settings => Vec2::new(settings::WIDTH, 600.0),
        Surface::Hud => Vec2::new(560.0, 640.0),
    };
    let viewport = ViewportBuilder::default()
        .with_title("Cantrip gallery")
        .with_inner_size(initial * args.scale)
        .with_resizable(false);
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        persist_window: false,
        ..Default::default()
    };
    let saved: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
    let saved2 = saved.clone();
    eframe::run_native(
        "cantrip-gallery",
        options,
        Box::new(move |cc| {
            let (sans, mono) = platform.system_fonts();
            theme::install_fonts(&cc.egui_ctx, sans, mono);
            theme::apply(&cc.egui_ctx, &theme::palette(args.dark));
            cc.egui_ctx.set_zoom_factor(args.scale);
            Ok(Box::new(Gallery {
                surface: args.surface,
                dark: args.dark,
                out: args.out.clone(),
                scale: args.scale,
                control,
                platform,
                frame: 0,
                size: Vec2::ZERO,
                requested: false,
                saved: saved2,
                popover_state: Default::default(),
                settings_state: Default::default(),
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("gallery event loop failed: {e}"))?;
    let result = saved.lock().unwrap().take();
    result.unwrap_or_else(|| {
        Err(anyhow::anyhow!(
            "gallery closed before the screenshot was taken"
        ))
    })
}

fn run_live(dark: bool) -> anyhow::Result<()> {
    use crate::platform::UiRequest;
    let mut snap = fixture();
    snap.hud = Hud::Listening;
    snap.dictating = true;
    snap.status = "Listening".into();
    let control = Arc::new(FakeControl::new(snap));
    let (tx, rx) = std::sync::mpsc::channel();
    let c2 = control.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let mut sent = (false, false);
        loop {
            let t = start.elapsed().as_secs_f32();
            c2.snap.lock().unwrap().level = 0.12 + 0.1 * (t * 5.0).sin().abs();
            if t > 0.6 && !sent.0 {
                let _ = tx.send(UiRequest::ShowPopover);
                sent.0 = true;
            }
            if t > 1.2 && !sent.1 {
                let _ = tx.send(UiRequest::ShowSettings);
                sent.1 = true;
            }
            super::wake();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    super::run(control, Arc::new(GalleryPlatform { dark }), rx)
}

struct Gallery {
    surface: Surface,
    dark: bool,
    out: PathBuf,
    scale: f32,
    control: Arc<FakeControl>,
    platform: GalleryPlatform,
    frame: u32,
    size: Vec2,
    requested: bool,
    saved: Arc<Mutex<Option<anyhow::Result<()>>>>,
    popover_state: popover::PopoverState,
    settings_state: settings::SettingsState,
}

impl eframe::App for Gallery {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = theme::palette(self.dark);
        let snap = self.control.snapshot();
        // Backdrop: a neutral desktop-like surface so the window edges show.
        let backdrop = if self.dark {
            egui::Color32::from_rgb(0x12, 0x12, 0x14)
        } else {
            egui::Color32::from_rgb(0xD8, 0xD8, 0xDC)
        };
        let full = ui.max_rect();
        let content = match self.surface {
            Surface::Popover => {
                ui.painter().rect_filled(full, 0, backdrop);
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(full.shrink(24.0)));
                let r = egui::Frame::new()
                    .fill(p.bg)
                    .stroke(egui::Stroke::new(1.0, p.hairline))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(6, 8))
                    .show(&mut child, |ui| {
                        ui.set_width(popover::WIDTH - 14.0);
                        for a in popover::show(ui, &p, &snap, &mut self.popover_state) {
                            if let popover::Action::Send(c) = a {
                                self.control.send(c);
                            }
                        }
                    })
                    .response
                    .rect;
                Vec2::new(r.right() + 24.0, r.bottom() + 24.0)
            }
            Surface::Settings => {
                ui.painter().rect_filled(full, 0, p.bg);
                let r = egui::Frame::new()
                    .show(ui, |ui| {
                        ui.set_width(settings::WIDTH);
                        for c in
                            settings::show(ui, &p, &snap, &mut self.settings_state, &self.platform)
                        {
                            self.control.send(c);
                        }
                    })
                    .response
                    .rect;
                Vec2::new(settings::WIDTH, r.bottom())
            }
            Surface::Hud => {
                ui.painter().rect_filled(full, 0, backdrop);
                let mut y = 24.0;
                let mut w: f32 = 0.0;
                for (state, level) in hud_states() {
                    let r = hud::paint_pill(ui, &p, Pos2::new(24.0, y), &state, level);
                    w = w.max(r.right());
                    y = r.bottom() + 12.0;
                }
                Vec2::new(w + 24.0, y + 12.0)
            }
        };
        self.frame += 1;
        let content = Vec2::new(content.x.ceil(), content.y.ceil());
        if (content - self.size).length() > 0.5 {
            self.size = content;
            ctx.send_viewport_cmd(ViewportCommand::InnerSize(content));
        }
        if self.frame >= 8 && !self.requested {
            self.requested = true;
            let out = self.out.clone();
            let saved = self.saved.clone();
            let ctx2 = ctx.clone();
            let _ = self.scale;
            ctx.request_screenshot(move |img| {
                let r = save_png(&img, &out);
                *saved.lock().unwrap() = Some(r);
                ctx2.send_viewport_cmd(ViewportCommand::Close);
            });
        }
        ctx.request_repaint();
    }
}

fn save_png(img: &egui::ColorImage, out: &Path) -> anyhow::Result<()> {
    let [w, h] = img.size;
    let mut buf = Vec::with_capacity(w * h * 4);
    for c in &img.pixels {
        buf.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    let image = image::RgbaImage::from_raw(w as u32, h as u32, buf)
        .ok_or_else(|| anyhow::anyhow!("bad image size"))?;
    image.save(out)?;
    Ok(())
}
