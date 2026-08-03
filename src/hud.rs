//! Always-on-top Wayland layer-shell status HUD.
//!
//! The HUD is a read-only mirror of the daemon. It polls the existing status
//! command and never sends a command which can change daemon state.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::{
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

use crate::ipc::{self, Command, Reply};

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const RESULT_FLASH: Duration = Duration::from_millis(2_500);
const HUD_HEIGHT: u32 = 44;
const FALLBACK_WIDTH: u32 = 420;
const MAX_WIDTH: u32 = 900;
const FONT_SIZE: f32 = 15.0;
const FONT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Run the HUD until the compositor closes it or the display disconnects.
///
/// Display and daemon failures are deliberately non-fatal. The HUD is an
/// optional client and must not affect the daemon's operation.
pub fn run() -> Result<()> {
    tracing::info!("[HUD] connecting to Wayland display");
    let connection = match Connection::connect_to_env() {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!("[HUD] Wayland display unavailable: {error}");
            return Ok(());
        }
    };

    let (globals, mut event_queue) = match registry_queue_init(&connection) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!("[HUD] Wayland registry unavailable: {error}");
            return Ok(());
        }
    };
    let queue_handle = event_queue.handle();

    let compositor = match CompositorState::bind(&globals, &queue_handle) {
        Ok(compositor) => compositor,
        Err(error) => {
            tracing::warn!("[HUD] wl_compositor unavailable: {error}");
            return Ok(());
        }
    };
    let layer_shell = match LayerShell::bind(&globals, &queue_handle) {
        Ok(layer_shell) => layer_shell,
        Err(error) => {
            tracing::warn!("[HUD] layer-shell unavailable: {error}");
            return Ok(());
        }
    };
    let shm = match Shm::bind(&globals, &queue_handle) {
        Ok(shm) => shm,
        Err(error) => {
            tracing::warn!("[HUD] wl_shm unavailable: {error}");
            return Ok(());
        }
    };

    let surface = compositor.create_surface(&queue_handle);
    let layer = layer_shell.create_layer_surface(
        &queue_handle,
        surface,
        Layer::Overlay,
        Some("cantrip-hud"),
        None,
    );
    layer.set_anchor(Anchor::TOP);
    // wlroots rejects a zero width with only the TOP anchor. A fixed width
    // keeps the surface top-centered while allowing the chip to resize inside.
    layer.set_size(FALLBACK_WIDTH, HUD_HEIGHT);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    // Never intercept pointer/touch: an empty input region (no rects added)
    // lets clicks pass through the chip to whatever is underneath.
    let empty_region = Region::new(&compositor).context("creating empty input region")?;
    layer
        .wl_surface()
        .set_input_region(Some(empty_region.wl_region()));
    layer.commit();
    tracing::info!("[HUD] layer surface created (overlay, top-center)");

    // Reserve two normal-sized buffers. SlotPool grows if a compositor keeps a
    // buffer busy longer than one polling interval.
    let pool_size = FALLBACK_WIDTH
        .checked_mul(HUD_HEIGHT)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| bytes.checked_mul(2))
        .context("calculating HUD shared-memory pool size")? as usize;
    let pool = match SlotPool::new(pool_size, &shm) {
        Ok(pool) => pool,
        Err(error) => {
            tracing::warn!("[HUD] cannot create shared-memory pool: {error}");
            return Ok(());
        }
    };

    let mut hud = HudState::new(
        RegistryState::new(&globals),
        OutputState::new(&globals, &queue_handle),
        shm,
        pool,
        layer,
    );
    if let Err(error) = event_queue.roundtrip(&mut hud) {
        tracing::warn!("[HUD] display disconnected during setup: {error}");
        return Ok(());
    }

    while !hud.exit {
        hud.poll_status();
        if let Err(error) = hud.redraw_if_needed() {
            tracing::warn!("[HUD] redraw failed: {error:#}");
        }
        // Read the Wayland socket with a bounded timeout. This services
        // buffer releases and disconnects each pass while keeping the
        // status poll and animation cadence.
        if let Err(error) = timed_dispatch(&mut event_queue, &mut hud, POLL_INTERVAL) {
            tracing::info!("[HUD] Wayland display disconnected; exiting ({error})");
            break;
        }
    }

    tracing::info!("[HUD] stopped");
    Ok(())
}

