//! Config file loading (Phase 6): a TOML file overriding the prefix key
//! and keybinding table. No other settings, and no live reloading.
//!
//! ```toml
//! # ~/.config/lux/config.toml
//! prefix = "C-a"
//!
//! [keys]
//! split-side-by-side = "v"   # command names per keys::command_by_name
//! scroll-mode = "C-y"        # "C-" prefix means Ctrl is held
//! ```
//!
//! Key specs are a single character, optionally prefixed with `C-`.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode as CtKeyCode;

use crate::server::keys::{KeyMatch, KeyTable, KeyTrie, KeyTrieNode, command_by_name};

/// `$XDG_CONFIG_HOME/lux/config.toml`, falling back to
/// `~/.config/lux/config.toml`.
fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("lux").join("config.toml"))
}

/// Load the key table at startup. Every failure path
/// falls back to the hardcoded defaults.
pub fn load() -> KeyTable {
    let Some(path) = config_path() else {
        return KeyTable::default();
    };
    match std::fs::read_to_string(&path) {
        // No config file is not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => KeyTable::default(),
        Err(err) => {
            eprintln!("lux: {}: {err}", path.display());
            KeyTable::default()
        }
        Ok(text) => from_toml(&text, &path.display().to_string()),
    }
}

/// One root-level table entry during resolution — a command or a chord
/// node; only commands are configurable. `config_idx` is
/// the entry's position in the config file, `None` for hardcoded defaults.
struct Entry {
    key: KeyMatch,
    trie: KeyTrie,
    config_idx: Option<usize>,
}

fn from_toml(text: &str, origin: &str) -> KeyTable {
    let doc: toml::Table = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(err) => {
            // Report the parse error, run on defaults.
            eprintln!("lux: {origin}: {err}");
            return KeyTable::default();
        }
    };
    let defaults = KeyTable::default();

    let mut prefix = defaults.prefix;
    if let Some(value) = doc.get("prefix") {
        match value.as_str().and_then(parse_key_spec) {
            // The configured prefix replaces the default.
            Some(key) => prefix = key,
            None => eprintln!("lux: {origin}: invalid prefix key {value}"),
        }
    }

    let mut entries: Vec<Entry> = defaults
        .root
        .bindings
        .into_iter()
        .map(|(key, trie)| Entry { key, trie, config_idx: None })
        .collect();

    if let Some(value) = doc.get("keys") {
        let Some(keys) = value.as_table() else {
            eprintln!("lux: {origin}: `keys` must be a table");
            return KeyTable { prefix, root: KeyTrieNode::root(resolve(entries, origin)) };
        };
        // File order via toml's preserve_order, for the
        // last-entry-wins rule.
        for (idx, (name, value)) in keys.iter().enumerate() {
            // Unknown command names are reported and
            // ignored.
            let Some(command) = command_by_name(name) else {
                eprintln!("lux: {origin}: unknown command `{name}`");
                continue;
            };
            let Some(key) = value.as_str().and_then(parse_key_spec) else {
                eprintln!("lux: {origin}: invalid key {value} for `{name}`");
                continue;
            };
            // The configured sequence replaces this
            // command's hardcoded default.
            let entry = entries
                .iter_mut()
                .find(|e| e.trie == KeyTrie::Command(command))
                .expect("every named command has a default entry");
            entry.key = key;
            entry.config_idx = Some(idx);
        }
    }

    KeyTable { prefix, root: KeyTrieNode::root(resolve(entries, origin)) }
}

/// Resolve duplicate key sequences: a config entry displaces a default on
/// the same key (including a chord node's — rebinding a command onto `m`
/// displaces the whole submap), and of two config entries the last defined
/// wins, with an error.
fn resolve(entries: Vec<Entry>, origin: &str) -> Vec<(KeyMatch, KeyTrie)> {
    let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(pos) = kept.iter().position(|k| k.key == entry.key) else {
            kept.push(entry);
            continue;
        };
        let winner_is_new = match (kept[pos].config_idx, entry.config_idx) {
            (Some(old), Some(new)) => {
                eprintln!(
                    "lux: {origin}: key {:?} bound to more than one command; using the last entry",
                    entry.key
                );
                new > old
            }
            (old, new) => new.is_some() && old.is_none(),
        };
        if winner_is_new {
            kept[pos] = entry;
        }
    }
    kept.into_iter().map(|e| (e.key, e.trie)).collect()
}

