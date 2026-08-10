//! Terminal color resolution: the configurable palette (16 ANSI + named
//! specials, from the `terminal.colors` KDL section) plus the computed
//! 6×6×6 cube and grayscale ramp, and the mapping from a cell's
//! `vte::ansi::Color` through runtime overrides (OSC 4/10/11) to concrete RGB.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

pub const FOREGROUND: Rgb = Rgb { r: 0xd8, g: 0xd8, b: 0xde };
pub const BACKGROUND: Rgb = Rgb { r: 0x22, g: 0x26, b: 0x2e };
const SELECTION: Rgb = Rgb { r: 0xc8, g: 0xd0, b: 0xe0 };

/// Base16 default-dark ANSI colors (alacritty's classic defaults) — muted
/// enough to sit on the DE plate.
const ANSI: [Rgb; 16] = [
    Rgb { r: 0x18, g: 0x18, b: 0x18 }, // black
    Rgb { r: 0xac, g: 0x42, b: 0x42 }, // red
    Rgb { r: 0x90, g: 0xa9, b: 0x59 }, // green
    Rgb { r: 0xf4, g: 0xbf, b: 0x75 }, // yellow
    Rgb { r: 0x6a, g: 0x9f, b: 0xb5 }, // blue
    Rgb { r: 0xaa, g: 0x75, b: 0x9f }, // magenta
    Rgb { r: 0x75, g: 0xb5, b: 0xaa }, // cyan
    Rgb { r: 0xd8, g: 0xd8, b: 0xd8 }, // white
    Rgb { r: 0x6b, g: 0x6b, b: 0x6b }, // bright black
    Rgb { r: 0xc5, g: 0x55, b: 0x55 }, // bright red
    Rgb { r: 0xaa, g: 0xc4, b: 0x74 }, // bright green
    Rgb { r: 0xfe, g: 0xca, b: 0x88 }, // bright yellow
    Rgb { r: 0x82, g: 0xb8, b: 0xc8 }, // bright blue
    Rgb { r: 0xc2, g: 0x8c, b: 0xb8 }, // bright magenta
    Rgb { r: 0x93, g: 0xd3, b: 0xc3 }, // bright cyan
    Rgb { r: 0xf8, g: 0xf8, b: 0xf8 }, // bright white
];

fn scale(rgb: Rgb, f: f32) -> Rgb {
    Rgb {
        r: (rgb.r as f32 * f) as u8,
        g: (rgb.g as f32 * f) as u8,
        b: (rgb.b as f32 * f) as u8,
    }
}

/// The user-configurable colors: the 16 ANSI slots plus the named specials
/// and the selection highlight. Loaded from the `terminal.colors` KDL
/// section; every field falls back to the compiled default.
#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            foreground: FOREGROUND,
            background: BACKGROUND,
            cursor: FOREGROUND,
            selection: SELECTION,
            ansi: ANSI,
        }
    }
}

const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright_black",
    "bright_red",
    "bright_green",
    "bright_yellow",
    "bright_blue",
    "bright_magenta",
    "bright_cyan",
    "bright_white",
];

fn config_rgb(key: &str) -> Option<Rgb> {
    let hex = cce_ui::config::get_string(&format!("/terminal/colors/{key}"))?;
    let [r, g, b, _] = cce_ui::color::parse_hex_bytes(&hex)?;
    Some(Rgb { r, g, b })
}

impl Palette {
    /// The compiled defaults with any `terminal.colors` config entries
    /// (shared config.kdl merged with the per-app override) applied on top.
    pub fn from_config() -> Self {
        let mut palette = Palette::default();
        if let Some(rgb) = config_rgb("foreground") {
            palette.foreground = rgb;
            // Cursor tracks the foreground unless set explicitly.
            palette.cursor = rgb;
        }
        if let Some(rgb) = config_rgb("background") {
            palette.background = rgb;
        }
        if let Some(rgb) = config_rgb("cursor") {
            palette.cursor = rgb;
        }
        if let Some(rgb) = config_rgb("selection") {
            palette.selection = rgb;
        }
        for (slot, name) in palette.ansi.iter_mut().zip(ANSI_NAMES) {
            if let Some(rgb) = config_rgb(name) {
                *slot = rgb;
            }
        }
        palette
    }
}

