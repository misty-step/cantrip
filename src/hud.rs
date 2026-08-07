//! Always-on-top Wayland layer-shell status HUD.
//!
//! The HUD is a read-only mirror of the daemon. It polls the existing status
//! command and never sends a command which can change daemon state.
//!
//! Visual design ("Warm Minimal", ADR 0010): a fixed 320×40 borderless
//! capsule, top-centre, filled with an opaque blend of the near-black floor
//! and a whisper of the state accent — the whole pill reads the mode. A
//! quiet UI-font label ("Listening…", "Cleaning…") sits centered as the
//! visual anchor, with a small state glyph in a 28px zone at the left and a
//! monospace mm:ss counter at the right while recording. Listening is a
//! pulsing dot. Transcribing and cleaning keep the indeterminate spinner.
//! Multi-chunk transcription also draws a real left-to-right fill that
//! eases from empty toward each `transcribing N/M` fraction and completes
//! to full through Cleaning (single-chunk stays spinner-only — no fake
//! meter). Continuous motion is the localized breathing pulse, spinner
//! turn, timed ease-in-out meter when determinate, the
//! ticking elapsed timer, and the ~2.5s outcome flashes. State changes
//! ease over ~260ms. A reduced-motion desktop freezes pulse/entry motion,
//! snaps the meter, draws the spinner as a calm static ring, and keeps
//! the elapsed timer ticking (it is data, not animation).

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};
use clap::ValueEnum;
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
    borrow::Cow,
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
/// Higher cadence while the chunk meter is easing so the fill reads smooth
/// on 60 Hz displays (~16 ms ≈ 60 fps).
const METER_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const RESULT_FLASH: Duration = Duration::from_millis(2_500);
/// Duration of the eased transition run on every visual state change.
const TRANSITION: Duration = Duration::from_millis(260);
/// Wall time to ease the chunk meter between targets. Chunk inference is
/// often <300 ms, so a timed ease always shows motion. Uses ease-in-out
/// for a steadier native feel than ease-out (which front-loads then lags).
const METER_EASE: Duration = Duration::from_millis(360);
/// Extra hold after the bar reaches full while Cleaning so a fast 2-chunk
/// take does not wipe the fill the instant STT ends.
const METER_COMPLETE_HOLD: Duration = Duration::from_millis(180);
/// Tail of the result flash spent fading out, inside the RESULT_FLASH window.
const FLASH_FADE_TAIL: f32 = 0.25;
const HUD_HEIGHT: u32 = 56;
const FALLBACK_WIDTH: u32 = 420;
const MAX_WIDTH: u32 = 900;
const FONT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

// Layout, in design units (pixels at pop-in scale 1.0). The capsule is a
// fixed geometry ("Warm Minimal", ADR 0010): it never resizes with content.
const CAPSULE_WIDTH: f32 = 320.0;
const CAPSULE_HEIGHT: f32 = 40.0;
/// Distance from the capsule's left edge to the state-glyph centre; the
/// glyph lives in a 28px zone at the left of the capsule.
const GLYPH_CENTER_X: f32 = 26.0;
/// The stage word is centered within
/// [left + WORD_AREA_LEFT, right - WORD_AREA_RIGHT].
const WORD_AREA_LEFT: f32 = 48.0;
/// Reserved width for the trailing timer cluster.
const WORD_AREA_RIGHT: f32 = 92.0;
const PAD_RIGHT: f32 = 18.0;
const LABEL_SIZE: f32 = 15.0;
const DETAIL_SIZE: f32 = 13.0;
/// Stronger tint for the Sent flash: the capsule "full-lits" green.
const SENT_TINT: f32 = 0.45;
/// Faint tint for the Notice flash: the capsule "drains" warm.
const NOTICE_TINT: f32 = 0.06;

// Palette: near-black floor, near-white text, one warm accent per state.
const FLOOR: [u8; 3] = [14, 14, 17];
const TEXT_PRIMARY: [u8; 4] = [242, 244, 248, 255];
const TEXT_SECONDARY: [u8; 4] = [242, 244, 248, 160];

/// Breathing pulse period in seconds; must divide the 60s phase window.
const PULSE_PERIOD: f32 = 2.0;
/// Breathing alpha range: the working glyph pulses between these opacities.
const BREATHE_MIN: f32 = 0.65;
/// Spinner turn period in seconds; a pure function of the phase window, so
/// a frozen phase draws a byte-identical arc.
const SPIN_PERIOD: f32 = 0.8;

/// Take the single-instance flock on `hud.lock`. Returns `None` when another
/// HUD already holds the lock. The returned file must stay open for the
/// process lifetime; the lock is released when the file drops or the process
/// exits, so a crashed HUD never leaves a stale lock behind.
/// Shared with the daemon, which uses the same lock to detect a missing HUD.
pub(crate) fn acquire_instance_lock() -> Result<Option<fs::File>> {
    acquire_lock_on(&crate::paths::hud_lock_path()?)
}

/// `acquire_instance_lock` against an explicit path (testable without the
/// real runtime directory).
fn acquire_lock_on(path: &Path) -> Result<Option<fs::File>> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening HUD lock {}", path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(error).with_context(|| format!("locking HUD instance file {}", path.display()))
}

