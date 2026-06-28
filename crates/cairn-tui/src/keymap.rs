//! The input keymap: maps terminal key events to high-level [`Action`]s.
//!
//! The default is an MC/vim-friendly scheme. Configurable presets arrive in a later milestone; the
//! mapping is centralized here so the rest of the UI deals only in [`Action`]s.

use cairn_core::Action;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Resolve a key event to an [`Action`], or `None` if it is unbound or not a press/repeat.
#[must_use]
pub fn action_for(key: KeyEvent) -> Option<Action> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }
    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(Action::Quit);
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
    fn unbound_key_is_none() {
        assert_eq!(action_for(press(KeyCode::Char('z'))), None);
    }

    #[test]
    fn release_is_ignored() {
        let mut ev = press(KeyCode::Char('q'));
        ev.kind = KeyEventKind::Release;
        assert_eq!(action_for(ev), None);
    }
}
