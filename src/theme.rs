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
    /// The theme's magenta, blue, green and cyan: the hues handoff targets draw from.
    pub targets: [[u8; 3]; 4],
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
            targets: [
                [0xbb, 0x9a, 0xf7],
                [0x7a, 0xa2, 0xf7],
                [0x9e, 0xce, 0x6a],
                [0x7d, 0xcf, 0xff],
            ],
        }
    }
}

/// Hue spacing of the synthesized colors used once a theme's own target hues run out.
const RING_STEP: f32 = 15.0;
/// A theme hue closer than this to the accent, attention or an earlier target is skipped.
const THEME_TARGET_SEPARATION: f32 = 30.0;

impl Palette {
    /// The color of the handoff target at `slot` in name order. Each slot takes the
    /// theme hue farthest from the accent (the default flow), attention and every
    /// earlier slot, while one stays at least 30° clear; after that, the farthest free
    /// point of a 15° ring at the theme hues' average saturation and lightness. At
    /// least the first 24 targets never share a color.
    pub fn target(&self, slot: usize) -> [u8; 3] {
        let [saturation, lightness] = self
            .targets
            .iter()
            .map(|&color| hsl(color))
            .fold([0.0, 0.0], |sum, [_, s, l]| [sum[0] + s, sum[1] + l])
            .map(|sum| sum / self.targets.len() as f32);
        let ring = || {
            (0..(360.0 / RING_STEP) as u16)
                .map(|step| rgb(f32::from(step) * RING_STEP, saturation, lightness))
                .collect::<Vec<_>>()
        };
        let mut taken = vec![hsl(self.accent)[0], hsl(self.attention)[0]];
        let mut theme = self.targets.to_vec();
        let mut synthetic = ring();
        let mut chosen = self.targets[0];
        for _ in 0..=slot {
            let farthest = |pool: &[[u8; 3]]| {
                pool.iter()
                    .map(|&color| {
                        taken
                            .iter()
                            .map(|&other| hue_distance(hsl(color)[0], other))
                            .fold(f32::MAX, f32::min)
                    })
                    .enumerate()
                    // Ties keep the earlier candidate.
                    .max_by(|(a, left), (b, right)| left.total_cmp(right).then(b.cmp(a)))
            };
            chosen = match farthest(&theme) {
                Some((index, distance)) if distance >= THEME_TARGET_SEPARATION => {
                    theme.remove(index)
                }
                _ => {
                    if synthetic.is_empty() {
                        synthetic = ring();
                    }
                    let (index, _) = farthest(&synthetic).expect("the ring has free hues");
                    synthetic.remove(index)
                }
            };
            taken.push(hsl(chosen)[0]);
        }
        chosen
    }
}

