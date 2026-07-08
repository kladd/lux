//! The keybinding table: every recognized prefix key sequence dispatches
//! through this single table. The table is hardcoded and
//! non-configurable; a config file may override only the prefix key.

use ratatui::crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyModifiers as CtMods};

use crate::server::layout::Dir;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    SplitSideBySide,
    SplitStacked,
    NewTab,
    NextTab,
    /// Terminate every non-focused window's child processes
    /// (vim's `<C-w>o` "only").
    OnlyWindow,
    FocusDir(Dir),
    ResizeDir(Dir),
    /// Previous tab, wrapping.
    PrevTab,
    /// Detach the client from its session.
    Detach,
    /// Open the session switcher.
    Switcher,
    /// Open the ex command line.
    OpenEx,
    /// Enter scroll mode for the focused window's active tab.
    ScrollMode,
    /// Make the focused window's tab at this index active.
    SelectTab(usize),
    /// Move the focused window's active tab into the spatially adjacent
    /// window in this direction.
    MoveTabDir(Dir),
    /// Reset every split in the layout tree to an even ratio.
    Rebalance,
    /// Open the rename prompt for the focused window's active tab.
    RenameTab,
    /// Terminate every tab in the focused window
    /// (tmux's `kill-pane`).
    CloseWindow,
}

impl Command {
    /// The short description the key-hint popup displays
    /// for this command. A method on `Command` itself, with an
    /// exhaustive match, so a new
    /// command can't ship without hint text and the popup can't drift
    /// from the real bindings.
    pub fn description(self) -> &'static str {
        match self {
            Command::SplitSideBySide => "split side by side",
            Command::SplitStacked => "split stacked",
            Command::NewTab => "new tab",
            Command::NextTab => "next tab",
            Command::PrevTab => "previous tab",
            Command::SelectTab(_) => "select tab by index",
            Command::OnlyWindow => "close other windows",
            Command::Detach => "detach from session",
            Command::Switcher => "session switcher",
            Command::OpenEx => "command line",
            Command::ScrollMode => "scroll mode",
            Command::FocusDir(Dir::Left) => "focus window left",
            Command::FocusDir(Dir::Down) => "focus window down",
            Command::FocusDir(Dir::Up) => "focus window up",
            Command::FocusDir(Dir::Right) => "focus window right",
            Command::ResizeDir(Dir::Left) => "resize window left",
            Command::ResizeDir(Dir::Down) => "resize window down",
            Command::ResizeDir(Dir::Up) => "resize window up",
            Command::ResizeDir(Dir::Right) => "resize window right",
            Command::MoveTabDir(Dir::Left) => "move tab left",
            Command::MoveTabDir(Dir::Down) => "move tab down",
            Command::MoveTabDir(Dir::Up) => "move tab up",
            Command::MoveTabDir(Dir::Right) => "move tab right",
            Command::Rebalance => "rebalance splits",
            Command::RenameTab => "rename tab",
            Command::CloseWindow => "close window",
        }
    }
}

/// One key sequence following the prefix: a key code plus whether Ctrl is
/// held. This is the identity table lookups match on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyMatch {
    pub code: CtKeyCode,
    pub ctrl: bool,
}

impl KeyMatch {
    pub fn from_event(key: KeyEvent) -> Self {
        Self {
            code: key.code,
            ctrl: key.modifiers.contains(CtMods::CONTROL),
        }
    }

    /// The key's display label in the hint popup, matching the config
    /// file's key-spec syntax (`x`, `C-x`).
    pub fn label(self) -> String {
        let key = match self.code {
            CtKeyCode::Char(c) => c.to_string(),
            // Unreachable today: the defaults and the config parser only
            // produce `Char` codes.
            other => format!("{other:?}"),
        };
        if self.ctrl { format!("C-{key}") } else { key }
    }
}

/// Ctrl-b is the default prefix key.
pub const DEFAULT_PREFIX: KeyMatch = KeyMatch {
    code: CtKeyCode::Char('b'),
    ctrl: true,
};

/// One entry in the keybinding tree: a key resolves to
/// either a command or a deeper node of further bindings, forming a
/// recursive trie.
#[derive(Clone, PartialEq, Debug)]
pub enum KeyTrie {
    Command(Command),
    Node(KeyTrieNode),
}

impl KeyTrie {
    /// The hint text for the key bound to this entry — the
    /// command's description, or the node's for a chord that continues.
    pub fn description(&self) -> &'static str {
        match self {
            KeyTrie::Command(command) => command.description(),
            KeyTrie::Node(node) => node.description,
        }
    }
}

