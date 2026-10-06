//! Native macOS surface for the shared HUD instrument.
//!
//! A borderless, nonactivating panel that ignores the pointer, joins every space
//! (including full-screen ones), sits at the bottom of the focused screen's usable
//! area and draws the instrument's pixels at that screen's backing scale. It never
//! becomes key, never activates Cantrip and never needs a permission.

use super::{
    acquire_instance_lock, save_screenshot, Instrument, Poller, ScreenshotState, CONTAINER_WIDTH,
    FRAME_INTERVAL, POLL_INTERVAL, PREFERENCE_INTERVAL, SURFACE_HEIGHT, SURFACE_WIDTH,
};
use crate::theme::{self, Palette};
use anyhow::{Context, Result};
use cantrip_engine::ipc::StatusSnapshot;
use objc2::{
    define_class, msg_send,
    rc::{autoreleasepool, Retained},
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapFormat,
    NSBitmapImageRep, NSColor, NSColorSpace, NSDeviceRGBColorSpace, NSEventMask, NSGraphicsContext,
    NSPanel, NSScreen, NSStatusWindowLevel, NSView, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWindowStyleMask, NSWorkspace,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext,
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};
use std::{
    cell::RefCell,
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::Arc,
    time::{Duration, Instant},
};

/// Gap above the bottom of the screen's usable area, matching the Wayland layer margin.
const BOTTOM_MARGIN: f64 = 36.0;

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
/// process can neither activate nor show a Dock icon.
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
    let placement = Placement::focused(mtm);
    let scale = placement.map_or(2, |placement| placement.scale);
    let width = surface_width(placement.map_or(f64::from(SURFACE_WIDTH), |placement| {
        placement.visible.size.width
    }));
    let container_width = CONTAINER_WIDTH.min(width as f32 - 12.0);
    let height = instrument.height(container_width);
    let presentation = instrument
        .prepare(now, 1.0, (width, height), scale, container_width, true)
        .context("composing the HUD screenshot")?;
    let (pixel_width, pixel_height) = (width * scale, height * scale);
    let view = HudView::new(
        mtm,
        NSRect::new(
            NSPoint::ZERO,
            NSSize::new(f64::from(width), f64::from(height)),
        ),
    );
    view.paint(pixel_width, pixel_height, |bytes| {
        instrument.paint(&presentation, bytes, pixel_width, pixel_height, scale)
    });
    instrument.presented(presentation);
    let bytes = view.render_offscreen(pixel_width, pixel_height)?;
    eprintln!("rendered the HUD view offscreen at {scale}x");
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

/// The live HUD on the AppKit main thread: the shared instrument fed by one
/// status reader, presented while this process holds the HUD instance lock.
pub(crate) struct LiveHud {
    instrument: Instrument,
    poller: Poller,
    panel: Panel,
    lock: Option<fs::File>,
    preferences_at: Instant,
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
            theme::load();
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
        self.instrument.palette
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

struct Panel {
    window: Retained<NSPanel>,
    view: Retained<HudView>,
    placement: Option<Placement>,
    ordered: bool,
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
        let view = HudView::new(mtm, frame);
        window.setContentView(Some(&view));
        Self {
            window,
            view,
            placement: None,
            ordered: false,
        }
    }

    fn present(&mut self, instrument: &mut Instrument, mtm: MainThreadMarker, now: Instant) {
        // Follow keyboard focus whenever the instrument appears; once shown it
        // stays put unless its screen goes away.
        if !self.ordered || self.window.screen().is_none() {
            self.placement = Placement::focused(mtm);
        }
        let Some(placement) = self.placement else {
            return;
        };
        let width = surface_width(placement.visible.size.width);
        let container_width = CONTAINER_WIDTH.min(width as f32 - 12.0);
        let height = instrument.height(container_width);
        let alpha = instrument.alpha(now);
        let Some(presentation) = instrument.prepare(
            now,
            alpha,
            (width, height),
            placement.scale,
            container_width,
            false,
        ) else {
            return;
        };
        let (pixel_width, pixel_height) = (width * placement.scale, height * placement.scale);
        self.view.paint(pixel_width, pixel_height, |bytes| {
            instrument.paint(
                &presentation,
                bytes,
                pixel_width,
                pixel_height,
                placement.scale,
            )
        });
        let frame = placement.frame(width, height);
        if self.window.frame() != frame {
            self.window.setFrame_display(frame, false);
        }
        self.view.setNeedsDisplay(true);
        if presentation.shown && !self.ordered {
            // Shown without activating Cantrip or taking keyboard focus.
            self.window.orderFrontRegardless();
            self.ordered = true;
        } else if !presentation.shown && self.ordered {
            self.window.orderOut(None);
            self.ordered = false;
        }
        instrument.presented(presentation);
    }
}

/// The instrument's latest premultiplied BGRA pixels.
struct Pixels {
    bytes: Arc<Vec<u8>>,
    width: usize,
    height: usize,
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

define_class!(
    // SAFETY: NSView has no subclassing requirements; `HudView` has no `Drop`.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CantripHudView"]
    #[ivars = RefCell<Pixels>]
    struct HudView;

    impl HudView {
        // SAFETY: the signature matches `-[NSView drawRect:]`.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl HudView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RefCell::new(Pixels {
            bytes: Arc::new(Vec::new()),
            width: 0,
            height: 0,
            // SAFETY: a framework constant, valid for the process lifetime.
            space: CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB })),
        }));
        // SAFETY: `initWithFrame:` is NSView's designated initializer.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Replace the pixels in place once Core Graphics has released the last frame.
    fn paint(&self, width: u32, height: u32, paint: impl FnOnce(&mut [u8])) {
        let mut pixels = self.ivars().borrow_mut();
        let bytes = Arc::make_mut(&mut pixels.bytes);
        bytes.resize(width as usize * height as usize * 4, 0);
        paint(bytes);
        pixels.width = width as usize;
        pixels.height = height as usize;
    }

    /// Draw the pixels one-to-one into the view's backing; transparent elsewhere.
    fn draw(&self) {
        let Some(context) = NSGraphicsContext::currentContext() else {
            return;
        };
        let context = context.CGContext();
        let bounds = self.bounds();
        CGContext::clear_rect(Some(&context), bounds);
        if let Some(image) = self.ivars().borrow().image() {
            CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
            CGContext::draw_image(Some(&context), bounds, Some(&image));
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
