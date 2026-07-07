//! The color [`Theme`] for rendering.
//!
//! Pick a built-in preset via `[ui] theme = "..."` — `dark` (default), `mc` (Midnight Commander),
//! `nord`, `gruvbox`, or `light` — then override individual roles on top via `[ui.colors]` in config
//! (e.g. `focused_border = "magenta"`). Colors parse with ratatui's `Color` `FromStr` (names or
//! `#rrggbb`); the optional `background`/`foreground` roles also accept `none` to clear a preset's
//! forced value. Unknown presets/roles and unparseable colors are skipped with a warning so a typo
//! never breaks rendering.

use ratatui::style::Color;
use std::str::FromStr;

/// The colors the renderer uses, by semantic role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// The base background painted behind everything. `None` leaves the terminal's own background
    /// (what the `dark` preset does), so a user's transparency/scheme shows through; a preset whose
    /// identity *is* its background (e.g. Midnight Commander's blue, or any light theme that would be
    /// illegible on a dark terminal) sets it `Some`.
    pub background: Option<Color>,
    /// The base foreground for text that no more-specific role covers. `None` leaves the terminal
    /// default; paired with [`background`](Self::background) so a forced background stays legible.
    pub foreground: Option<Color>,
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
        background: None,
        foreground: None,
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

    /// Classic **Midnight Commander**: a blue panel background with cyan directories, bright-white
    /// files, and a black-on-cyan selection bar. Uses the 16 **named ANSI colors** (not truecolor
    /// `Rgb`) on purpose — MC's identity *is* the ANSI palette, and named colors map to standard SGR
    /// codes every terminal understands, so this preset renders correctly even on a plain 16-color
    /// console (unlike the truecolor presets, which are best-effort there).
    pub const MC: Theme = Theme {
        background: Some(Color::Blue),
        foreground: Some(Color::White),
        focused_border: Color::White,
        unfocused_border: Color::Gray,
        dir: Color::Cyan,
        hidden_dir: Color::DarkGray,
        file: Color::White,
        hidden_file: Color::Gray,
        archive: Color::Yellow,
        executable: Color::LightGreen,
        symlink: Color::LightCyan,
        stream: Color::LightMagenta,
        special: Color::LightRed,
        error: Color::Red,
        status: Color::Gray,
        remote: Color::LightYellow,
        selection_bg: Color::Cyan,
        selection_fg: Color::Black,
    };

    /// **Nord**: the arctic, bluish palette (truecolor).
    pub const NORD: Theme = Theme {
        background: Some(Color::Rgb(0x2e, 0x34, 0x40)),
        foreground: Some(Color::Rgb(0xd8, 0xde, 0xe9)),
        focused_border: Color::Rgb(0x88, 0xc0, 0xd0),
        unfocused_border: Color::Rgb(0x4c, 0x56, 0x6a),
        dir: Color::Rgb(0x81, 0xa1, 0xc1),
        hidden_dir: Color::Rgb(0x4c, 0x56, 0x6a),
        file: Color::Rgb(0xd8, 0xde, 0xe9),
        hidden_file: Color::Rgb(0x43, 0x4c, 0x5e),
        archive: Color::Rgb(0xeb, 0xcb, 0x8b),
        executable: Color::Rgb(0xa3, 0xbe, 0x8c),
        symlink: Color::Rgb(0x8f, 0xbc, 0xbb),
        stream: Color::Rgb(0xb4, 0x8e, 0xad),
        special: Color::Rgb(0xbf, 0x61, 0x6a),
        error: Color::Rgb(0xbf, 0x61, 0x6a),
        status: Color::Rgb(0x4c, 0x56, 0x6a),
        remote: Color::Rgb(0xd0, 0x87, 0x70),
        selection_bg: Color::Rgb(0x43, 0x4c, 0x5e),
        selection_fg: Color::Rgb(0xec, 0xef, 0xf4),
    };

    /// **Gruvbox** (dark, hard contrast): warm retro tones (truecolor).
    pub const GRUVBOX: Theme = Theme {
        background: Some(Color::Rgb(0x28, 0x28, 0x28)),
        foreground: Some(Color::Rgb(0xeb, 0xdb, 0xb2)),
        focused_border: Color::Rgb(0xfa, 0xbd, 0x2f),
        // Distinct from `selection_bg` (0x504945) so the unfocused-pane cursor bar (which fills with
        // this) reads dimmer than the focused one instead of identical.
        unfocused_border: Color::Rgb(0x3c, 0x38, 0x36),
        dir: Color::Rgb(0x83, 0xa5, 0x98),
        hidden_dir: Color::Rgb(0x66, 0x5c, 0x54),
        file: Color::Rgb(0xeb, 0xdb, 0xb2),
        hidden_file: Color::Rgb(0x92, 0x83, 0x74),
        archive: Color::Rgb(0xfe, 0x80, 0x19),
        executable: Color::Rgb(0xb8, 0xbb, 0x26),
        symlink: Color::Rgb(0x8e, 0xc0, 0x7c),
        stream: Color::Rgb(0xd3, 0x86, 0x9b),
        special: Color::Rgb(0xfb, 0x49, 0x34),
        error: Color::Rgb(0xfb, 0x49, 0x34),
        status: Color::Rgb(0x92, 0x83, 0x74),
        remote: Color::Rgb(0xfa, 0xbd, 0x2f),
        selection_bg: Color::Rgb(0x50, 0x49, 0x45),
        selection_fg: Color::Rgb(0xfb, 0xf1, 0xc7),
    };

    /// **Light**: a clean light scheme. Forces both background and foreground so it stays legible even
    /// when the user's terminal itself is dark.
    pub const LIGHT: Theme = Theme {
        background: Some(Color::Rgb(0xf6, 0xf8, 0xfa)),
        foreground: Some(Color::Rgb(0x24, 0x29, 0x2e)),
        focused_border: Color::Rgb(0x03, 0x66, 0xd6),
        // A medium gray, not a near-white: this role also fills the unfocused-pane cursor bar, which
        // draws `selection_fg` (white) on top — a near-white value there would be illegible.
        unfocused_border: Color::Rgb(0x6e, 0x77, 0x81),
        dir: Color::Rgb(0x03, 0x66, 0xd6),
        hidden_dir: Color::Rgb(0x8d, 0xa3, 0xc4),
        file: Color::Rgb(0x24, 0x29, 0x2e),
        hidden_file: Color::Rgb(0x8a, 0x8f, 0x98),
        archive: Color::Rgb(0xb0, 0x80, 0x00),
        executable: Color::Rgb(0x22, 0x86, 0x3a),
        symlink: Color::Rgb(0x05, 0x69, 0x8c),
        stream: Color::Rgb(0x6f, 0x42, 0xc1),
        special: Color::Rgb(0xd7, 0x3a, 0x49),
        error: Color::Rgb(0xd7, 0x3a, 0x49),
        status: Color::Rgb(0x57, 0x60, 0x6a),
        remote: Color::Rgb(0xb0, 0x80, 0x00),
        selection_bg: Color::Rgb(0x03, 0x66, 0xd6),
        selection_fg: Color::Rgb(0xff, 0xff, 0xff),
    };

    /// Resolve a theme from a preset name (`dark`/`mc`/`nord`/`gruvbox`/`light`) plus per-role color
    /// overrides applied on top. The optional `background`/`foreground` roles additionally accept
    /// `none`/`unset`/`default` to clear a preset's forced value back to the terminal default. Returns
    /// the theme and a list of human-readable warnings for an unknown preset/role or unparseable color
    /// (all skipped, so a typo never breaks rendering).
    #[must_use]
    pub fn resolve<I, K, V>(preset: &str, overrides: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        // Select the built-in preset by name; an unrecognized one falls back to `dark` with a warning.
        let mut warnings = Vec::new();
        let mut theme = match preset {
            "dark" | "default" | "" => Self::DARK,
            "mc" => Self::MC,
            "nord" => Self::NORD,
            "gruvbox" => Self::GRUVBOX,
            "light" => Self::LIGHT,
            other => {
                warnings.push(format!("theme: unknown preset `{other}`, using `dark`"));
                Self::DARK
            }
        };
        for (role, value) in overrides {
            let (role, value) = (role.as_ref(), value.as_ref());
            // `background`/`foreground` are optional: a `none`/`unset` value clears a preset's forced
            // color back to the terminal default (e.g. keep `mc`'s colors but drop its blue panel).
            if matches!(role, "background" | "foreground") {
                let slot = if role == "background" {
                    &mut theme.background
                } else {
                    &mut theme.foreground
                };
                if matches!(value, "none" | "unset" | "default") {
                    *slot = None;
                } else if let Ok(color) = Color::from_str(value) {
                    *slot = Some(color);
                } else {
                    warnings.push(format!("theme: unparseable color `{value}` for `{role}`"));
                }
                continue;
            }
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
        // Every known preset and the empty default resolve without a warning.
        for ok in ["dark", "default", "", "mc", "nord", "gruvbox", "light"] {
            assert!(
                Theme::resolve(ok, none()).1.is_empty(),
                "`{ok}` should be a known preset"
            );
        }
    }

    #[test]
    fn each_preset_resolves_to_its_constant() {
        let none = std::iter::empty::<(&str, &str)>;
        for (name, expected) in [
            ("dark", Theme::DARK),
            ("mc", Theme::MC),
            ("nord", Theme::NORD),
            ("gruvbox", Theme::GRUVBOX),
            ("light", Theme::LIGHT),
        ] {
            assert_eq!(Theme::resolve(name, none()).0, expected, "preset `{name}`");
        }
        // Only `mc` and `light` (and the other non-dark presets) force a background; `dark` leaves it.
        assert_eq!(Theme::DARK.background, None);
        assert_eq!(Theme::MC.background, Some(Color::Blue));
    }

    #[test]
    fn every_preset_keeps_entry_types_distinguishable() {
        // The #141 entry-type coloring must survive on every preset: the type roles a listing uses to
        // tell folders/files/archives/etc. apart must be pairwise distinct within each preset.
        for (name, t) in [
            ("dark", Theme::DARK),
            ("mc", Theme::MC),
            ("nord", Theme::NORD),
            ("gruvbox", Theme::GRUVBOX),
            ("light", Theme::LIGHT),
        ] {
            let roles = [
                t.dir,
                t.hidden_dir,
                t.file,
                t.hidden_file,
                t.archive,
                t.executable,
                t.symlink,
                t.stream,
                t.special,
            ];
            let unique: std::collections::HashSet<_> = roles.iter().collect();
            assert_eq!(
                unique.len(),
                roles.len(),
                "preset `{name}` must keep entry-type colors distinct"
            );
        }
    }

    #[test]
    fn unfocused_selection_bar_stays_legible_on_every_preset() {
        // The unfocused-pane cursor bar draws `selection_fg` on a `unfocused_border` fill (see
        // `render_pane`). A preset whose `unfocused_border` is too close in luminance to
        // `selection_fg` makes the inactive-pane selection invisible (the LIGHT-preset regression:
        // white on near-white). Guard it with a WCAG-style contrast ratio for the truecolor presets;
        // the named-ANSI `mc` preset uses Black text, which contrasts on any non-black bar.
        fn rgb(c: Color) -> Option<(f64, f64, f64)> {
            if let Color::Rgb(r, g, b) = c {
                Some((f64::from(r), f64::from(g), f64::from(b)))
            } else {
                None
            }
        }
        fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
            let lin = |c: f64| {
                let c = c / 255.0;
                if c <= 0.03928 {
                    c / 12.92
                } else {
                    ((c + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
        }
        for (name, t) in [
            ("dark", Theme::DARK),
            ("nord", Theme::NORD),
            ("gruvbox", Theme::GRUVBOX),
            ("light", Theme::LIGHT),
        ] {
            let (Some(bar), Some(fg)) = (rgb(t.unfocused_border), rgb(t.selection_fg)) else {
                continue;
            };
            let (l1, l2) = (luminance(bar), luminance(fg));
            let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
            let ratio = (hi + 0.05) / (lo + 0.05);
            assert!(
                ratio >= 3.0,
                "preset `{name}`: unfocused selection bar contrast {ratio:.2} < 3.0 (illegible)"
            );
        }
    }

    #[test]
    fn background_foreground_overrides_apply_and_clear() {
        // A color value sets the optional role…
        let (t, w) = Theme::resolve("dark", [("background", "#101010"), ("foreground", "white")]);
        assert!(w.is_empty());
        assert_eq!(t.background, Some(Color::Rgb(0x10, 0x10, 0x10)));
        assert_eq!(t.foreground, Some(Color::White));
        // …and `none`/`unset` clears a preset's forced background back to the terminal default.
        let (t, w) = Theme::resolve("mc", [("background", "none")]);
        assert!(w.is_empty());
        assert_eq!(t.background, None, "mc's blue bg cleared");
        assert_eq!(t.dir, Color::Cyan, "other mc roles untouched");
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
