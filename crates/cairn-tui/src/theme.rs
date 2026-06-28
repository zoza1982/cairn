//! The color [`Theme`] for rendering.
//!
//! A built-in preset (`dark`) supplies the defaults; users override individual roles via
//! `[ui.colors]` in config (e.g. `focused_border = "magenta"`). Colors parse with ratatui's
//! `Color` `FromStr` (names or `#rrggbb`); unknown roles / unparseable colors are skipped with a
//! warning so a typo never breaks rendering.

use ratatui::style::Color;
use std::str::FromStr;

/// The colors the renderer uses, by semantic role.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Border of the focused pane.
    pub focused_border: Color,
    /// Border of the unfocused pane.
    pub unfocused_border: Color,
    /// Directory entries.
    pub dir: Color,
    /// Error text.
    pub error: Color,
    /// The status bar.
    pub status: Color,
    /// Background of the selected row in the focused pane.
    pub selection_bg: Color,
    /// Foreground of the selected row in the focused pane.
    pub selection_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self::DARK
    }
}

impl Theme {
    /// The built-in dark preset (the original hard-coded palette).
    pub const DARK: Theme = Theme {
        focused_border: Color::Cyan,
        unfocused_border: Color::DarkGray,
        dir: Color::Blue,
        error: Color::Red,
        status: Color::Gray,
        selection_bg: Color::Cyan,
        selection_fg: Color::Black,
    };

    /// Resolve a theme from a preset name plus per-role color overrides. Returns the theme and a list
    /// of human-readable warnings for unknown roles or unparseable colors (those are skipped).
    #[must_use]
    pub fn resolve<I, K, V>(preset: &str, overrides: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        // Only `dark` exists today; an unrecognized preset falls back to it with a warning.
        let mut theme = Self::DARK;
        let mut warnings = Vec::new();
        if !matches!(preset, "dark" | "default" | "") {
            warnings.push(format!("theme: unknown preset `{preset}`, using `dark`"));
        }
        for (role, value) in overrides {
            let (role, value) = (role.as_ref(), value.as_ref());
            let Ok(color) = Color::from_str(value) else {
                warnings.push(format!("theme: unparseable color `{value}` for `{role}`"));
                continue;
            };
            match role {
                "focused_border" => theme.focused_border = color,
                "unfocused_border" => theme.unfocused_border = color,
                "dir" => theme.dir = color,
                "error" => theme.error = color,
                "status" => theme.status = color,
                "selection_bg" => theme.selection_bg = color,
                "selection_fg" => theme.selection_fg = color,
                other => warnings.push(format!("theme: unknown color role `{other}`")),
            }
        }
        (theme, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_dark_preset() {
        assert_eq!(Theme::default().focused_border, Color::Cyan);
    }

    #[test]
    fn unknown_preset_warns_and_falls_back() {
        let none = std::iter::empty::<(&str, &str)>;
        let (theme, warnings) = Theme::resolve("solarized", none());
        assert_eq!(theme.focused_border, Theme::DARK.focused_border);
        assert_eq!(warnings.len(), 1);
        // The known presets and the empty default do not warn.
        for ok in ["dark", "default", ""] {
            assert!(Theme::resolve(ok, none()).1.is_empty());
        }
    }

    #[test]
    fn resolve_accepts_a_string_map_directly() {
        // Mirrors `Keymap::from_overrides`: a `&BTreeMap<String,String>` works without adapting.
        let map: std::collections::BTreeMap<String, String> =
            [("dir".to_owned(), "green".to_owned())]
                .into_iter()
                .collect();
        let (theme, warnings) = Theme::resolve("dark", &map);
        assert!(warnings.is_empty());
        assert_eq!(theme.dir, Color::Green);
    }

    #[test]
    fn overrides_apply_and_bad_entries_warn() {
        let (theme, warnings) = Theme::resolve(
            "dark",
            [
                ("focused_border", "magenta"),
                ("dir", "#00ff00"),
                ("bogus_role", "red"),
                ("error", "notacolor"),
            ],
        );
        assert_eq!(theme.focused_border, Color::Magenta);
        assert_eq!(theme.dir, Color::Rgb(0, 255, 0));
        // The unknown role and the bad color each warn; valid roles still applied.
        assert_eq!(warnings.len(), 2);
        assert_eq!(theme.error, Theme::DARK.error); // unchanged (override was invalid)
    }
}
