//! Terminal color resolution: the default 269-entry palette (16 ANSI + 6×6×6
//! cube + grayscale ramp + named specials) and the mapping from a cell's
//! `vte::ansi::Color` through runtime overrides (OSC 4/10/11) to concrete RGB.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

pub const FOREGROUND: Rgb = Rgb { r: 0xd8, g: 0xd8, b: 0xde };
pub const BACKGROUND: Rgb = Rgb { r: 0x22, g: 0x26, b: 0x2e };

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

/// Default color for a palette index (0..269), matching alacritty's layout:
/// 0–15 ANSI, 16–231 the xterm 6×6×6 cube, 232–255 the grayscale ramp, then
/// the `NamedColor` specials (256 = Foreground …).
pub fn default_color(index: usize) -> Rgb {
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let i = index - 16;
            let comp = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: comp(i / 36), g: comp((i / 6) % 6), b: comp(i % 6) }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        i if i == NamedColor::Foreground as usize => FOREGROUND,
        i if i == NamedColor::Background as usize => BACKGROUND,
        i if i == NamedColor::Cursor as usize => FOREGROUND,
        i if i == NamedColor::BrightForeground as usize => FOREGROUND,
        i if i == NamedColor::DimForeground as usize => scale(FOREGROUND, 0.66),
        // DimBlack..=DimWhite
        i if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
            scale(ANSI[i - NamedColor::DimBlack as usize], 0.66)
        }
        _ => FOREGROUND,
    }
}

/// Palette index → RGB through the terminal's runtime overrides.
pub fn indexed(index: usize, overrides: &Colors) -> Rgb {
    overrides[index].unwrap_or_else(|| default_color(index))
}

/// A cell color → RGB. `dim` applies the SGR 2 treatment: named colors route
/// to their Dim* palette slots, direct/indexed colors scale by 0.66.
pub fn resolve(color: Color, overrides: &Colors, dim: bool) -> Rgb {
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
            indexed(name as usize, overrides)
        }
        Color::Indexed(i) => {
            let rgb = indexed(i as usize, overrides);
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
        // 16 = cube (0,0,0); 231 = cube (255,255,255); 244 = mid gray.
        assert_eq!(default_color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(default_color(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(default_color(196), Rgb { r: 255, g: 0, b: 0 }); // cube (5,0,0)
        assert_eq!(default_color(244), Rgb { r: 128, g: 128, b: 128 });
    }

    #[test]
    fn named_specials() {
        assert_eq!(default_color(NamedColor::Foreground as usize), FOREGROUND);
        assert_eq!(default_color(NamedColor::Background as usize), BACKGROUND);
        assert_eq!(
            default_color(NamedColor::DimRed as usize),
            scale(ANSI[1], 0.66)
        );
    }

    #[test]
    fn resolve_respects_overrides() {
        let mut overrides = Colors::default();
        let custom = Rgb { r: 1, g: 2, b: 3 };
        overrides[NamedColor::Red as usize] = Some(custom);
        assert_eq!(resolve(Color::Named(NamedColor::Red), &overrides, false), custom);
        assert_eq!(
            resolve(Color::Named(NamedColor::Green), &overrides, false),
            ANSI[2]
        );
        assert_eq!(resolve(Color::Indexed(1), &overrides, false), custom);
    }
}
