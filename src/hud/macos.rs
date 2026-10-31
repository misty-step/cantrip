//! Native macOS surface for the shared HUD instrument.
//!
//! A borderless, nonactivating panel that ignores the pointer, joins every space
//! (including full-screen ones), sits at the bottom of the focused screen's usable
//! area and draws the instrument's pixels at that screen's backing scale. It never
//! becomes key, never activates Cantrip and never needs a permission.
//!
//! The instrument's container is a system material (ADR 0034): Liquid Glass on
//! macOS 26 and later, the HUD window material before, and an opaque system
//! background while Reduce Transparency or Increase Contrast is on. The shared
//! painter draws only the field, rail and captions over it.

use super::{
    acquire_instance_lock, container_rect, save_screenshot, Backing, Instrument, Poller,
    ScreenshotState, CONTAINER_WIDTH, FRAME_INTERVAL, POLL_INTERVAL, PREFERENCE_INTERVAL,
    ROUTE_WASH, SURFACE_HEIGHT, SURFACE_WIDTH,
};
use crate::theme::{self, Palette};
use anyhow::{Context, Result};
use block2::RcBlock;
use cantrip_engine::ipc::StatusSnapshot;
use objc2::{
    available, define_class, msg_send,
    rc::{autoreleasepool, Retained},
    runtime::Bool,
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSAppearanceCustomization, NSApplication, NSApplicationActivationPolicy,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSBitmapFormat, NSBitmapImageRep,
    NSColor, NSColorSpace, NSDeviceRGBColorSpace, NSEventMask, NSGlassEffectView,
    NSGlassEffectViewStyle, NSGraphicsContext, NSImage, NSPanel, NSScreen, NSStatusWindowLevel,
    NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState,
    NSVisualEffectView, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext,
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::Arc,
    time::{Duration, Instant},
};

/// Gap above the bottom of the screen's usable area, matching the Wayland layer margin.
const BOTTOM_MARGIN: f64 = 36.0;
/// The container's corner radius. The field's corner cells stay about four
/// points inside the curve.
const CORNER_RADIUS: f64 = 12.0;

/// The system Reduce Motion setting. NSWorkspace answers on any thread; the
/// status reader's thread has no ambient autorelease pool.
pub(super) fn desktop_reduced_motion() -> Option<bool> {
    Some(autoreleasepool(|_| {
        NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }))
}

pub(super) fn run_native(
    screenshot: Option<PathBuf>,
    state: Option<ScreenshotState>,
    handoff: Option<String>,
) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the HUD must run on the main thread")?;
    if let Some(path) = screenshot {
        return render_offscreen(
            mtm,
            &path,
            state.unwrap_or(ScreenshotState::Recording),
            handoff,
        );
    }
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let mut hud = LiveHud::start(mtm)?;
    if !hud.owns_surface() {
        // Another HUD already presents this session, as on Linux.
        return Ok(());
    }
    app.finishLaunching();
    loop {
        let interval = autoreleasepool(|_| hud.tick(mtm, Instant::now()));
        autoreleasepool(|_| pump(&app, interval));
    }
}

/// Wait up to `timeout` for the first event, then dispatch every queued event.
fn pump(app: &NSApplication, timeout: Duration) {
    let deadline = NSDate::dateWithTimeIntervalSinceNow(timeout.as_secs_f64());
    // SAFETY: a framework constant, valid for the process lifetime.
    let mode = unsafe { NSDefaultRunLoopMode };
    let mut until = Some(&*deadline);
    while let Some(event) =
        app.nextEventMatchingMask_untilDate_inMode_dequeue(NSEventMask::Any, until, mode, true)
    {
        app.sendEvent(&event);
        until = None;
    }
}

