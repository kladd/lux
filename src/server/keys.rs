//! The keybinding table: every recognized prefix key sequence dispatches
//! through this single table (REQ-KEY-001). The default table and prefix
//! are hardcoded; a config file may override both (REQ-CONFIG-003/005/006).

use ratatui::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers as CtMods};

use crate::server::layout::Dir;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    SplitSideBySide,
    SplitStacked,
    NewTab,
    NextTab,
    FocusNext,
    FocusDir(Dir),
    ResizeDir(Dir),
    /// Previous tab, wrapping (REQ-TAB-018).
    PrevTab,
    /// Detach the client from its session (REQ-KEY-006, real as of
    /// REQ-SESSION-012).
    Detach,
    /// Open the session switcher (REQ-SESSION-015).
    Switcher,
    /// Open the ex command line (REQ-EX-001).
    OpenEx,
    /// Enter scroll mode for the focused window's active tab
    /// (REQ-SCROLL-003).
    ScrollMode,
}

/// The config-file name for each command (REQ-CONFIG-006), or `None` for a
/// name no table command carries (REQ-CONFIG-007).
pub fn command_by_name(name: &str) -> Option<Command> {
    Some(match name {
        "split-side-by-side" => Command::SplitSideBySide,
        "split-stacked" => Command::SplitStacked,
        "new-tab" => Command::NewTab,
        "next-tab" => Command::NextTab,
        "previous-tab" => Command::PrevTab,
        "focus-next" => Command::FocusNext,
        "detach" => Command::Detach,
        "session-switcher" => Command::Switcher,
        "open-ex" => Command::OpenEx,
        "scroll-mode" => Command::ScrollMode,
        "focus-left" => Command::FocusDir(Dir::Left),
        "focus-down" => Command::FocusDir(Dir::Down),
        "focus-up" => Command::FocusDir(Dir::Up),
        "focus-right" => Command::FocusDir(Dir::Right),
        "resize-left" => Command::ResizeDir(Dir::Left),
        "resize-down" => Command::ResizeDir(Dir::Down),
        "resize-up" => Command::ResizeDir(Dir::Up),
        "resize-right" => Command::ResizeDir(Dir::Right),
        _ => return None,
    })
}

/// One key sequence following the prefix: a key code plus whether Ctrl is
/// held. This is the identity REQ-CONFIG-008 deduplicates on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyMatch {
    pub code: CtKeyCode,
    pub ctrl: bool,
}

impl KeyMatch {
    pub fn from_event(key: KeyEvent) -> Self {
        let ctrl = key.modifiers.contains(CtMods::CONTROL);
        // Legacy encodings send Ctrl-h as Ctrl-Backspace.
        let code = match key.code {
            CtKeyCode::Backspace if ctrl => CtKeyCode::Char('h'),
            code => code,
        };
        Self { code, ctrl }
    }
}

/// REQ-WINDOW-003: Ctrl-b is the default prefix key.
pub const DEFAULT_PREFIX: KeyMatch = KeyMatch { code: CtKeyCode::Char('b'), ctrl: true };

/// The active prefix key and keybinding table dispatch goes through
/// (REQ-KEY-001): the hardcoded defaults, or defaults with config
/// overrides applied.
pub struct KeyTable {
    pub prefix: KeyMatch,
    pub bindings: Vec<(KeyMatch, Command)>,
}

