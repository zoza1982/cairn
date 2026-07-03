//! Static reference content for the `F1` help overlay and the `F9` action menu.
//!
//! Both overlays are driven by plain data tables here rather than by ad hoc strings scattered
//! through `render.rs`, mirroring how [`crate::forms`] centralizes the connection-scheme reference
//! data used by both the reducer and the renderer. Keeping this content in `cairn-core` (rather
//! than `cairn-tui`) means the reducer can size/clamp scroll and cursor state
//! ([`help_line_count`], [`menu_entries`]) without duplicating the tables.

use crate::msg::Action;

/// The `F1` help overlay's content: sections in display order, each a `(keys, description)` list.
///
/// This is a static reference, not a live dump of the active `cairn_tui::Keymap` — like the bottom
/// status line's hint text, it documents the tool's default bindings and does not reflect user
/// `[ui.keybindings]` overrides configured in `cairn-tui`.
pub const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Navigation",
        &[
            ("↑/k  ↓/j", "Move the cursor"),
            ("g/Home  G/End", "Jump to top / bottom"),
            ("Enter/l/→", "Open / enter the highlighted entry"),
            ("Backspace/h/←", "Go up a directory"),
            ("Tab", "Switch the active pane"),
            ("Space/Insert", "Mark / unmark the highlighted entry"),
            ("/", "Filter the listing as you type"),
            (".", "Toggle hidden files"),
            ("s", "Cycle sort mode (name → size → modified → type)"),
            ("r", "Refresh the active pane"),
        ],
    ),
    (
        "File ops",
        &[
            ("F5/c", "Copy marked (or current) entries"),
            ("F6/m", "Move marked (or current) entries"),
            ("F8/Delete/d", "Delete marked (or current) entries"),
            ("F2", "Rename the highlighted entry"),
            ("F7", "Create a new directory"),
            ("p", "Pause / resume the active transfer(s)"),
            ("Esc", "Cancel the active transfer(s)"),
            ("Ctrl-T", "Open the transfer queue"),
        ],
    ),
    (
        "View/Edit",
        &[
            ("F3", "View the highlighted entry (read-only pager)"),
            ("F4", "Edit the highlighted entry in $VISUAL/$EDITOR/vi"),
            ("L", "Stream logs from a container/pod entry"),
        ],
    ),
    (
        "Connections",
        &[
            ("Ctrl-O", "Open the connection switcher"),
            ("Ctrl-N", "Add a new connection"),
            ("e", "Edit the highlighted saved profile (in the switcher)"),
            (
                "d",
                "Delete the highlighted saved profile (in the switcher)",
            ),
            ("t", "Test the highlighted connection's reachability"),
            ("P/H/S", "Pin / hide / show-hidden (in the switcher)"),
        ],
    ),
    (
        "Vault",
        &[("Ctrl-U", "Unlock (or first-time create) the secrets vault")],
    ),
    (
        "AI",
        &[("Ctrl-A", "Ask the AI assistant to propose a plan")],
    ),
    (
        "General",
        &[
            (
                "F1",
                "Show this help, from the main view (Esc or F1 again to close; press Esc first \
                 to leave another overlay)",
            ),
            (
                "F9",
                "Open the action menu, from the main view (press Esc first to leave another \
                 overlay)",
            ),
            (
                "F10/q/Ctrl-C",
                "Quit Cairn — works from anywhere, overlay or not",
            ),
        ],
    ),
];

/// Total display rows in [`HELP_SECTIONS`] — one row per section header plus one per entry.
/// Used by the reducer to clamp `Overlay::Help`'s scroll position without duplicating the table.
#[must_use]
pub fn help_line_count() -> usize {
    HELP_SECTIONS.iter().map(|(_, rows)| 1 + rows.len()).sum()
}

/// A single selectable row in the `F9` action menu: a label, the shortcut shown alongside it, and
/// the [`Action`] dispatched when the user selects it with `Enter`.
#[derive(Debug, Clone, Copy)]
pub struct MenuEntry {
    /// Display label (e.g. `"Copy"`).
    pub label: &'static str,
    /// The shortcut shown next to the label (e.g. `"F5"`, `"Ctrl-O"`) — also a discoverability aid.
    pub shortcut: &'static str,
    /// The action to dispatch on selection, via the same path a direct keypress would take.
    pub action: Action,
}

/// The `F9` menu's categories, in display order. Each category header is followed by its
/// [`MenuEntry`] rows; the cursor in `Overlay::Menu` indexes only the flattened entries (see
/// [`menu_entries`]) — headers are never selectable.
pub const MENU_SECTIONS: &[(&str, &[MenuEntry])] = &[
    (
        "FILE",
        &[
            MenuEntry {
                label: "Copy",
                shortcut: "F5",
                action: Action::Copy,
            },
            MenuEntry {
                label: "Move",
                shortcut: "F6",
                action: Action::Move,
            },
            MenuEntry {
                label: "Delete",
                shortcut: "F8",
                action: Action::Delete,
            },
            MenuEntry {
                label: "Rename",
                shortcut: "F2",
                action: Action::Rename,
            },
            MenuEntry {
                label: "MakeDir",
                shortcut: "F7",
                action: Action::MakeDir,
            },
        ],
    ),
    (
        "VIEW",
        &[
            MenuEntry {
                label: "View",
                shortcut: "F3",
                action: Action::View,
            },
            MenuEntry {
                label: "Edit",
                shortcut: "F4",
                action: Action::Edit,
            },
        ],
    ),
    (
        "CONNECTIONS",
        &[
            MenuEntry {
                label: "Switch",
                shortcut: "Ctrl-O",
                action: Action::OpenConnections,
            },
            MenuEntry {
                label: "New",
                shortcut: "Ctrl-N",
                action: Action::NewConnection,
            },
        ],
    ),
    (
        "VAULT",
        &[MenuEntry {
            label: "Unlock",
            shortcut: "Ctrl-U",
            action: Action::VaultUnlock,
        }],
    ),
    (
        "AI",
        &[MenuEntry {
            label: "Ask",
            shortcut: "Ctrl-A",
            action: Action::AiPropose,
        }],
    ),
    (
        "GENERAL",
        &[
            MenuEntry {
                label: "Help",
                shortcut: "F1",
                action: Action::ShowHelp,
            },
            MenuEntry {
                label: "Quit",
                shortcut: "F10",
                action: Action::Quit,
            },
        ],
    ),
];

/// Flatten [`MENU_SECTIONS`] into just its selectable entries (skipping category headers), in
/// display order — used both for cursor bounds-checking (`.count()`) and dispatch (`.nth(cursor)`).
pub fn menu_entries() -> impl Iterator<Item = &'static MenuEntry> {
    MENU_SECTIONS.iter().flat_map(|(_, entries)| entries.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_line_count_matches_the_table() {
        let expected: usize = HELP_SECTIONS
            .iter()
            .map(|(_, rows)| 1 + rows.len())
            .sum::<usize>();
        assert_eq!(help_line_count(), expected);
        assert!(help_line_count() > 0);
    }

    #[test]
    fn menu_entries_are_non_empty_and_flattened() {
        let entries: Vec<_> = menu_entries().collect();
        assert!(!entries.is_empty());
        let expected: usize = MENU_SECTIONS.iter().map(|(_, e)| e.len()).sum();
        assert_eq!(entries.len(), expected);
    }

    #[test]
    fn every_menu_entry_has_a_non_empty_label_and_shortcut() {
        for entry in menu_entries() {
            assert!(!entry.label.is_empty());
            assert!(!entry.shortcut.is_empty());
        }
    }
}