/// Render a composed scenario through the real HUD view into an owned bitmap.
/// No window is ordered on screen, nothing is read from the display, and the
/// process can neither activate nor show a Dock icon. The window server
/// composites system materials only on screen, so the container shows the
/// opaque system background that Reduce Transparency selects.
fn render_offscreen(
    mtm: MainThreadMarker,
    path: &Path,
    state: ScreenshotState,
    handoff: Option<String>,
) -> Result<()> {
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let now = Instant::now();
    let mut instrument = Instrument::preview(state, handoff, now)?;
    // The focused screen's geometry and scale, like the live panel; a session
    // without a screen renders at the Retina scale every current Mac uses.
    let placement = Placement::focused(mtm).unwrap_or(Placement {
        visible: NSRect::new(
            NSPoint::ZERO,
            NSSize::new(f64::from(SURFACE_WIDTH), f64::from(SURFACE_HEIGHT)),
        ),
        scale: 2,
    });
    let view = HudView::new(mtm);
    view.set_underlay(Underlay::Opaque(route(&instrument)));
    let frame = paint_frame(&view, &mut instrument, placement, now, true)
        .context("composing the HUD screenshot")?;
    let size = frame.container.size;
    view.setFrameSize(size);
    let (pixel_width, pixel_height) = (
        size.width as u32 * placement.scale,
        size.height as u32 * placement.scale,
    );
    let bytes = view.render_offscreen(pixel_width, pixel_height)?;
    eprintln!("rendered the HUD view offscreen at {}x", placement.scale);
    save_screenshot(path, &bytes, pixel_width, pixel_height)
}

/// The per-user runtime directory, validated and made owner-only exactly as
/// the engine prepares it, before this process places a lock file there.
pub(crate) fn runtime_dir() -> Result<PathBuf> {
    cantrip_engine::paths::ensure_dir(cantrip_engine::paths::runtime_dir()?)
}

fn acquire_lock() -> Option<fs::File> {
    match runtime_dir().and_then(|_| acquire_instance_lock()) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!("[HUD] instance lock unavailable: {error:#}");
            None
        }
    }
}

/// Usable logical width on an output `available` points wide.
fn surface_width(available: f64) -> u32 {
    (available.max(1.0) as u32).clamp(80, SURFACE_WIDTH)
}

/// The color of the take's handoff route, if it has one.
fn route(instrument: &Instrument) -> Option<[u8; 3]> {
    instrument
        .model
        .route
        .as_ref()
        .map(|route| route.handoff.color)
}

/// One painted frame: the panel's logical size, the container's frame within
/// it, and whether it shows the instrument.
struct Frame {
    width: u32,
    height: u32,
    container: NSRect,
    shown: bool,
}

/// Size the instrument for `placement` and paint its next frame into `view`,
/// which shows the container, unless the presented pixels already match.
fn paint_frame(
    view: &HudView,
    instrument: &mut Instrument,
    placement: Placement,
    now: Instant,
    force: bool,
) -> Option<Frame> {
    let width = surface_width(placement.visible.size.width);
    let container_width = CONTAINER_WIDTH.min(width as f32 - 12.0);
    let height = instrument.height(container_width);
    let scale = placement.scale;
    // The whole panel fades, so the material fades with the pixels.
    let presentation =
        instrument.prepare(now, 1.0, (width, height), scale, container_width, force)?;
    let [left, top, container_width, container_height] =
        container_rect(width as f32, height as f32, container_width);
    // AppKit counts up from the panel's bottom edge.
    let container = NSRect::new(
        NSPoint::new(
            f64::from(left),
            f64::from(height as f32 - top - container_height),
        ),
        NSSize::new(f64::from(container_width), f64::from(container_height)),
    );
    // The whole surface, placed so that its container fills the view.
    let surface = NSRect::new(
        NSPoint::new(-container.origin.x, -container.origin.y),
        NSSize::new(f64::from(width), f64::from(height)),
    );
    let (pixel_width, pixel_height) = (width * scale, height * scale);
    view.paint(pixel_width, pixel_height, surface, |bytes| {
        instrument.paint(
            &presentation,
            bytes,
            pixel_width,
            pixel_height,
            scale,
            Backing::Native,
        )
    });
    let shown = presentation.shown;
    instrument.presented(presentation);
    Some(Frame {
        width,
        height,
        container,
        shown,
    })
}