/// `[hue°, saturation, lightness]`.
fn hsl(color: [u8; 3]) -> [f32; 3] {
    let [r, g, b] = color.map(|channel| f32::from(channel) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let lightness = (max + min) / 2.0;
    if delta == 0.0 {
        return [0.0, 0.0, lightness];
    }
    let saturation = delta / (1.0 - (2.0 * lightness - 1.0).abs());
    let sector = if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    [sector * 60.0, saturation, lightness]
}

fn rgb(hue: f32, saturation: f32, lightness: f32) -> [u8; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
    let [r, g, b] = match sector as u8 {
        0 => [chroma, second, 0.0],
        1 => [second, chroma, 0.0],
        2 => [0.0, chroma, second],
        3 => [0.0, second, chroma],
        4 => [second, 0.0, chroma],
        _ => [chroma, 0.0, second],
    };
    let offset = lightness - chroma / 2.0;
    [r, g, b].map(|channel| ((channel + offset) * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn hue_distance(a: f32, b: f32) -> f32 {
    let difference = (a - b).abs() % 360.0;
    difference.min(360.0 - difference)
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
    // Optional: a theme without them keeps its base palette; only handoff tints fall back.
    magenta: Option<String>,
    blue: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
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
        targets: chromatic_targets({
            let fallback = Palette::default().targets;
            let pick = |value: &Option<String>, index: usize| {
                value
                    .as_deref()
                    .and_then(hex_rgb)
                    .unwrap_or(fallback[index])
            };
            [
                pick(&colors.magenta, 0),
                pick(&colors.blue, 1),
                pick(&colors.green, 2),
                pick(&colors.cyan, 3),
            ]
        }),
    })
}

/// Monochrome themes (Omarchy's vantablack and white) define gray "colors", which
/// cannot tell targets apart. Keep each one's lightness, so it still suits the
/// theme's background, and give it a distinct hue at moderate saturation.
fn chromatic_targets(targets: [[u8; 3]; 4]) -> [[u8; 3]; 4] {
    const HUES: [f32; 4] = [300.0, 220.0, 120.0, 180.0];
    let saturation = targets.iter().map(|&color| hsl(color)[1]).sum::<f32>() / 4.0;
    if saturation >= 0.15 {
        return targets;
    }
    std::array::from_fn(|index| rgb(HUES[index], 0.6, hsl(targets[index])[2].clamp(0.3, 0.7)))
}

pub(crate) fn hex_rgb(text: &str) -> Option<[u8; 3]> {
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
    use super::{hsl, hue_distance, parse, Palette};

    #[test]
    fn malformed_theme_cannot_mix_light_surface_with_dark_fallback_text() {
        let theme = "background = '#ffffff'\nforeground = 'invalid'\nmuted = '#888888'\naccent = '#123456'\nyellow = '#aabbcc'";
        assert!(parse(theme).is_none());
        let valid = theme.replace("'invalid'", "'#121212'");
        let palette = parse(&valid).expect("complete theme");
        assert_eq!(palette.surface, [255; 3]);
        assert_eq!(palette.foreground, [0x12; 3]);
    }

    fn theme(accent: &str) -> Palette {
        parse(&format!(
            "background = '#15182f'\nforeground = '#f0ebff'\nmuted = '#b9b7d2'\naccent = '{accent}'\n\
             yellow = '#e5c974'\nmagenta = '#e68bd5'\nblue = '#8fb9f1'\ngreen = '#82cd9d'\ncyan = '#6bd8e6'"
        ))
        .expect("complete theme")
    }

    #[test]
    fn handoff_targets_avoid_the_default_accent_and_each_other() {
        // neon-alley-dark: the cyan accent leaves Kaylee magenta, the next target green.
        let cyan_accent = theme("#6bd8e6");
        assert_eq!(cyan_accent.target(0), [0xe6, 0x8b, 0xd5]);
        assert_eq!(cyan_accent.target(1), [0x82, 0xcd, 0x9d]);
        // A pink accent must not hand the first target its own near-identical magenta.
        let pink_accent = theme("#f591bd");
        assert_ne!(pink_accent.target(0), [0xe6, 0x8b, 0xd5]);
        // Beyond the theme's own hues (and past its cyan, which equals the neon accent),
        // every target still keeps clear of the default, attention and earlier targets.
        for palette in [cyan_accent, pink_accent] {
            let mut taken = vec![hsl(palette.accent)[0], hsl(palette.attention)[0]];
            for slot in 0..8 {
                let hue = hsl(palette.target(slot))[0];
                let nearest = taken
                    .iter()
                    .map(|&other| hue_distance(hue, other))
                    .fold(f32::MAX, f32::min);
                assert!(
                    nearest >= 15.0,
                    "slot {slot} is {nearest}° from a taken hue"
                );
                taken.push(hue);
            }
        }
    }

    #[test]
    fn monochrome_themes_still_give_targets_distinct_colors_without_touching_the_base() {
        // Omarchy vantablack: every named color is gray.
        let palette = parse(
            "background = '#0d0d0d'\nforeground = '#ffffff'\nmuted = '#8d8d8d'\naccent = '#8d8d8d'\n\
             yellow = '#cecece'\nmagenta = '#9b9b9b'\nblue = '#8d8d8d'\ngreen = '#b6b6b6'\ncyan = '#b0b0b0'",
        )
        .expect("complete theme");
        assert_eq!(palette.accent, [0x8d; 3]);
        let slots: Vec<_> = (0..6).map(|slot| palette.target(slot)).collect();
        for (index, color) in slots.iter().enumerate() {
            assert!(hsl(*color)[1] > 0.3, "slot {index} is gray: {color:?}");
            assert!(
                !slots[..index].contains(color),
                "slot {index} reused {color:?}"
            );
        }
    }
}
