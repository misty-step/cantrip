//! Always-on-top Wayland layer-shell status HUD.
//!
//! The HUD is a read-only mirror of the daemon. It polls the existing status
//! command and never sends a command which can change daemon state.
//!
//! Visual design: a 22-cell knight track floats on a fully transparent
//! 420×56 surface. Cells carry the current state accent: eased waveform
//! energy while recording, an amber sweep and chunk progress while
//! transcribing, static violet mid-cells while cleaning, and full green or
//! amber holds for terminal results. Reduced motion freezes the sweep at its
//! midpoint while visibility and result fades still apply.
//!
//! ADR 0010 is superseded by operator direction on 2026-09-04: the former
//! capsule, text, timer, meter overlay, and perimeter trace were replaced by
//! this track-only chip.

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

use crate::ipc::{self, AudioWaveform, StatusSnapshot, TerminalOutcome, AUDIO_WAVEFORM_BINS};
use crate::pipeline::Stage;

/// Status poll cadence. Matches the daemon's 100 ms signal sampling so each
/// poll carries a fresh envelope; the round-trip is millisecond-scale.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Render tick while the chip is visible; IPC polling stays at POLL_INTERVAL.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Higher cadence while measured waveform or chunk-meter values are easing
/// (~16 ms ≈ 60 fps).
const METER_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const RESULT_FLASH: Duration = Duration::from_millis(2_500);
/// Duration of the eased transition run on every visual state change.
const TRANSITION: Duration = Duration::from_millis(260);
/// Wall time to ease the chunk meter between targets. Chunk inference is
/// often <300 ms, so a timed ease always shows motion. Uses ease-in-out
/// for a steadier native feel than ease-out (which front-loads then lags).
const METER_EASE: Duration = Duration::from_millis(360);
/// Short interpolation between measured 200 ms waveform frames. The ease
/// completes well before the next frame lands, so motion settles instead of
/// dragging behind the data. It smooths the raster transition without
/// inventing any unmeasured oscillation.
const WAVEFORM_EASE: Duration = Duration::from_millis(90);
/// Extra hold after the bar reaches full while Cleaning so a fast 2-chunk
/// take does not wipe the fill the instant STT ends.
const METER_COMPLETE_HOLD: Duration = Duration::from_millis(180);
/// Tail of the result flash spent fading out, inside the RESULT_FLASH window.
const FLASH_FADE_TAIL: f32 = 0.25;
const HUD_HEIGHT: u32 = 56;
const FALLBACK_WIDTH: u32 = 420;
const MAX_WIDTH: u32 = 900;
const SCREENSHOT_WAVEFORM: AudioWaveform = [
    [-18, 22],
    [-35, 41],
    [-64, 72],
    [-48, 55],
    [-86, 94],
    [-61, 68],
    [-29, 36],
    [-70, 78],
    [-45, 51],
    [-25, 33],
    [-12, 18],
];