/// The live HUD on the AppKit main thread: the shared instrument fed by one
/// status reader, presented while this process holds the HUD instance lock.
pub(crate) struct LiveHud {
    instrument: Instrument,
    poller: Poller,
    panel: Panel,
    lock: Option<fs::File>,
    preferences_at: Instant,
    /// The system appearance's colors, for surfaces beyond the panel.
    palette: Palette,
}

impl LiveHud {
    pub(crate) fn start(mtm: MainThreadMarker) -> Result<Self> {
        let now = Instant::now();
        Ok(Self {
            // Resolves the appearance on this thread before the reader's first sample.
            instrument: Instrument::live(now)?,
            poller: Poller::start()?,
            panel: Panel::new(mtm),
            lock: acquire_lock(),
            preferences_at: now,
            palette: theme::load(),
        })
    }

    /// False while another HUD process presents this session's status.
    pub(crate) fn owns_surface(&self) -> bool {
        self.lock.is_some()
    }

    /// Fold status, present one frame and return the delay before the next tick.
    pub(crate) fn tick(&mut self, mtm: MainThreadMarker, now: Instant) -> Duration {
        if now.duration_since(self.preferences_at) >= PREFERENCE_INTERVAL {
            self.preferences_at = now;
            // AppKit resolves the appearance only here; the status reader
            // publishes this snapshot with its next sample.
            self.palette = theme::load();
            self.panel.refresh(mtm);
            if self.lock.is_none() {
                self.lock = acquire_lock();
            }
        }
        self.instrument.poll(&self.poller, now);
        if self.lock.is_none() {
            return POLL_INTERVAL;
        }
        self.panel.present(&mut self.instrument, mtm, now);
        if self.instrument.animate(now) {
            FRAME_INTERVAL
        } else {
            POLL_INTERVAL
        }
    }

    /// The engine's latest snapshot while its status connection is live.
    pub(crate) fn status(&self) -> Option<&StatusSnapshot> {
        let model = &self.instrument.model;
        model
            .snapshot
            .as_ref()
            .filter(|_| model.lost_since.is_none())
    }

    pub(crate) fn reduced_motion(&self) -> bool {
        self.instrument.model.reduced_motion
    }

    pub(crate) fn palette(&self) -> Palette {
        self.palette
    }
}

/// Where the instrument appears: one screen's usable area and backing scale.
#[derive(Clone, Copy)]
struct Placement {
    visible: NSRect,
    scale: u32,
}

impl Placement {
    /// AppKit's main screen holds the keyboard focus; otherwise the first screen.
    fn focused(mtm: MainThreadMarker) -> Option<Self> {
        let screen = NSScreen::mainScreen(mtm).or_else(|| NSScreen::screens(mtm).firstObject())?;
        Some(Self {
            visible: screen.visibleFrame(),
            scale: screen.backingScaleFactor().round().max(1.0) as u32,
        })
    }

    /// Bottom-centred in the usable area; growth extends upward like the layer.
    fn frame(&self, width: u32, height: u32) -> NSRect {
        let width = f64::from(width);
        NSRect::new(
            NSPoint::new(
                (self.visible.origin.x + (self.visible.size.width - width) / 2.0).round(),
                (self.visible.origin.y + BOTTOM_MARGIN).round(),
            ),
            NSSize::new(width, f64::from(height)),
        )
    }
}

/// What shows through the instrument's container.
enum Backdrop {
    /// Liquid Glass, macOS 26 and later, with the instrument as its content
    /// view.
    Glass(Retained<NSGlassEffectView>),
    /// The HUD window material on earlier releases, holding the instrument.
    Material(Retained<NSVisualEffectView>),
    /// No material: the instrument's view fills an opaque system background.
    Opaque,
}

