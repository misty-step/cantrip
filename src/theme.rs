//! Native Cantrip colors, shared by the passive HUD and deliberate actions window.
//!
//! Omarchy's current theme moved from the config directory to the state directory.
//! Read the active colors file, not a theme name or a generated application's skin.
//! Callers cache this inexpensive snapshot and refresh it off the render hot path.

use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub background: [u8; 3],
    pub surface: [u8; 3],
    pub border: [u8; 3],
    pub foreground: [u8; 3],
    pub accent: [u8; 3],
    pub attention: [u8; 3],
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: [0x13, 0x14, 0x1c],
            surface: [0x1a, 0x1b, 0x26],
            border: [0x41, 0x48, 0x68],
            foreground: [0xa9, 0xb1, 0xd6],
            accent: [0x7a, 0xa2, 0xf7],
            attention: [0xe0, 0xaf, 0x68],
        }
    }
}

/// Load the installed active theme using XDG directories; otherwise Tokyo Night.
/// No subprocess, theme mutation, or dependency on a running desktop shell.
pub fn load() -> Palette {
    let candidates = [dirs::state_dir(), dirs::config_dir()];
    candidates
        .into_iter()
        .flatten()
        .find_map(|root| load_file(&root.join("omarchy/current/theme/colors.toml")))
        .unwrap_or_default()
}

fn load_file(path: &Path) -> Option<Palette> {
    // Theme files are small. Reject an accidental large/non-file replacement.
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    parse(&fs::read_to_string(path).ok()?)
}

#[derive(Deserialize)]
struct Colors {
    background: String,
    foreground: String,
    muted: String,
    accent: String,
    yellow: String,
    dark_background: Option<String>,
}

fn parse(text: &str) -> Option<Palette> {
    let colors: Colors = toml::from_str(text).ok()?;
    let surface = hex_rgb(&colors.background)?;
    Some(Palette {
        background: colors
            .dark_background
            .as_deref()
            .and_then(hex_rgb)
            .unwrap_or(surface),
        surface,
        border: hex_rgb(&colors.muted)?,
        foreground: hex_rgb(&colors.foreground)?,
        accent: hex_rgb(&colors.accent)?,
        attention: hex_rgb(&colors.yellow)?,
    })
}

fn hex_rgb(text: &str) -> Option<[u8; 3]> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    Some([
        u8::from_str_radix(&digits[0..2], 16).ok()?,
        u8::from_str_radix(&digits[2..4], 16).ok()?,
        u8::from_str_radix(&digits[4..6], 16).ok()?,
    ])
}

/// Linear blend of two sRGB colours; `amount` 0 is `from`, 1 is `to`.
pub fn mix(from: [u8; 3], to: [u8; 3], amount: f32) -> [u8; 3] {
    let amount = amount.clamp(0.0, 1.0);
    std::array::from_fn(|channel| {
        (f32::from(from[channel]) + (f32::from(to[channel]) - f32::from(from[channel])) * amount)
            .round() as u8
    })
}

/// WCAG relative luminance, 0 (black) to 1 (white).
pub fn luminance(rgb: [u8; 3]) -> f32 {
    let linear = |channel: u8| {
        let value = f32::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// WCAG contrast ratio between two colours, 1 to 21.
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Every surface tone is a mix of two theme roles, so any installed theme,
/// dark or light, stays coherent without a second palette to maintain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tones {
    pub canvas: [u8; 3],
    pub panel: [u8; 3],
    pub raised: [u8; 3],
    pub well: [u8; 3],
    pub hairline: [u8; 3],
    pub hairline_strong: [u8; 3],
    pub text: [u8; 3],
    pub text_muted: [u8; 3],
    pub text_faint: [u8; 3],
    pub accent: [u8; 3],
    pub accent_soft: [u8; 3],
    pub accent_line: [u8; 3],
    pub on_accent: [u8; 3],
    pub attention: [u8; 3],
    pub attention_soft: [u8; 3],
    pub attention_line: [u8; 3],
    pub on_attention: [u8; 3],
}

impl Palette {
    pub fn is_light(&self) -> bool {
        luminance(self.background) > 0.5
    }

    pub fn tones(&self) -> Tones {
        let surface = self.surface;
        // Prefer the theme's own ink on filled controls; fall back to pure
        // black or white only when neither role reaches 4.5:1.
        let readable_on = |fill: [u8; 3]| {
            let best = |candidates: [[u8; 3]; 2]| {
                if contrast(fill, candidates[0]) >= contrast(fill, candidates[1]) {
                    candidates[0]
                } else {
                    candidates[1]
                }
            };
            let themed = best([self.background, self.foreground]);
            if contrast(fill, themed) >= 4.5 {
                themed
            } else {
                best([[0; 3], [255; 3]])
            }
        };
        Tones {
            canvas: self.background,
            panel: surface,
            raised: mix(surface, self.foreground, 0.05),
            well: mix(surface, self.background, 0.6),
            hairline: mix(surface, self.border, 0.35),
            hairline_strong: mix(surface, self.border, 0.6),
            text: self.foreground,
            text_muted: mix(self.foreground, surface, 0.32),
            text_faint: mix(self.foreground, surface, 0.52),
            accent: self.accent,
            accent_soft: mix(surface, self.accent, 0.2),
            accent_line: mix(surface, self.accent, 0.55),
            on_accent: readable_on(self.accent),
            attention: self.attention,
            attention_soft: mix(surface, self.attention, 0.14),
            attention_line: mix(surface, self.attention, 0.6),
            on_attention: readable_on(self.attention),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn malformed_theme_cannot_mix_light_surface_with_dark_fallback_text() {
        let theme = "background = '#ffffff'\nforeground = 'invalid'\nmuted = '#888888'\naccent = '#123456'\nyellow = '#aabbcc'";
        assert!(parse(theme).is_none());
        let valid = theme.replace("'invalid'", "'#121212'");
        let palette = parse(&valid).expect("complete theme");
        assert_eq!(palette.surface, [255; 3]);
        assert_eq!(palette.foreground, [0x12; 3]);
    }
}