impl Default for KeyTable {
    /// REQ-KEY-003 (and REQ-CONFIG-003): the hardcoded defaults, covering
    /// every prefix sequence defined by REQ-WINDOW-005/006 (splits),
    /// REQ-WINDOW-016 (focus cycle), REQ-WINDOW-017 (resize),
    /// REQ-TAB-006/007/018 (tabs), REQ-KEY-004 (directional focus),
    /// REQ-KEY-006 (detach stub), REQ-SESSION-015 (session switcher), and
    /// REQ-EX-001 (ex command line).
    fn default() -> Self {
        fn plain(c: char) -> KeyMatch {
            KeyMatch { code: CtKeyCode::Char(c), ctrl: false }
        }
        fn ctrl(c: char) -> KeyMatch {
            KeyMatch { code: CtKeyCode::Char(c), ctrl: true }
        }
        Self {
            prefix: DEFAULT_PREFIX,
            bindings: vec![
                (plain('%'), Command::SplitSideBySide),
                (plain('"'), Command::SplitStacked),
                (plain('c'), Command::NewTab),
                (plain('n'), Command::NextTab),
                (plain('p'), Command::PrevTab),
                (plain('o'), Command::FocusNext),
                (plain('d'), Command::Detach),
                (plain('s'), Command::Switcher),
                (plain(':'), Command::OpenEx),
                (plain('['), Command::ScrollMode),
                (plain('h'), Command::FocusDir(Dir::Left)),
                (plain('j'), Command::FocusDir(Dir::Down)),
                (plain('k'), Command::FocusDir(Dir::Up)),
                (plain('l'), Command::FocusDir(Dir::Right)),
                (ctrl('h'), Command::ResizeDir(Dir::Left)),
                (ctrl('j'), Command::ResizeDir(Dir::Down)),
                (ctrl('k'), Command::ResizeDir(Dir::Up)),
                (ctrl('l'), Command::ResizeDir(Dir::Right)),
            ],
        }
    }
}

impl KeyTable {
    /// Whether `key` is the prefix key (REQ-WINDOW-003, REQ-CONFIG-005).
    pub fn is_prefix(&self, key: KeyEvent) -> bool {
        KeyMatch::from_event(key) == self.prefix
    }

    /// Look up the command bound to the key following the prefix. `None`
    /// means the sequence is unrecognized and both keys are discarded
    /// (REQ-WINDOW-007).
    pub fn lookup(&self, key: KeyEvent) -> Option<Command> {
        let m = KeyMatch::from_event(key);
        self.bindings.iter().find(|(k, _)| *k == m).map(|(_, c)| *c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: CtKeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_and_ctrl_hjkl_are_distinct() {
        let table = KeyTable::default();
        assert_eq!(
            table.lookup(key(CtKeyCode::Char('h'), KeyModifiers::NONE)),
            Some(Command::FocusDir(Dir::Left))
        );
        assert_eq!(
            table.lookup(key(CtKeyCode::Char('h'), KeyModifiers::CONTROL)),
            Some(Command::ResizeDir(Dir::Left))
        );
        assert_eq!(
            table.lookup(key(CtKeyCode::Backspace, KeyModifiers::CONTROL)),
            Some(Command::ResizeDir(Dir::Left))
        );
    }

    #[test]
    fn unrecognized_sequences_map_to_none() {
        let table = KeyTable::default();
        assert_eq!(table.lookup(key(CtKeyCode::Char('x'), KeyModifiers::NONE)), None);
        assert_eq!(table.lookup(key(CtKeyCode::Char('%'), KeyModifiers::CONTROL)), None);
        assert_eq!(table.lookup(key(CtKeyCode::Esc, KeyModifiers::NONE)), None);
    }

    #[test]
    fn detach_and_ex_are_recognized_commands() {
        let table = KeyTable::default();
        assert_eq!(
            table.lookup(key(CtKeyCode::Char('d'), KeyModifiers::NONE)),
            Some(Command::Detach)
        );
        assert_eq!(
            table.lookup(key(CtKeyCode::Char(':'), KeyModifiers::SHIFT)),
            Some(Command::OpenEx)
        );
    }

    #[test]
    fn prev_tab_and_switcher_are_bound_by_default() {
        // REQ-KEY-003 via REQ-TAB-018 and REQ-SESSION-015.
        let table = KeyTable::default();
        assert_eq!(
            table.lookup(key(CtKeyCode::Char('p'), KeyModifiers::NONE)),
            Some(Command::PrevTab)
        );
        assert_eq!(
            table.lookup(key(CtKeyCode::Char('s'), KeyModifiers::NONE)),
            Some(Command::Switcher)
        );
    }

    #[test]
    fn default_prefix_is_ctrl_b() {
        let table = KeyTable::default();
        assert!(table.is_prefix(key(CtKeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!table.is_prefix(key(CtKeyCode::Char('b'), KeyModifiers::NONE)));
    }
}