impl Backdrop {
    /// Reduce Transparency and Increase Contrast ask for an opaque background.
    fn opaque_preferred() -> bool {
        let workspace = NSWorkspace::sharedWorkspace();
        workspace.accessibilityDisplayShouldReduceTransparency()
            || workspace.accessibilityDisplayShouldIncreaseContrast()
    }

    /// The backdrop the display preferences allow, holding `view` at the
    /// container's `frame` in `root`.
    fn new(mtm: MainThreadMarker, root: &NSView, view: &HudView, frame: NSRect) -> Self {
        // Liquid Glass may lay its content view out with constraints; the
        // others place the view by its frame.
        view.setTranslatesAutoresizingMaskIntoConstraints(true);
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);
        let backdrop = if Self::opaque_preferred() {
            root.addSubview(view);
            Self::Opaque
        } else if available!(macos = 26.0) {
            let glass = NSGlassEffectView::initWithFrame(NSGlassEffectView::alloc(mtm), frame);
            glass.setStyle(NSGlassEffectViewStyle::Regular);
            glass.setCornerRadius(CORNER_RADIUS);
            fill(view, &glass);
            glass.setContentView(Some(view));
            root.addSubview(&glass);
            Self::Glass(glass)
        } else {
            let material = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
            material.setMaterial(NSVisualEffectMaterial::HUDWindow);
            material.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            // The panel is never key; keep the material active regardless.
            material.setState(NSVisualEffectState::Active);
            fill(view, &material);
            material.addSubview(view);
            root.addSubview(&material);
            Self::Material(material)
        };
        backdrop.place(view, frame);
        backdrop
    }

    /// Take the backdrop and the instrument's view out of the panel.
    fn remove(&self, view: &HudView) {
        match self {
            Self::Glass(glass) => {
                glass.setContentView(None);
                glass.removeFromSuperview();
            }
            Self::Material(material) => material.removeFromSuperview(),
            Self::Opaque => {}
        }
        view.removeFromSuperview();
    }

    /// Move the container to `frame` in the panel.
    fn place(&self, view: &HudView, frame: NSRect) {
        match self {
            Self::Glass(glass) => glass.setFrame(frame),
            Self::Material(material) => {
                material.setFrame(frame);
                material.setMaskImage(Some(&rounded(frame.size)));
            }
            Self::Opaque => view.setFrame(frame),
        }
    }

    /// Carry a handoff route's color: Liquid Glass tints toward it; otherwise
    /// the instrument's view washes and edges its container with it.
    fn tint(&self, view: &HudView, route: Option<[u8; 3]>) {
        view.set_underlay(match self {
            Self::Glass(glass) => {
                let tint = route.map(|color| srgb(color, f64::from(ROUTE_WASH)));
                glass.setTintColor(tint.as_deref());
                Underlay::Glass
            }
            Self::Material(_) => Underlay::Material(route),
            Self::Opaque => Underlay::Opaque(route),
        });
    }
}

/// Keep `view` filling `parent` as the container resizes.
fn fill(view: &HudView, parent: &NSView) {
    view.setFrame(parent.bounds());
    view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
}

/// The container's rounded shape at `size`, as a material's mask.
fn rounded(size: NSSize) -> Retained<NSImage> {
    let draw = RcBlock::new(|bounds: NSRect| -> Bool {
        NSColor::blackColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            bounds,
            CORNER_RADIUS,
            CORNER_RADIUS,
        )
        .fill();
        Bool::YES
    });
    NSImage::imageWithSize_flipped_drawingHandler(size, false, &draw)
}

fn srgb([red, green, blue]: [u8; 3], alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(red) / 255.0,
        f64::from(green) / 255.0,
        f64::from(blue) / 255.0,
        alpha,
    )
}

