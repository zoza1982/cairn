//! The input keymap: maps terminal key events to high-level [`Action`]s.
//!
//! The built-in scheme is MC/vim-friendly. A [`Keymap`] layers user overrides (from
//! `config.ui.keybindings`) on top of it; the bare [`action_for`] function is the default with no
//! overrides. The mapping is centralized here so the rest of the UI deals only in [`Action`]s.

use cairn_core::{Action, TextEdit};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashMap;

/// A resolved keymap: user overrides applied on top of the built-in defaults.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    overrides: HashMap<(KeyCode, KeyModifiers), Action>,
}

impl Keymap {
    /// Build a keymap from `chord → action-name` config overrides. Returns the keymap plus a list of
    /// human-readable warnings for entries that could not be parsed (these are skipped, not fatal).
    #[must_use]
    pub fn from_overrides<K, V, I>(bindings: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut overrides = HashMap::new();
        let mut warnings = Vec::new();
        for (chord, name) in bindings {
            let (chord, name) = (chord.as_ref(), name.as_ref());
            match (parse_chord(chord), action_from_name(name)) {
                (Some((code, mods)), Some(action)) => {
                    overrides.insert(chord_key(code, mods), action);
                }
                (chord_res, action_res) => {
                    // Report each field that failed independently (a both-bad entry warns twice).
                    if chord_res.is_none() {
                        warnings.push(format!("keybinding: unknown chord `{chord}`"));
                    }
                    if action_res.is_none() {
                        warnings.push(format!("keybinding: unknown action `{name}`"));
                    }
                }
            }
        }
        (Self { overrides }, warnings)
    }

    /// Build a keymap from `bindings` (as [`from_overrides`](Self::from_overrides)) and then register
    /// user-defined shell actions: each `(index, chord)` maps its key to `Action::RunShellAction(index)`.
    /// The index must be the action's position in the validated action list the runtime also holds, so
    /// the keymap, `AppState::shell_actions`, and the runner stay aligned. Shell-action chords win over
    /// built-ins (an explicit `key` is intentional); an unparseable chord is skipped with a warning, and
    /// shadowing another binding warns but proceeds. Returns the keymap plus warnings.
    #[must_use]
    pub fn with_shell_actions<K, V, I, S, C>(bindings: I, shell_actions: S) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
        S: IntoIterator<Item = (usize, C)>,
        C: AsRef<str>,
    {
        let (mut km, mut warnings) = Self::from_overrides(bindings);
        for (index, chord) in shell_actions {
            let chord = chord.as_ref();
            let Some((code, mods)) = parse_chord(chord) else {
                warnings.push(format!("shell action: unknown chord `{chord}`"));
                continue;
            };
            let key = chord_key(code, mods);
            // Warn if the chord already does something — a prior override/shell action, or a built-in
            // default (resolved via `action_for`) — so repurposing e.g. `c` (copy) is never silent.
            let shadows_builtin = action_for(KeyEvent::new(code, mods)).is_some();
            if km.overrides.contains_key(&key) || shadows_builtin {
                warnings.push(format!(
                    "shell action: chord `{chord}` shadows another binding"
                ));
            }
            km.overrides.insert(key, Action::RunShellAction(index));
        }
        (km, warnings)
    }

    /// Resolve a key event: a user override wins, otherwise the built-in default applies.
    #[must_use]
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        // Ctrl-C is reserved — it always quits, even if an override would shadow it, so a user can
        // never accidentally remove their only way out of the app.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(Action::Quit);
        }
        if let Some(action) = self.overrides.get(&chord_key(key.code, key.modifiers)) {
            return Some(*action);
        }
        action_for(key)
    }
}

