//! The input keymap: maps terminal key events to high-level [`Action`]s.
//!
//! The built-in scheme is MC/vim-friendly. A [`Keymap`] layers user overrides (from
//! `config.ui.keybindings`) on top of it; the bare [`action_for`] function is the default with no
//! overrides. The mapping is centralized here so the rest of the UI deals only in [`Action`]s.

use cairn_core::Action;
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
    pub fn from_overrides<'a, I>(bindings: I) -> (Self, Vec<String>)
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut overrides = HashMap::new();
        let mut warnings = Vec::new();
        for (chord, name) in bindings {
            match (parse_chord(chord), action_from_name(name)) {
                (Some(key), Some(action)) => {
                    overrides.insert(key, action);
                }
                (None, _) => warnings.push(format!("keybinding: unknown chord `{chord}`")),
                (_, None) => warnings.push(format!("keybinding: unknown action `{name}`")),
            }
        }
        (Self { overrides }, warnings)
    }

    /// Resolve a key event: a user override wins, otherwise the built-in default applies.
    #[must_use]
    pub fn action_for(&self, key: KeyEvent) -> Option<Action> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        if let Some(action) = self
            .overrides
            .get(&(key.code, relevant_mods(key.modifiers)))
        {
            return Some(*action);
        }
        action_for(key)
    }
}

/// Keep only the modifiers we bind on (Ctrl/Alt/Shift), discarding state bits that vary by terminal.
fn relevant_mods(m: KeyModifiers) -> KeyModifiers {
    m & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// Map a snake_case action name (as used in config) to an [`Action`].
#[must_use]
pub fn action_from_name(name: &str) -> Option<Action> {
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
        "ai_propose" => Action::AiPropose,
        "approve_all" => Action::ApproveAll,
        "reject" => Action::Reject,
        "quit" => Action::Quit,
        _ => return None,
    })
}

/// Parse a key-chord string like `"ctrl+a"`, `"j"`, `"f5"`, `"enter"`, `"space"` into a
/// `(KeyCode, KeyModifiers)`. Modifier tokens (`ctrl`/`alt`/`shift`) are case-insensitive; the final
/// token is the key. Returns `None` for anything it can't parse.
#[must_use]
pub fn parse_chord(chord: &str) -> Option<(KeyCode, KeyModifiers)> {
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
        fk if fk.starts_with('f') && fk[1..].parse::<u8>().is_ok() => {
            KeyCode::F(fk[1..].parse().ok()?)
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
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char('q') => Some(Action::Quit),
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
        KeyCode::Char('r') => Some(Action::Refresh),
        // Plan-overlay actions (no-ops when no overlay is open).
        // NOTE: revisit when text-input overlays land — 'a'/'x' must not fire while a text field
        // is capturing input.
        KeyCode::Char('a') => Some(Action::ApproveAll),
        KeyCode::Char('x') => Some(Action::Reject),
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
    fn bad_overrides_are_warned_not_fatal() {
        let (km, warnings) = Keymap::from_overrides([("nope_key", "quit"), ("a", "not_an_action")]);
        assert_eq!(warnings.len(), 2);
        // The keymap still works via defaults.
        assert_eq!(km.action_for(press(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn release_is_ignored() {
        let mut ev = press(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(action_for(ev), None);
    }
}
