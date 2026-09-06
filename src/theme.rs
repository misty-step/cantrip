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
