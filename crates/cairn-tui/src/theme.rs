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
    /// Hidden (dot-prefixed) directory entries — a dimmed variant of [`dir`](Self::dir) so a hidden
    /// folder still reads as a folder but visually recedes.
    pub hidden_dir: Color,
    /// Regular file entries.
    pub file: Color,
    /// Hidden (dot-prefixed) file entries — a dimmed variant of [`file`](Self::file).
    pub hidden_file: Color,
    /// Archive files (`.zip`/`.tar`/`.tar.gz`/…), by extension.
    pub archive: Color,
    /// Executable files (any Unix execute bit set).
    pub executable: Color,
    /// Symbolic links.
    pub symlink: Color,
    /// Live streams (e.g. container/pod logs).
    pub stream: Color,
    /// Special nodes (sockets, devices, fifos).
    pub special: Color,
    /// Error text.
    pub error: Color,
    /// The status bar (also the dim secondary text used for the pane permission/date columns).
    pub status: Color,
    /// Accent for a pane header on a remote backend (SSH/S3/…), so it stands out from a local pane.
    pub remote: Color,
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
    /// The built-in dark preset. A modern truecolor palette (Tokyo-Night family) tuned so entry
    /// *type* reads at a glance: blue folders, amber archives, green executables, cyan links; hidden
    /// entries are the dimmed variant of their type.
    pub const DARK: Theme = Theme {
        focused_border: Color::Rgb(0x7d, 0xcf, 0xff),
        unfocused_border: Color::Rgb(0x54, 0x5c, 0x7e),
        dir: Color::Rgb(0x7a, 0xa2, 0xf7),
        hidden_dir: Color::Rgb(0x4c, 0x5a, 0x8c),
        file: Color::Rgb(0xc0, 0xca, 0xf5),
        hidden_file: Color::Rgb(0x6b, 0x73, 0x94),
        archive: Color::Rgb(0xe0, 0xaf, 0x68),
        executable: Color::Rgb(0x9e, 0xce, 0x6a),
        symlink: Color::Rgb(0x7d, 0xcf, 0xff),
        stream: Color::Rgb(0xbb, 0x9a, 0xf7),
        special: Color::Rgb(0xf7, 0x76, 0x8e),
        error: Color::Rgb(0xf7, 0x76, 0x8e),
        status: Color::Rgb(0x56, 0x5f, 0x89),
        remote: Color::Rgb(0xe0, 0xaf, 0x68),
        selection_bg: Color::Rgb(0x33, 0x40, 0x6e),
        selection_fg: Color::Rgb(0xc0, 0xca, 0xf5),
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
                "hidden_dir" => theme.hidden_dir = color,
                "file" => theme.file = color,
                "hidden_file" => theme.hidden_file = color,
                "archive" => theme.archive = color,
                "executable" => theme.executable = color,
                "symlink" => theme.symlink = color,
                "stream" => theme.stream = color,
                "special" => theme.special = color,
                "error" => theme.error = color,
                "status" => theme.status = color,
                "remote" => theme.remote = color,
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
        assert_eq!(
            Theme::default().focused_border,
            Color::Rgb(0x7d, 0xcf, 0xff)
        );
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
                ("remote", "green"),
                ("archive", "#e0af68"),
                ("hidden_dir", "blue"),
                ("bogus_role", "red"),
                ("error", "notacolor"),
            ],
        );
        assert_eq!(theme.focused_border, Color::Magenta);
        assert_eq!(theme.dir, Color::Rgb(0, 255, 0));
        assert_eq!(theme.remote, Color::Green);
        assert_eq!(theme.archive, Color::Rgb(0xe0, 0xaf, 0x68));
        assert_eq!(theme.hidden_dir, Color::Blue);
        // The unknown role and the bad color each warn; valid roles still applied.
        assert_eq!(warnings.len(), 2);
        assert_eq!(theme.error, Theme::DARK.error); // unchanged (override was invalid)
    }
}
