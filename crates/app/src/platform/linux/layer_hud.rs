//! The recording HUD on native Wayland compositors with `zwlr_layer_shell_v1`
//! (Sway and other wlroots compositors, KDE Plasma, Hyprland): a layer
//! surface on `Layer::Overlay`, anchored to the bottom edge (horizontally
//! centred) with a bottom margin, `KeyboardInteractivity::None` (it can never
//! take focus) and an empty input region (pointer clicks pass through). Layer
//! surfaces belong to an output, not a workspace, so the pill stays visible on
//! every workspace. Pixels come from the UI's CPU rendering of the same pill
//! (`crate::ui::raster`) into a `wl_shm` buffer.
//!
//! One thread owns the Wayland connection; the UI sends it frames and writes
//! a byte to a wake pipe, so the thread (blocked in `poll` on the Wayland fd
//! and that pipe) acts on a new frame at once instead of on a timer. Showing
//! creates the surface, hiding destroys it (a fresh configure on each show).
//! A compositor without the protocol (GNOME) gets `None` from
//! [`LayerHud::start`], and the UI keeps its own HUD window.

use crate::platform::{HudFrame, NativeHud};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

/// Longest wait in `poll` with nothing to do (the wake pipe and the Wayland
/// fd end it early; this only bounds a lost wake-up).
const POLL: Duration = Duration::from_millis(500);

enum Msg {
    Show(HudFrame),
    Hide,
}

/// Handle to the layer-shell HUD thread.
pub struct LayerHud {
    tx: Mutex<Sender<Msg>>,
    /// Write end of the wake pipe (non-blocking).
    wake: OwnedFd,
    scale: Arc<AtomicU32>,
    alive: Arc<AtomicBool>,
}

impl LayerHud {
    /// Connect to `$WAYLAND_DISPLAY` and bind the layer shell. `None` when
    /// the session has no compositor connection or no layer-shell protocol.
    pub fn start() -> Option<LayerHud> {
        let conn = Connection::connect_to_env().ok()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).ok()?;
        let layer_shell = LayerShell::bind(&globals, &qh).ok()?;
        let shm = Shm::bind(&globals, &qh).ok()?;
        let pool = SlotPool::new(256 * 64 * 4, &shm).ok()?;
        let scale = Arc::new(AtomicU32::new(1));
        let alive = Arc::new(AtomicBool::new(true));
        let mut state = State {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            compositor,
            layer_shell,
            shm,
            pool,
            layer: None,
            logical: (0, 0),
            configured: false,
            pending: None,
            scale: scale.clone(),
        };
        queue.roundtrip(&mut state).ok()?;
        state.update_scale_from_outputs();
        let (tx, rx) = mpsc::channel();
        let (wake_rx, wake_tx) = wake_pipe()?;
        let alive2 = alive.clone();
        std::thread::Builder::new()
            .name("cantrip-layer-hud".into())
            .spawn(move || {
                if let Err(e) = run(&conn, &mut queue, &mut state, &rx, &wake_rx) {
                    eprintln!("[cantrip] layer-shell HUD stopped: {e}");
                }
                alive2.store(false, Ordering::SeqCst);
            })
            .ok()?;
        Some(LayerHud {
            tx: Mutex::new(tx),
            wake: wake_tx,
            scale,
            alive,
        })
    }
}

impl NativeHud for LayerHud {
    fn scale(&self) -> u32 {
        self.scale.load(Ordering::SeqCst).max(1)
    }

    fn present(&self, frame: Option<HudFrame>) -> bool {
        if !self.alive.load(Ordering::SeqCst) {
            return false;
        }
        let msg = match frame {
            Some(f) => Msg::Show(f),
            None => Msg::Hide,
        };
        let sent = self
            .tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(msg)
            .is_ok();
        // SAFETY: one-byte write to our own non-blocking pipe; a full pipe
        // (EAGAIN) already holds a pending wake-up.
        unsafe { libc::write(self.wake.as_raw_fd(), b"w".as_ptr().cast(), 1) };
        sent
    }
}

/// A non-blocking, close-on-exec pipe: (read end, write end).
fn wake_pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills two fds we then own.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return None;
    }
    // SAFETY: both fds were just created and are owned by nobody else.
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Empty the wake pipe.
fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a local buffer from our own non-blocking pipe.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