struct Panel {
    window: Retained<NSPanel>,
    /// The panel's content view; the backdrop sits in it at the container's frame.
    root: Retained<NSView>,
    view: Retained<HudView>,
    backdrop: Backdrop,
    /// The container's frame in the panel.
    container: NSRect,
    /// The handoff route color the backdrop carries.
    route: Option<[u8; 3]>,
    placement: Option<Placement>,
    /// The colors of the instrument view's effective appearance, followed as
    /// soon as AppKit changes it.
    palette: Palette,
}

impl Panel {
    fn new(mtm: MainThreadMarker) -> Self {
        let frame = NSRect::new(
            NSPoint::ZERO,
            NSSize::new(f64::from(SURFACE_WIDTH), f64::from(SURFACE_HEIGHT)),
        );
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            true,
        );
        // SAFETY: `Panel` owns the only reference AppKit must not release on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        // Pointer input always reaches the windows beneath the instrument.
        window.setIgnoresMouseEvents(true);
        window.setLevel(NSStatusWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHidesOnDeactivate(false);
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);
        window.setExcludedFromWindowsMenu(true);
        let root = NSView::initWithFrame(NSView::alloc(mtm), frame);
        window.setContentView(Some(&root));
        let view = HudView::new(mtm);
        let container = NSRect::ZERO;
        let backdrop = Backdrop::new(mtm, &root, &view, container);
        backdrop.tint(&view, None);
        let palette = theme::resolve(&view.effectiveAppearance());
        Self {
            window,
            root,
            view,
            backdrop,
            container,
            route: None,
            placement: None,
            palette,
        }
    }

    /// Follow the display preferences: the backdrop they allow and the colors
    /// of the instrument view's appearance.
    fn refresh(&mut self, mtm: MainThreadMarker) {
        if Backdrop::opaque_preferred() != matches!(self.backdrop, Backdrop::Opaque) {
            self.backdrop.remove(&self.view);
            self.backdrop = Backdrop::new(mtm, &self.root, &self.view, self.container);
            self.backdrop.tint(&self.view, self.route);
        }
        self.palette = theme::resolve(&self.view.effectiveAppearance());
    }

    fn present(&mut self, instrument: &mut Instrument, mtm: MainThreadMarker, now: Instant) {
        // Follow keyboard focus whenever the instrument appears; once shown it
        // stays put unless its screen goes away.
        let ordered = self.window.isVisible();
        if !ordered || self.window.screen().is_none() {
            self.placement = Placement::focused(mtm);
        }
        let Some(placement) = self.placement else {
            return;
        };
        if self.view.ivars().appearance_changed.take() {
            self.palette = theme::resolve(&self.view.effectiveAppearance());
        }
        instrument.palette = self.palette;
        let route = route(instrument);
        if route != self.route {
            self.route = route;
            self.backdrop.tint(&self.view, route);
        }
        let alpha = f64::from(instrument.alpha(now));
        if self.window.alphaValue() != alpha {
            self.window.setAlphaValue(alpha);
        }
        let Some(frame) = paint_frame(&self.view, instrument, placement, now, false) else {
            return;
        };
        let window_frame = placement.frame(frame.width, frame.height);
        if self.window.frame() != window_frame {
            self.window.setFrame_display(window_frame, false);
        }
        if self.container != frame.container {
            self.container = frame.container;
            self.backdrop.place(&self.view, frame.container);
        }
        self.view.setNeedsDisplay(true);
        if frame.shown && !ordered {
            // Shown without activating Cantrip or taking keyboard focus.
            self.window.orderFrontRegardless();
        } else if !frame.shown && ordered {
            self.window.orderOut(None);
        }
    }
}

/// What the instrument's view draws inside the container's rounded shape,
/// beneath its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Underlay {
    /// Nothing: Liquid Glass supplies the surface, its rim and the route's tint.
    Glass,
    /// Over a material, a one-point edge and any route's wash, both in the
    /// route's color when the take has one, as the painted container's border.
    Material(Option<[u8; 3]>),
    /// The same over an opaque system background.
    Opaque(Option<[u8; 3]>),
}