/// Flush, then wait up to `timeout` for Wayland events and dispatch them.
fn timed_dispatch(
    queue: &mut EventQueue<HudState>,
    data: &mut HudState,
    timeout: Duration,
) -> Result<()> {
    queue.flush()?;
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(data)?;
        return Ok(());
    };
    let mut pollfd = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, timeout.as_millis() as i32) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            tracing::warn!("[HUD] polling Wayland socket failed: {error}");
        }
        queue.dispatch_pending(data)?;
        return Ok(());
    }
    if ready > 0 && pollfd.revents & libc::POLLIN != 0 {
        if let Err(error) = guard.read() {
            // A spurious wake or EAGAIN must not kill the HUD; a real
            // disconnect surfaces on the next flush().
            tracing::debug!("[HUD] Wayland read skipped: {error}");
        }
    }
    queue.dispatch_pending(data)?;
    Ok(())
}

struct HudState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    font: Option<FontArc>,
    font_retry_at: Instant,
    state: UiState,
    previous_state: Option<UiStateKind>,
    flash_until: Option<Instant>,
    flash_text: Option<String>,
    flash_ok: bool,
    started_at: Instant,
    last_render: Option<RenderKey>,
    configured: bool,
    width: u32,
    height: u32,
    visible: bool,
    daemon_available: bool,
    exit: bool,
}

impl HudState {
    fn new(
        registry_state: RegistryState,
        output_state: OutputState,
        shm: Shm,
        pool: SlotPool,
        layer: LayerSurface,
    ) -> Self {
        Self {
            registry_state,
            output_state,
            shm,
            pool,
            layer,
            font: None,
            font_retry_at: Instant::now(),
            state: UiState::Idle,
            previous_state: None,
            flash_until: None,
            flash_text: None,
            flash_ok: false,
            started_at: Instant::now(),
            last_render: None,
            configured: false,
            width: FALLBACK_WIDTH,
            height: HUD_HEIGHT,
            visible: false,
            daemon_available: false,
            exit: false,
        }
    }

    fn poll_status(&mut self) {
        match ipc::send(Command::Status) {
            Ok(reply) => {
                if !self.daemon_available {
                    tracing::info!("[HUD] daemon status stream available");
                    self.daemon_available = true;
                }
                self.apply_reply(&reply, Instant::now());
            }
            Err(error) => {
                if self.daemon_available {
                    tracing::warn!("[HUD] daemon status unavailable: {error:#}");
                    self.daemon_available = false;
                }
                // A dead daemon must not leave a live Recording chip on
                // screen; collapse to idle and hide.
                self.force_idle();
            }
        }
    }

    fn force_idle(&mut self) {
        self.state = UiState::Idle;
        self.flash_until = None;
        self.flash_text = None;
        self.flash_ok = false;
        self.previous_state = Some(UiStateKind::Idle);
        if self.visible {
            self.layer.wl_surface().attach(None, 0, 0);
            self.layer.commit();
            self.visible = false;
            self.last_render = None;
            tracing::info!("[HUD] surface hidden (daemon unavailable)");
        }
    }

    fn apply_reply(&mut self, reply: &Reply, now: Instant) {
        let next_state = UiState::from_reply(reply);
        let next_kind = next_state.kind();
        if self.previous_state != Some(next_kind) {
            match &next_state {
                UiState::Idle => tracing::info!("[HUD] state=idle"),
                UiState::Recording { elapsed } => {
                    tracing::info!("[HUD] state=recording elapsed={elapsed}s")
                }
                UiState::Processing { stage } => {
                    tracing::info!("[HUD] state=processing stage={stage}")
                }
            }

            if self
                .previous_state
                .is_some_and(|state| state != UiStateKind::Idle)
                && next_state.is_idle()
            {
                self.flash_until = Some(now + RESULT_FLASH);
                // Show the daemon's actual terminal message (Typed N chars,
                // Heard nothing, Cancelled, ...) — never a fake success.
                self.flash_text = reply.last.clone();
                self.flash_ok = reply.last_ok.unwrap_or(false);
                tracing::info!(
                    "[HUD] state=idle result flash ok={} text=\"{}\"",
                    self.flash_ok,
                    self.flash_text.as_deref().unwrap_or("")
                );
            }
            self.previous_state = Some(next_kind);
        }
        self.state = next_state;
        if self.state.is_idle() && self.flash_until.is_some_and(|until| now >= until) {
            self.flash_until = None;
        }
    }

