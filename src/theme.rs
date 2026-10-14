//! Colors from the active Omarchy theme, reloaded when the theme changes.

use ratatui::style::Color;
use std::{collections::HashMap, path::PathBuf};

/// Every color the interface uses. Subtle shades are blended from the theme's
/// foreground and background, so they suit dark and light themes alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub text: Color,
    pub subtext: Color,
    pub dim: Color,
    pub border: Color,
    pub track: Color,
    pub selection: Color,
    pub error: Color,
    pub on_accent: Color,
    pub background: Color,
}

type Rgb = (u8, u8, u8);

impl Default for Theme {
    /// Spotify's palette, for systems without an Omarchy theme.
    fn default() -> Self {
        Self::from_palette(&Palette {
            accent: (0x1D, 0xB9, 0x54),
            foreground: (0xFF, 0xFF, 0xFF),
            secondary: (0xB3, 0xB3, 0xB3),
            background: (0x12, 0x12, 0x12),
            selection: (0x2A, 0x2A, 0x2A),
            red: (0xF1, 0x5E, 0x6C),
        })
    }
}

struct Palette {
    accent: Rgb,
    foreground: Rgb,
    secondary: Rgb,
    background: Rgb,
    selection: Rgb,
    red: Rgb,
}

impl Theme {
    fn from_palette(palette: &Palette) -> Self {
        let blend = |amount| rgb(mix(palette.foreground, palette.background, amount));
        Self {
            accent: rgb(palette.accent),
            text: rgb(palette.foreground),
            subtext: rgb(palette.secondary),
            dim: rgb(mix(palette.secondary, palette.background, 0.5)),
            border: blend(0.2),
            track: blend(0.25),
            selection: rgb(palette.selection),
            error: rgb(palette.red),
            on_accent: rgb(palette.background),
            background: rgb(palette.background),
        }
    }

    /// Reads an Omarchy `colors.toml`; `None` if it lacks the essential colors.
    pub fn from_omarchy(text: &str) -> Option<Self> {
        let table: HashMap<String, toml::Value> = toml::from_str(text).ok()?;
        let color = |key: &str| table.get(key)?.as_str().and_then(parse_hex);
        let foreground = color("foreground")?;
        let background = color("background")?;
        Some(Self::from_palette(&Palette {
            accent: color("accent").or_else(|| color("blue"))?,
            foreground,
            secondary: color("light_foreground")
                .unwrap_or_else(|| mix(foreground, background, 0.75)),
            background,
            selection: color("selection").unwrap_or_else(|| mix(foreground, background, 0.12)),
            red: color("red").unwrap_or((0xF1, 0x5E, 0x6C)),
        }))
    }
}

/// Watches the theme file; `poll` returns a new theme once its contents change.
pub struct ThemeSource {
    path: Option<PathBuf>,
    last: Option<String>,
}

impl ThemeSource {
    pub fn new() -> Self {
        Self {
            path: omarchy_colors(),
            last: None,
        }
    }

    pub fn load(&mut self) -> Theme {
        self.poll().unwrap_or_default()
    }

    pub fn poll(&mut self) -> Option<Theme> {
        let text = std::fs::read_to_string(self.path.as_ref()?).ok()?;
        if self.last.as_deref() == Some(text.as_str()) {
            return None;
        }
        let theme = Theme::from_omarchy(&text);
        self.last = Some(text);
        theme
    }
}

fn omarchy_colors() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    [
        state.join("omarchy/current/theme/colors.toml"),
        home.join(".config/omarchy/current/theme/colors.toml"),
    ]
    .into_iter()
    .find(|path| path.exists())
}

fn parse_hex(text: &str) -> Option<Rgb> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let channel = |range| u8::from_str_radix(hex.get(range)?, 16).ok();
    Some((channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

/// `amount` of `top` over `base`.
fn mix(top: Rgb, base: Rgb, amount: f32) -> Rgb {
    let channel = |high: u8, low: u8| {
        (f32::from(high) * amount + f32::from(low) * (1.0 - amount)).round() as u8
    };
    (
        channel(top.0, base.0),
        channel(top.1, base.1),
        channel(top.2, base.2),
    )
}

fn rgb((red, green, blue): Rgb) -> Color {
    Color::Rgb(red, green, blue)
}

#[cfg(test)]
mod tests {
    use super::{Theme, mix, parse_hex};
    use ratatui::style::Color;

    #[test]
    fn omarchy_colors_map_onto_the_theme() {
        let text = "mode = \"dark\"\naccent = \"#7fbbb3\"\nselection = \"#3d484d\"\n\
                    background = \"#2d353b\"\nforeground = \"#d3c6aa\"\n\
                    light_foreground = \"#9da9a0\"\nred = \"#e67e80\"\n";
        let theme = Theme::from_omarchy(text).expect("a complete theme");
        assert_eq!(theme.accent, Color::Rgb(0x7f, 0xbb, 0xb3));
        assert_eq!(theme.selection, Color::Rgb(0x3d, 0x48, 0x4d));
        assert_eq!(theme.on_accent, Color::Rgb(0x2d, 0x35, 0x3b));
        assert!(Theme::from_omarchy("accent = \"#ffffff\"").is_none());
    }

    #[test]
    fn hex_and_mixing() {
        assert_eq!(parse_hex("#0a0B0c"), Some((10, 11, 12)));
        assert_eq!(parse_hex("0a0b0c"), None);
        assert_eq!(mix((200, 0, 100), (0, 0, 0), 0.5), (100, 0, 50));
    }
}