/// Default color for a palette index (0..269), matching alacritty's layout:
/// 0–15 ANSI, 16–231 the xterm 6×6×6 cube, 232–255 the grayscale ramp, then
/// the `NamedColor` specials (256 = Foreground …).
pub fn default_color(index: usize, palette: &Palette) -> Rgb {
    match index {
        0..=15 => palette.ansi[index],
        16..=231 => {
            let i = index - 16;
            let comp = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: comp(i / 36), g: comp((i / 6) % 6), b: comp(i % 6) }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        i if i == NamedColor::Foreground as usize => palette.foreground,
        i if i == NamedColor::Background as usize => palette.background,
        i if i == NamedColor::Cursor as usize => palette.cursor,
        i if i == NamedColor::BrightForeground as usize => palette.foreground,
        i if i == NamedColor::DimForeground as usize => scale(palette.foreground, 0.66),
        // DimBlack..=DimWhite
        i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
            scale(palette.ansi[i - NamedColor::DimBlack as usize], 0.66)
        }
        _ => palette.foreground,
    }
}

/// Palette index → RGB through the terminal's runtime overrides.
pub fn indexed(index: usize, overrides: &Colors, palette: &Palette) -> Rgb {
    overrides[index].unwrap_or_else(|| default_color(index, palette))
}

/// A cell color → RGB. `dim` applies the SGR 2 treatment: named colors route
/// to their Dim* palette slots, direct/indexed colors scale by 0.66.
pub fn resolve(color: Color, overrides: &Colors, dim: bool, palette: &Palette) -> Rgb {
    match color {
        Color::Spec(rgb) => {
            if dim {
                scale(rgb, 0.66)
            } else {
                rgb
            }
        }
        Color::Named(name) => {
            let name = if dim { name.to_dim() } else { name };
            indexed(name as usize, overrides, palette)
        }
        Color::Indexed(i) => {
            let rgb = indexed(i as usize, overrides, palette);
            if dim {
                scale(rgb, 0.66)
            } else {
                rgb
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_gray_ramp() {
        let p = Palette::default();
        // 16 = cube (0,0,0); 231 = cube (255,255,255); 244 = mid gray.
        assert_eq!(default_color(16, &p), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(default_color(231, &p), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(default_color(196, &p), Rgb { r: 255, g: 0, b: 0 }); // cube (5,0,0)
        assert_eq!(default_color(244, &p), Rgb { r: 128, g: 128, b: 128 });
    }

    #[test]
    fn named_specials() {
        let p = Palette::default();
        assert_eq!(default_color(NamedColor::Foreground as usize, &p), FOREGROUND);
        assert_eq!(default_color(NamedColor::Background as usize, &p), BACKGROUND);
        assert_eq!(default_color(NamedColor::DimRed as usize, &p), scale(ANSI[1], 0.66));
    }

    #[test]
    fn resolve_respects_overrides_and_palette() {
        let mut overrides = Colors::default();
        let custom = Rgb { r: 1, g: 2, b: 3 };
        overrides[NamedColor::Red as usize] = Some(custom);
        let mut p = Palette::default();
        p.ansi[2] = Rgb { r: 9, g: 9, b: 9 }; // configured green
        // OSC override wins over the configured palette; palette wins over
        // the compiled default.
        assert_eq!(resolve(Color::Named(NamedColor::Red), &overrides, false, &p), custom);
        assert_eq!(
            resolve(Color::Named(NamedColor::Green), &overrides, false, &p),
            Rgb { r: 9, g: 9, b: 9 }
        );
        assert_eq!(resolve(Color::Indexed(1), &overrides, false, &p), custom);
    }
}