/// A single character, optionally prefixed with `C-` for Ctrl.
fn parse_key_spec(spec: &str) -> Option<KeyMatch> {
    let (ctrl, rest) = match spec.strip_prefix("C-") {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    let mut chars = rest.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(KeyMatch { code: CtKeyCode::Char(c), ctrl })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::keys::Command;
    use crate::server::layout::Dir;

    fn table(text: &str) -> KeyTable {
        from_toml(text, "test")
    }

    fn find(table: &KeyTable, command: Command) -> Vec<KeyMatch> {
        table
            .root
            .bindings
            .iter()
            .filter(|(_, t)| *t == KeyTrie::Command(command))
            .map(|(k, _)| *k)
            .collect()
    }

    fn plain(c: char) -> KeyMatch {
        KeyMatch { code: CtKeyCode::Char(c), ctrl: false }
    }

    #[test]
    fn empty_config_yields_defaults() {
        let t = table("");
        let d = KeyTable::default();
        assert_eq!(t.prefix, d.prefix);
        assert_eq!(t.root, d.root);
    }

    #[test]
    fn malformed_toml_yields_defaults() {
        let t = table("prefix = [broken");
        assert_eq!(t.prefix, crate::server::keys::DEFAULT_PREFIX);
        assert_eq!(t.root, KeyTable::default().root);
    }

    #[test]
    fn configured_prefix_replaces_default() {
        let t = table("prefix = \"C-a\"");
        assert_eq!(t.prefix, KeyMatch { code: CtKeyCode::Char('a'), ctrl: true });
    }

    #[test]
    fn invalid_prefix_keeps_default() {
        assert_eq!(table("prefix = \"C-\"").prefix, crate::server::keys::DEFAULT_PREFIX);
        assert_eq!(table("prefix = \"abc\"").prefix, crate::server::keys::DEFAULT_PREFIX);
        assert_eq!(table("prefix = 5").prefix, crate::server::keys::DEFAULT_PREFIX);
    }

    #[test]
    fn configured_binding_replaces_that_commands_default() {
        let t = table("[keys]\nnew-tab = \"t\"");
        assert_eq!(find(&t, Command::NewTab), vec![plain('t')]);
    }

    #[test]
    fn unknown_command_is_ignored() {
        let t = table("[keys]\nfly-to-the-moon = \"q\"");
        assert_eq!(t.root, KeyTable::default().root);
        assert_eq!(t.lookup_char('q'), None);
    }

    #[test]
    fn config_binding_displaces_the_chord_node_on_its_key() {
        // The chord node's key follows the same displacement rule as any
        // default: rebinding a command onto `m` removes
        // the move-tab submap.
        let t = table("[keys]\nnew-tab = \"m\"");
        assert_eq!(find(&t, Command::NewTab), vec![plain('m')]);
        assert_eq!(t.root.get(plain('m')), Some(&KeyTrie::Command(Command::NewTab)));
    }

    #[test]
    fn config_binding_displaces_default_on_same_key() {
        // `n` is next-tab's default; rebinding only-window to it leaves
        // next-tab unreachable on that key and only-window off `o`.
        let t = table("[keys]\nonly-window = \"n\"");
        assert_eq!(find(&t, Command::OnlyWindow), vec![plain('n')]);
        assert_eq!(find(&t, Command::NextTab), vec![]);
    }

    #[test]
    fn duplicate_config_key_uses_last_entry() {
        // Both bound to `g`; the later entry wins.
        let t = table("[keys]\nnew-tab = \"g\"\nnext-tab = \"g\"");
        assert_eq!(find(&t, Command::NextTab), vec![plain('g')]);
        assert_eq!(find(&t, Command::NewTab), vec![]);
    }

    #[test]
    fn ctrl_specs_parse() {
        let t = table("[keys]\nscroll-mode = \"C-y\"");
        assert_eq!(
            find(&t, Command::ScrollMode),
            vec![KeyMatch { code: CtKeyCode::Char('y'), ctrl: true }]
        );
    }

    #[test]
    fn chorded_resize_commands_have_no_config_names() {
        // Chord bindings aren't configurable yet; a resize name is
        // reported as unknown and ignored, like any other typo.
        let t = table("[keys]\nresize-left = \"C-y\"");
        assert_eq!(t.root, KeyTable::default().root);
        assert!(find(&t, Command::ResizeDir(Dir::Left)).is_empty());
    }

    impl KeyTable {
        fn lookup_char(&self, c: char) -> Option<Command> {
            match self.root.get(plain(c)) {
                Some(KeyTrie::Command(command)) => Some(*command),
                _ => None,
            }
        }
    }
}