    fn redraw_if_needed(&mut self) -> Result<()> {
        if !self.configured {
            return Ok(());
        }

        let now = Instant::now();
        let view = self.view(now);
        let key = view
            .as_ref()
            .map(|view| RenderKey::from_view(view, self.width, self.height));
        if key == self.last_render && view.is_some() == self.visible {
            return Ok(());
        }

        match view {
            Some(view) => {
                let was_visible = self.visible;
                if self.draw(&view)? {
                    self.visible = true;
                    self.last_render = key;
                    if !was_visible {
                        tracing::info!("[HUD] surface shown");
                    }
                } else {
                    self.last_render = None;
                }
            }
            None => {
                if self.visible {
                    self.layer.wl_surface().attach(None, 0, 0);
                    self.layer.commit();
                    self.visible = false;
                    self.last_render = None;
                    tracing::info!("[HUD] surface hidden");
                }
            }
        }
        Ok(())
    }

    fn view(&mut self, now: Instant) -> Option<ChipView> {
        if self.state.is_idle() {
            if self.flash_until.is_some_and(|until| now < until) {
                let message = self.flash_text.clone().unwrap_or_else(|| "Done".to_owned());
                return Some(ChipView::result(message, self.flash_ok));
            }
            return None;
        }

        let animation_seconds = now.duration_since(self.started_at).as_secs_f32();
        match &self.state {
            UiState::Recording { elapsed } => {
                let phase = animation_seconds * std::f32::consts::TAU / 1.5;
                let alpha = (0.55 + 0.45 * phase.sin()).clamp(0.0, 1.0);
                Some(ChipView {
                    text: format!("REC {}  Listening…", format_elapsed(*elapsed)),
                    kind: ChipKind::Recording,
                    alpha: (alpha * 255.0).round() as u8,
                    spinner: 0,
                })
            }
            UiState::Processing { stage } => {
                let (text, kind) = if stage == "cleaning" {
                    ("Cleaning up…".to_owned(), ChipKind::Cleaning)
                } else {
                    ("Transcribing…".to_owned(), ChipKind::Transcribing)
                };
                Some(ChipView {
                    text,
                    kind,
                    alpha: 255,
                    spinner: ((animation_seconds * 8.0) as u8) % 8,
                })
            }
            UiState::Idle => None,
        }
    }

    fn draw(&mut self, view: &ChipView) -> Result<bool> {
        if self.font.is_none() && Instant::now() >= self.font_retry_at {
            self.font = load_font();
            self.font_retry_at = Instant::now() + FONT_RETRY_INTERVAL;
            if self.font.is_none() {
                tracing::warn!("[HUD] no monospace font found; retrying");
            }
        }
        let Some(font) = self.font.as_ref() else {
            return Ok(false);
        };

        let width = self.width.clamp(FALLBACK_WIDTH.min(200), MAX_WIDTH);
        let height = self.height.max(HUD_HEIGHT);
        let stride = width
            .checked_mul(4)
            .context("calculating HUD buffer stride")? as i32;
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .context("creating HUD buffer")?;
        for pixel in canvas.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 0, 0]);
        }

        let scale = PxScale::from(FONT_SIZE);
        let text_width = measure_text(font, &view.text, scale);
        let pill_width = (text_width.ceil() as u32 + 42).min(width.saturating_sub(16));
        let pill_width = pill_width.max(100);
        let pill_height = 36_u32.min(height);
        let pill_x = (width.saturating_sub(pill_width)) / 2;
        let pill_y = (height.saturating_sub(pill_height)) / 2;
        rounded_rect(
            canvas,
            width,
            height,
            pill_x,
            pill_y,
            pill_x + pill_width,
            pill_y + pill_height,
            18,
            [24, 24, 27, 224],
        );

        let accent = match view.kind {
            ChipKind::Recording => [239, 68, 68, view.alpha],
            ChipKind::Transcribing => [251, 191, 36, 255],
            ChipKind::Cleaning => [168, 85, 247, 255],
            ChipKind::Sent => [74, 222, 128, 255],
            ChipKind::Notice => [245, 158, 11, 255],
        };
        match view.kind {
            ChipKind::Recording | ChipKind::Notice => circle(
                canvas,
                width,
                height,
                pill_x as f32 + 18.0,
                pill_y as f32 + pill_height as f32 / 2.0,
                5.5,
                accent,
            ),
            ChipKind::Transcribing | ChipKind::Cleaning => spinner(
                canvas,
                width,
                height,
                pill_x as f32 + 18.0,
                pill_y as f32 + pill_height as f32 / 2.0,
                view.spinner,
                accent,
            ),
            ChipKind::Sent => draw_check(
                canvas,
                width,
                height,
                pill_x as f32 + 18.0,
                pill_y as f32 + pill_height as f32 / 2.0,
                accent,
            ),
        }

        let scaled = font.as_scaled(scale);
        let baseline =
            pill_y as f32 + (pill_height as f32 - scaled.height()) / 2.0 + scaled.ascent();
        draw_text(
            canvas,
            width,
            height,
            font,
            &view.text,
            pill_x as f32 + 32.0,
            baseline,
            scale,
            [241, 245, 249, 255],
        );
        let _ = canvas;
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.layer.wl_surface())
            .context("attaching HUD buffer")?;
        self.layer.commit();
        Ok(true)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UiState {
    Idle,
    Recording { elapsed: u64 },
    Processing { stage: String },
}