impl Underlay {
    /// Draw into `bounds`, which `shape` rounds and clips.
    fn draw(self, bounds: NSRect, shape: &NSBezierPath) {
        let route = match self {
            Self::Glass => return,
            Self::Material(route) => route,
            Self::Opaque(route) => {
                NSColor::windowBackgroundColor().setFill();
                shape.fill();
                route
            }
        };
        if let Some(color) = route {
            srgb(color, f64::from(ROUTE_WASH)).setFill();
            shape.fill();
        }
        let edge = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            NSRect::new(
                NSPoint::new(bounds.origin.x + 0.5, bounds.origin.y + 0.5),
                NSSize::new(bounds.size.width - 1.0, bounds.size.height - 1.0),
            ),
            CORNER_RADIUS - 0.5,
            CORNER_RADIUS - 0.5,
        );
        edge.setLineWidth(1.0);
        match route {
            Some(color) => srgb(color, 1.0),
            None => NSColor::separatorColor(),
        }
        .setStroke();
        edge.stroke();
    }
}

/// The instrument's latest premultiplied BGRA pixels.
struct Pixels {
    bytes: Arc<Vec<u8>>,
    width: usize,
    height: usize,
    /// Where the surface they cover lies in the view, in points.
    surface: NSRect,
    /// Theme colors are sRGB; Core Graphics matches them to each display.
    space: Option<CFRetained<CGColorSpace>>,
}

impl Pixels {
    /// A Core Graphics image over these pixels. Its provider holds one strong
    /// count, so painting the next frame copies instead of mutating them while
    /// Core Graphics may still read them.
    fn image(&self) -> Option<CFRetained<CGImage>> {
        let space = self.space.as_deref()?;
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let info = Arc::into_raw(Arc::clone(&self.bytes))
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: `info` is one leaked strong count of the buffer `data` points
        // into; `release_pixels` reclaims it exactly once when the provider dies.
        let provider = unsafe {
            CGDataProvider::with_data(
                info,
                self.bytes.as_ptr().cast(),
                self.bytes.len(),
                Some(release_pixels),
            )
        };
        let Some(provider) = provider else {
            // SAFETY: no provider was created, so the count leaked above is still ours.
            drop(unsafe { Arc::from_raw(info.cast_const().cast::<Vec<u8>>()) });
            return None;
        };
        // SAFETY: the provider holds `width * height` 32-bit pixels in rows of
        // `width * 4` bytes; no decode array is passed.
        unsafe {
            CGImage::new(
                self.width,
                self.height,
                8,
                32,
                self.width * 4,
                Some(space),
                CGBitmapInfo(
                    CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0,
                ),
                Some(&provider),
                ptr::null(),
                false,
                CGColorRenderingIntent::RenderingIntentDefault,
            )
        }
    }
}

unsafe extern "C-unwind" fn release_pixels(
    info: *mut c_void,
    _data: NonNull<c_void>,
    _size: usize,
) {
    // SAFETY: `info` is the strong count `Pixels::image` leaked for this provider.
    drop(unsafe { Arc::from_raw(info.cast_const().cast::<Vec<u8>>()) });
}

/// The instrument's pixels and what its view draws beneath them.
struct HudState {
    pixels: RefCell<Pixels>,
    underlay: Cell<Underlay>,
    /// AppKit changed the view's effective appearance, as switching between
    /// Light and Dark does.
    appearance_changed: Cell<bool>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; `HudView` has no `Drop`.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripHudView"]
    #[ivars = HudState]
    struct HudView;

    impl HudView {
        // SAFETY: the signature matches `-[NSView drawRect:]`.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        // SAFETY: the signature matches `-[NSView viewDidChangeEffectiveAppearance]`.
        #[unsafe(method(viewDidChangeEffectiveAppearance))]
        fn effective_appearance_changed(&self) {
            self.ivars().appearance_changed.set(true);
            // SAFETY: NSView implements this notification, with no arguments.
            let () = unsafe { msg_send![super(self), viewDidChangeEffectiveAppearance] };
        }
    }
);

