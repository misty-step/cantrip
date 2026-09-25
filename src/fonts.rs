//! Bundled Geist typefaces (SIL OFL 1.1, see `assets/fonts/OFL.txt` and LICENSE).
//!
//! Rendering never depends on installed fonts: the HUD rasterizes these bytes
//! directly and every egui window installs the same families once at startup.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

pub const REGULAR: &[u8] = include_bytes!("../assets/fonts/Geist-Regular.ttf");
pub const MEDIUM: &[u8] = include_bytes!("../assets/fonts/Geist-Medium.ttf");
pub const SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.ttf");
pub const MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono-Regular.ttf");

pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// Install the families into an egui context. Call once per window; the
/// atlas is rebuilt on every call, so palette refreshes must not repeat it.
pub fn install(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    for (name, bytes) in [
        ("Geist", REGULAR),
        ("Geist Medium", MEDIUM),
        ("Geist SemiBold", SEMIBOLD),
        ("Geist Mono", MONO),
    ] {
        fonts
            .font_data
            .insert(name.to_owned(), FontData::from_static(bytes));
    }
    // egui's bundled faces stay as fallbacks for symbols Geist does not cover.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let with_fallback = |first: &str| {
        let mut family = vec![first.to_owned()];
        family.extend(fallback.iter().cloned());
        family
    };
    fonts
        .families
        .insert(FontFamily::Proportional, with_fallback("Geist"));
    fonts
        .families
        .insert(medium(), with_fallback("Geist Medium"));
    fonts
        .families
        .insert(semibold(), with_fallback("Geist SemiBold"));
    if let Some(mono) = fonts.families.get_mut(&FontFamily::Monospace) {
        mono.insert(0, "Geist Mono".to_owned());
    }
    ctx.set_fonts(fonts);
}