/// Canonicalize a `(key, modifiers)` pair for override lookup so the same physical key matches across
/// terminals. Modifiers are masked to Ctrl/Alt/Shift; for an alphabetic character key, case is folded
/// into the char and `Shift` is dropped — terminals variously deliver Shift+`g` as `Char('G')+Shift`,
/// `Char('G')`, or `Char('g')+Shift`, all of which must resolve to the same binding.
fn chord_key(code: KeyCode, mods: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
    match code {
        KeyCode::Char(c) if c.is_alphabetic() => {
            if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                // Terminals deliver Ctrl/Alt+letter as the lowercase code regardless of case/Shift.
                (
                    KeyCode::Char(c.to_ascii_lowercase()),
                    mods - KeyModifiers::SHIFT,
                )
            } else {
                // Otherwise fold case into the char and drop the (terminal-dependent) Shift bit.
                let upper = mods.contains(KeyModifiers::SHIFT) || c.is_uppercase();
                let c = if upper { c.to_ascii_uppercase() } else { c };
                (KeyCode::Char(c), mods - KeyModifiers::SHIFT)
            }
        }
        _ => (code, mods),
    }
}

/// Map a snake_case action name (as used in config) to an [`Action`].
#[must_use]
pub(crate) fn action_from_name(name: &str) -> Option<Action> {
    Some(match name {
        "cursor_up" => Action::CursorUp,
        "cursor_down" => Action::CursorDown,
        "cursor_top" => Action::CursorTop,
        "cursor_bottom" => Action::CursorBottom,
        "enter" => Action::Enter,
        "leave" => Action::Leave,
        "switch_pane" => Action::SwitchPane,
        "toggle_mark" => Action::ToggleMark,
        "copy" => Action::Copy,
        "move" => Action::Move,
        "delete" => Action::Delete,
        "confirm" => Action::Confirm,
        "cancel" => Action::Cancel,
        "refresh" => Action::Refresh,
        "cycle_sort" => Action::CycleSort,
        "toggle_hidden" => Action::ToggleHidden,
        "filter" => Action::Filter,
        "open_queue" => Action::OpenQueue,
        "queue_move_up" => Action::QueueMoveUp,
        "queue_move_down" => Action::QueueMoveDown,
        "toggle_pause" => Action::TogglePause,
        "make_dir" => Action::MakeDir,
        "rename" => Action::Rename,
        "open_connections" => Action::OpenConnections,
        "vault_unlock" => Action::VaultUnlock,
        "ai_propose" => Action::AiPropose,
        "approve_all" => Action::ApproveAll,
        "reject" => Action::Reject,
        "open_log_viewer" => Action::OpenLogViewer,
        "view" => Action::View,
        "edit" => Action::Edit,
        "page_up" => Action::PageUp,
        "page_down" => Action::PageDown,
        "quit" => Action::Quit,
        "new_connection" => Action::NewConnection,
        "edit_connection" => Action::EditConnection,
        "delete_connection" => Action::DeleteConnection,
        "test_connection" => Action::TestConnection,
        "pin_connection" => Action::PinConnection,
        "hide_connection" => Action::HideConnection,
        "toggle_show_hidden" => Action::ToggleShowHidden,
        "show_help" => Action::ShowHelp,
        "show_menu" => Action::ShowMenu,
        _ => return None,
    })
}

/// Parse a key-chord string like `"ctrl+a"`, `"j"`, `"f5"`, `"enter"`, `"space"` into a
/// `(KeyCode, KeyModifiers)`. Modifier tokens (`ctrl`/`alt`/`shift`) are case-insensitive; the final
/// token is the key. Returns `None` for anything it can't parse.
///
/// For alphabetic keys prefer the bare uppercase form (`"G"`) over `"shift+g"`: terminals differ on
/// how they report Shift+letter, but the keymap canonicalizes both, so either works.
#[must_use]
pub(crate) fn parse_chord(chord: &str) -> Option<(KeyCode, KeyModifiers)> {
    let chord = chord.trim();
    if chord.is_empty() {
        return None;
    }
    let mut mods = KeyModifiers::NONE;
    let mut parts: Vec<&str> = chord.split('+').collect();
    let key_tok = parts.pop()?; // the last token is the key
    for m in parts {
        match m.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let key_tok = key_tok.trim();
    let code = match key_tok.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        // `f` followed by digits is a function key (bare `"f"` falls through to the char branch).
        fk if fk.starts_with('f')
            && fk.len() > 1
            && fk[1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            match fk[1..].parse::<u8>() {
                Ok(n @ 1..=24) => KeyCode::F(n),
                _ => return None,
            }
        }
        _ => {
            let mut chars = key_tok.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None; // multi-char token that isn't a known key name
            }
            KeyCode::Char(c)
        }
    };
    Some((code, mods))
}

