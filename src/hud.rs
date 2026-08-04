//! Always-on-top Wayland layer-shell status HUD.
//!
//! The HUD is a read-only mirror of the daemon. It polls the existing status
//! command and never sends a command which can change daemon state.
//!
//! Visual design: a top-centre capsule with a soft drop shadow and a 1px top
//! rim light, one quiet UI-font label ("Listening…", "Transcribing…",
//! "Cleaning…") with a small inline state glyph, and a trailing mm:ss counter
//! while recording. Every visual state change eases over ~260ms
//! (easeOutCubic): the pill pops in with scale+alpha, accent colors crossfade,
//! and the pill width follows the content. The recording dot breathes with an
//! expanding ping ring, processing shows a smooth rotating comet arc, and the
//! result flash lands with a gentle check pop.

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
/// Render tick while the chip is visible; IPC polling stays at POLL_INTERVAL.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const RESULT_FLASH: Duration = Duration::from_millis(2_500);
/// Duration of the eased transition run on every visual state change.
const TRANSITION: Duration = Duration::from_millis(260);
/// Tail of the result flash spent fading out, inside the RESULT_FLASH window.
const FLASH_FADE_TAIL: f32 = 0.25;
const HUD_HEIGHT: u32 = 56;
const FALLBACK_WIDTH: u32 = 420;
const MAX_WIDTH: u32 = 900;
const FONT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

// Layout, in design units (pixels at pop-in scale 1.0).
const PILL_HEIGHT: f32 = 38.0;
const MIN_PILL_WIDTH: f32 = 112.0;
/// Distance from the pill's left edge to the state-glyph centre.
const GLYPH_CENTER_X: f32 = 19.0;
/// Distance from the pill's left edge to the label's left edge.
const LABEL_X: f32 = 34.0;
const PAD_RIGHT: f32 = 18.0;
/// Minimum gap between the label and the trailing detail text.
const DETAIL_GAP: f32 = 14.0;
const LABEL_SIZE: f32 = 15.0;
const DETAIL_SIZE: f32 = 13.0;

// Palette: near-black capsule, near-white text, one warm accent per state.
const PILL_FILL: [u8; 4] = [14, 14, 17, 225];
const RIM_LIGHT: [u8; 4] = [255, 255, 255, 34];
const SHADOW_COLOR: [u8; 4] = [0, 0, 0, 255];
const TEXT_PRIMARY: [u8; 4] = [242, 244, 248, 255];
const TEXT_SECONDARY: [u8; 4] = [242, 244, 248, 160];