/// One node of the keybinding tree: the keys recognized at one level of a
/// pending chord.
#[derive(Clone, PartialEq, Debug)]
pub struct KeyTrieNode {
    /// The hint text for the key that enters this node.
    pub description: &'static str,
    pub bindings: Vec<(KeyMatch, KeyTrie)>,
}

impl KeyTrieNode {
    /// The table's root: no key enters it, so it carries no description.
    pub fn root(bindings: Vec<(KeyMatch, KeyTrie)>) -> Self {
        Self {
            description: "",
            bindings,
        }
    }

    /// The entry bound to `key` at this node. `None` means the pending
    /// sequence dead-ends and is discarded.
    pub fn get(&self, key: KeyMatch) -> Option<&KeyTrie> {
        self.bindings
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, t)| t)
    }

    /// The key-hint popup's rows for this node, at whichever
    /// chord depth is pending, built directly from the
    /// table so they cover exactly the keys `get` recognizes. Keys sharing
    /// a description collapse into one row, in table order.
    pub fn hints(&self) -> Vec<(String, &'static str)> {
        let mut rows: Vec<(Vec<KeyMatch>, &'static str)> = Vec::new();
        for (key, trie) in &self.bindings {
            let desc = trie.description();
            match rows.iter_mut().find(|(_, d)| *d == desc) {
                Some((keys, _)) => keys.push(*key),
                None => rows.push((vec![*key], desc)),
            }
        }
        rows.into_iter()
            .map(|(keys, desc)| (key_group_label(&keys), desc))
            .collect()
    }
}

/// The active prefix key and keybinding table dispatch goes through:
/// the hardcoded defaults, or defaults with config
/// overrides applied.
pub struct KeyTable {
    pub prefix: KeyMatch,
    /// The root node: the keys recognized directly after the prefix.
    pub root: KeyTrieNode,
}

impl Default for KeyTable {
    /// The hardcoded defaults, covering every prefix sequence for
    /// splits, only-window, resize, tabs, directional focus,
    /// the detach stub, the session switcher, and the ex command line.
    fn default() -> Self {
        fn plain(c: char) -> KeyMatch {
            KeyMatch {
                code: CtKeyCode::Char(c),
                ctrl: false,
            }
        }
        fn cmd(c: char, command: Command) -> (KeyMatch, KeyTrie) {
            (plain(c), KeyTrie::Command(command))
        }
        let mut bindings = vec![
            cmd('%', Command::SplitSideBySide),
            cmd('"', Command::SplitStacked),
            cmd('c', Command::NewTab),
            cmd('n', Command::NextTab),
            cmd('p', Command::PrevTab),
            cmd('o', Command::OnlyWindow),
            cmd('d', Command::Detach),
            cmd('s', Command::Switcher),
            cmd(':', Command::OpenEx),
            cmd('[', Command::ScrollMode),
            cmd('h', Command::FocusDir(Dir::Left)),
            cmd('j', Command::FocusDir(Dir::Down)),
            cmd('k', Command::FocusDir(Dir::Up)),
            cmd('l', Command::FocusDir(Dir::Right)),
            // `=` evokes making the splits equal.
            cmd('=', Command::Rebalance),
            // tmux's rename-window key.
            cmd(',', Command::RenameTab),
            // tmux's kill-pane key.
            cmd('x', Command::CloseWindow),
            // Prefix+m enters the move-tab submap,
            // then a vim-directional key picks the destination window.
            (
                plain('m'),
                KeyTrie::Node(KeyTrieNode {
                    description: "move tab to window",
                    bindings: vec![
                        cmd('h', Command::MoveTabDir(Dir::Left)),
                        cmd('j', Command::MoveTabDir(Dir::Down)),
                        cmd('k', Command::MoveTabDir(Dir::Up)),
                        cmd('l', Command::MoveTabDir(Dir::Right)),
                    ],
                }),
            ),
            // Prefix+r enters the resize submap; the same
            // direction keys resize toward where the unshifted root keys
            // focus.
            (
                plain('r'),
                KeyTrie::Node(KeyTrieNode {
                    description: "resize window",
                    bindings: vec![
                        cmd('h', Command::ResizeDir(Dir::Left)),
                        cmd('j', Command::ResizeDir(Dir::Down)),
                        cmd('k', Command::ResizeDir(Dir::Up)),
                        cmd('l', Command::ResizeDir(Dir::Right)),
                    ],
                }),
            ),
        ];
        // Prefix+0-9 selects the tab at that index.
        for d in 0..=9 {
            let c = char::from_digit(d, 10).expect("single digit");
            bindings.push(cmd(c, Command::SelectTab(d as usize)));
        }
        Self {
            prefix: DEFAULT_PREFIX,
            root: KeyTrieNode::root(bindings),
        }
    }
}

