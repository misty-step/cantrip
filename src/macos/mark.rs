//! Cantrip's pixel C in the menu bar, as template images AppKit draws in the
//! menu bar's own ink. States follow the Omarchy bar mark (ADR 0029): one quiet
//! fixed-width mark, never a count or text.

use block2::RcBlock;
use objc2::{rc::Retained, runtime::Bool};
use objc2_app_kit::{NSColor, NSImage, NSRectFill, NSStatusBarButton};
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
/// Rest and unknown tones, then solid, then one image per chase position.
const IMAGES: usize = 3 + CELLS.len();

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
    fn image_slot(self) -> usize {
        match self {
            Self::Unknown => 0,
            Self::Rest => 1,
            Self::Recording(_) | Self::Attention(_) => 2,
            Self::Processing(_, chase) => 3 + chase % CELLS.len(),
        }
    }

    fn alphas(self) -> [f64; CELLS.len()] {
        match self {
            Self::Unknown => [0.3; CELLS.len()],
            Self::Rest => [0.55; CELLS.len()],
            Self::Recording(_) | Self::Attention(_) => [1.0; CELLS.len()],
            Self::Processing(_, chase) => std::array::from_fn(|index| {
                if (index + CELLS.len() - chase % CELLS.len()) % CELLS.len() > 2 {
                    0.35
                } else {
                    1.0
                }
            }),
        }
    }

    fn tint(self) -> Option<[u8; 3]> {
        match self {
            Self::Recording(color) | Self::Processing(color, _) | Self::Attention(color) => {
                Some(color)
            }
            Self::Unknown | Self::Rest => None,
        }
    }
}

pub(super) struct Mark {
    images: [Option<Retained<NSImage>>; IMAGES],
    shown: Option<Tone>,
}

impl Mark {
    pub(super) fn new() -> Self {
        Self {
            images: std::array::from_fn(|_| None),
            shown: None,
        }
    }

    pub(super) fn show(&mut self, button: &NSStatusBarButton, tone: Tone) {
        if self.shown == Some(tone) {
            return;
        }
        let image = self.images[tone.image_slot()].get_or_insert_with(|| draw_mark(tone.alphas()));
        button.setImage(Some(&**image));
        let tint = tone.tint().map(|[red, green, blue]| {
            NSColor::colorWithSRGBRed_green_blue_alpha(
                f64::from(red) / 255.0,
                f64::from(green) / 255.0,
                f64::from(blue) / 255.0,
                1.0,
            )
        });
        button.setContentTintColor(tint.as_deref());
        self.shown = Some(tone);
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
