//! Cantrip's pixel marks as template images AppKit draws in the ink of the menu
//! or control holding them: the C in the menu bar, whose states follow the
//! Omarchy bar mark (ADR 0029), one quiet fixed-width mark, never a count or
//! text; each mode's glyph, cell for cell as the HUD paints it; and the radio
//! marks of the menu's mode picker.

use crate::modes::{self, GLYPH_SIZE};
use block2::RcBlock;
use cantrip_engine::config::Mode;
use objc2::{rc::Retained, runtime::Bool};
use objc2_app_kit::{NSBezierPath, NSColor, NSImage, NSRectFill, NSStatusBarButton};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use std::time::Duration;

/// The favicon's eight cells on a 4×4 grid, in order around the C.
const CELLS: [(f64, f64); 8] = [
    (3.0, 0.0),
    (2.0, 0.0),
    (1.0, 0.0),
    (0.0, 1.0),
    (0.0, 2.0),
    (1.0, 3.0),
    (2.0, 3.0),
    (3.0, 3.0),
];
/// Whole-point cells: twelve points of ink, like neighbouring menu-bar symbols.
const CELL: f64 = 3.0;
/// Processing moves three lit cells around the C at this cadence.
pub(super) const CHASE_STEP: Duration = Duration::from_millis(140);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// No live engine status.
    Unknown,
    /// Idle, with nothing needing the operator.
    Rest,
    /// Recording, in the take's route color.
    Recording([u8; 3]),
    /// Processing, in the route color, three cells lit from `chase`.
    Processing([u8; 3], usize),
    /// The latest take, the microphone or the shortcut needs the operator.
    Attention([u8; 3]),
}

impl Tone {
    /// Draw this tone on the status item: dimmed menu-bar ink at rest, or the
    /// tone's color while dictation runs or needs the operator.
    pub(super) fn show(self, button: &NSStatusBarButton) {
        let (alphas, tint) = match self {
            Self::Unknown => ([0.3; CELLS.len()], None),
            Self::Rest => ([0.55; CELLS.len()], None),
            Self::Recording(color) | Self::Attention(color) => ([1.0; CELLS.len()], Some(color)),
            Self::Processing(color, chase) => (
                std::array::from_fn(|index| {
                    if (index + CELLS.len() - chase % CELLS.len()) % CELLS.len() > 2 {
                        0.35
                    } else {
                        1.0
                    }
                }),
                Some(color),
            ),
        };
        button.setImage(Some(&draw_mark(alphas)));
        let tint = tint.map(|[red, green, blue]| {
            NSColor::colorWithSRGBRed_green_blue_alpha(
                f64::from(red) / 255.0,
                f64::from(green) / 255.0,
                f64::from(blue) / 255.0,
                1.0,
            )
        });
        button.setContentTintColor(tint.as_deref());
    }
}

/// Resolution-independent cells, drawn crisp at every backing scale.
fn draw_mark(alphas: [f64; CELLS.len()]) -> Retained<NSImage> {
    let draw = RcBlock::new(move |bounds: NSRect| -> Bool {
        let cell = bounds.size.width / 4.0;
        for (&(x, y), alpha) in CELLS.iter().zip(alphas) {
            NSColor::blackColor()
                .colorWithAlphaComponent(alpha)
                .setFill();
            NSRectFill(NSRect::new(
                NSPoint::new(bounds.origin.x + x * cell, bounds.origin.y + y * cell),
                NSSize::new(cell, cell),
            ));
        }
        Bool::YES
    });
    // Flipped, so the grid's rows count down from the top like the favicon.
    let image = NSImage::imageWithSize_flipped_drawingHandler(
        NSSize::new(4.0 * CELL, 4.0 * CELL),
        true,
        &draw,
    );
    image.setTemplate(true);
    image
}

/// A mode glyph's cell, as wide as the HUD's.
const GLYPH_CELL: f64 = 2.0;
/// The radio mark's square.
const RADIO: f64 = 10.0;

/// `mode`'s glyph as a template image.
pub(crate) fn mode_glyph(mode: Mode) -> Retained<NSImage> {
    let draw = RcBlock::new(move |bounds: NSRect| -> Bool {
        let cell = bounds.size.width / GLYPH_SIZE as f64;
        NSColor::blackColor().setFill();
        for (column, row) in modes::glyph_cells(mode) {
            NSRectFill(NSRect::new(
                NSPoint::new(
                    bounds.origin.x + column as f64 * cell,
                    bounds.origin.y + row as f64 * cell,
                ),
                NSSize::new(cell, cell),
            ));
        }
        Bool::YES
    });
    let side = GLYPH_SIZE as f64 * GLYPH_CELL;
    // Flipped, so rows count down from the top as the glyph lists them.
    let image = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(side, side), true, &draw);
    image.setTemplate(true);
    image
}

/// A radio mark: a dot for the chosen item, a ring for the others.
pub(crate) fn radio(chosen: bool) -> Retained<NSImage> {
    let draw = RcBlock::new(move |bounds: NSRect| -> Bool {
        let inset = if chosen { 2.0 } else { 2.5 };
        let oval = NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(bounds.origin.x + inset, bounds.origin.y + inset),
            NSSize::new(
                bounds.size.width - 2.0 * inset,
                bounds.size.height - 2.0 * inset,
            ),
        ));
        NSColor::blackColor().set();
        if chosen {
            oval.fill();
        } else {
            oval.setLineWidth(1.0);
            oval.stroke();
        }
        Bool::YES
    });
    let image =
        NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(RADIO, RADIO), false, &draw);
    image.setTemplate(true);
    image
}
