//! Wayland layer-shell surface for the shared HUD instrument.
//!
//! The compositor owns placement, scale and frame cadence; this module only
//! sizes the layer, attaches the instrument's pixels and follows its outputs.

use super::{
    acquire_instance_lock, save_screenshot, Instrument, Poller, ScreenshotState, CONTAINER_WIDTH,
    FRAME_INTERVAL, POLL_INTERVAL, SURFACE_HEIGHT, SURFACE_WIDTH,
};
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
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
    io::Read,
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

/// Query the preference belonging to the running desktop, not an unrelated
/// installed settings service. Unknown desktops can use the explicit config.
pub(super) fn desktop_reduced_motion() -> Option<bool> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    if desktop.split(':').any(|name| name == "hyprland") {
        let output = preference_output("hyprctl", &["-j", "getoption", "animations:enabled"])?;
        let value: serde_json::Value = serde_json::from_str(&output).ok()?;
        value
            .get("bool")
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                value
                    .get("int")
                    .and_then(serde_json::Value::as_i64)
                    .map(|value| value != 0)
            })
            .map(|enabled| !enabled)
    } else if desktop.split(':').any(|name| name == "gnome") {
        match preference_output(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "enable-animations"],
        )?
        .trim()
        {
            "false" => Some(true),
            "true" => Some(false),
            _ => None,
        }
    } else {
        None
    }
}

fn preference_output(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(250);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    if !success {
        return None;
    }
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(4096)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

pub(super) fn run_native(
    screenshot: Option<PathBuf>,
    state: Option<ScreenshotState>,
    handoff: Option<String>,
) -> Result<()> {
    let _lock = if screenshot.is_some() {
        None
    } else {
        match acquire_instance_lock()? {
            Some(file) => Some(file),
            None => return Ok(()),
        }
    };
    let connection = Connection::connect_to_env().context("connecting HUD to Wayland")?;
    let (globals, mut queue) =
        registry_queue_init(&connection).context("reading Wayland globals")?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("binding HUD compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("binding HUD layer shell")?;
    let shm = Shm::bind(&globals, &qh).context("binding HUD shared memory")?;
    let pool = SlotPool::new((SURFACE_WIDTH * SURFACE_HEIGHT * 8) as usize, &shm)
        .context("allocating HUD buffer pool")?;
    let now = Instant::now();
    let instrument = if screenshot.is_some() {
        Instrument::preview(state.unwrap_or(ScreenshotState::Recording), handoff, now)?
    } else {
        Instrument::live(now)?
    };
    let mut hud = HudState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        layer: None,
        layer_output: None,
        configured: false,
        frame_pending: false,
        width: SURFACE_WIDTH,
        height: SURFACE_HEIGHT,
        requested_size: (SURFACE_WIDTH, SURFACE_HEIGHT),
        buffer_scale: 1,
        instrument,
        screenshot,
        screenshot_at: now,
        screenshot_done: false,
        surface_lifecycle: SurfaceLifecycle::WaitingForOutput,
    };
    hud.create_layer(&qh)?;
    queue
        .roundtrip(&mut hud)
        .context("configuring HUD surface")?;
    let poller = if hud.screenshot.is_none() {
        Some(Poller::start()?)
    } else {
        None
    };
    loop {
        let now = Instant::now();
        if let Some(poller) = &poller {
            hud.instrument.poll(poller, now);
        }
        if hud.layer.is_none()
            && hud.surface_lifecycle.can_create()
            && hud.output_state.outputs().next().is_some()
        {
            hud.create_layer(&qh)?;
        }
        hud.redraw(&qh, now)?;
        if hud.screenshot_done {
            return Ok(());
        }
        if hud.screenshot.is_some()
            && now.duration_since(hud.screenshot_at) > Duration::from_secs(5)
        {
            anyhow::bail!("compositor did not configure the HUD screenshot surface");
        }
        let interval = if !hud.frame_pending && hud.instrument.animate(now) {
            FRAME_INTERVAL
        } else {
            POLL_INTERVAL
        };
        timed_dispatch(&mut queue, &mut hud, interval)?;
    }
}

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
    if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        return Err(std::io::Error::last_os_error()).context("polling HUD Wayland socket");
    }
    if ready > 0 && pollfd.revents & libc::POLLIN != 0 {
        guard.read().context("reading HUD Wayland events")?;
    } else {
        drop(guard);
    }
    queue.dispatch_pending(data)?;
    Ok(())
}

struct HudState {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    layer_output: Option<wl_output::WlOutput>,
    configured: bool,
    frame_pending: bool,
    width: u32,
    height: u32,
    requested_size: (u32, u32),
    buffer_scale: u32,
    instrument: Instrument,
    screenshot: Option<PathBuf>,
    screenshot_at: Instant,
    screenshot_done: bool,
    surface_lifecycle: SurfaceLifecycle,
}

/// A compositor close is an instruction, not proof that an output vanished.
/// Only an actual output event can authorize another surface after closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SurfaceLifecycle {
    WaitingForOutput,
    Open,
    Closed,
}

impl SurfaceLifecycle {
    fn can_create(self) -> bool {
        self == Self::WaitingForOutput
    }
    fn opened(&mut self) {
        *self = Self::Open;
    }
    fn closed(&mut self) {
        *self = Self::Closed;
    }
    fn output_changed(&mut self) {
        *self = Self::WaitingForOutput;
    }
}

