use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Up,
    Down,
    ToggleFilter,
    JumpAndVisit,
    Open,
    Dismiss,
    StartSearch,
    SearchInput(char),
    SearchBackspace,
    LockSearch,
    ClearSearch,
    Quit,
}

/// Pure mapper from `(KeyCode, KeyModifiers, search_active)` to `Action`.
pub fn map_key(code: KeyCode, modifiers: KeyModifiers, search_active: bool) -> Action {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);

    if search_active {
        if ctrl {
            return match code {
                KeyCode::Char('c') | KeyCode::Char('[') => Action::ClearSearch,
                _ => Action::None,
            };
        }
        return match code {
            KeyCode::Esc => Action::ClearSearch,
            KeyCode::Enter => Action::LockSearch,
            KeyCode::Backspace => Action::SearchBackspace,
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::ALT) => Action::SearchInput(c),
            _ => Action::None,
        };
    }

    if ctrl {
        return match code {
            KeyCode::Char('n') => Action::Down,
            KeyCode::Char('p') => Action::Up,
            KeyCode::Char('c') | KeyCode::Char('[') => Action::ClearSearch,
            _ => Action::None,
        };
    }

    match code {
        KeyCode::Char('j') | KeyCode::Down => Action::Down,
        KeyCode::Char('k') | KeyCode::Up => Action::Up,
        KeyCode::Char('h')
        | KeyCode::Char('l')
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Char('f') => Action::ToggleFilter,
        KeyCode::Enter => Action::JumpAndVisit,
        KeyCode::Char('o') => Action::Open,
        KeyCode::Char('d') => Action::Dismiss,
        KeyCode::Char('/') => Action::StartSearch,
        KeyCode::Esc => Action::ClearSearch,
        KeyCode::Char('q') => Action::Quit,
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normal_mode_keybindings() {
        assert_eq!(map_key(KeyCode::Char('j'), KeyModifiers::NONE, false), Action::Down);
        assert_eq!(map_key(KeyCode::Down, KeyModifiers::NONE, false), Action::Down);
        assert_eq!(
            map_key(KeyCode::Char('n'), KeyModifiers::CONTROL, false),
            Action::Down
        );

        assert_eq!(map_key(KeyCode::Char('k'), KeyModifiers::NONE, false), Action::Up);
        assert_eq!(map_key(KeyCode::Up, KeyModifiers::NONE, false), Action::Up);
        assert_eq!(
            map_key(KeyCode::Char('p'), KeyModifiers::CONTROL, false),
            Action::Up
        );

        for code in [
            KeyCode::Char('h'),
            KeyCode::Char('l'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('f'),
        ] {
            assert_eq!(map_key(code, KeyModifiers::NONE, false), Action::ToggleFilter);
        }

        assert_eq!(
            map_key(KeyCode::Enter, KeyModifiers::NONE, false),
            Action::JumpAndVisit
        );
        assert_eq!(map_key(KeyCode::Char('o'), KeyModifiers::NONE, false), Action::Open);
        assert_eq!(
            map_key(KeyCode::Char('d'), KeyModifiers::NONE, false),
            Action::Dismiss
        );
        assert_eq!(
            map_key(KeyCode::Char('/'), KeyModifiers::NONE, false),
            Action::StartSearch
        );
        assert_eq!(
            map_key(KeyCode::Esc, KeyModifiers::NONE, false),
            Action::ClearSearch
        );
        assert_eq!(
            map_key(KeyCode::Char('c'), KeyModifiers::CONTROL, false),
            Action::ClearSearch
        );
        assert_eq!(map_key(KeyCode::Char('q'), KeyModifiers::NONE, false), Action::Quit);
    }

    #[test]
    fn test_search_typing_mode_keybindings() {
        assert_eq!(
            map_key(KeyCode::Char('j'), KeyModifiers::NONE, true),
            Action::SearchInput('j')
        );
        assert_eq!(
            map_key(KeyCode::Char('q'), KeyModifiers::NONE, true),
            Action::SearchInput('q')
        );
        assert_eq!(
            map_key(KeyCode::Backspace, KeyModifiers::NONE, true),
            Action::SearchBackspace
        );
        assert_eq!(
            map_key(KeyCode::Enter, KeyModifiers::NONE, true),
            Action::LockSearch
        );
        assert_eq!(
            map_key(KeyCode::Esc, KeyModifiers::NONE, true),
            Action::ClearSearch
        );
        assert_eq!(
            map_key(KeyCode::Char('c'), KeyModifiers::CONTROL, true),
            Action::ClearSearch
        );
    }
}