fn run(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    rx: &Receiver<Msg>,
    wake: &OwnedFd,
) -> Result<(), String> {
    let qh = queue.handle();
    loop {
        drain(wake);
        // Newest message wins; frames in between are stale.
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(m) => latest = Some(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    state.hide();
                    let _ = conn.flush();
                    return Ok(());
                }
            }
        }
        match latest {
            Some(Msg::Show(f)) => state.show(f, &qh)?,
            Some(Msg::Hide) => state.hide(),
            None => {}
        }
        state.draw()?;
        dispatch(queue, state, POLL, wake)?;
    }
}

/// Flush, wait up to `timeout` for compositor events or a wake-up from the
/// UI, dispatch the events.
fn dispatch(
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
    wake: &OwnedFd,
) -> Result<(), String> {
    // A wake-up written since the last drain is still in the pipe, so a frame
    // sent meanwhile never waits for the timeout.
    let w = libc::pollfd {
        fd: wake.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    queue.flush().map_err(|e| e.to_string())?;
    let Some(guard) = queue.prepare_read() else {
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        return Ok(());
    };
    let mut fds = [
        libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        w,
    ];
    // SAFETY: poll on two valid pollfds.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.as_millis() as i32) };
    if n > 0 && fds[0].revents & libc::POLLIN != 0 {
        guard.read().map_err(|e| e.to_string())?;
    } else {
        drop(guard);
    }
    queue.dispatch_pending(state).map_err(|e| e.to_string())?;
    Ok(())
}

struct State {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    /// Requested logical size of the current surface.
    logical: (u32, u32),
    configured: bool,
    /// The frame to attach once configured.
    pending: Option<HudFrame>,
    scale: Arc<AtomicU32>,
}

impl State {
    fn update_scale_from_outputs(&self) {
        let s = self
            .outputs
            .outputs()
            .filter_map(|o| self.outputs.info(&o))
            .map(|i| i.scale_factor)
            .max()
            .unwrap_or(1)
            .max(1);
        self.scale.store(s as u32, Ordering::SeqCst);
    }

    fn show(&mut self, f: HudFrame, qh: &QueueHandle<State>) -> Result<(), String> {
        let s = f.scale.max(1);
        let logical = (f.width / s, f.height / s);
        match &self.layer {
            None => {
                let surface = self.compositor.create_surface(qh);
                let layer = self.layer_shell.create_layer_surface(
                    qh,
                    surface,
                    Layer::Overlay,
                    Some("cantrip-hud"),
                    None,
                );
                layer.set_anchor(Anchor::BOTTOM);
                layer.set_margin(0, 0, f.bottom_margin as i32, 0);
                layer.set_exclusive_zone(0);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.set_size(logical.0, logical.1);
                let empty = Region::new(&self.compositor).map_err(|e| e.to_string())?;
                layer.set_input_region(Some(empty.wl_region()));
                layer.commit();
                self.layer = Some(layer);
                self.configured = false;
            }
            Some(layer) if logical != self.logical => {
                layer.set_size(logical.0, logical.1);
                layer.commit();
                self.configured = false;
            }
            Some(_) => {}
        }
        self.logical = logical;
        self.pending = Some(f);
        Ok(())
    }

    fn hide(&mut self) {
        self.pending = None;
        self.configured = false;
        // Dropping destroys the layer surface and its wl_surface: unmapped.
        self.layer = None;
    }

    fn draw(&mut self) -> Result<(), String> {
        if !self.configured {
            return Ok(());
        }
        let (Some(layer), Some(f)) = (&self.layer, self.pending.take()) else {
            return Ok(());
        };
        let stride = f.width as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                f.width as i32,
                f.height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .map_err(|e| e.to_string())?;
        let n = canvas.len().min(f.argb.len());
        canvas[..n].copy_from_slice(&f.argb[..n]);
        let _ = layer.set_buffer_scale(f.scale.max(1));
        layer
            .wl_surface()
            .damage_buffer(0, 0, f.width as i32, f.height as i32);
        buffer
            .attach_to(layer.wl_surface())
            .map_err(|e| e.to_string())?;
        layer.commit();
        Ok(())
    }

    fn is_current(&self, surface: &wl_surface::WlSurface) -> bool {
        self.layer
            .as_ref()
            .is_some_and(|l| l.wl_surface() == surface)
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if self.is_current(surface) {
            self.scale.store(factor.max(1) as u32, Ordering::SeqCst);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor withdrew the surface (output gone): the next frame
        // creates a fresh one.
        if self.is_current(layer.wl_surface()) {
            self.layer = None;
            self.configured = false;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.is_current(layer.wl_surface()) {
            self.configured = true;
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_scale_from_outputs();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.update_scale_from_outputs();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);