impl HudState {
    fn create_layer(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("cantrip-hud"),
            None,
        );
        layer.set_anchor(Anchor::BOTTOM);
        layer.set_margin(0, 0, 36, 0);
        layer.set_size(self.requested_size.0, self.requested_size.1);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        let empty = Region::new(&self.compositor).context("creating HUD pass-through region")?;
        layer.set_input_region(Some(empty.wl_region()));
        layer.commit();
        self.layer = Some(layer);
        self.surface_lifecycle.opened();
        self.layer_output = None;
        self.buffer_scale = 1;
        self.configured = false;
        self.frame_pending = false;
        self.instrument.detached();
        Ok(())
    }

    fn current_surface(&self, surface: &wl_surface::WlSurface) -> bool {
        self.layer
            .as_ref()
            .is_some_and(|layer| layer.wl_surface() == surface)
    }

    fn available_width(&self) -> u32 {
        self.layer_output
            .as_ref()
            .and_then(|output| self.output_state.info(output))
            .and_then(|info| info.logical_size)
            .map(|(width, _)| width.max(1) as u32)
            .unwrap_or(SURFACE_WIDTH)
            .min(SURFACE_WIDTH)
    }

    fn redraw(&mut self, qh: &QueueHandle<Self>, now: Instant) -> Result<()> {
        if !self.configured || self.frame_pending {
            return Ok(());
        }
        let Some(layer) = &self.layer else {
            return Ok(());
        };
        let model_now = if self.screenshot.is_some() {
            self.screenshot_at
        } else {
            now
        };
        let logical_width = self.available_width().max(80);
        let container_width = CONTAINER_WIDTH.min(self.width.min(logical_width) as f32 - 12.0);
        let target_height = self.instrument.height(container_width);
        let desired = (logical_width, target_height);
        if self.requested_size != desired {
            self.requested_size = desired;
            layer.set_size(desired.0, desired.1);
            layer.commit();
            self.configured = false;
            // No old-size frame can become the final screenshot.
            return Ok(());
        }
        let alpha = if self.screenshot.is_some() {
            1.0
        } else {
            self.instrument.alpha(now)
        };
        let Some(presentation) = self.instrument.prepare(
            model_now,
            alpha,
            (self.width, self.height),
            self.buffer_scale,
            container_width,
            self.screenshot.is_some(),
        ) else {
            return Ok(());
        };
        let width = self
            .width
            .checked_mul(self.buffer_scale)
            .context("HUD buffer width overflow")?;
        let height = self
            .height
            .checked_mul(self.buffer_scale)
            .context("HUD buffer height overflow")?;
        let stride = width.checked_mul(4).context("HUD buffer stride overflow")?;
        let (buffer, bytes) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .context("creating HUD frame")?;
        self.instrument
            .paint(&presentation, bytes, width, height, self.buffer_scale);
        let _ = layer.set_buffer_scale(self.buffer_scale);
        layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(layer.wl_surface())
            .context("attaching HUD frame")?;
        if self.screenshot.is_none() {
            // Present at the compositor's cadence, never on buffer-release timing.
            layer
                .wl_surface()
                .frame(qh, FrameCallbackData(layer.wl_surface().clone()));
            self.frame_pending = true;
        }
        layer.commit();
        self.instrument.presented(presentation);
        // Keep a transparent mapped frame while idle: remapping requires a
        // second configure handshake some compositors do not send.
        if let Some(path) = &self.screenshot {
            save_screenshot(path, bytes, width, height)?;
            self.screenshot_done = true;
        }
        Ok(())
    }
}

impl CompositorHandler for HudState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if !self.current_surface(surface) {
            return;
        }
        let scale = factor.max(1) as u32;
        use wayland_client::Proxy;
        self.buffer_scale = if surface.version() >= 3 { scale } else { 1 };
        self.instrument.invalidate();
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _transform: wl_output::Transform,
    ) {
        if self.current_surface(surface) {
            self.instrument.invalidate();
        }
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if self.current_surface(surface) {
            self.frame_pending = false;
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if self.current_surface(surface) {
            self.layer_output = Some(output.clone());
            self.instrument.invalidate();
        }
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // Keep the last output identity until it is destroyed or a new enter
        // arrives; wl_surface.leave often precedes output_destroyed.
    }
}

impl LayerShellHandler for HudState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.current_surface(layer.wl_surface()) {
            self.layer = None;
            self.configured = false;
            self.instrument.detached();
            self.surface_lifecycle.closed();
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if !self.current_surface(layer.wl_surface()) {
            return;
        }
        self.width = if configure.new_size.0 > 0 {
            configure.new_size.0
        } else {
            self.requested_size.0
        };
        self.height = if configure.new_size.1 > 0 {
            configure.new_size.1
        } else {
            self.requested_size.1
        };
        self.configured = true;
        self.instrument.invalidate();
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
        if self.layer.is_none() {
            self.surface_lifecycle.output_changed();
        }
    }
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.layer_output.as_ref() == Some(&output) {
            self.instrument.invalidate();
            if self.layer.is_none() {
                self.surface_lifecycle.output_changed();
            }
        }
    }
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.layer_output.as_ref() == Some(&output) || self.layer.is_none() {
            self.layer = None;
            self.layer_output = None;
            self.configured = false;
            self.instrument.detached();
            self.surface_lifecycle.output_changed();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_close_waits_for_an_output_event_before_reopening() {
        let mut lifecycle = SurfaceLifecycle::Open;
        lifecycle.closed();
        assert!(
            !lifecycle.can_create(),
            "ordinary loop ticks must respect compositor closure"
        );
        lifecycle.output_changed();
        assert!(lifecycle.can_create(), "real hotplug permits a replacement");
        lifecycle.opened();
        assert!(
            !lifecycle.can_create(),
            "an existing surface must not be duplicated"
        );
    }
}