impl HudView {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HudState {
            pixels: RefCell::new(Pixels {
                bytes: Arc::new(Vec::new()),
                width: 0,
                height: 0,
                surface: NSRect::ZERO,
                // SAFETY: a framework constant, valid for the process lifetime.
                space: CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })),
            }),
            underlay: Cell::new(Underlay::Glass),
            appearance_changed: Cell::new(true),
        });
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }

    /// Replace the pixels in place once Core Graphics has released the last
    /// frame. They cover `surface`, in the view's points.
    fn paint(&self, width: u32, height: u32, surface: NSRect, paint: impl FnOnce(&mut [u8])) {
        let mut pixels = self.ivars().pixels.borrow_mut();
        let bytes = Arc::make_mut(&mut pixels.bytes);
        bytes.resize(width as usize * height as usize * 4, 0);
        paint(bytes);
        pixels.width = width as usize;
        pixels.height = height as usize;
        pixels.surface = surface;
    }

    fn set_underlay(&self, underlay: Underlay) {
        if self.ivars().underlay.replace(underlay) != underlay {
            self.setNeedsDisplay(true);
        }
    }

    /// Draw the underlay, then the pixels one-to-one into the view's backing,
    /// all inside the container's rounded shape; transparent elsewhere.
    fn draw(&self) {
        let Some(context) = NSGraphicsContext::currentContext() else {
            return;
        };
        let context = context.CGContext();
        let bounds = self.bounds();
        CGContext::clear_rect(Some(&context), bounds);
        let shape = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
            bounds,
            CORNER_RADIUS,
            CORNER_RADIUS,
        );
        // The shape also clips the attention rail along the container's left edge.
        shape.addClip();
        self.ivars().underlay.get().draw(bounds, &shape);
        let pixels = self.ivars().pixels.borrow();
        if let Some(image) = pixels.image() {
            CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
            CGContext::draw_image(Some(&context), pixels.surface, Some(&image));
        }
    }

    /// Draw this view through AppKit into an owned sRGB bitmap, never from the
    /// display, and return its pixels as premultiplied BGRA.
    fn render_offscreen(&self, width: u32, height: u32) -> Result<Vec<u8>> {
        let row = width as usize * 4;
        // SAFETY: null planes ask AppKit to allocate the single-plane buffer.
        let rep = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                ptr::null_mut(),
                width as isize,
                height as isize,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                NSBitmapFormat::empty(),
                row as isize,
                32,
            )
        }
        .context("allocating the offscreen HUD bitmap")?
        .bitmapImageRepByRetaggingWithColorSpace(&NSColorSpace::sRGBColorSpace())
        .context("tagging the offscreen HUD bitmap as sRGB")?;
        // Points, so the context draws at the requested backing scale.
        rep.setSize(self.bounds().size);
        let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep)
            .context("creating the offscreen HUD context")?;
        self.displayRectIgnoringOpacity_inContext(self.bounds(), &context);
        context.flushGraphics();
        anyhow::ensure!(
            rep.bytesPerRow() == row as isize && rep.bitmapFormat() == NSBitmapFormat::empty(),
            "the offscreen HUD bitmap changed its pixel layout"
        );
        let data = rep.bitmapData();
        anyhow::ensure!(!data.is_null(), "the offscreen HUD bitmap has no pixels");
        // SAFETY: the single-plane bitmap holds `height` rows of `row` bytes and
        // stays alive (and unchanged) while this slice is read.
        let rgba = unsafe { std::slice::from_raw_parts(data, row * height as usize) };
        let mut bgra = Vec::with_capacity(rgba.len());
        for pixel in rgba.as_chunks::<4>().0 {
            bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        Ok(bgra)
    }
}