/// Run the HUD until the compositor closes it or the display disconnects.
///
/// Display and daemon failures are deliberately non-fatal. The HUD is an
/// optional client and must not affect the daemon's operation.
///
/// With `--screenshot <path>` the HUD renders one state (fixed 00:07 timer
/// for recording, no daemon polling), dumps a settled frame to a PNG, and
/// exits — the same visual-test hook the settings window has. `state`
/// selects the composition; None means Recording.
pub fn run(screenshot: Option<PathBuf>, state: Option<ScreenshotState>) -> Result<()> {
    // Single instance: hold an exclusive flock for the process lifetime so
    // the daemon can detect this HUD (and respawn one when it is missing).
    // Screenshot mode is a test hook and deliberately skips the lock.
    let _instance_lock = match screenshot {
        Some(_) => None,
        None => match acquire_instance_lock() {
            Ok(Some(file)) => Some(file),
            Ok(None) => {
                tracing::info!("[HUD] another HUD instance is running; exiting");
                return Ok(());
            }
            Err(error) => {
                tracing::warn!("[HUD] cannot take the instance lock: {error:#}");
                return Ok(());
            }
        },
    };

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
    // keeps the surface top-centered for the fixed-size capsule inside.
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

    // Screenshot mode is the visual-test hook: it must render the same
    // settled frame regardless of the host's animation setting, so it
    // forces the animation-free path (frozen phase and progress —
    // byte-identical output). Live mode honors the desktop preference.
    let reduced_motion = screenshot.is_none() && prefers_reduced_motion();
    let mut hud = HudState::new(
        RegistryState::new(&globals),
        OutputState::new(&globals, &queue_handle),
        shm,
        pool,
        layer,
        screenshot,
        state,
        reduced_motion,
    );
    if let Err(error) = event_queue.roundtrip(&mut hud) {
        tracing::warn!("[HUD] display disconnected during setup: {error}");
        return Ok(());
    }

    let mut last_poll: Option<Instant> = None;
    while !hud.exit {
        let now = Instant::now();
        if hud.screenshot.is_none()
            && last_poll.is_none_or(|at| now.duration_since(at) >= POLL_INTERVAL)
        {
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
    /// Current eased 0..=1 fill for multi-chunk transcription.
    meter_display: f32,
    /// Value at the start of the active meter ease.
    meter_from: f32,
    /// Target fraction for the active meter ease (`chunk/total`).
    meter_to: f32,
    /// When the active meter ease began.
    meter_ease_at: Instant,
    /// True after the first multi-chunk fraction this run; drives the
    /// complete-to-full hold through Cleaning.
    meter_armed: bool,
    /// Keep showing a full bar until this instant after the ease lands on 1.0.
    meter_hold_until: Option<Instant>,
    /// Desktop animations disabled (gsettings enable-animations=false):
    /// freezes the breathing pulse and skips entry motion.
    reduced_motion: bool,
    last_render: Option<RenderKey>,
    configured: bool,
    width: u32,
    height: u32,
    visible: bool,
    daemon_available: bool,
    exit: bool,
    screenshot: Option<PathBuf>,
    /// State to render in screenshot mode; None selects Recording.
    screenshot_state: Option<ScreenshotState>,
    screenshot_done: bool,
}

impl HudState {
    fn clear_meter(&mut self) {
        self.meter_display = 0.0;
        self.meter_from = 0.0;
        self.meter_to = 0.0;
        self.meter_armed = false;
        self.meter_hold_until = None;
    }

    #[allow(clippy::too_many_arguments)] // construction plumbing (registry, pool, surface)
    fn new(
        registry_state: RegistryState,
        output_state: OutputState,
        shm: Shm,
        pool: SlotPool,
        layer: LayerSurface,
        screenshot: Option<PathBuf>,
        screenshot_state: Option<ScreenshotState>,
        reduced_motion: bool,
    ) -> Self {
        // Screenshot mode skips daemon polling so the frame is stable and
        // offline; `view` renders the requested state deterministically.
        let state = if screenshot.is_some() {
            UiState::Recording { elapsed: 7 }
        } else {
            UiState::Idle
        };
        Self {
            registry_state,
            output_state,
            shm,
            pool,
            layer,
            fonts: None,
            font_retry_at: Instant::now(),
            state,
            previous_state: None,
            flash_until: None,
            flash_text: None,
            flash_ok: false,
            started_at: Instant::now(),
            shown_kind: None,
            transition_at: Instant::now(),
            transition_from: None,
            meter_display: 0.0,
            meter_from: 0.0,
            meter_to: 0.0,
            meter_ease_at: Instant::now(),
            meter_armed: false,
            meter_hold_until: None,
            reduced_motion,
            last_render: None,
            configured: false,
            width: FALLBACK_WIDTH,
            height: HUD_HEIGHT,
            visible: false,
            daemon_available: false,
            exit: false,
            screenshot,
            screenshot_state,
            screenshot_done: false,
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
        self.clear_meter();
        self.hide_surface(" (daemon unavailable)");
    }

    /// Frame pacing: animate while the chip is on screen, otherwise idle at
    /// the status poll cadence.
    fn tick_interval(&self) -> Duration {
        if !self.visible {
            return POLL_INTERVAL;
        }
        // Meter ease benefits from ~60 fps; spinner/breathe are fine at 30.
        let meter_moving = (self.meter_armed && (self.meter_display - self.meter_to).abs() > 0.002)
            || self
                .meter_hold_until
                .is_some_and(|until| Instant::now() < until);
        if meter_moving {
            METER_FRAME_INTERVAL
        } else {
            FRAME_INTERVAL
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
                && matches!(next_state, UiState::Idle)
            {
                self.flash_until = Some(now + RESULT_FLASH);
                // Keep the daemon terminal message for logs/status; the chip
                // flash uses a short label (Success / notice text).
                self.flash_text = reply.last.clone();
                self.flash_ok = reply.last_ok.unwrap_or(false);
                let chip = if self.flash_ok {
                    "Success"
                } else {
                    self.flash_text.as_deref().unwrap_or("Notice")
                };
                tracing::info!(
                    "[HUD] state=idle result flash ok={} chip=\"{}\" detail_chars={}",
                    self.flash_ok,
                    chip,
                    self.flash_text
                        .as_deref()
                        .map(|s| s.chars().count())
                        .unwrap_or(0)
                );
            }
            self.previous_state = Some(next_kind);
        }
        self.state = next_state;
        if matches!(self.state, UiState::Idle) && self.flash_until.is_some_and(|until| now >= until)
        {
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
        // Screenshot hook: render exactly the requested state, settled
        // (progress 1.0, phase 0.0) and with no time-window flash fade, so
        // every run captures the same byte-identical frame.
        if let Some(state) = self.screenshot_state {
            let content = match state {
                ScreenshotState::Recording => (
                    "Listening…".to_owned(),
                    Some(format_elapsed(7)),
                    ChipKind::Recording,
                ),
                ScreenshotState::Transcribing => {
                    ("Transcribing…".to_owned(), None, ChipKind::Transcribing)
                }
                ScreenshotState::Cleaning => ("Cleaning…".to_owned(), None, ChipKind::Cleaning),
                ScreenshotState::Sent => ("Success".to_owned(), None, ChipKind::Sent),
                ScreenshotState::Notice => ("Heard nothing".to_owned(), None, ChipKind::Notice),
            };
            self.shown_kind = Some(content.2);
            return Some(ChipView {
                label: content.0,
                detail: content.1,
                kind: content.2,
                from: None,
                progress: 1.0,
                fade: 1.0,
                phase: 0.0,
                meter: None,
            });
        }
        let content = match &self.state {
            UiState::Idle => match self.flash_until {
                Some(until) if now < until => {
                    // Delivered dictations flash a short word; notices keep
                    // the operator-facing reason (Heard nothing, Cancelled).
                    let label = if self.flash_ok {
                        "Success".to_owned()
                    } else {
                        self.flash_text
                            .clone()
                            .unwrap_or_else(|| "Notice".to_owned())
                    };
                    let kind = if self.flash_ok {
                        ChipKind::Sent
                    } else {
                        ChipKind::Notice
                    };
                    let remaining = until.duration_since(now).as_secs_f32();
                    let fade = ease_out_cubic(remaining / FLASH_FADE_TAIL);
                    Some((label, None, kind, fade, None))
                }
                _ => None,
            },
            UiState::Recording { elapsed } => Some((
                "Listening…".to_owned(),
                Some(format_elapsed(*elapsed)),
                ChipKind::Recording,
                1.0,
                None,
            )),
            UiState::Processing { stage } => {
                let (label, kind, meter) = if stage == "cleaning" || stage.starts_with("cleaning") {
                    // Finish the bar through Cleaning when multi-chunk STT
                    // armed a meter — never snap-clear mid-ease.
                    let hold = self.meter_armed
                        || self.meter_display > 0.001
                        || self.meter_hold_until.is_some_and(|until| now < until);
                    (
                        "Cleaning…".to_owned(),
                        ChipKind::Cleaning,
                        if hold { Some(1.0) } else { None },
                    )
                } else if let Some(rest) = stage.strip_prefix("transcribing ") {
                    // "transcribing 2/5" from the daemon stage field.
                    (
                        format!("Transcribing… {rest}"),
                        ChipKind::Transcribing,
                        parse_chunk_meter(stage),
                    )
                } else {
                    ("Transcribing…".to_owned(), ChipKind::Transcribing, None)
                };
                Some((label, None, kind, 1.0, meter))
            }
        };
        let Some((label, detail, kind, fade, meter_target)) = content else {
            self.shown_kind = None;
            self.clear_meter();
            return None;
        };

        if self.shown_kind != Some(kind) {
            self.transition_from = self.shown_kind;
            self.transition_at = now;
            let entering_transcribing = matches!(kind, ChipKind::Transcribing)
                && !matches!(self.shown_kind, Some(ChipKind::Transcribing));
            self.shown_kind = Some(kind);
            // Only reset when a new transcription run begins. Leaving
            // Transcribing → Cleaning must keep the fill and complete to 1.0.
            if entering_transcribing {
                self.meter_display = 0.0;
                self.meter_from = 0.0;
                self.meter_to = 0.0;
                self.meter_ease_at = now;
                self.meter_armed = false;
                self.meter_hold_until = None;
            }
        }
        let meter = match meter_target {
            Some(target) if self.reduced_motion || self.screenshot.is_some() => {
                let target = target.clamp(0.0, 1.0);
                self.meter_display = target;
                self.meter_from = target;
                self.meter_to = target;
                if target > 0.0 {
                    self.meter_armed = true;
                }
                Some(target)
            }
            Some(target) => {
                let target = target.clamp(0.0, 1.0);
                if target > 0.0 {
                    self.meter_armed = true;
                }
                // New target: timed ease from the current display value.
                // First multi-chunk frame starts from empty (reset above).
                if (target - self.meter_to).abs() > 0.0005 {
                    self.meter_from = self.meter_display;
                    self.meter_to = target;
                    self.meter_ease_at = now;
                }
                let t = (now.duration_since(self.meter_ease_at).as_secs_f32()
                    / METER_EASE.as_secs_f32())
                .clamp(0.0, 1.0);
                let eased =
                    self.meter_from + (self.meter_to - self.meter_from) * ease_in_out_cubic(t);
                self.meter_display = eased.clamp(0.0, 1.0);
                // After we land on full, hold briefly so Cleaning does not
                // look empty if postproc is instant.
                if self.meter_to >= 0.999 && t >= 1.0 && self.meter_hold_until.is_none() {
                    self.meter_hold_until = Some(now + METER_COMPLETE_HOLD);
                }
                Some(self.meter_display)
            }
            None => {
                // Outcome flash / non-metered: drop only after any complete hold.
                if self.meter_hold_until.is_some_and(|until| now < until) {
                    self.meter_display = 1.0;
                    Some(1.0)
                } else {
                    self.clear_meter();
                    None
                }
            }
        };
        let eased = ease_out_cubic(
            now.duration_since(self.transition_at).as_secs_f32() / TRANSITION.as_secs_f32(),
        );
        // Reduced motion and screenshot mode swap states instantly: the
        // former disables entry animations, the latter ensures the first
        // redraw captures the settled frame (no quantization race).
        let progress = if self.reduced_motion || self.screenshot.is_some() {
            1.0
        } else {
            eased
        };
        // A 60s window keeps f32 phase math precise over long uptimes; the
        // breathing pulse period divides it, so motion never jumps. Reduced
        // motion and screenshot mode freeze the phase: the former renders a
        // static chip (and stops rerasterizing every frame), the latter
        // captures byte-identical frames independent of scheduling.
        let phase = if self.reduced_motion || self.screenshot.is_some() {
            0.0
        } else {
            (now.duration_since(self.started_at).as_secs_f64() % 60.0) as f32
        };
        Some(ChipView {
            label,
            detail,
            kind,
            from: self.transition_from,
            progress,
            fade,
            phase,
            meter,
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

        // Layout: fixed capsule geometry ("Warm Minimal") — it never
        // resizes with content, so state reads by composition, not by shape.
        let capsule_width = CAPSULE_WIDTH.min(width as f32 - 8.0);
        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0 - 1.0;
        let half_width = capsule_width / 2.0 * scale_factor;
        let half_height = CAPSULE_HEIGHT.min(height as f32 - 12.0) / 2.0 * scale_factor;
        // The opaque fill carries a whisper of the state accent, so the
        // pill body itself (not just the glyph) signals the current mode.
        let fill_rgb = match view.from {
            Some(from) if view.progress < 1.0 => {
                mix_rgb(capsule_blend(from), capsule_blend(view.kind), view.progress)
            }
            _ => capsule_blend(view.kind),
        };
        let fill = [fill_rgb[0], fill_rgb[1], fill_rgb[2], 255];
        pill(
            canvas,
            width,
            height,
            center_x,
            center_y,
            half_width,
            half_height,
            fill,
            visibility,
        );
        // Real multi-chunk STT fraction only — never a decorative fill.
        if let Some(meter) = view.meter.filter(|value| *value > 0.001) {
            let meter_rgb = mix_rgb(fill_rgb, accent, 0.42);
            let meter_fill = [meter_rgb[0], meter_rgb[1], meter_rgb[2], 255];
            pill_meter(
                canvas,
                width,
                height,
                center_x,
                center_y,
                half_width,
                half_height,
                meter.clamp(0.0, 1.0),
                meter_fill,
                visibility * 0.92,
            );
        }

        let left = center_x - half_width;
        let right = center_x + half_width;
        let content_alpha = visibility * swap;
        let glyph_x = left + GLYPH_CENTER_X * scale_factor;
        let accent_solid = [accent[0], accent[1], accent[2], 255];
        // Working states breathe (alpha/scale, localized to the glyph);
        // flashes are static compositions. Reduced motion freezes both.
        let (breathe_alpha, breathe_scale) = breathe(view.phase, self.reduced_motion);
        // Every state change eases the fresh glyph in from 62% scale
        // (ease-out-back, ~260ms). Reduced motion and screenshot mode run
        // at progress 1.0, so the composition is static and deterministic.
        let glyph_in = 0.62 + 0.38 * ease_out_back(view.progress);
        // The spinner turns only while live; reduced motion and the
        // screenshot hook freeze it as a calm full ring (an open static arc
        // could be misread as a partially-filled meter).
        let spinner_animated = !(self.reduced_motion || self.screenshot.is_some());
        match view.kind {
            ChipKind::Recording => circle(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                4.2 * scale_factor * breathe_scale * glyph_in,
                scale_alpha(accent_solid, content_alpha * breathe_alpha),
            ),
            ChipKind::Transcribing | ChipKind::Cleaning => spinner(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                4.8 * scale_factor * breathe_scale * glyph_in,
                2.2 * scale_factor,
                view.phase,
                spinner_animated,
                scale_alpha(accent_solid, content_alpha * breathe_alpha),
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
            ChipKind::Notice => slashed_ring(
                canvas,
                width,
                height,
                glyph_x,
                center_y,
                scale_factor * glyph_in,
                scale_alpha(accent_solid, content_alpha),
            ),
        }

        let label_px = PxScale::from(LABEL_SIZE * scale_factor);
        let metrics = fonts.strong.as_scaled(label_px);
        let baseline = center_y - metrics.height() / 2.0 + metrics.ascent();
        // The stage word drifts up ~3px while it fades in on every state
        // change; reduced motion and screenshot run at progress 1.0, so the
        // composition settles at the same baseline on every platform.
        let label_y = baseline + 3.0 * (1.0 - ease_out_cubic(view.progress));
        // The stage word is the centered anchor, clamped to the zone between
        // the glyph and the trailing timer; overlong words truncate. The
        // zone constants scale with the pop-in so the whole composition
        // stays proportional.
        let left_zone = WORD_AREA_LEFT * scale_factor;
        let right_zone = WORD_AREA_RIGHT * scale_factor;
        let word_budget = (right - left) - left_zone - right_zone;
        let label = fit_text_measured(
            |candidate| measure_text(&fonts.strong, candidate, label_px),
            &view.label,
            word_budget,
        );
        let label_width = measure_text(&fonts.strong, &label, label_px);
        let label_x = (center_x - label_width / 2.0).clamp(
            left + left_zone,
            (right - right_zone - label_width).max(left + left_zone),
        );
        draw_text(
            canvas,
            width,
            height,
            &fonts.strong,
            &label,
            label_x,
            label_y,
            label_px,
            scale_alpha(TEXT_PRIMARY, content_alpha),
        );
        if let Some(detail) = view.detail.as_deref() {
            let detail_px = PxScale::from(DETAIL_SIZE * scale_factor);
            // Right-aligned timer, clamped so it can never enter the glyph
            // zone even if the mono face runs wide.
            let detail_x =
                (right - PAD_RIGHT * scale_factor - measure_text(&fonts.mono, detail, detail_px))
                    .max(left + left_zone);
            draw_text(
                canvas,
                width,
                height,
                &fonts.mono,
                detail,
                detail_x,
                baseline,
                detail_px,
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

        // Screenshot mode: once the pill has finished its pop-in, dump the
        // frame and exit. The buffer is premultiplied ARGB; convert to
        // straight RGBA so the PNG shows the intended colors.
        //
        // Capture at 0.99, not 1.0: the render key quantizes progress to a
        // byte, so the draw at progress == 1.0 is skipped as a duplicate of
        // the 0.9987 frame (rounds to the same key) — waiting for it would
        // hang the hook. The 0.99+ frame is visually settled.
        if !self.screenshot_done && view.progress >= 0.99 {
            let path = match &self.screenshot {
                Some(path) => path.clone(),
                None => return Ok(true),
            };
            self.screenshot_done = true;
            let mut rgba = Vec::with_capacity(canvas.len());
            for pixel in canvas.chunks_exact(4) {
                let (b, g, r, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
                if a == 0 {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                } else {
                    let scale = 255.0 / a as f32;
                    let un = |channel: u8| ((channel as f32 * scale).round() as u16).min(255) as u8;
                    rgba.extend_from_slice(&[un(r), un(g), un(b), a]);
                }
            }
            match image::save_buffer(&path, &rgba, width, height, image::ColorType::Rgba8)
                .with_context(|| format!("writing {}", path.display()))
            {
                Ok(()) => {
                    eprintln!("saved HUD screenshot to {}", path.display());
                    std::process::exit(0);
                }
                Err(error) => {
                    eprintln!("HUD screenshot save failed: {error:#}");
                    std::process::exit(1);
                }
            }
        }
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

/// A state to render deterministically with `--screenshot` (the visual-test
/// hook), for verifying every composition offline. Defaults to Recording
/// when the hook runs without `--state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScreenshotState {
    Recording,
    Transcribing,
    Cleaning,
    Sent,
    Notice,
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
    /// Wrapped seconds driving the continuous breathing pulse.
    phase: f32,
    /// Determinate capsule fill 0..=1 from multi-chunk STT; None = no meter.
    meter: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderKey {
    label: String,
    detail: Option<String>,
    kind: ChipKind,
    progress: u8,
    fade: u8,
    phase: u16,
    meter: u8,
    width: u32,
    height: u32,
}

impl RenderKey {
    fn from_view(view: &ChipView, width: u32, height: u32) -> Self {
        // Continuous motion only exists for these kinds; a result flash is
        // static once its transition and fade are settled. Meter quantize
        // forces redraws while the fill eases toward the latest chunk.
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
            meter: view
                .meter
                .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
                .unwrap_or(0),
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
fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

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

/// Capsule: a plain opaque rounded-rectangle pill ("Warm Minimal" —
/// borderless, no rim light, no underglow, no drop shadow). The fill is
/// already the fully opaque tint blend; `alpha` is the global visibility.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn pill(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    fill: [u8; 4],
    alpha: f32,
) {
    let radius = half_height;
    let flat = (half_width - radius).max(0.0);
    let min_x = (center_x - half_width - 1.0).max(0.0) as u32;
    let max_x = (center_x + half_width + 1.0).min(width as f32) as u32;
    let min_y = (center_y - half_height - 1.0).max(0.0) as u32;
    let max_y = (center_y + half_height + 1.0).min(height as f32) as u32;
    let fill = scale_alpha(fill, alpha);
    for y in min_y..max_y {
        let dy = y as f32 + 0.5 - center_y;
        for x in min_x..max_x {
            let dx = x as f32 + 0.5 - center_x;
            let qx = (dx.abs() - flat).max(0.0);
            let signed = (qx * qx + dy * dy).sqrt() - radius;
            let coverage = (0.5 - signed).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, fill, coverage);
            }
        }
    }
}

/// Left-to-right fill clipped to the capsule SDF. `meter` is 0..=1 of the
/// capsule width; used only for measured multi-chunk transcription.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn pill_meter(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    meter: f32,
    fill: [u8; 4],
    alpha: f32,
) {
    let meter = meter.clamp(0.0, 1.0);
    if meter <= 0.0 {
        return;
    }
    let radius = half_height;
    let flat = (half_width - radius).max(0.0);
    let left = center_x - half_width;
    let fill_right = left + (half_width * 2.0) * meter;
    let min_x = (center_x - half_width - 1.0).max(0.0) as u32;
    let max_x = fill_right.min(width as f32).max(0.0) as u32;
    let min_y = (center_y - half_height - 1.0).max(0.0) as u32;
    let max_y = (center_y + half_height + 1.0).min(height as f32) as u32;
    let fill = scale_alpha(fill, alpha);
    for y in min_y..max_y {
        let dy = y as f32 + 0.5 - center_y;
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            if px > fill_right {
                continue;
            }
            let dx = px - center_x;
            let qx = (dx.abs() - flat).max(0.0);
            let signed = (qx * qx + dy * dy).sqrt() - radius;
            let coverage = (0.5 - signed).clamp(0.0, 1.0);
            // Soft edge at the moving fill front.
            let edge = (fill_right - px + 0.5).clamp(0.0, 1.0);
            let coverage = coverage * edge;
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, fill, coverage);
            }
        }
    }
}

/// Parse `transcribing N/M` into a 0..=1 fraction. Single-chunk and bare
/// `transcribing` return None so the HUD keeps the indeterminate spinner.
fn parse_chunk_meter(stage: &str) -> Option<f32> {
    let rest = stage.strip_prefix("transcribing ")?;
    let (chunk, total) = rest.split_once('/')?;
    let chunk: u32 = chunk.parse().ok()?;
    let total: u32 = total.parse().ok()?;
    if total <= 1 || chunk == 0 {
        return None;
    }
    Some((chunk as f32 / total as f32).clamp(0.0, 1.0))
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

/// Uniform round-capped ring, used by the notice's slashed-ring glyph.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn ring(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    radius: f32,
    thickness: f32,
    color: [u8; 4],
) {
    let half = thickness / 2.0;
    let reach = radius + half + 1.0;
    let min_x = (center_x - reach).max(0.0) as u32;
    let max_x = (center_x + reach).min(width as f32) as u32;
    let min_y = (center_y - reach).max(0.0) as u32;
    let max_y = (center_y + reach).min(height as f32) as u32;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let dx = px - center_x;
            let dy = py - center_y;
            let distance = (dx * dx + dy * dy).sqrt();
            let coverage = (half + 0.5 - (distance - radius).abs()).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, color, coverage);
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

/// Indeterminate spinner glyph (transcribing): a round-capped open arc that
/// rotates a full turn without ever completing — "busy, unmeasured", never
/// a determinate meter. The rotation angle is a pure function of `phase`,
/// so a frozen phase draws a fixed, byte-identical frame. With
/// `animated == false` (reduced motion, screenshot) it draws the plain
/// uniform `ring`: a static open arc could be misread as a partially-filled
/// progress ring.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin, phase)
fn spinner(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    radius: f32,
    thickness: f32,
    phase: f32,
    animated: bool,
    color: [u8; 4],
) {
    if !animated {
        return ring(
            canvas, width, height, center_x, center_y, radius, thickness, color,
        );
    }
    /// Arc length of the spinner in radians: a 264° sweep leaves a clear
    /// gap, unmistakably a spinner rather than a ring.
    const SWEEP: f32 = 4.608;
    const SEGMENTS: usize = 30;
    let start = (phase / SPIN_PERIOD).fract() * std::f32::consts::TAU;
    for segment_index in 0..SEGMENTS {
        let t0 = start + SWEEP * (segment_index as f32 / SEGMENTS as f32);
        let t1 = start + SWEEP * ((segment_index + 1) as f32 / SEGMENTS as f32);
        segment(
            canvas,
            width,
            height,
            center_x + radius * t0.cos(),
            center_y + radius * t0.sin(),
            center_x + radius * t1.cos(),
            center_y + radius * t1.sin(),
            thickness,
            color,
        );
    }
}

/// Slashed ring glyph (notice): a uniform ring with a diagonal slash. Reads
/// as "rejected / nothing heard" by shape, not color.
fn slashed_ring(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    scale: f32,
    color: [u8; 4],
) {
    ring(
        canvas,
        width,
        height,
        center_x,
        center_y,
        5.2 * scale,
        2.2 * scale,
        color,
    );
    segment(
        canvas,
        width,
        height,
        center_x - 4.6 * scale,
        center_y - 4.6 * scale,
        center_x + 4.6 * scale,
        center_y + 4.6 * scale,
        2.0 * scale,
        color,
    );
}

/// Localized "alive" pulse for the working glyphs: alpha between
/// `BREATHE_MIN` and 1.0, scale between 0.94 and 1.0. With reduced motion
/// the pulse freezes at full strength. This is the only pulse of the
/// working states — it never travels or grows across the capsule (the
/// spinner's turn is separate motion, see `spinner`).
fn breathe(phase: f32, reduced_motion: bool) -> (f32, f32) {
    if reduced_motion {
        return (1.0, 1.0);
    }
    let k = pulse((phase / PULSE_PERIOD).fract());
    (BREATHE_MIN + (1.0 - BREATHE_MIN) * k, 0.94 + 0.06 * k)
}

/// Opaque capsule fill for a chip kind: `FLOOR` mixed with the state accent
/// at the "Warm Minimal" tint ratio. The pill body (not just the glyph)
/// carries the mode, and it never claims progress.
fn capsule_blend(kind: ChipKind) -> [u8; 3] {
    let ratio = match kind {
        ChipKind::Recording => 0.15,
        ChipKind::Transcribing | ChipKind::Cleaning => 0.13,
        ChipKind::Sent => SENT_TINT,
        ChipKind::Notice => NOTICE_TINT,
    };
    mix_rgb(FLOOR, accent_color(kind), ratio)
}

/// Shorten `text` with a trailing ellipsis so it fits `max_width` at
/// `scale`, using `measure` for widths (injectable for tests). Borrows the
/// input when it already fits — the render path allocates only for
/// overlong words. Returns empty when not even the ellipsis fits.
fn fit_text_measured<'a, F: Fn(&str) -> f32>(
    measure: F,
    text: &'a str,
    max_width: f32,
) -> Cow<'a, str> {
    if max_width <= 0.0 {
        return Cow::Borrowed("");
    }
    if measure(text) <= max_width {
        return Cow::Borrowed(text);
    }
    const ELLIPSIS: &str = "…";
    let ellipsis_width = measure(ELLIPSIS);
    if ellipsis_width > max_width {
        return Cow::Borrowed("");
    }
    let budget = max_width - ellipsis_width;
    let mut result = String::new();
    let mut width = 0.0;
    for character in text.chars() {
        let char_width = measure(&character.to_string());
        if width + char_width > budget {
            break;
        }
        result.push(character);
        width += char_width;
    }
    result.push_str(ELLIPSIS);
    Cow::Owned(result)
}

/// Read the desktop's animation preference once at startup. Standard
/// GNOME/COSMIC key; without gsettings (or on a desktop that does not
/// expose it) animations stay on.
fn prefers_reduced_motion() -> bool {
    let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).trim() == "false"
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

/// Medium ("strong") UI face for the stage word plus a monospace face for
/// the tabular elapsed timer; both fall back to the regular face.
#[derive(Clone)]
struct Fonts {
    strong: FontArc,
    mono: FontArc,
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
/// Monospace faces give the timer true tabular digits on every platform.
const UI_FONT_MONO_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf",
    "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansMono-Regular.otf",
];

fn load_fonts() -> Option<Fonts> {
    let regular = first_font(UI_FONT_CANDIDATES).or_else(fallback_font)?;
    let strong = first_font(UI_FONT_STRONG_CANDIDATES).unwrap_or_else(|| regular.clone());
    let mono = first_font(UI_FONT_MONO_CANDIDATES).unwrap_or(regular);
    Some(Fonts { strong, mono })
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
    use super::{
        acquire_lock_on, breathe, capsule_blend, ease_in_out_cubic, ease_out_back, ease_out_cubic,
        fit_text_measured, format_elapsed, mix_rgb, parse_chunk_meter, pill, pill_meter, pulse,
        spinner, ChipKind, BREATHE_MIN,
    };
    use std::fs;
    use std::path::PathBuf;

    fn lock_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("cantrip-hud-lock-test-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        path
    }

    #[test]
    fn lock_is_exclusive_until_the_file_drops() {
        let path = lock_path();
        let first = acquire_lock_on(&path).expect("first lock should succeed");
        assert!(first.is_some());

        // A second open of the same inode must contend (flock is per fd,
        // not per process), so a duplicate HUD instance cannot start.
        let second = acquire_lock_on(&path).expect("lock check should not error");
        assert!(second.is_none());

        // Dropping the file releases the lock: the daemon can respawn.
        drop(first);
        let third = acquire_lock_on(&path).expect("re-lock after drop should succeed");
        assert!(third.is_some());
        drop(third);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn formats_elapsed_as_minutes_and_seconds() {
        assert_eq!(format_elapsed(0), "00:00");
        assert_eq!(format_elapsed(12), "00:12");
        assert_eq!(format_elapsed(125), "02:05");
    }

    #[test]
    fn ease_in_out_cubic_is_symmetric_and_smooth() {
        assert!(ease_in_out_cubic(-1.0).abs() < 1e-6);
        assert!((ease_in_out_cubic(1.0) - 1.0).abs() < 1e-6);
        assert!((ease_in_out_cubic(0.5) - 0.5).abs() < 1e-5);
        // Mid slope gentler than ease-out at t=0.25 (less front-loaded).
        assert!(
            ease_in_out_cubic(0.25) < ease_out_cubic(0.25),
            "in-out should lag ease-out early"
        );
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

    #[test]
    fn capsule_blend_matches_warm_minimal_tint_blends() {
        // Floor #0e0e11 mixed with the state accent at the locked ratios:
        // recording 0.15, transcribing/cleaning 0.13, sent 0.45, notice 0.06.
        assert_eq!(capsule_blend(ChipKind::Recording), [50, 28, 28]);
        assert_eq!(capsule_blend(ChipKind::Transcribing), [45, 36, 24]);
        assert_eq!(capsule_blend(ChipKind::Cleaning), [37, 31, 48]);
        assert_eq!(capsule_blend(ChipKind::Sent), [60, 107, 77]);
        assert_eq!(capsule_blend(ChipKind::Notice), [28, 24, 22]);
    }

    #[test]
    fn breathe_stays_localized_and_freezes_with_reduced_motion() {
        for step in 0..=120 {
            let phase = step as f32 / 120.0 * 60.0;
            let (alpha, scale) = breathe(phase, false);
            assert!((BREATHE_MIN..=1.0).contains(&alpha));
            assert!((0.94..=1.0).contains(&scale));
        }
        assert_eq!(breathe(0.0, true), (1.0, 1.0));
        assert_eq!(breathe(123.4, true), (1.0, 1.0));
        // Periodicity: one wrapped 60s phase must repeat the same pulse
        // every PULSE_PERIOD (2s), not drift with the raw phase value.
        assert_eq!(breathe(1.0, false), breathe(3.0, false));
        assert_eq!(breathe(0.5, false), breathe(2.5, false));
    }

    #[test]
    fn fit_text_truncates_with_ellipsis_within_budget() {
        // Fake measure: 4.0 units per character, ellipsis included.
        let measure = |text: &str| text.chars().count() as f32 * 4.0;
        assert_eq!(fit_text_measured(measure, "short", 200.0), "short");
        assert_eq!(fit_text_measured(measure, "longword", 20.0), "long…");
        assert_eq!(fit_text_measured(measure, "longword", 0.0), "");
        // Narrower than the ellipsis itself: nothing can fit, return empty
        // rather than overflowing the declared width.
        assert_eq!(fit_text_measured(measure, "longword", 2.0), "");
        // First character already exceeds the budget: drop it and keep only
        // the ellipsis — never admit text beyond the declared width.
        assert_eq!(fit_text_measured(measure, "longword", 5.0), "…");
    }

    #[test]
    fn parse_chunk_meter_only_for_multi_chunk_stages() {
        assert_eq!(parse_chunk_meter("transcribing"), None);
        assert_eq!(parse_chunk_meter("transcribing 1/1"), None);
        assert_eq!(parse_chunk_meter("cleaning"), None);
        assert_eq!(parse_chunk_meter("transcribing 1/4"), Some(0.25));
        assert_eq!(parse_chunk_meter("transcribing 2/5"), Some(0.4));
        assert_eq!(parse_chunk_meter("transcribing 5/5"), Some(1.0));
        assert_eq!(parse_chunk_meter("transcribing 0/3"), None);
    }

    #[test]
    fn pill_meter_lights_left_side_only() {
        let mut canvas = [0u8; 80 * 40 * 4];
        pill(
            &mut canvas,
            80,
            40,
            40.0,
            20.0,
            30.0,
            12.0,
            [20, 20, 20, 255],
            1.0,
        );
        pill_meter(
            &mut canvas,
            80,
            40,
            40.0,
            20.0,
            30.0,
            12.0,
            0.5,
            [200, 100, 40, 255],
            1.0,
        );
        let left = canvas[(20 * 80 + 20) as usize * 4];
        let right = canvas[(20 * 80 + 55) as usize * 4];
        assert!(
            left > right,
            "meter must light the left half more than the right (left={left} right={right})"
        );
    }

    fn alpha_at(canvas: &[u8], width: u32, x: u32, y: u32) -> u8 {
        canvas[(y * width + x) as usize * 4 + 3]
    }

    #[test]
    fn spinner_rotation_is_periodic_in_phase() {
        // The spinner must be a pure function of `phase` so the 60s phase
        // window never drifts: one SPIN_PERIOD later draws the same frame.
        let mut first = [0u8; 32 * 32 * 4];
        spinner(
            &mut first,
            32,
            32,
            16.0,
            16.0,
            4.8,
            2.2,
            1.7,
            true,
            [255, 255, 255, 255],
        );
        let mut second = [0u8; 32 * 32 * 4];
        spinner(
            &mut second,
            32,
            32,
            16.0,
            16.0,
            4.8,
            2.2,
            2.5,
            true,
            [255, 255, 255, 255],
        );
        assert_eq!(first, second, "one SPIN_PERIOD later must repeat the frame");
        // A different phase inside the period must rotate the arc.
        let mut third = [0u8; 32 * 32 * 4];
        spinner(
            &mut third,
            32,
            32,
            16.0,
            16.0,
            4.8,
            2.2,
            2.0,
            true,
            [255, 255, 255, 255],
        );
        assert_ne!(first, third, "mid-turn phases must differ");
    }

    #[test]
    fn spinner_frozen_draws_a_full_ring() {
        // Reduced motion / screenshot freeze the spinner: the frozen glyph
        // must be phase-independent (byte-identical) and cover the full
        // circle rather than a static open arc (which could read as a
        // partially-filled meter).
        let mut first = [0u8; 32 * 32 * 4];
        spinner(
            &mut first,
            32,
            32,
            16.0,
            16.0,
            4.8,
            2.2,
            0.0,
            false,
            [255, 255, 255, 255],
        );
        let mut second = [0u8; 32 * 32 * 4];
        spinner(
            &mut second,
            32,
            32,
            16.0,
            16.0,
            4.8,
            2.2,
            9.3,
            false,
            [255, 255, 255, 255],
        );
        assert_eq!(first, second);
        // The ring must be lit all the way around (top, right, bottom, left).
        for (x, y) in [(16, 11), (21, 16), (16, 21), (11, 16)] {
            assert!(
                alpha_at(&first, 32, x, y) > 0,
                "frozen ring must cover ({x},{y})"
            );
        }
    }
}