impl UiState {
    fn from_reply(reply: &Reply) -> Self {
        match reply.state.as_str() {
            "recording" => Self::Recording {
                elapsed: reply.elapsed.unwrap_or_default(),
            },
            "processing" => Self::Processing {
                stage: reply.stage.as_deref().unwrap_or("transcribing").to_owned(),
            },
            _ => Self::Idle,
        }
    }

    fn kind(&self) -> UiStateKind {
        match self {
            Self::Idle => UiStateKind::Idle,
            Self::Recording { .. } => UiStateKind::Recording,
            Self::Processing { stage } if stage == "cleaning" => UiStateKind::Cleaning,
            Self::Processing { .. } => UiStateKind::Transcribing,
        }
    }

    fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiStateKind {
    Idle,
    Recording,
    Transcribing,
    Cleaning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChipKind {
    Sent,
    Notice,
    Recording,
    Transcribing,
    Cleaning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChipView {
    text: String,
    kind: ChipKind,
    alpha: u8,
    spinner: u8,
}

impl ChipView {
    fn result(message: String, ok: bool) -> Self {
        Self {
            text: message,
            kind: if ok { ChipKind::Sent } else { ChipKind::Notice },
            alpha: 255,
            spinner: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderKey {
    text: String,
    kind: ChipKind,
    alpha: u8,
    spinner: u8,
    width: u32,
    height: u32,
}

impl RenderKey {
    fn from_view(view: &ChipView, width: u32, height: u32) -> Self {
        Self {
            text: view.text.clone(),
            kind: view.kind,
            alpha: view.alpha,
            spinner: view.spinner,
            width,
            height,
        }
    }
}

impl CompositorHandler for HudState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        self.last_render = None;
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        self.last_render = None;
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Animation is driven by the bounded status polling loop.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for HudState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.configured = true;
        self.last_render = None;
    }
}
impl OutputHandler for HudState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for HudState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(HudState);

impl ProvidesRegistryState for HudState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    smithay_client_toolkit::registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(HudState);

fn format_elapsed(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn measure_text(font: &FontArc, text: &str, scale: PxScale) -> f32 {
    let scaled = font.as_scaled(scale);
    let mut width = 0.0;
    let mut previous = None;
    for character in text.chars() {
        let glyph = scaled.scaled_glyph(character);
        if let Some(previous) = previous {
            width += scaled.kern(previous, glyph.id);
        }
        width += scaled.h_advance(glyph.id);
        previous = Some(glyph.id);
    }
    width
}

#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: &FontArc,
    text: &str,
    x: f32,
    baseline: f32,
    scale: PxScale,
    color: [u8; 4],
) {
    let scaled = font.as_scaled(scale);
    let mut cursor = x;
    let mut previous = None;
    for character in text.chars() {
        let mut glyph = scaled.scaled_glyph(character);
        let glyph_id = glyph.id;
        if let Some(previous) = previous {
            cursor += scaled.kern(previous, glyph_id);
        }
        glyph.position = ab_glyph::point(cursor, baseline);
        if let Some(outline) = scaled.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            let origin_x = bounds.min.x.round() as i32;
            let origin_y = bounds.min.y.round() as i32;
            outline.draw(|glyph_x, glyph_y, coverage| {
                let px = origin_x + glyph_x as i32;
                let py = origin_y + glyph_y as i32;
                if px >= 0 && py >= 0 {
                    blend_pixel(canvas, width, height, px as u32, py as u32, color, coverage);
                }
            });
        }
        cursor += scaled.h_advance(glyph_id);
        previous = Some(glyph_id);
    }
}

#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn rounded_rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    radius: u32,
    color: [u8; 4],
) {
    let radius = radius
        .min((x1.saturating_sub(x0)) / 2)
        .min((y1.saturating_sub(y0)) / 2);
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let near_left = x < x0 + radius;
            let near_right = x + radius >= x1;
            let near_top = y < y0 + radius;
            let near_bottom = y + radius >= y1;
            let inside = if (near_left || near_right) && (near_top || near_bottom) {
                let cx = if near_left {
                    x0 + radius
                } else {
                    x1.saturating_sub(radius + 1)
                };
                let cy = if near_top {
                    y0 + radius
                } else {
                    y1.saturating_sub(radius + 1)
                };
                let dx = x as i32 - cx as i32;
                let dy = y as i32 - cy as i32;
                dx * dx + dy * dy <= (radius as i32) * (radius as i32)
            } else {
                true
            };
            if inside {
                blend_pixel(canvas, width, height, x, y, color, 1.0);
            }
        }
    }
}