/// Resolve a key event to an [`Action`] using the built-in defaults (no user overrides), or `None`
/// if it is unbound or not a press/repeat.
#[must_use]
pub fn action_for(key: KeyEvent) -> Option<Action> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            // Ctrl-C always quits.
            KeyCode::Char('c') => Some(Action::Quit),
            // Ctrl-A asks the AI assistant to propose a plan.
            KeyCode::Char('a') => Some(Action::AiPropose),
            // Ctrl-O opens the connection switcher.
            KeyCode::Char('o') => Some(Action::OpenConnections),
            // Ctrl-T opens the transfer-queue view.
            KeyCode::Char('t') => Some(Action::OpenQueue),
            // Ctrl-U opens the vault-unlock overlay.
            KeyCode::Char('u') => Some(Action::VaultUnlock),
            // Ctrl-N opens the add-connection form (new connection).
            KeyCode::Char('n') => Some(Action::NewConnection),
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
        // F1 = help (opens a scrollable keybinding reference); F9 = the categorized action menu;
        // F10 = quit — all three are the classic MC function-bar conventions (RFC: function-key bar).
        KeyCode::F(1) => Some(Action::ShowHelp),
        KeyCode::F(9) => Some(Action::ShowMenu),
        KeyCode::F(10) => Some(Action::Quit),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::CursorDown),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::CursorUp),
        KeyCode::Char('g') | KeyCode::Home => Some(Action::CursorTop),
        KeyCode::Char('G') | KeyCode::End => Some(Action::CursorBottom),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Some(Action::Enter),
        KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => Some(Action::Leave),
        KeyCode::Tab => Some(Action::SwitchPane),
        KeyCode::Char(' ') | KeyCode::Insert => Some(Action::ToggleMark),
        KeyCode::F(5) | KeyCode::Char('c') => Some(Action::Copy),
        KeyCode::F(6) | KeyCode::Char('m') => Some(Action::Move),
        KeyCode::F(8) | KeyCode::Delete | KeyCode::Char('d') => Some(Action::Delete),
        KeyCode::Char('y') => Some(Action::Confirm),
        KeyCode::Char('n') | KeyCode::Esc => Some(Action::Cancel),
        // 'e' opens the edit-connection form for the selected profile in the switcher.
        // RFC-0012 P2 binds in-place file editing to F4 (`Action::Edit`) instead of 'e', so this
        // global binding is unaffected — no conflict.
        KeyCode::Char('e') => Some(Action::EditConnection),
        KeyCode::Char('r') => Some(Action::Refresh),
        // Shift-K/J move the selected pending transfer up/down in the queue view (no-op elsewhere).
        KeyCode::Char('K') => Some(Action::QueueMoveUp),
        KeyCode::Char('J') => Some(Action::QueueMoveDown),
        // 's' cycles the sort mode; '.' toggles hidden entries (ranger/vim convention).
        KeyCode::Char('s') => Some(Action::CycleSort),
        KeyCode::Char('.') => Some(Action::ToggleHidden),
        // 'p' pauses/resumes the active transfer (no-op when none is running).
        KeyCode::Char('p') => Some(Action::TogglePause),
        // F7 = make directory, F2 = rename (Total Commander / Norton convention).
        KeyCode::F(7) => Some(Action::MakeDir),
        KeyCode::F(2) => Some(Action::Rename),
        // F3 = view (MC convention): opens the read-only pager on the entry under the cursor.
        KeyCode::F(3) => Some(Action::View),
        // F4 = edit (MC convention): always opens $VISUAL/$EDITOR/vi on the entry under the
        // cursor, regardless of text/binary content (no sniff — F3's is skipped intentionally).
        KeyCode::F(4) => Some(Action::Edit),
        // '/' starts filter-as-you-type (vim/less convention).
        KeyCode::Char('/') => Some(Action::Filter),
        // Plan-overlay actions (no-ops when no overlay is open). These letters are safe because while
        // a text prompt is capturing input the event loop routes keys to [`text_edit_for`] instead of
        // resolving actions, so 'a'/'x' type into the field rather than firing here.
        KeyCode::Char('a') => Some(Action::ApproveAll),
        KeyCode::Char('x') => Some(Action::Reject),
        // 'L' opens the log viewer for the entry under the cursor.
        KeyCode::Char('L') => Some(Action::OpenLogViewer),
        // PgUp/PgDn scroll the active overlay.
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::PageDown => Some(Action::PageDown),
        // RFC-0011 P6: only meaningful inside the connection switcher (Overlay::Connections);
        // the reducer no-ops them everywhere else, matching 'e'/'d' above. Plain 't' is unbound
        // by default elsewhere; Shift-P/Shift-H/Shift-S follow the existing Shift-G/K/J precedent
        // (a direct uppercase-char match, distinct from their lowercase counterparts).
        KeyCode::Char('t') => Some(Action::TestConnection),
        KeyCode::Char('P') => Some(Action::PinConnection),
        KeyCode::Char('H') => Some(Action::HideConnection),
        KeyCode::Char('S') => Some(Action::ToggleShowHidden),
        _ => None,
    }
}