impl KeyTable {
    /// Whether `key` is the prefix key.
    pub fn is_prefix(&self, key: KeyEvent) -> bool {
        KeyMatch::from_event(key) == self.prefix
    }

    /// The node a pending chord's accumulated keys lead to, walking from
    /// the root. `None` when the path doesn't resolve to
    /// a node.
    pub fn node_at(&self, path: &[KeyMatch]) -> Option<&KeyTrieNode> {
        let mut node = &self.root;
        for key in path {
            match node.get(*key)? {
                KeyTrie::Node(next) => node = next,
                KeyTrie::Command(_) => return None,
            }
        }
        Some(node)
    }
}

/// Label a group of keys sharing one hint row: a run of consecutive plain
/// characters (the digit row) reads as a range (`0-9`); anything else
/// joins with commas.
fn key_group_label(keys: &[KeyMatch]) -> String {
    if keys.len() > 2 {
        let chars: Option<Vec<char>> = keys
            .iter()
            .map(|k| match (k.code, k.ctrl) {
                (CtKeyCode::Char(c), false) => Some(c),
                _ => None,
            })
            .collect();
        if let Some(chars) = chars
            && chars.windows(2).all(|w| w[1] as u32 == w[0] as u32 + 1)
        {
            return format!("{}-{}", chars[0], chars[chars.len() - 1]);
        }
    }
    keys.iter()
        .map(|k| k.label())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(code: CtKeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// The command bound to `key` at the table's root; `None` for a chord
    /// node or an unbound key.
    fn lookup(table: &KeyTable, key: KeyEvent) -> Option<Command> {
        match table.root.get(KeyMatch::from_event(key)) {
            Some(KeyTrie::Command(command)) => Some(*command),
            _ => None,
        }
    }

    #[test]
    fn hjkl_focus_and_shifted_letters_are_unbound() {
        // Lowercase h focuses; resize lives behind the r submap, not
        // shifted letters.
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('h'), KeyModifiers::NONE)),
            Some(Command::FocusDir(Dir::Left))
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('H'), KeyModifiers::SHIFT)),
            None
        );
        // No Ctrl chords remain in the default table.
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('h'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Backspace, KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn unrecognized_sequences_map_to_none() {
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('q'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('%'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Esc, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn detach_and_ex_are_recognized_commands() {
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('d'), KeyModifiers::NONE)),
            Some(Command::Detach)
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char(':'), KeyModifiers::SHIFT)),
            Some(Command::OpenEx)
        );
    }

    #[test]
    fn prev_tab_and_switcher_are_bound_by_default() {
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('p'), KeyModifiers::NONE)),
            Some(Command::PrevTab)
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('s'), KeyModifiers::NONE)),
            Some(Command::Switcher)
        );
    }

    #[test]
    fn digits_select_tabs_by_index() {
        // Every digit is bound to direct tab selection.
        let table = KeyTable::default();
        for d in 0..=9u32 {
            let c = char::from_digit(d, 10).unwrap();
            assert_eq!(
                lookup(&table, key(CtKeyCode::Char(c), KeyModifiers::NONE)),
                Some(Command::SelectTab(d as usize))
            );
        }
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('3'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn rename_and_close_window_use_tmux_keys() {
        // Comma matches tmux's rename-window, x its kill-pane.
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char(','), KeyModifiers::NONE)),
            Some(Command::RenameTab)
        );
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Command::CloseWindow)
        );
    }

    #[test]
    fn rebalance_is_bound_to_equals() {
        let table = KeyTable::default();
        assert_eq!(
            lookup(&table, key(CtKeyCode::Char('='), KeyModifiers::NONE)),
            Some(Command::Rebalance)
        );
    }

    #[test]
    fn prefix_m_is_a_chord_node_of_directional_moves() {
        // M resolves to a node, not a command, and
        // its submap binds exactly the four vim directions.
        let table = KeyTable::default();
        let plain = |c| KeyMatch {
            code: CtKeyCode::Char(c),
            ctrl: false,
        };
        let Some(KeyTrie::Node(node)) = table.root.get(plain('m')) else {
            panic!("m is not a chord node");
        };
        assert_eq!(node.description, "move tab to window");
        for (c, dir) in [
            ('h', Dir::Left),
            ('j', Dir::Down),
            ('k', Dir::Up),
            ('l', Dir::Right),
        ] {
            assert_eq!(
                node.get(plain(c)),
                Some(&KeyTrie::Command(Command::MoveTabDir(dir)))
            );
        }
        // Any other key dead-ends inside the node.
        assert_eq!(node.get(plain('x')), None);
        assert_eq!(node.get(plain('m')), None);
        assert_eq!(node.bindings.len(), 4);
    }

    #[test]
    fn prefix_r_is_a_chord_node_of_directional_resizes() {
        // R resolves to the resize submap, binding exactly the four vim
        // directions.
        let table = KeyTable::default();
        let plain = |c| KeyMatch {
            code: CtKeyCode::Char(c),
            ctrl: false,
        };
        let Some(KeyTrie::Node(node)) = table.root.get(plain('r')) else {
            panic!("r is not a chord node");
        };
        assert_eq!(node.description, "resize window");
        for (c, dir) in [
            ('h', Dir::Left),
            ('j', Dir::Down),
            ('k', Dir::Up),
            ('l', Dir::Right),
        ] {
            assert_eq!(
                node.get(plain(c)),
                Some(&KeyTrie::Command(Command::ResizeDir(dir)))
            );
        }
        assert_eq!(node.get(plain('r')), None);
        assert_eq!(node.bindings.len(), 4);
    }

    #[test]
    fn node_at_walks_the_pending_path() {
        let table = KeyTable::default();
        let plain = |c| KeyMatch {
            code: CtKeyCode::Char(c),
            ctrl: false,
        };
        // The empty path is the root (a bare prefix press).
        assert_eq!(
            table.node_at(&[]).expect("root").bindings.len(),
            table.root.bindings.len()
        );
        // M leads one level deeper.
        assert!(table.node_at(&[plain('m')]).is_some());
        // A command key ends a chord; an unbound key is
        // never a node.
        assert!(table.node_at(&[plain('c')]).is_none());
        assert!(table.node_at(&[plain('x')]).is_none());
        assert!(table.node_at(&[plain('m'), plain('h')]).is_none());
    }

    #[test]
    fn chord_node_hints_list_its_own_keys() {
        // While a chord is pending the popup's rows come
        // from the pending node, not the root.
        let table = KeyTable::default();
        let m = KeyMatch {
            code: CtKeyCode::Char('m'),
            ctrl: false,
        };
        let rows = table.node_at(&[m]).expect("m node").hints();
        assert_eq!(rows.len(), 4);
        assert!(rows.contains(&("h".to_string(), "move tab left")));
        assert!(rows.contains(&("l".to_string(), "move tab right")));
        // The root's rows include the node entry itself.
        assert!(
            table
                .root
                .hints()
                .contains(&("m".to_string(), "move tab to window"))
        );
    }

    #[test]
    fn every_binding_has_a_description() {
        // The table associates hint text with every entry
        // it binds, at every depth (the exhaustive match guarantees
        // command coverage at compile time; this pins that none of it —
        // including node descriptions — is blank).
        fn check(node: &KeyTrieNode) {
            for (_, trie) in &node.bindings {
                assert!(
                    !trie.description().is_empty(),
                    "{trie:?} has no description"
                );
                if let KeyTrie::Node(node) = trie {
                    check(node);
                }
            }
        }
        check(&KeyTable::default().root);
    }

    #[test]
    fn hints_cover_every_binding_without_duplicates() {
        // Rows come straight from the table — every bound
        // key appears in exactly one row's key label.
        let table = KeyTable::default();
        let rows = table.root.hints();
        let descs: Vec<_> = rows.iter().map(|(_, d)| *d).collect();
        let mut unique = descs.clone();
        unique.dedup();
        assert_eq!(descs.len(), unique.len(), "duplicate description rows");
        for (key, trie) in &table.root.bindings {
            let row = rows
                .iter()
                .find(|(_, d)| *d == trie.description())
                .expect("every entry's description has a row");
            assert!(
                row.0.contains(&key.label()) || row.0.contains('-'),
                "{key:?} missing from its row {row:?}"
            );
        }
    }

    #[test]
    fn hint_rows_group_and_label_keys() {
        let table = KeyTable::default();
        let rows = table.root.hints();
        // The ten digit bindings share one description and collapse to a
        // range label.
        assert!(rows.contains(&("0-9".to_string(), "select tab by index")));
        assert!(rows.contains(&("%".to_string(), "split side by side")));
        // Ctrl chords label with the config file's C- syntax.
        let ctrl = KeyMatch {
            code: CtKeyCode::Char('x'),
            ctrl: true,
        };
        assert_eq!(ctrl.label(), "C-x");
        // Two keys on one description join with a comma, not a range.
        let pair = [
            KeyMatch {
                code: CtKeyCode::Char('a'),
                ctrl: false,
            },
            KeyMatch {
                code: CtKeyCode::Char('b'),
                ctrl: false,
            },
        ];
        assert_eq!(key_group_label(&pair), "a, b");
    }

    #[test]
    fn default_prefix_is_ctrl_b() {
        let table = KeyTable::default();
        assert!(table.is_prefix(key(CtKeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!table.is_prefix(key(CtKeyCode::Char('b'), KeyModifiers::NONE)));
    }
}