fn circle(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    radius: f32,
    color: [u8; 4],
) {
    let min_x = (center_x - radius - 1.0).max(0.0) as u32;
    let max_x = (center_x + radius + 1.0).min(width as f32) as u32;
    let min_y = (center_y - radius - 1.0).max(0.0) as u32;
    let max_y = (center_y + radius + 1.0).min(height as f32) as u32;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let dx = x as f32 + 0.5 - center_x;
            let dy = y as f32 + 0.5 - center_y;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (radius + 0.75 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, color, coverage);
            }
        }
    }
}

fn spinner(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    phase: u8,
    color: [u8; 4],
) {
    for index in 0..8_u8 {
        let angle = index as f32 * std::f32::consts::TAU / 8.0;
        let x = center_x + angle.cos() * 6.0;
        let y = center_y + angle.sin() * 6.0;
        let distance = index.wrapping_add(8).wrapping_sub(phase) % 8;
        let alpha = 0.25 + 0.75 * (1.0 - distance as f32 / 8.0);
        let mut dot_color = color;
        dot_color[3] = (dot_color[3] as f32 * alpha) as u8;
        circle(canvas, width, height, x, y, 1.8, dot_color);
    }
}

fn draw_check(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    color: [u8; 4],
) {
    for offset in 0..3 {
        circle(
            canvas,
            width,
            height,
            center_x - 5.0 + offset as f32 * 2.0,
            center_y + 1.0 - offset as f32 * 2.0,
            1.5,
            color,
        );
    }
    for offset in 0..5 {
        circle(
            canvas,
            width,
            height,
            center_x + offset as f32 * 2.0 - 1.0,
            center_y - 3.0 - offset as f32 * 2.0,
            1.5,
            color,
        );
    }
}

fn blend_pixel(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    color: [u8; 4],
    coverage: f32,
) {
    if x >= width || y >= height {
        return;
    }
    let index = ((y * width + x) * 4) as usize;
    if index + 3 >= canvas.len() {
        return;
    }
    let source_alpha = (color[3] as f32 * coverage.clamp(0.0, 1.0)) / 255.0;
    let destination_alpha = canvas[index + 3] as f32 / 255.0;
    let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
    if output_alpha <= f32::EPSILON {
        return;
    }
    let destination = [canvas[index + 2], canvas[index + 1], canvas[index]];
    for (offset, channel) in color[..3].iter().enumerate() {
        let value = (*channel as f32 * source_alpha
            + destination[offset] as f32 * destination_alpha * (1.0 - source_alpha))
            / output_alpha;
        canvas[index + 2 - offset] = value.round().clamp(0.0, 255.0) as u8;
    }
    canvas[index + 3] = (output_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn load_font() -> Option<FontArc> {
    let path = find_font_path()?;
    let bytes = fs::read(&path).ok()?;
    match FontArc::try_from_vec(bytes) {
        Ok(font) => {
            tracing::info!("[HUD] loaded font {}", path.display());
            Some(font)
        }
        Err(error) => {
            tracing::warn!("[HUD] cannot load font {}: {error}", path.display());
            None
        }
    }
}

fn find_font_path() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansMono-Regular.ttf"),
        PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansMono[wght].ttf"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(&home).join(".local/share/fonts"));
        candidates.push(PathBuf::from(home).join(".fonts"));
    }
    for candidate in candidates {
        if candidate.is_file() {
            return Some(candidate);
        }
        if candidate.is_dir() {
            if let Some(path) = find_any_font(&candidate) {
                return Some(path);
            }
        }
    }
    find_any_font(Path::new("/usr/share/fonts"))
}

fn find_any_font(root: &Path) -> Option<PathBuf> {
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(directory).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| {
                let extension = extension.to_string_lossy();
                extension.eq_ignore_ascii_case("ttf") || extension.eq_ignore_ascii_case("otf")
            }) {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;

    #[test]
    fn formats_elapsed_as_minutes_and_seconds() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(12), "00:12");
        assert_eq!(format_elapsed(125), "02:05");
    }
}