/// Map a key event to a [`TextEdit`] for a focused text prompt, or `None` to ignore it.
///
/// The event loop calls this (instead of [`Keymap::action_for`]) while a prompt is capturing input,
/// so ordinary keys edit the field. `Enter`/`Esc` submit/cancel; modified character keys (Ctrl/Alt)
/// are ignored here — `Ctrl-C` is intercepted as quit upstream.
#[must_use]
pub fn text_edit_for(key: KeyEvent) -> Option<TextEdit> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    match key.code {
        KeyCode::Enter => Some(TextEdit::Submit),
        KeyCode::Esc => Some(TextEdit::Cancel),
        KeyCode::Backspace => Some(TextEdit::Backspace),
        // Tab / Shift-Tab cycle focus between fields in the connection form. Both are delivered as
        // TextEdit so the form's text handler intercepts them without disturbing the action keymap.
        KeyCode::Tab => Some(TextEdit::NextField),
        KeyCode::BackTab => Some(TextEdit::PrevField),
        // Up / Down also navigate fields in the connection form, and are otherwise no-ops in text
        // capture mode (no action should fire while a prompt is active).
        KeyCode::Up => Some(TextEdit::PrevField),
        KeyCode::Down => Some(TextEdit::NextField),
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            Some(TextEdit::Insert(c))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(
            action_for(press(KeyCode::Char('j'))),
            Some(Action::CursorDown)
        );
        assert_eq!(action_for(press(KeyCode::Up)), Some(Action::CursorUp));
        assert_eq!(action_for(press(KeyCode::Enter)), Some(Action::Enter));
        assert_eq!(action_for(press(KeyCode::Backspace)), Some(Action::Leave));
        assert_eq!(action_for(press(KeyCode::Tab)), Some(Action::SwitchPane));
    }

    #[test]
    fn quit_keys() {
        assert_eq!(action_for(press(KeyCode::Char('q'))), Some(Action::Quit));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(action_for(ctrl_c), Some(Action::Quit));
    }

    #[test]
    fn ai_keys() {
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(action_for(ctrl_a), Some(Action::AiPropose));
        let ctrl_o = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(action_for(ctrl_o), Some(Action::OpenConnections));
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(action_for(ctrl_u), Some(Action::VaultUnlock));
        // Plain 'a'/'x' drive the plan overlay (no-ops elsewhere).
        assert_eq!(
            action_for(press(KeyCode::Char('a'))),
            Some(Action::ApproveAll)
        );
        assert_eq!(action_for(press(KeyCode::Char('x'))), Some(Action::Reject));
    }

    #[test]
    fn unbound_key_is_none() {
        assert_eq!(action_for(press(KeyCode::Char('z'))), None);
    }

    #[test]
    fn view_key() {
        assert_eq!(action_for(press(KeyCode::F(3))), Some(Action::View));
    }

    #[test]
    fn edit_key() {
        assert_eq!(action_for(press(KeyCode::F(4))), Some(Action::Edit));
    }

    #[test]
    fn connection_switcher_p6_keys() {
        // RFC-0011 P6: test/pin/hide/show-hidden. These are global default bindings resolved the
        // same way as 'e' (EditConnection) — meaningful only while Overlay::Connections is open,
        // a no-op elsewhere — so this only asserts the keymap resolves them to the right Action.
        assert_eq!(
            action_for(press(KeyCode::Char('t'))),
            Some(Action::TestConnection)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('P'))),
            Some(Action::PinConnection)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('H'))),
            Some(Action::HideConnection)
        );
        assert_eq!(
            action_for(press(KeyCode::Char('S'))),
            Some(Action::ToggleShowHidden)
        );
    }

    #[test]
    fn pause_key() {
        assert_eq!(
            action_for(press(KeyCode::Char('p'))),
            Some(Action::TogglePause)
        );
    }

    #[test]
    fn parse_chord_handles_modifiers_and_named_keys() {
        assert_eq!(
            parse_chord("ctrl+a"),
            Some((KeyCode::Char('a'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_chord("j"),
            Some((KeyCode::Char('j'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("enter"),
            Some((KeyCode::Enter, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("space"),
            Some((KeyCode::Char(' '), KeyModifiers::NONE))
        );
        assert_eq!(parse_chord("f5"), Some((KeyCode::F(5), KeyModifiers::NONE)));
        assert_eq!(parse_chord("bogus_key"), None);
        assert_eq!(parse_chord("hyper+a"), None);
    }

    #[test]
    fn override_takes_precedence_over_default() {
        // Bind 'z' (unbound by default) to quit, and remap Enter to refresh.
        let (km, warnings) = Keymap::from_overrides([("z", "quit"), ("enter", "refresh")]);
        assert!(warnings.is_empty());
        assert_eq!(km.action_for(press(KeyCode::Char('z'))), Some(Action::Quit));
        assert_eq!(km.action_for(press(KeyCode::Enter)), Some(Action::Refresh));
        // A key with no override still falls back to the default.
        assert_eq!(
            km.action_for(press(KeyCode::Char('j'))),
            Some(Action::CursorDown)
        );
    }

    #[test]
    fn shell_actions_register_by_index_and_override_builtins() {
        let no_bindings: [(&str, &str); 0] = [];
        let (km, warnings) =
            Keymap::with_shell_actions(no_bindings, [(0usize, "ctrl+h"), (1usize, "c")]);
        // 'c' is Copy by default; a shell action bound to it wins.
        assert_eq!(
            km.action_for(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            Some(Action::RunShellAction(0))
        );
        assert_eq!(
            km.action_for(press(KeyCode::Char('c'))),
            Some(Action::RunShellAction(1)),
            "shell action overrides the built-in copy binding"
        );
        // The shadow of 'c' is reported.
        assert!(warnings.iter().any(|w| w.contains("shadows")));
    }

    #[test]
    fn shell_action_with_bad_chord_is_skipped_with_warning() {
        let no_bindings: [(&str, &str); 0] = [];
        let (km, warnings) = Keymap::with_shell_actions(no_bindings, [(0usize, "bogus_key")]);
        assert!(warnings.iter().any(|w| w.contains("unknown chord")));
        // Nothing registered for index 0; defaults are intact.
        assert_eq!(
            km.action_for(press(KeyCode::Char('j'))),
            Some(Action::CursorDown)
        );
    }

    #[test]
    fn uppercase_letter_overrides_match_across_terminal_encodings() {
        // Binding "G" (or "shift+g") must fire regardless of how the terminal encodes Shift+g.
        for chord in ["G", "shift+g", "shift+G"] {
            let (km, warnings) = Keymap::from_overrides([(chord, "quit")]);
            assert!(warnings.is_empty(), "chord {chord} should parse");
            // Legacy: uppercase char carries SHIFT.
            assert_eq!(
                km.action_for(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT)),
                Some(Action::Quit),
                "legacy encoding for {chord}"
            );
            // Kitty alternate-keys: uppercase char, no SHIFT.
            assert_eq!(
                km.action_for(press(KeyCode::Char('G'))),
                Some(Action::Quit),
                "kitty-uppercase encoding for {chord}"
            );
            // Kitty base: lowercase char + SHIFT.
            assert_eq!(
                km.action_for(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::SHIFT)),
                Some(Action::Quit),
                "kitty-base encoding for {chord}"
            );
        }
    }

    #[test]
    fn ctrl_c_is_reserved_even_when_overridden() {
        let (km, _) = Keymap::from_overrides([("ctrl+c", "cursor_down")]);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(km.action_for(ctrl_c), Some(Action::Quit));
    }

    #[test]
    fn out_of_range_function_keys_are_rejected() {
        assert!(parse_chord("f0").is_none());
        assert!(parse_chord("f25").is_none());
        assert_eq!(
            parse_chord("f12"),
            Some((KeyCode::F(12), KeyModifiers::NONE))
        );
    }

    #[test]
    fn bad_overrides_are_warned_not_fatal() {
        let (km, warnings) = Keymap::from_overrides([("nope_key", "quit"), ("a", "not_an_action")]);
        assert_eq!(warnings.len(), 2);
        // The keymap still works via defaults.
        assert_eq!(km.action_for(press(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn both_bad_entry_warns_for_each_field() {
        let (_, warnings) = Keymap::from_overrides([("nope_key", "not_an_action")]);
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn ctrl_letter_overrides_are_case_insensitive() {
        // crossterm delivers Ctrl+letter as the lowercase code; "ctrl+A" must still match.
        let (km, warnings) = Keymap::from_overrides([("ctrl+A", "refresh")]);
        assert!(warnings.is_empty());
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(km.action_for(ctrl_a), Some(Action::Refresh));
    }

    #[test]
    fn parse_chord_handles_multiple_modifiers() {
        assert_eq!(
            parse_chord("ctrl+alt+a"),
            Some((
                KeyCode::Char('a'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
        );
        assert!(parse_chord("").is_none());
    }

    #[test]
    fn action_from_name_covers_every_published_name() {
        // Keep in sync with the snake_case names documented for `[ui.keybindings]`. `Action` is
        // `#[non_exhaustive]`; if a variant is added, add its name here and to `action_from_name`.
        let names = [
            "cursor_up",
            "cursor_down",
            "cursor_top",
            "cursor_bottom",
            "enter",
            "leave",
            "switch_pane",
            "toggle_mark",
            "copy",
            "move",
            "delete",
            "confirm",
            "cancel",
            "refresh",
            "cycle_sort",
            "toggle_hidden",
            "filter",
            "open_queue",
            "queue_move_up",
            "queue_move_down",
            "toggle_pause",
            "make_dir",
            "rename",
            "open_connections",
            "vault_unlock",
            "ai_propose",
            "approve_all",
            "reject",
            "open_log_viewer",
            "view",
            "edit",
            "page_up",
            "page_down",
            "quit",
            "new_connection",
            "edit_connection",
            "delete_connection",
            "test_connection",
            "pin_connection",
            "hide_connection",
            "toggle_show_hidden",
            "show_help",
            "show_menu",
        ];
        for name in names {
            assert!(
                action_from_name(name).is_some(),
                "missing mapping for {name}"
            );
        }
        assert_eq!(names.len(), 43);
    }

    #[test]
    fn function_bar_keys() {
        // F1 help / F9 menu / F10 quit — the new MC-style function-bar bindings.
        assert_eq!(action_for(press(KeyCode::F(1))), Some(Action::ShowHelp));
        assert_eq!(action_for(press(KeyCode::F(9))), Some(Action::ShowMenu));
        assert_eq!(action_for(press(KeyCode::F(10))), Some(Action::Quit));
    }

    #[test]
    fn release_is_ignored() {
        let mut ev = press(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(action_for(ev), None);
    }
}
