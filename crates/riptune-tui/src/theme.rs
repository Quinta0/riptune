//! The TUI's color choices, centralized so panes, gauges, and the help
//! overlay read as a single deliberate scheme instead of independent picks.
//! Some colors are user-overridable via config.toml's `[theme]` table (see
//! `riptune_types::ThemeColors`); the rest are fixed.

use ratatui::style::Color;
use riptune_types::ThemeColors;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub border_focused: Color,
    pub border_unfocused: Color,
    pub accent: Color,
    pub now_playing: Color,
    pub progress: Color,
    pub volume: Color,
    pub text_muted: Color,
    pub error: Color,
    pub help_border: Color,
    // Keyboard-cursor selection is a background band, deliberately distinct
    // from `now_playing`'s foreground-color-plus-icon treatment -- the two
    // need to layer legibly when they land on the same row, not collide the
    // way an inverted/REVERSED highlight over colored text does.
    pub selection_bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border_focused: Color::Rgb(137, 180, 250),
            border_unfocused: Color::Rgb(88, 91, 112),
            accent: Color::Rgb(137, 180, 250),
            now_playing: Color::Rgb(166, 227, 161),
            progress: Color::Rgb(137, 180, 250),
            volume: Color::Rgb(166, 227, 161),
            text_muted: Color::Rgb(127, 132, 156),
            error: Color::Rgb(243, 139, 168),
            help_border: Color::Rgb(203, 166, 247),
            selection_bg: Color::Rgb(49, 50, 68),
        }
    }
}

impl Theme {
    /// Builds a theme from config.toml overrides, falling back to the
    /// built-in default for any color that's absent or fails to parse as a
    /// `#rrggbb` hex string.
    pub fn from_config(colors: &ThemeColors) -> Self {
        let default = Self::default();
        Self {
            accent: parse_hex(&colors.accent).unwrap_or(default.accent),
            now_playing: parse_hex(&colors.now_playing).unwrap_or(default.now_playing),
            error: parse_hex(&colors.error).unwrap_or(default.error),
            border_focused: parse_hex(&colors.border_focused).unwrap_or(default.border_focused),
            border_unfocused: parse_hex(&colors.border_unfocused).unwrap_or(default.border_unfocused),
            selection_bg: parse_hex(&colors.selection_bg).unwrap_or(default.selection_bg),
            ..default
        }
    }
}

fn parse_hex(value: &Option<String>) -> Option<Color> {
    let s = value.as_ref()?.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}