const SPIN_TURNS_PER_SECOND: f32 = 0.9;
const ARC_SWEEP: f32 = std::f32::consts::TAU * 0.3;
/// Recording pulse period in seconds; must divide the 60s phase window.
const PULSE_PERIOD: f32 = 2.0;

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

    let mut last_poll: Option<Instant> = None;
    while !hud.exit {
        let now = Instant::now();
        if last_poll.is_none_or(|at| now.duration_since(at) >= POLL_INTERVAL) {
            hud.poll_status();
            last_poll = Some(now);
        }
        if let Err(error) = hud.redraw_if_needed() {
            tracing::warn!("[HUD] redraw failed: {error:#}");
        }
        // Read the Wayland socket with a bounded timeout. This services
        // buffer releases and disconnects each pass.
        let timeout = hud.tick_interval();
        if let Err(error) = timed_dispatch(&mut event_queue, &mut hud, timeout) {
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
    fonts: Option<Fonts>,
    font_retry_at: Instant,
    state: UiState,
    previous_state: Option<UiStateKind>,
    flash_until: Option<Instant>,
    flash_text: Option<String>,
    flash_ok: bool,
    started_at: Instant,
    /// Chip kind currently on screen; None while hidden.
    shown_kind: Option<ChipKind>,
    /// Start of the eased transition begun by the latest visual change.
    transition_at: Instant,
    /// Kind faded from during the current transition; None means pop-in.
    transition_from: Option<ChipKind>,
    /// Pill width at the moment the current transition started.
    pill_width_from: f32,
    /// Pill width drawn on the previous frame.
    pill_width_last: f32,
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
            fonts: None,
            font_retry_at: Instant::now(),
            state: UiState::Idle,
            previous_state: None,
            flash_until: None,
            flash_text: None,
            flash_ok: false,
            started_at: Instant::now(),
            shown_kind: None,
            transition_at: Instant::now(),
            transition_from: None,
            pill_width_from: 0.0,
            pill_width_last: 0.0,
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
        self.shown_kind = None;
        self.hide_surface(" (daemon unavailable)");
    }

    /// Frame pacing: animate while the chip is on screen, otherwise idle at
    /// the status poll cadence.
    fn tick_interval(&self) -> Duration {
        if self.visible {
            FRAME_INTERVAL
        } else {
            POLL_INTERVAL
        }
    }

    /// Buffer size for one frame: the configured surface size, clamped.
    fn frame_size(&self) -> (u32, u32) {
        (
            self.width.clamp(FALLBACK_WIDTH.min(200), MAX_WIDTH),
            self.height.max(HUD_HEIGHT),
        )
    }

    /// Hide the chip by painting a fully transparent frame.
    ///
    /// The surface deliberately stays mapped. Unmapping it (a null buffer)
    /// requires repeating the configure handshake before another buffer may be
    /// attached, and COSMIC never sends that second configure: the compositor
    /// either kills the client or the chip never returns. A transparent frame
    /// is invisible, keeps the empty input region passing clicks through, and
    /// costs one buffer per hide.
    fn hide_surface(&mut self, reason: &str) {
        if !self.visible {
            return;
        }
        if let Err(error) = self.blank() {
            tracing::warn!("[HUD] hiding the chip failed: {error:#}");
            return;
        }
        self.visible = false;
        self.last_render = None;
        tracing::info!("[HUD] surface hidden{reason}");
    }

    fn blank(&mut self) -> Result<()> {
        let (width, height) = self.frame_size();
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
        canvas.fill(0);
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(self.layer.wl_surface())
            .context("attaching HUD buffer")?;
        self.layer.commit();
        Ok(())
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
            None => self.hide_surface(""),
        }
        Ok(())
    }

    fn view(&mut self, now: Instant) -> Option<ChipView> {
        let content = match &self.state {
            UiState::Idle => match self.flash_until {
                Some(until) if now < until => {
                    let label = self.flash_text.clone().unwrap_or_else(|| "Done".to_owned());
                    let kind = if self.flash_ok {
                        ChipKind::Sent
                    } else {
                        ChipKind::Notice
                    };
                    let remaining = until.duration_since(now).as_secs_f32();
                    let fade = ease_out_cubic(remaining / FLASH_FADE_TAIL);
                    Some((label, None, kind, fade))
                }
                _ => None,
            },
            UiState::Recording { elapsed } => Some((
                "Listening…".to_owned(),
                Some(format_elapsed(*elapsed)),
                ChipKind::Recording,
                1.0,
            )),
            UiState::Processing { stage } => {
                let (label, kind) = if stage == "cleaning" {
                    ("Cleaning…", ChipKind::Cleaning)
                } else {
                    ("Transcribing…", ChipKind::Transcribing)
                };
                Some((label.to_owned(), None, kind, 1.0))
            }
        };
        let Some((label, detail, kind, fade)) = content else {
            self.shown_kind = None;
            return None;
        };

        if self.shown_kind != Some(kind) {
            self.transition_from = self.shown_kind;
            self.transition_at = now;
            self.pill_width_from = self.pill_width_last;
            self.shown_kind = Some(kind);
        }
        let progress = ease_out_cubic(
            now.duration_since(self.transition_at).as_secs_f32() / TRANSITION.as_secs_f32(),
        );
        // A 60s window keeps f32 phase math precise over long uptimes; the
        // pulse and spinner periods both divide it, so motion never jumps.
        let phase = (now.duration_since(self.started_at).as_secs_f64() % 60.0) as f32;
        Some(ChipView {
            label,
            detail,
            kind,
            from: self.transition_from,
            progress,
            fade,
            phase,
        })
    }

    fn draw(&mut self, view: &ChipView) -> Result<bool> {
        if self.fonts.is_none() && Instant::now() >= self.font_retry_at {
            self.fonts = load_fonts();
            self.font_retry_at = Instant::now() + FONT_RETRY_INTERVAL;
            if self.fonts.is_none() {
                tracing::warn!("[HUD] no usable UI font found; retrying");
            }
        }
        // FontArc is an Arc; cloning the handles frees `self` for the
        // mutable pool borrow below.
        let Some(fonts) = self.fonts.clone() else {
            return Ok(false);
        };

        let (width, height) = self.frame_size();
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
        canvas.fill(0);

        // Motion inputs: pop-in scale/alpha from hidden, content crossfade
        // between kinds, and the flash fade-out tail.
        let appear = if view.from.is_none() {
            view.progress
        } else {
            1.0
        };
        let swap = if view.from.is_some() {
            0.35 + 0.65 * view.progress
        } else {
            1.0
        };
        let visibility = appear * view.fade;
        let scale_factor = 0.94 + 0.06 * appear;
        let accent = match view.from {
            Some(from) if view.progress < 1.0 => {
                mix_rgb(accent_color(from), accent_color(view.kind), view.progress)
            }
            _ => accent_color(view.kind),
        };

        // Layout in design units; the pill width eases towards its content.
        let label_width = measure_text(&fonts.strong, &view.label, PxScale::from(LABEL_SIZE));
        let detail_width = view.detail.as_deref().map_or(0.0, |detail| {
            measure_text(&fonts.regular, detail, PxScale::from(DETAIL_SIZE))
        });
        let mut target_width = LABEL_X + label_width + PAD_RIGHT;
        if view.detail.is_some() {
            target_width += DETAIL_GAP + detail_width;
        }
        let target_width = target_width.clamp(MIN_PILL_WIDTH, width as f32 - 16.0);
        let pill_width = if view.from.is_some() && view.progress < 1.0 && self.pill_width_from > 0.0
        {
            self.pill_width_from + (target_width - self.pill_width_from) * view.progress
        } else {
            target_width
        };
        self.pill_width_last = pill_width;

        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0 - 1.0;
        let half_width = pill_width / 2.0 * scale_factor;
        let half_height = PILL_HEIGHT.min(height as f32 - 12.0) / 2.0 * scale_factor;
        pill(
            canvas,
            width,
            height,
            center_x,
            center_y,
            half_width,
            half_height,
            visibility,
        );

        let left = center_x - half_width;
        let content_alpha = visibility * swap;
        let glyph_x = left + GLYPH_CENTER_X * scale_factor;
        let accent_solid = [accent[0], accent[1], accent[2], 255];
        match view.kind {
            ChipKind::Recording => recording_dot(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                view.phase,
                scale_factor,
                scale_alpha(accent_solid, content_alpha),
            ),
            ChipKind::Transcribing | ChipKind::Cleaning => arc(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                6.4 * scale_factor,
                2.4 * scale_factor,
                view.phase * std::f32::consts::TAU * SPIN_TURNS_PER_SECOND,
                ARC_SWEEP,
                0.08,
                scale_alpha(accent_solid, content_alpha),
            ),
            ChipKind::Sent => check(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                ease_out_back(view.progress) * scale_factor,
                scale_alpha(accent_solid, content_alpha),
            ),
            ChipKind::Notice => circle(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                4.2 * scale_factor,
                scale_alpha(accent_solid, 0.95 * content_alpha),
            ),
        }

        let label_px = PxScale::from(LABEL_SIZE * scale_factor);
        let metrics = fonts.strong.as_scaled(label_px);
        let baseline = center_y - metrics.height() / 2.0 + metrics.ascent();
        draw_text(
            canvas,
            width,
            height,
            &fonts.strong,
            &view.label,
            left + LABEL_X * scale_factor,
            baseline,
            label_px,
            scale_alpha(TEXT_PRIMARY, content_alpha),
        );
        if let Some(detail) = view.detail.as_deref() {
            let detail_x = center_x + half_width - (PAD_RIGHT + detail_width) * scale_factor;
            draw_text(
                canvas,
                width,
                height,
                &fonts.regular,
                detail,
                detail_x,
                baseline,
                PxScale::from(DETAIL_SIZE * scale_factor),
                scale_alpha(TEXT_SECONDARY, content_alpha),
            );
        }

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

/// One frame of chip content plus the motion inputs which style it.
#[derive(Clone, Debug)]
struct ChipView {
    label: String,
    detail: Option<String>,
    kind: ChipKind,
    /// Kind the transition fades from; None means pop-in from hidden.
    from: Option<ChipKind>,
    /// Eased 0..=1 progress of the current transition.
    progress: f32,
    /// Global fade multiplier for the result-flash tail.
    fade: f32,
    /// Wrapped seconds driving continuous motion (pulse, spinner).
    phase: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderKey {
    label: String,
    detail: Option<String>,
    kind: ChipKind,
    progress: u8,
    fade: u8,
    phase: u16,
    width: u32,
    height: u32,
}

impl RenderKey {
    fn from_view(view: &ChipView, width: u32, height: u32) -> Self {
        // Continuous motion only exists for these kinds; a result flash is
        // static once its transition and fade are settled.
        let animated = matches!(
            view.kind,
            ChipKind::Recording | ChipKind::Transcribing | ChipKind::Cleaning
        );
        Self {
            label: view.label.clone(),
            detail: view.detail.clone(),
            kind: view.kind,
            progress: (view.progress * 255.0).round() as u8,
            fade: (view.fade * 255.0).round() as u8,
            phase: if animated {
                (view.phase * 30.0).round() as u16
            } else {
                0
            },
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
        // Animation is timed by the bounded event-loop timeout in run().
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

/// Cubic ease-out: fast start, gentle landing. Input is clamped to [0, 1].
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// Ease-out with a gentle ~5% overshoot, used for the check pop-in.
fn ease_out_back(t: f32) -> f32 {
    const OVERSHOOT: f32 = 1.2;
    let u = t.clamp(0.0, 1.0) - 1.0;
    1.0 + (OVERSHOOT + 1.0) * u * u * u + OVERSHOOT * u * u
}

/// Breathing pulse: quick eased attack, slow exponential release.
/// `u` is the position within one cycle; output is in [0, 1] with the peak
/// at the end of the attack.
fn pulse(u: f32) -> f32 {
    const ATTACK: f32 = 0.12;
    let u = u.rem_euclid(1.0);
    if u < ATTACK {
        ease_out_cubic(u / ATTACK)
    } else {
        (-(u - ATTACK) * 3.2).exp()
    }
}

fn mix_rgb(from: [u8; 3], to: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let mut mixed = [0_u8; 3];
    for (channel, value) in mixed.iter_mut().enumerate() {
        let a = from[channel] as f32;
        let b = to[channel] as f32;
        *value = (a + (b - a) * t).round() as u8;
    }
    mixed
}

fn scale_alpha(color: [u8; 4], factor: f32) -> [u8; 4] {
    let mut scaled = color;
    scaled[3] = (scaled[3] as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    scaled
}

fn accent_color(kind: ChipKind) -> [u8; 3] {
    match kind {
        ChipKind::Recording => [255, 106, 92],    // warm coral
        ChipKind::Transcribing => [255, 186, 74], // amber
        ChipKind::Cleaning => [190, 142, 255],    // violet
        ChipKind::Sent => [116, 220, 150],        // soft green
        ChipKind::Notice => [255, 178, 92],       // warm amber
    }
}

/// Capsule with a soft drop shadow below and a 1px top rim light.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn pill(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    alpha: f32,
) {
    const SHADOW_SPREAD: f32 = 6.0;
    const SHADOW_DROP: f32 = 2.5;
    let radius = half_height;
    let flat = (half_width - radius).max(0.0);
    let min_x = (center_x - half_width - SHADOW_SPREAD - 1.0).max(0.0) as u32;
    let max_x = (center_x + half_width + SHADOW_SPREAD + 1.0).min(width as f32) as u32;
    let min_y = (center_y - half_height - 2.0).max(0.0) as u32;
    let max_y =
        (center_y + half_height + SHADOW_DROP + SHADOW_SPREAD + 1.0).min(height as f32) as u32;
    let fill = scale_alpha(PILL_FILL, alpha);
    let rim = scale_alpha(RIM_LIGHT, alpha);
    let shadow_strength = 0.45 * alpha;
    for y in min_y..max_y {
        let dy = y as f32 + 0.5 - center_y;
        for x in min_x..max_x {
            let dx = x as f32 + 0.5 - center_x;
            let qx = (dx.abs() - flat).max(0.0);
            let signed = (qx * qx + dy * dy).sqrt() - radius;
            let fill_coverage = (0.5 - signed).clamp(0.0, 1.0);
            if fill_coverage < 1.0 {
                let shadow_dy = dy - SHADOW_DROP;
                let shadow_signed = (qx * qx + shadow_dy * shadow_dy).sqrt() - radius;
                if shadow_signed < SHADOW_SPREAD {
                    let falloff = 1.0 - (shadow_signed.max(0.0) / SHADOW_SPREAD);
                    let coverage = falloff * falloff * shadow_strength * (1.0 - fill_coverage);
                    if coverage > 0.004 {
                        blend_pixel(canvas, width, height, x, y, SHADOW_COLOR, coverage);
                    }
                }
            }
            if fill_coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, fill, fill_coverage);
                if dy < 0.0 {
                    let edge = (1.0 - (signed + 1.1).abs()).clamp(0.0, 1.0);
                    if edge > 0.0 {
                        let vertical = (-dy / half_height).clamp(0.0, 1.0);
                        blend_pixel(canvas, width, height, x, y, rim, edge * vertical);
                    }
                }
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

/// Round-capped circular arc ending at `head`, sweeping `sweep` radians
/// behind it. Alpha tapers from 1.0 at the head to `tail_alpha` at the tail;
/// pass `sweep = TAU`, `tail_alpha = 1.0` for a uniform ring.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn arc(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    radius: f32,
    thickness: f32,
    head: f32,
    sweep: f32,
    tail_alpha: f32,
    color: [u8; 4],
) {
    use std::f32::consts::TAU;
    let half = thickness / 2.0;
    let reach = radius + half + 1.0;
    let min_x = (center_x - reach).max(0.0) as u32;
    let max_x = (center_x + reach).min(width as f32) as u32;
    let min_y = (center_y - reach).max(0.0) as u32;
    let max_y = (center_y + reach).min(height as f32) as u32;
    let head_point = (
        center_x + head.cos() * radius,
        center_y + head.sin() * radius,
    );
    let tail = head - sweep;
    let tail_point = (
        center_x + tail.cos() * radius,
        center_y + tail.sin() * radius,
    );
    for y in min_y..max_y {
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let dx = px - center_x;
            let dy = py - center_y;
            let band = ((dx * dx + dy * dy).sqrt() - radius).abs();
            if band > half + 1.0 {
                continue;
            }
            let behind_head = (head - dy.atan2(dx)).rem_euclid(TAU);
            let (distance, taper) = if behind_head <= sweep {
                (band, behind_head / sweep)
            } else {
                let to_head = (px - head_point.0).hypot(py - head_point.1);
                let to_tail = (px - tail_point.0).hypot(py - tail_point.1);
                if to_head <= to_tail {
                    (to_head, 0.0)
                } else {
                    (to_tail, 1.0)
                }
            };
            let coverage = (half + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                let fade = 1.0 - taper * (1.0 - tail_alpha);
                blend_pixel(canvas, width, height, x, y, color, coverage * fade);
            }
        }
    }
}

/// Round-capped line segment.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn segment(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    color: [u8; 4],
) {
    let half = thickness / 2.0;
    let min_x = (x0.min(x1) - half - 1.0).max(0.0) as u32;
    let max_x = (x0.max(x1) + half + 1.0).min(width as f32) as u32;
    let min_y = (y0.min(y1) - half - 1.0).max(0.0) as u32;
    let max_y = (y0.max(y1) + half + 1.0).min(height as f32) as u32;
    let vx = x1 - x0;
    let vy = y1 - y0;
    let length_sq = (vx * vx + vy * vy).max(f32::EPSILON);
    for y in min_y..max_y {
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let t = (((px - x0) * vx + (py - y0) * vy) / length_sq).clamp(0.0, 1.0);
            let distance = (px - x0 - vx * t).hypot(py - y0 - vy * t);
            let coverage = (half + 0.5 - distance).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, color, coverage);
            }
        }
    }
}

/// Check mark scaled about its centre; `scale <= 0` draws nothing.
fn check(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    scale: f32,
    color: [u8; 4],
) {
    if scale <= 0.0 {
        return;
    }
    let vertex = (center_x - 1.6 * scale, center_y + 3.8 * scale);
    let thickness = 2.4 * scale;
    segment(
        canvas,
        width,
        height,
        center_x - 5.2 * scale,
        center_y + 0.4 * scale,
        vertex.0,
        vertex.1,
        thickness,
        color,
    );
    segment(
        canvas,
        width,
        height,
        vertex.0,
        vertex.1,
        center_x + 5.6 * scale,
        center_y - 3.4 * scale,
        thickness,
        color,
    );
}

/// Breathing recording dot: a core which swells with each pulse plus an
/// expanding, fading ping ring.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn recording_dot(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    phase: f32,
    scale: f32,
    color: [u8; 4],
) {
    let u = (phase / PULSE_PERIOD).fract();
    let k = pulse(u);
    let ring_alpha = (1.0 - u) * (1.0 - u) * 0.4;
    if ring_alpha > 0.01 {
        arc(
            canvas,
            width,
            height,
            center_x,
            center_y,
            (4.6 + 6.5 * u) * scale,
            1.5 * scale,
            0.0,
            std::f32::consts::TAU,
            1.0,
            scale_alpha(color, ring_alpha),
        );
    }
    circle(
        canvas,
        width,
        height,
        center_x,
        center_y,
        (4.2 + 0.6 * k) * scale,
        scale_alpha(color, 0.6 + 0.4 * k),
    );
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

/// Regular + medium ("strong") UI faces; strong falls back to regular.
#[derive(Clone)]
struct Fonts {
    regular: FontArc,
    strong: FontArc,
}

/// Known UI faces, preferred over the recursive any-font fallback.
const UI_FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/inter/Inter-Regular.ttf",
    "/usr/share/fonts/opentype/inter/Inter-Regular.otf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSans-Regular.otf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/opentype/fira/FiraSans-Regular.otf",
];
const UI_FONT_STRONG_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/inter/Inter-Medium.ttf",
    "/usr/share/fonts/opentype/inter/Inter-Medium.otf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-M.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/opentype/fira/FiraSans-Medium.otf",
];