/// Fixed capsule container dimensions ("Warm Minimal", centered in the 420×56 surface).
/// Sizing is rock-solid and identical across all states.
const CONTAINER_WIDTH: f32 = 336.0;
const CONTAINER_HEIGHT: f32 = 44.0;
const TRACK_WIDTH: f32 = 304.0;
const KNIGHT_CELL_GAP: f32 = 2.0;
/// Number of segmented cells in the full-width scanner track.
const KNIGHT_CELLS: usize = 22;

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
    layer.set_anchor(Anchor::BOTTOM);
    layer.set_margin(0, 0, 36, 0);
    // wlroots rejects a zero width with only the BOTTOM anchor. A fixed width
    // keeps the surface bottom-centered for the fixed-size capsule inside.
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
    tracing::info!("[HUD] layer surface created (overlay, bottom-center)");

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
    state: UiState,
    previous_state: Option<UiStateKind>,
    /// Last terminal payload observed from the daemon. It is sticky in the
    /// status stream, so only a changed payload should retrigger the flash.
    last_outcome: Option<TerminalOutcome>,
    outcome_seen: bool,
    /// When the current result flash expires; None means no flash.
    flash_until: Option<Instant>,
    /// Operator-facing reason shown while a notice flash is live.
    flash_text: Option<String>,
    /// True when the flash reports a delivered dictation.
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
    /// Measured envelope at the start and end of the current short visual ease.
    waveform_from: [[f32; 2]; AUDIO_WAVEFORM_BINS],
    waveform_to: [[f32; 2]; AUDIO_WAVEFORM_BINS],
    waveform_ease_at: Instant,
    /// Desktop animations disabled (gsettings enable-animations=false):
    /// freezes the scanner phase and skips entry motion.
    reduced_motion: bool,
    /// Output scale used for the wl_shm buffer and every device-space draw.
    buffer_scale: u32,
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
    fn clear_waveform(&mut self, now: Instant) {
        self.waveform_from = [[0.0; 2]; AUDIO_WAVEFORM_BINS];
        self.waveform_to = [[0.0; 2]; AUDIO_WAVEFORM_BINS];
        self.waveform_ease_at = now;
    }

    fn waveform_frame(&self, now: Instant) -> AudioWaveform {
        let t = if self.reduced_motion || self.screenshot.is_some() {
            1.0
        } else {
            (now.duration_since(self.waveform_ease_at).as_secs_f32() / WAVEFORM_EASE.as_secs_f32())
                .clamp(0.0, 1.0)
        };
        interpolate_waveform(self.waveform_from, self.waveform_to, ease_in_out_cubic(t))
    }

    fn set_waveform_target(&mut self, target: AudioWaveform, now: Instant) {
        let target = target.map(|bin| bin.map(f32::from));
        if self.waveform_to == target {
            return;
        }
        self.waveform_from = self.waveform_frame(now).map(|bin| bin.map(f32::from));
        self.waveform_to = target;
        self.waveform_ease_at = now;
    }

    fn waveform_moving(&self, now: Instant) -> bool {
        !self.reduced_motion
            && self.screenshot.is_none()
            && self.waveform_from != self.waveform_to
            && now.duration_since(self.waveform_ease_at) < WAVEFORM_EASE
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
            UiState::Recording {
                elapsed: 7,
                audio_waveform: Some(SCREENSHOT_WAVEFORM),
                audio_silent: false,
            }
        } else {
            UiState::Idle
        };
        Self {
            registry_state,
            output_state,
            shm,
            pool,
            layer,
            state,
            previous_state: None,
            last_outcome: None,
            outcome_seen: false,
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
            waveform_from: [[0.0; 2]; AUDIO_WAVEFORM_BINS],
            waveform_to: [[0.0; 2]; AUDIO_WAVEFORM_BINS],
            waveform_ease_at: Instant::now(),
            reduced_motion,
            buffer_scale: 1,
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
    /// Logical surface size for one frame: the configured size, clamped.
    fn frame_size(&self) -> (u32, u32) {
        (
            self.width.clamp(FALLBACK_WIDTH.min(200), MAX_WIDTH),
            self.height.max(HUD_HEIGHT),
        )
    }

    /// Device-pixel buffer size advertised through wl_shm.
    fn buffer_size(&self) -> Result<(u32, u32)> {
        let (width, height) = self.frame_size();
        let width = width
            .checked_mul(self.buffer_scale)
            .context("calculating HUD buffer width")?;
        let height = height
            .checked_mul(self.buffer_scale)
            .context("calculating HUD buffer height")?;
        Ok((width, height))
    }

    fn poll_status(&mut self) {
        match ipc::status() {
            Ok(status) => {
                if !self.daemon_available {
                    tracing::info!("[HUD] daemon status stream available");
                    self.daemon_available = true;
                }
                self.apply_status(&status, Instant::now());
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
        self.clear_waveform(Instant::now());
        self.hide_surface(" (daemon unavailable)");
    }

    /// Frame pacing: animate while the chip is on screen, otherwise idle at
    /// the status poll cadence.
    fn tick_interval(&self) -> Duration {
        if !self.visible {
            return POLL_INTERVAL;
        }
        let now = Instant::now();
        let recording = matches!(self.state, UiState::Recording { .. });
        let processing = matches!(self.state, UiState::Processing { .. });
        let outcome = matches!(self.state, UiState::Idle) && self.flash_until.is_some();
        let meter_moving = (self.meter_armed && (self.meter_display - self.meter_to).abs() > 0.002)
            || self.meter_hold_until.is_some_and(|until| now < until);
        if recording || processing || outcome || meter_moving || self.waveform_moving(now) {
            METER_FRAME_INTERVAL
        } else {
            FRAME_INTERVAL
        }
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
        let (width, height) = self.buffer_size()?;
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

    fn apply_status(&mut self, status: &StatusSnapshot, now: Instant) {
        if let StatusSnapshot::Recording {
            signal: Some(signal),
            ..
        } = status
        {
            self.set_waveform_target(signal.waveform, now);
        } else if !matches!(status, StatusSnapshot::Recording { .. }) {
            self.clear_waveform(now);
        }

        let outcome = status.outcome();
        // The daemon keeps the terminal payload in every later status reply.
        // Seed the edge tracker silently, then flash only on a newly changed
        // payload; in particular this catches idle → idle rejection outcomes.
        let payload_changed = self.outcome_seen && self.last_outcome.as_ref() != outcome;
        let outcome_changed = payload_changed && outcome.is_some();
        if !self.outcome_seen || payload_changed {
            self.last_outcome = outcome.cloned();
        }
        self.outcome_seen = true;

        let next_state = UiState::from_status(status);
        let next_kind = next_state.kind();
        let returned_to_idle = self
            .previous_state
            .is_some_and(|state| state != UiStateKind::Idle)
            && matches!(next_state, UiState::Idle);
        if self.previous_state != Some(next_kind) {
            match &next_state {
                UiState::Idle => tracing::info!("[HUD] state=idle"),
                UiState::Recording {
                    elapsed,
                    audio_silent,
                    ..
                } => {
                    tracing::info!(
                        "[HUD] state=recording elapsed={elapsed}s audio_silent={audio_silent}"
                    )
                }
                UiState::Processing { stage } => {
                    tracing::info!("[HUD] state=processing stage={stage}")
                }
            }
            self.previous_state = Some(next_kind);
        }

        if returned_to_idle || outcome_changed {
            self.flash_until = Some(now + RESULT_FLASH);
            // Keep the daemon terminal message for logs/status; the chip
            // flash uses a short label (Success / notice text).
            self.flash_text = status.outcome().map(|outcome| outcome.message.clone());
            self.flash_ok = status.outcome().is_some_and(|outcome| outcome.ok);
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
            .map(|view| RenderKey::from_view(view, self.width, self.height, self.buffer_scale));
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
                    Some(SCREENSHOT_WAVEFORM),
                ),
                ScreenshotState::NoSignal => (
                    "No mic signal".to_owned(),
                    Some(format_elapsed(7)),
                    ChipKind::NoSignal,
                    Some([[0, 0]; AUDIO_WAVEFORM_BINS]),
                ),
                ScreenshotState::Transcribing => (
                    "Transcribing…".to_owned(),
                    None,
                    ChipKind::Transcribing,
                    None,
                ),
                ScreenshotState::Cleaning => {
                    ("Cleaning…".to_owned(), None, ChipKind::Cleaning, None)
                }
                ScreenshotState::Sent => ("Success".to_owned(), None, ChipKind::Sent, None),
                ScreenshotState::Notice => {
                    ("Heard nothing".to_owned(), None, ChipKind::Notice, None)
                }
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
                waveform: content.3,
            });
        }
        let waveform_frame = self.waveform_frame(now);
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
                    Some((label, None, kind, fade, None, None))
                }
                _ => None,
            },
            UiState::Recording {
                elapsed,
                audio_waveform,
                audio_silent,
            } => Some((
                if *audio_silent {
                    "No mic signal".to_owned()
                } else {
                    "Listening…".to_owned()
                },
                Some(format_elapsed(*elapsed)),
                if *audio_silent {
                    ChipKind::NoSignal
                } else {
                    ChipKind::Recording
                },
                1.0,
                None,
                audio_waveform.map(|_| waveform_frame),
            )),
            UiState::Processing { stage } => {
                let keep_meter = self.meter_armed
                    || self.meter_display > 0.001
                    || self.meter_hold_until.is_some_and(|until| now < until);
                let (label, kind, meter) = processing_chip_content(stage, keep_meter);
                Some((label, None, kind, 1.0, meter, None))
            }
        };
        let Some((label, detail, kind, fade, meter_target, waveform)) = content else {
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
        // scanner periods divide it, so motion never jumps. Reduced motion
        // and screenshot mode freeze the phase: the former renders a static
        // chip (and stops rerasterizing every frame), the latter captures
        // byte-identical frames independent of scheduling.
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
            waveform,
        })
    }

    fn draw(&mut self, view: &ChipView) -> Result<bool> {
        let (width, height) = self.buffer_size()?;
        let (logical_width, _) = self.frame_size();
        let output_scale = self.buffer_scale as f32;
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
        let scale_factor = if view.from.is_none() {
            0.94 + 0.06 * appear
        } else {
            1.0
        };

        let container_width = CONTAINER_WIDTH.min(logical_width as f32 - 8.0) * output_scale;
        let container_height = CONTAINER_HEIGHT * output_scale;
        let center_x = width as f32 / 2.0;
        let center_y = height as f32 / 2.0;
        let half_width = (container_width / 2.0) * scale_factor;
        let half_height = (container_height / 2.0) * scale_factor;
        let content_alpha = visibility * swap;

        // 1. Background container: sharp rectangular box ("Omarchy Aesthetic", 100% opaque floor + 1px border)
        rect_container(
            canvas,
            width,
            height,
            center_x,
            center_y,
            half_width,
            half_height,
            view.kind,
            output_scale * scale_factor,
            visibility,
        );

        // 2. 22-cell segmented track inside the container:
        let progress_param = match view.kind {
            ChipKind::Transcribing => view.meter,
            _ => None,
        };

        knight_track(
            canvas,
            width,
            height,
            center_x,
            center_y,
            half_width,
            half_height,
            output_scale * scale_factor,
            view.kind,
            view.from,
            view.progress,
            view.phase,
            view.waveform,
            progress_param,
            content_alpha,
        );

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
            for pixel in canvas.as_chunks::<4>().0 {
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
    Recording {
        elapsed: u64,
        audio_waveform: Option<AudioWaveform>,
        audio_silent: bool,
    },
    Processing {
        stage: Stage,
    },
}

impl UiState {
    fn from_status(status: &StatusSnapshot) -> Self {
        match status {
            StatusSnapshot::Recording {
                elapsed, signal, ..
            } => Self::Recording {
                elapsed: *elapsed,
                audio_waveform: signal.map(|signal| signal.waveform),
                audio_silent: signal.is_some_and(|signal| signal.silent),
            },
            StatusSnapshot::Processing { stage, .. } => Self::Processing {
                stage: stage.clone(),
            },
            StatusSnapshot::Idle { .. } | StatusSnapshot::Unknown { .. } => Self::Idle,
        }
    }

    fn kind(&self) -> UiStateKind {
        match self {
            Self::Idle => UiStateKind::Idle,
            Self::Recording {
                audio_silent: true, ..
            } => UiStateKind::NoSignal,
            Self::Recording { .. } => UiStateKind::Recording,
            Self::Processing {
                stage: Stage::CleaningUp,
            } => UiStateKind::Cleaning,
            Self::Processing { .. } => UiStateKind::Transcribing,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiStateKind {
    Idle,
    Recording,
    NoSignal,
    Transcribing,
    Cleaning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChipKind {
    Sent,
    Notice,
    Recording,
    NoSignal,
    Transcribing,
    Cleaning,
}

/// A state to render deterministically with `--screenshot` (the visual-test
/// hook), for verifying every composition offline. Defaults to Recording
/// when the hook runs without `--state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ScreenshotState {
    Recording,
    NoSignal,
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
    /// Wrapped seconds driving scanner sweep and perimeter-trace motion.
    phase: f32,
    /// Determinate capsule fill 0..=1 from multi-chunk STT; None = no meter.
    meter: Option<f32>,
    /// Measured chronological min/max PCM envelope for the latest window.
    waveform: Option<AudioWaveform>,
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
    waveform: AudioWaveform,
    width: u32,
    height: u32,
    buffer_scale: u32,
}

impl RenderKey {
    fn from_view(view: &ChipView, width: u32, height: u32, buffer_scale: u32) -> Self {
        let animated = matches!(
            view.kind,
            ChipKind::Transcribing | ChipKind::Cleaning | ChipKind::Recording | ChipKind::Sent
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
            waveform: view.waveform.unwrap_or([[0, 0]; AUDIO_WAVEFORM_BINS]),
            width,
            height,
            buffer_scale,
        }
    }
}

impl CompositorHandler for HudState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let next_scale = new_factor.max(1) as u32;
        if next_scale != self.buffer_scale {
            if self.layer.set_buffer_scale(next_scale).is_ok() {
                self.buffer_scale = next_scale;
            } else {
                tracing::warn!(
                    "[HUD] compositor rejected output buffer scale {}; falling back to 1x",
                    next_scale
                );
                self.buffer_scale = 1;
            }
        }
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

fn scale_alpha(color: [u8; 4], factor: f32) -> [u8; 4] {
    let mut scaled = color;
    scaled[3] = (scaled[3] as f32 * factor.clamp(0.0, 1.0)).round() as u8;
    scaled
}
fn mix_rgb(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    [
        (a[0] as f32 + (b[0] as f32 - a[0] as f32) * t).round() as u8,
        (a[1] as f32 + (b[1] as f32 - a[1] as f32) * t).round() as u8,
        (a[2] as f32 + (b[2] as f32 - a[2] as f32) * t).round() as u8,
    ]
}

fn accent_color(kind: ChipKind) -> [u8; 3] {
    match kind {
        ChipKind::Recording => [255, 106, 92],    // warm coral
        ChipKind::NoSignal => [255, 178, 92],     // warning amber
        ChipKind::Transcribing => [255, 186, 74], // amber
        ChipKind::Cleaning => [190, 142, 255],    // violet
        ChipKind::Sent => [64, 218, 120],         // bright emerald
        ChipKind::Notice => [255, 178, 92],       // warm amber
    }
}

/// Map typed processing state to the HUD's static content and measured meter
/// target. `keep_meter` carries an armed multi-chunk fill through Cleaning.
fn processing_chip_content(stage: &Stage, keep_meter: bool) -> (String, ChipKind, Option<f32>) {
    match stage {
        Stage::CleaningUp => (
            "Cleaning…".to_owned(),
            ChipKind::Cleaning,
            keep_meter.then_some(1.0),
        ),
        Stage::Transcribing { .. } | Stage::Unknown(_) => {
            if let Some((chunk, total)) = stage.measured_progress() {
                (
                    format!("Transcribing… {chunk}/{total}"),
                    ChipKind::Transcribing,
                    Some(chunk as f32 / total as f32),
                )
            } else {
                ("Transcribing…".to_owned(), ChipKind::Transcribing, None)
            }
        }
    }
}

fn interpolate_waveform(
    from: [[f32; 2]; AUDIO_WAVEFORM_BINS],
    to: [[f32; 2]; AUDIO_WAVEFORM_BINS],
    t: f32,
) -> AudioWaveform {
    let t = t.clamp(0.0, 1.0);
    std::array::from_fn(|index| {
        std::array::from_fn(|edge| {
            (from[index][edge] + (to[index][edge] - from[index][edge]) * t)
                .round()
                .clamp(-100.0, 100.0) as i8
        })
    })
}

/// Return the transcribing head position in cell coordinates. The head travels
/// left-to-right, then right-to-left, with a pure phase function so frozen
/// phases remain byte-identical.
fn knight_sweep_head(phase: f32) -> f32 {
    let cycle = (phase / 2.0).rem_euclid(2.0);
    let position = if cycle <= 1.0 { cycle } else { 2.0 - cycle };
    position * (KNIGHT_CELLS.saturating_sub(1) as f32)
}

/// Compute normalized scanner-cell energy for a chip kind.
fn knight_cell_levels(
    kind: ChipKind,
    phase: f32,
    waveform: Option<AudioWaveform>,
) -> [f32; KNIGHT_CELLS] {
    match kind {
        ChipKind::Recording => {
            // Peak absolute amplitude per bin: a loud negative-only swing
            // must light cells exactly like its positive mirror.
            let mut peaks = [0.0_f32; AUDIO_WAVEFORM_BINS];
            if let Some(waveform) = waveform {
                for (index, bin) in waveform.into_iter().enumerate() {
                    peaks[index] = f32::from(bin[0].unsigned_abs().max(bin[1].unsigned_abs()));
                }
            }
            let last_bin = AUDIO_WAVEFORM_BINS.saturating_sub(1) as f32;
            std::array::from_fn(|index| {
                let t = index as f32 * last_bin / (KNIGHT_CELLS.saturating_sub(1) as f32);
                (smooth_bin(&peaks, t).max(0.0) / 100.0).clamp(0.0, 1.0)
            })
        }
        ChipKind::NoSignal => [0.04; KNIGHT_CELLS],
        ChipKind::Transcribing => {
            let head = knight_sweep_head(phase);
            std::array::from_fn(|index| {
                let distance = (index as f32 - head).abs();
                (1.0 - distance / 5.0).clamp(0.0, 1.0)
            })
        }
        ChipKind::Cleaning => [0.5; KNIGHT_CELLS],
        ChipKind::Sent | ChipKind::Notice => [1.0; KNIGHT_CELLS],
    }
}
/// Sharp rectangular background container ("Omarchy Aesthetic", centered in the HUD surface).
/// Sits on an opaque dark floor (#13141c) with a crisp 1px anti-aliased border, completely
/// occluding any text, window content, or desktop background underneath.
#[allow(clippy::too_many_arguments)]
fn rect_container(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    kind: ChipKind,
    scale: f32,
    alpha: f32,
) {
    if half_width <= 0.0 || half_height <= 0.0 || alpha <= 0.0 {
        return;
    }
    let min_x = (center_x - half_width - 1.0).max(0.0) as u32;
    let max_x = (center_x + half_width + 1.0).min(width as f32) as u32;
    let min_y = (center_y - half_height - 1.0).max(0.0) as u32;
    let max_y = (center_y + half_height + 1.0).min(height as f32) as u32;

    let accent = accent_color(kind);
    // Deep Omarchy dark background (#13141c = [19, 20, 28]) with 4% accent tint:
    let floor_rgb = mix_rgb([19, 20, 28], accent, 0.04);
    let floor_color = [floor_rgb[0], floor_rgb[1], floor_rgb[2], 255];

    // Crisp Omarchy 1px border (#a9b1d6 with accent tint):
    let border_rgb = mix_rgb([169, 177, 214], accent, 0.35);
    let border_color = [border_rgb[0], border_rgb[1], border_rgb[2], 80];
    let border_width = 1.0 * scale;

    for y in min_y..max_y {
        let dy = (y as f32 + 0.5 - center_y).abs() - half_height;
        for x in min_x..max_x {
            let dx = (x as f32 + 0.5 - center_x).abs() - half_width;
            let d = dx.max(dy);
            let outer_coverage = (0.5 - d).clamp(0.0, 1.0);
            if outer_coverage > 0.0 {
                let inner_d = d + border_width;
                let inner_coverage = (-inner_d + 0.5).clamp(0.0, 1.0);
                let border_coverage = (outer_coverage - inner_coverage).clamp(0.0, 1.0);

                if inner_coverage > 0.0 {
                    blend_pixel(
                        canvas,
                        width,
                        height,
                        x,
                        y,
                        floor_color,
                        inner_coverage * alpha,
                    );
                }
                if border_coverage > 0.0 {
                    blend_pixel(
                        canvas,
                        width,
                        height,
                        x,
                        y,
                        border_color,
                        border_coverage * alpha,
                    );
                }
            }
        }
    }
}

/// Paint the full-width segmented scanner track for one chip state.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, geometry)
fn knight_track(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    scale: f32,
    kind: ChipKind,
    from: Option<ChipKind>,
    transition: f32,
    phase: f32,
    waveform: Option<AudioWaveform>,
    progress: Option<f32>,
    content_alpha: f32,
) {
    if content_alpha <= 0.0 {
        return;
    }
    let track_width = (TRACK_WIDTH * scale).min((half_width * 2.0 - 16.0 * scale).max(0.0));
    let track_left = center_x - track_width / 2.0;
    let track_top = center_y - half_height;
    let track_bottom = center_y + half_height;
    if track_width <= 0.0 || track_bottom <= track_top {
        return;
    }
    let slot_width = track_width / KNIGHT_CELLS as f32;
    let cell_width = slot_width - KNIGHT_CELL_GAP * scale;
    if cell_width <= 0.0 {
        return;
    }

    let rgb = accent_color(kind);
    let levels = knight_cell_levels(kind, phase, waveform);
    let t = ease_out_cubic(transition);

    // Height rule: Sizing is 100% consistent across Transcribing, Cleaning, Sent, and Notice.
    // The ONLY state with variable cell heights is Recording (Listening), where heights
    // represent live voice audio levels.
    for (index, &level) in levels.iter().enumerate() {
        let cell_center_x = track_left + slot_width * (index as f32 + 0.5);

        let (cell_height, cell_color, cell_alpha) = match kind {
            ChipKind::Recording => {
                // 1. Gating & non-linear dynamic range expansion:
                let gated = ((level - 0.18).max(0.0) / 0.72).clamp(0.0, 1.0);
                let punch = gated.powf(1.4);

                // 2. Spatial variation: center-weighted vocal equalizer envelope:
                let center_norm = (index as f32 - 10.5).abs() / 10.5;
                let bell = (1.0 - center_norm * center_norm * 0.70).max(0.25);

                // 3. Formant ripple shimmer while speaking:
                let ripple = if gated > 0.02 {
                    let w1 = (phase * 12.0 + index as f32 * 0.75).sin();
                    let w2 = (phase * 7.5 - index as f32 * 0.50).cos();
                    0.82 + 0.18 * (w1 * 0.6 + w2 * 0.4)
                } else {
                    1.0
                };

                let energy = (punch * bell * ripple).clamp(0.0, 1.0);
                let target_h = (4.0 + 22.0 * energy) * scale;
                let color_rgb = if energy > 0.05 {
                    mix_rgb(rgb, [255, 180, 160], energy * 0.45)
                } else {
                    rgb
                };
                let alpha = 0.28 + 0.72 * energy;
                (
                    target_h,
                    [color_rgb[0], color_rgb[1], color_rgb[2], 255],
                    alpha,
                )
            }
            ChipKind::NoSignal => (4.0 * scale, [rgb[0], rgb[1], rgb[2], 255], 0.35),
            ChipKind::Transcribing => {
                // Sizing is constant (14.0px).
                // Unfilled cells are sleek dark track slots.
                // Filled cells are brilliant glowing gold.
                let base_h = 14.0 * scale;
                // Elegant transition from recording: audio bars smoothly ease down into 14px!
                let height = if from == Some(ChipKind::Recording) && t < 1.0 {
                    let rec_levels = knight_cell_levels(ChipKind::Recording, phase, waveform);
                    let rec_level = rec_levels[index];
                    let gated = ((rec_level - 0.18).max(0.0) / 0.72).clamp(0.0, 1.0);
                    let rec_h = (4.0 + 22.0 * gated.powf(1.4)) * scale;
                    rec_h + (base_h - rec_h) * t
                } else {
                    base_h
                };

                let (active_color, alpha) = match progress {
                    Some(p) => {
                        let fill_edge = p.clamp(0.0, 1.0) * KNIGHT_CELLS as f32;
                        let cell_pos = index as f32;
                        if cell_pos < fill_edge - 0.5 {
                            ([255, 186, 74, 255], 1.0)
                        } else if (cell_pos - fill_edge).abs() <= 0.8 {
                            ([255, 230, 120, 255], 1.0)
                        } else {
                            ([42, 44, 52, 255], 0.18)
                        }
                    }
                    None => {
                        let head = knight_sweep_head(phase);
                        let dist = (index as f32 - head).abs();
                        if dist < 2.5 {
                            let intensity = ((2.5 - dist) / 2.5).powi(2);
                            let sweep_rgb = mix_rgb([255, 186, 74], [255, 235, 140], intensity);
                            let alpha = 0.18 + 0.82 * intensity;
                            ([sweep_rgb[0], sweep_rgb[1], sweep_rgb[2], 255], alpha)
                        } else {
                            ([42, 44, 52, 255], 0.18)
                        }
                    }
                };
                let final_color = if from == Some(ChipKind::Recording) && t < 1.0 {
                    let c = mix_rgb(
                        accent_color(ChipKind::Recording),
                        [active_color[0], active_color[1], active_color[2]],
                        t,
                    );
                    [c[0], c[1], c[2], 255]
                } else {
                    active_color
                };
                (height, final_color, alpha)
            }
            ChipKind::Cleaning => {
                // Sizing is constant (14.0px, matching Transcribing!).
                // Traveling violet shimmer wave across all 22 cells.
                let height = 14.0 * scale;
                let wave = (phase * 6.0 - index as f32 * 0.45).sin();
                let crest = ((wave + 1.0) / 2.0).powi(2);
                let wave_rgb = mix_rgb([155, 105, 240], [230, 195, 255], crest);
                let alpha = 0.28 + 0.72 * crest;

                // Elegant transition from transcribing: color transformation sweep from left to right!
                let color = if from == Some(ChipKind::Transcribing) && t < 1.0 {
                    let sweep_front = t * (KNIGHT_CELLS as f32 + 2.0);
                    let cell_pos = index as f32;
                    if cell_pos < sweep_front - 1.0 {
                        [wave_rgb[0], wave_rgb[1], wave_rgb[2], 255]
                    } else if (cell_pos - sweep_front).abs() <= 1.2 {
                        [245, 230, 255, 255] // transformation crest flash
                    } else {
                        let amber = accent_color(ChipKind::Transcribing);
                        [amber[0], amber[1], amber[2], 255]
                    }
                } else {
                    [wave_rgb[0], wave_rgb[1], wave_rgb[2], 255]
                };
                (height, color, alpha)
            }
            ChipKind::Sent => {
                // Sizing is constant (14.0px, matching Transcribing and Cleaning!).
                let height = 14.0 * scale;
                let center_dist = (index as f32 - 10.5).abs();
                let ripple_front = t * 14.0;
                let dist_to_front = (center_dist - ripple_front).abs();

                // Celebratory emerald base with traveling mint shimmer:
                let shimmer = (phase * 4.5 - index as f32 * 0.4).sin();
                let crest = ((shimmer + 1.0) / 2.0).powi(2);
                let emerald_shimmer = mix_rgb([64, 218, 120], [185, 255, 210], crest * 0.45);

                if center_dist <= ripple_front {
                    if dist_to_front < 1.8 && t < 0.95 {
                        // Radiant leading flash crest:
                        (height, [220, 255, 235, 255], 1.0)
                    } else {
                        // Alive emerald hold with gentle travelling mint gleam:
                        (
                            height,
                            [
                                emerald_shimmer[0],
                                emerald_shimmer[1],
                                emerald_shimmer[2],
                                255,
                            ],
                            1.0,
                        )
                    }
                } else if from == Some(ChipKind::Cleaning) {
                    let wave = (phase * 6.0 - index as f32 * 0.45).sin();
                    let crest = ((wave + 1.0) / 2.0).powi(2);
                    let wave_rgb = mix_rgb([155, 105, 240], [230, 195, 255], crest);
                    (
                        height,
                        [wave_rgb[0], wave_rgb[1], wave_rgb[2], 255],
                        0.28 + 0.72 * crest,
                    )
                } else {
                    (height, [42, 44, 52, 255], 0.20)
                }
            }
            ChipKind::Notice => (14.0 * scale, [rgb[0], rgb[1], rgb[2], 255], 1.0),
        };

        cell_rect(
            canvas,
            width,
            height,
            cell_center_x,
            center_y,
            cell_width / 2.0,
            cell_height / 2.0,
            cell_color,
            content_alpha * cell_alpha,
        );
    }
}

/// Sharp square/rectangular cell with 1px anti-aliased edges.
#[allow(clippy::too_many_arguments)] // paint primitive plumbing (canvas, origin)
fn cell_rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    half_width: f32,
    half_height: f32,
    color: [u8; 4],
    alpha: f32,
) {
    if half_width <= 0.0 || half_height <= 0.0 || alpha <= 0.0 {
        return;
    }
    let fill = scale_alpha(color, alpha);
    let min_x = (center_x - half_width - 1.0).max(0.0) as u32;
    let max_x = (center_x + half_width + 1.0).min(width as f32) as u32;
    let min_y = (center_y - half_height - 1.0).max(0.0) as u32;
    let max_y = (center_y + half_height + 1.0).min(height as f32) as u32;

    for y in min_y..max_y {
        let dy = (y as f32 + 0.5 - center_y).abs() - half_height;
        for x in min_x..max_x {
            let dx = (x as f32 + 0.5 - center_x).abs() - half_width;
            let coverage = (0.5 - dx.max(dy)).clamp(0.0, 1.0);
            if coverage > 0.0 {
                blend_pixel(canvas, width, height, x, y, fill, coverage);
            }
        }
    }
}

/// Catmull-Rom sample of uniform bin values at fractional position `t`.
/// The raw spline can overshoot between bins, so the result is clamped to
/// the bracketing measured values: the band smooths but never draws
/// amplitude outside the captured min/max envelope. Clamped ends keep the
/// curve inside measured data at the band edges.
fn smooth_bin(values: &[f32], t: f32) -> f32 {
    let count = values.len();
    let clamped = t.clamp(0.0, (count - 1) as f32);
    let index = (clamped.floor() as usize).min(count - 2);
    let f = clamped - index as f32;
    let p0 = values[index.saturating_sub(1)];
    let p1 = values[index];
    let p2 = values[index + 1];
    let p3 = values[(index + 2).min(count - 1)];
    let spline = 0.5
        * (2.0 * p1
            + (-p0 + p2) * f
            + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * f * f
            + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * f * f * f);
    spline.clamp(p1.min(p2), p1.max(p2))
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
    let src_a = (color[3] as f32 * coverage.clamp(0.0, 1.0)) / 255.0;
    if src_a <= f32::EPSILON {
        return;
    }
    let src_b = color[2] as f32 * src_a; // color is [R, G, B, A]; canvas is Argb8888 little-endian [B, G, R, A]
    let src_g = color[1] as f32 * src_a;
    let src_r = color[0] as f32 * src_a;

    let dst_b = canvas[index] as f32;
    let dst_g = canvas[index + 1] as f32;
    let dst_r = canvas[index + 2] as f32;
    let dst_a = canvas[index + 3] as f32 / 255.0;

    let inv_src_a = 1.0 - src_a;
    let out_b = src_b + dst_b * inv_src_a;
    let out_g = src_g + dst_g * inv_src_a;
    let out_r = src_r + dst_r * inv_src_a;
    let out_a = src_a + dst_a * inv_src_a;

    canvas[index] = out_b.round().clamp(0.0, 255.0) as u8;
    canvas[index + 1] = out_g.round().clamp(0.0, 255.0) as u8;
    canvas[index + 2] = out_r.round().clamp(0.0, 255.0) as u8;
    canvas[index + 3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_lock_on, ease_in_out_cubic, ease_out_cubic, format_elapsed, interpolate_waveform,
        knight_cell_levels, knight_sweep_head, knight_track, processing_chip_content,
        rect_container, smooth_bin, ChipKind, UiState, UiStateKind, KNIGHT_CELLS,
        SCREENSHOT_WAVEFORM,
    };
    use crate::ipc::{AudioSignal, StatusSnapshot, AUDIO_WAVEFORM_BINS};
    use crate::pipeline::Stage;
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
    fn recording_status_carries_waveform_and_warning_into_hud_state() {
        let waveform = SCREENSHOT_WAVEFORM;
        let active = StatusSnapshot::Recording {
            elapsed: 12,
            signal: Some(AudioSignal {
                level: 72,
                silent: false,
                waveform,
            }),
            outcome: None,
        };
        assert_eq!(
            UiState::from_status(&active),
            UiState::Recording {
                elapsed: 12,
                audio_waveform: Some(waveform),
                audio_silent: false,
            }
        );

        let silent = StatusSnapshot::Recording {
            elapsed: 12,
            signal: Some(AudioSignal {
                level: 0,
                silent: true,
                waveform: [[0, 0]; AUDIO_WAVEFORM_BINS],
            }),
            outcome: None,
        };
        let state = UiState::from_status(&silent);
        assert!(matches!(
            state,
            UiState::Recording {
                audio_silent: true,
                ..
            }
        ));
    }

    #[test]
    fn unknown_daemon_status_is_safe_for_an_older_hud() {
        let status = StatusSnapshot::Unknown {
            state: "calibrating".to_owned(),
            outcome: None,
        };
        assert_eq!(UiState::from_status(&status), UiState::Idle);
    }

    #[test]
    fn waveform_interpolation_stays_between_measured_frames() {
        let from = [[0.0; 2]; AUDIO_WAVEFORM_BINS];
        let to = SCREENSHOT_WAVEFORM.map(|bin| bin.map(f32::from));
        assert_eq!(
            interpolate_waveform(from, to, 0.0),
            [[0, 0]; AUDIO_WAVEFORM_BINS]
        );
        let midpoint = interpolate_waveform(from, to, 0.5);
        for (midpoint, target) in midpoint.into_iter().zip(SCREENSHOT_WAVEFORM) {
            for edge in 0..2 {
                assert!((i16::from(midpoint[edge]) * 2 - i16::from(target[edge])).abs() <= 1);
            }
        }
        assert_eq!(interpolate_waveform(from, to, 1.0), SCREENSHOT_WAVEFORM);
    }

    #[test]
    fn smooth_bin_never_overshoots_measured_bins() {
        let cases: [[f32; 7]; 3] = [
            [0.0, 90.0, -20.0, 60.0, -90.0, 30.0, 0.0],
            [10.0, 10.0, 10.0, 80.0, 10.0, 10.0, 10.0],
            [-94.0, 94.0, -94.0, 94.0, -94.0, 94.0, -94.0],
        ];
        for values in cases {
            for step in 0..=60 {
                let t = step as f32 / 10.0;
                let index = (t.floor() as usize).min(values.len() - 2);
                let low = values[index].min(values[index + 1]);
                let high = values[index].max(values[index + 1]);
                let sample = smooth_bin(&values, t);
                assert!(
                    (low..=high).contains(&sample),
                    "t={t}: {sample} outside [{low}, {high}]"
                );
            }
        }
    }

    #[test]
    fn knight_scanner_sweep_head_is_deterministic_at_fixed_phase() {
        let phase = 1.25;
        let first = knight_sweep_head(phase);
        let second = knight_sweep_head(phase);
        assert_eq!(first, second);
        assert_eq!(knight_sweep_head(0.0), 0.0);
        assert_eq!(knight_sweep_head(1.0), 10.5);
        assert_eq!(
            knight_sweep_head(2.0),
            (KNIGHT_CELLS.saturating_sub(1) as f32)
        );
        assert_eq!(knight_sweep_head(4.0), 0.0);
    }

    #[test]
    fn sent_knight_track_lights_every_cell() {
        let width = 360;
        let height = 56;
        let mut canvas = vec![0_u8; width * height * 4];
        knight_track(
            &mut canvas,
            width as u32,
            height as u32,
            180.0,
            28.0,
            160.0,
            20.0,
            1.0,
            ChipKind::Sent,
            None,
            1.0,
            0.0,
            None,
            None,
            1.0,
        );
        let slot_width = 304.0 / KNIGHT_CELLS as f32;
        for index in 0..KNIGHT_CELLS {
            let x = (28.0 + slot_width * (index as f32 + 0.5)).round() as u32;
            assert!(
                alpha_at(&canvas, width as u32, x, 28) > 0,
                "sent cell {index} must be lit"
            );
        }
    }

    #[test]
    fn knight_track_paints_inside_capsule_inset() {
        let width = 360;
        let height = 56;
        let mut canvas = vec![0_u8; width * height * 4];
        knight_track(
            &mut canvas,
            width as u32,
            height as u32,
            180.0,
            28.0,
            160.0,
            20.0,
            1.0,
            ChipKind::Cleaning,
            None,
            1.0,
            1.1,
            None,
            None,
            1.0,
        );
        let mut painted = false;
        for (index, pixel) in canvas.as_chunks::<4>().0.iter().enumerate() {
            if pixel[3] == 0 {
                continue;
            }
            painted = true;
            let x = (index % width) as f32 + 0.5;
            let y = (index / width) as f32 + 0.5;
            assert!((24.0..=336.0).contains(&x));
            assert!((12.0..=44.0).contains(&y));
        }
        assert!(painted, "cleaning scanner should paint static cells");
    }

    #[test]
    fn frozen_knight_phase_is_byte_deterministic() {
        let render = || {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                ChipKind::Transcribing,
                None,
                1.0,
                0.0,
                None,
                None,
                1.0,
            );
            canvas
        };
        let first = render();
        let second = render();
        assert_eq!(first, second);
    }

    #[test]
    fn knight_recording_lights_negative_only_envelopes() {
        let negative = [[-80, -40]; AUDIO_WAVEFORM_BINS];
        let levels = knight_cell_levels(ChipKind::Recording, 0.0, Some(negative));
        assert!(
            levels.iter().all(|level| (*level - 0.8).abs() < 0.001),
            "negative-only swing must read peak amplitude: {levels:?}"
        );
        let mirror = [[40, 80]; AUDIO_WAVEFORM_BINS];
        assert_eq!(
            levels,
            knight_cell_levels(ChipKind::Recording, 0.0, Some(mirror))
        );
    }

    #[test]
    fn processing_status_keeps_typed_stage_and_safe_kind() {
        let progress = StatusSnapshot::Processing {
            stage: Stage::Transcribing { chunk: 2, total: 5 },
            outcome: None,
        };
        assert_eq!(
            UiState::from_status(&progress),
            UiState::Processing {
                stage: Stage::Transcribing { chunk: 2, total: 5 },
            }
        );
        assert_eq!(
            UiState::from_status(&progress).kind(),
            UiStateKind::Transcribing
        );

        let cleaning = StatusSnapshot::Processing {
            stage: Stage::CleaningUp,
            outcome: None,
        };
        assert_eq!(
            UiState::from_status(&cleaning).kind(),
            UiStateKind::Cleaning
        );

        let future = StatusSnapshot::Processing {
            stage: Stage::Unknown("aligning".to_owned()),
            outcome: None,
        };
        assert_eq!(
            UiState::from_status(&future).kind(),
            UiStateKind::Transcribing
        );
    }

    #[test]
    fn processing_chip_uses_only_typed_measured_progress() {
        assert_eq!(
            processing_chip_content(&Stage::Transcribing { chunk: 2, total: 5 }, false),
            (
                "Transcribing… 2/5".to_owned(),
                ChipKind::Transcribing,
                Some(0.4)
            )
        );
        assert_eq!(
            processing_chip_content(&Stage::Transcribing { chunk: 1, total: 1 }, false),
            ("Transcribing…".to_owned(), ChipKind::Transcribing, None)
        );
        assert_eq!(
            processing_chip_content(&Stage::Unknown("aligning".to_owned()), false),
            ("Transcribing…".to_owned(), ChipKind::Transcribing, None)
        );
        assert_eq!(
            processing_chip_content(&Stage::CleaningUp, false),
            ("Cleaning…".to_owned(), ChipKind::Cleaning, None)
        );
        assert_eq!(
            processing_chip_content(&Stage::CleaningUp, true),
            ("Cleaning…".to_owned(), ChipKind::Cleaning, Some(1.0))
        );
    }

    #[test]
    fn transcribing_progress_fills_cells_left_to_right() {
        let render = |progress| {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                ChipKind::Transcribing,
                None,
                1.0,
                0.0,
                None,
                progress,
                1.0,
            );
            canvas
        };
        let half = render(Some(0.5));
        let left = alpha_at(&half, 360, 60, 28);
        let right = alpha_at(&half, 360, 300, 28);
        assert!(left > 200, "filled cell must be near-opaque (left={left})");
        assert!(right < 60, "unfilled cell must stay dim (right={right})");
        let full = render(Some(1.0));
        assert!(alpha_at(&full, 360, 300, 28) > 200);
    }

    #[test]
    fn track_leaves_surface_transparent_outside_cells() {
        let mut canvas = vec![0_u8; 360 * 56 * 4];
        knight_track(
            &mut canvas,
            360,
            56,
            180.0,
            28.0,
            160.0,
            20.0,
            1.0,
            ChipKind::Sent,
            None,
            1.0,
            0.0,
            None,
            None,
            1.0,
        );
        assert_eq!(alpha_at(&canvas, 360, 0, 0), 0);
        assert_eq!(alpha_at(&canvas, 360, 359, 55), 0);
        assert!(alpha_at(&canvas, 360, 187, 28) > 200);
    }

    #[test]
    fn track_cells_carry_state_accent() {
        let render = |kind| {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                kind,
                None,
                1.0,
                0.0,
                Some([[50, 50]; AUDIO_WAVEFORM_BINS]),
                None,
                1.0,
            );
            let base = (28 * 360 + 187) as usize * 4;
            [canvas[base], canvas[base + 1], canvas[base + 2]]
        };
        let [b, g, r] = render(ChipKind::Recording);
        assert!(r > g && r > b, "coral cell must be red-dominant");
        let [b, g, r] = render(ChipKind::Cleaning);
        assert!(b > r && b > g, "violet cell must be blue-dominant");
    }

    #[test]
    fn rect_container_paints_opaque_floor_and_border() {
        let width = 420;
        let height = 56;
        let mut canvas = vec![0_u8; width * height * 4];
        rect_container(
            &mut canvas,
            width as u32,
            height as u32,
            210.0,
            28.0,
            168.0,
            22.0,
            ChipKind::Transcribing,
            1.0,
            1.0,
        );
        // Interior floor is 100% opaque (255)
        assert_eq!(alpha_at(&canvas, width as u32, 210, 28), 255);
        // Outside is transparent
        assert_eq!(alpha_at(&canvas, width as u32, 10, 10), 0);
        assert_eq!(alpha_at(&canvas, width as u32, 410, 50), 0);
    }

    #[test]
    fn state_cell_heights_are_constant_except_recording() {
        let states = [
            ChipKind::Transcribing,
            ChipKind::Cleaning,
            ChipKind::Sent,
            ChipKind::Notice,
        ];
        for kind in states {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                kind,
                None,
                1.0,
                0.0,
                None,
                None,
                1.0,
            );
            for (index, pixel) in canvas.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] == 0 {
                    continue;
                }
                let y = (index / 360) as f32 + 0.5;
                // Constant 14px height centered at y=28 (20.5..=35.5)
                assert!(
                    (20.5..=35.5).contains(&y),
                    "{kind:?} pixel at y={y} exceeded 14px constant height bounds"
                );
            }
        }
    }

    #[test]
    fn recording_cell_height_scales_with_audio_energy() {
        let render_recording = |peak| {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                ChipKind::Recording,
                None,
                1.0,
                0.0,
                Some([[peak, peak]; AUDIO_WAVEFORM_BINS]),
                None,
                1.0,
            );
            let mut min_y = 56.0_f32;
            let mut max_y = 0.0_f32;
            for (index, pixel) in canvas.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] > 50 {
                    let y = (index / 360) as f32 + 0.5;
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
            max_y - min_y
        };
        let silent_height = render_recording(0);
        let loud_height = render_recording(90);
        assert!(
            silent_height <= 6.0,
            "silent input should render minimal resting height (got {silent_height})"
        );
        assert!(
            loud_height >= 18.0,
            "loud voice input should expand dramatically (got {loud_height})"
        );
    }

    #[test]
    fn transition_from_recording_to_transcribing_morphs_height_and_color() {
        let render_transition = |t| {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                ChipKind::Transcribing,
                Some(ChipKind::Recording),
                t,
                0.0,
                Some([[80, 80]; AUDIO_WAVEFORM_BINS]),
                Some(1.0),
                1.0,
            );
            let base = (28 * 360 + 187) as usize * 4;
            let (b, g, r) = (canvas[base], canvas[base + 1], canvas[base + 2]);
            let mut min_y = 56.0_f32;
            let mut max_y = 0.0_f32;
            for (index, pixel) in canvas.as_chunks::<4>().0.iter().enumerate() {
                if (index % 360) == 187 && pixel[3] > 100 {
                    let y = (index / 360) as f32 + 0.5;
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
            (max_y - min_y, [b, g, r])
        };
        let (early_h, [_, _, early_r]) = render_transition(0.0);
        let (settled_h, [_, _, settled_r]) = render_transition(1.0);
        assert!(
            early_h > 18.0,
            "early height should be high from recording (got {early_h})"
        );
        assert!(
            (settled_h - 14.0).abs() <= 1.0,
            "settled height should be 14px (got {settled_h})"
        );
        assert!(
            early_r >= settled_r,
            "early color should carry recording red"
        );
    }

    #[test]
    fn sent_state_displays_shimmer_and_ripple_crest() {
        let render_sent = |t, phase| {
            let mut canvas = vec![0_u8; 360 * 56 * 4];
            knight_track(
                &mut canvas,
                360,
                56,
                180.0,
                28.0,
                160.0,
                20.0,
                1.0,
                ChipKind::Sent,
                None,
                t,
                phase,
                None,
                None,
                1.0,
            );
            canvas
        };

        // 1. Mid-ripple (t=0.2): center cell 11 (x=187) is settled green, while wave front at cell 17 (x=270) has radiant mint crest:
        let mid = render_sent(0.2, 0.0);
        let center_base = (28 * 360 + 187) as usize * 4;
        let [b_center, g_center, r_center] =
            [mid[center_base], mid[center_base + 1], mid[center_base + 2]];
        assert!(
            g_center > r_center && g_center > b_center,
            "center cell must be emerald green"
        );
        let crest_base = (28 * 360 + 270) as usize * 4;
        let [b_crest, g_crest, r_crest] =
            [mid[crest_base], mid[crest_base + 1], mid[crest_base + 2]];
        assert!(
            r_crest >= 200 && g_crest >= 240 && b_crest >= 200,
            "wave front must flash radiant mint crest (got rgb [{r_crest}, {g_crest}, {b_crest}])"
        );

        // 2. Settled hold: live phase movement modulates the celebratory shimmer
        let phase0 = render_sent(1.0, 0.0);
        let phase1 = render_sent(1.0, 1.0);
        assert_ne!(
            phase0, phase1,
            "settled success state must have live phase shimmer"
        );
    }
    fn alpha_at(canvas: &[u8], width: u32, x: u32, y: u32) -> u8 {
        canvas[(y * width + x) as usize * 4 + 3]
    }
}