fn load_fonts() -> Option<Fonts> {
    let regular = first_font(UI_FONT_CANDIDATES).or_else(fallback_font)?;
    let strong = first_font(UI_FONT_STRONG_CANDIDATES).unwrap_or_else(|| regular.clone());
    Some(Fonts { regular, strong })
}

fn first_font(candidates: &[&str]) -> Option<FontArc> {
    candidates
        .iter()
        .map(Path::new)
        .filter(|path| path.is_file())
        .find_map(load_font_file)
}

/// Last resort: any TTF/OTF from the user's font dirs, then the system dir.
fn fallback_font() -> Option<FontArc> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(&home).join(".local/share/fonts"));
        roots.push(PathBuf::from(home).join(".fonts"));
    }
    roots.push(PathBuf::from("/usr/share/fonts"));
    roots
        .iter()
        .find_map(|root| find_any_font(root))
        .and_then(|path| load_font_file(&path))
}

fn load_font_file(path: &Path) -> Option<FontArc> {
    let bytes = fs::read(path).ok()?;
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
    use super::{ease_out_back, ease_out_cubic, format_elapsed, mix_rgb, pulse};

    #[test]
    fn formats_elapsed_as_minutes_and_seconds() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(12), "00:12");
        assert_eq!(format_elapsed(125), "02:05");
    }

    #[test]
    fn ease_out_cubic_clamps_and_decelerates() {
        assert!(ease_out_cubic(-1.0).abs() < 1e-6);
        assert!((ease_out_cubic(2.0) - 1.0).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
        let mut previous = 0.0_f32;
        for step in 0..=20 {
            let value = ease_out_cubic(step as f32 / 20.0);
            assert!(value >= previous, "must be monotonic");
            previous = value;
        }
        assert!(ease_out_cubic(0.5) > 0.5, "ease-out is front-loaded");
    }

    #[test]
    fn ease_out_back_lands_with_gentle_overshoot() {
        assert!(ease_out_back(0.0).abs() < 1e-6);
        assert!((ease_out_back(1.0) - 1.0).abs() < 1e-6);
        let peak = (1..40)
            .map(|step| ease_out_back(step as f32 / 40.0))
            .fold(0.0_f32, f32::max);
        assert!(peak > 1.0, "must overshoot");
        assert!(peak < 1.15, "overshoot must stay gentle");
    }

    #[test]
    fn pulse_attacks_then_decays_within_bounds() {
        for step in 0..=40 {
            let value = pulse(step as f32 / 40.0);
            assert!((0.0..=1.0).contains(&value));
        }
        assert!(pulse(0.12) > 0.99, "peak sits at the end of the attack");
        assert!(pulse(0.06) < pulse(0.12), "attack rises");
        assert!(pulse(0.5) < pulse(0.2), "release decays");
        assert!(pulse(0.95) < 0.1, "cycle ends near rest");
    }

    #[test]
    fn mix_rgb_interpolates_between_endpoints() {
        assert_eq!(mix_rgb([0, 0, 0], [255, 255, 255], 0.0), [0, 0, 0]);
        assert_eq!(mix_rgb([0, 0, 0], [255, 255, 255], 1.0), [255, 255, 255]);
        assert_eq!(mix_rgb([0, 0, 0], [200, 100, 50], 0.5), [100, 50, 25]);
        assert_eq!(mix_rgb([10, 20, 30], [10, 20, 30], 0.7), [10, 20, 30]);
    }
}
