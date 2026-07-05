//! Config file loading (Phase 6): a TOML file overriding the prefix key
//! and keybinding table. No other settings, and no live reloading
//! (REQ-CONFIG-009).
//!
//! ```toml
//! # ~/.config/lux/config.toml
//! prefix = "C-a"
//!
//! [keys]
//! split-side-by-side = "v"   # command names per keys::command_by_name
//! resize-left = "C-y"        # "C-" prefix means Ctrl is held
//! ```
//!
//! Key specs are a single character, optionally prefixed with `C-`.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode as CtKeyCode;

use crate::server::keys::{Command, KeyMatch, KeyTable, command_by_name};

/// REQ-CONFIG-001: `$XDG_CONFIG_HOME/lux/config.toml`, falling back to
/// `~/.config/lux/config.toml`.
fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("lux").join("config.toml"))
}

/// Load the key table at startup (REQ-CONFIG-002). Every failure path
/// falls back to the hardcoded defaults (REQ-CONFIG-003/004).
pub fn load() -> KeyTable {
    let Some(path) = config_path() else {
        return KeyTable::default();
    };
    match std::fs::read_to_string(&path) {
        // REQ-CONFIG-003: no config file is not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => KeyTable::default(),
        Err(err) => {
            eprintln!("lux: {}: {err}", path.display());
            KeyTable::default()
        }
        Ok(text) => from_toml(&text, &path.display().to_string()),
    }
}

/// One table entry during resolution; `config_idx` is the entry's position
/// in the config file, `None` for hardcoded defaults.
struct Entry {
    key: KeyMatch,
    command: Command,
    config_idx: Option<usize>,
}

fn from_toml(text: &str, origin: &str) -> KeyTable {
    let doc: toml::Table = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(err) => {
            // REQ-CONFIG-004: report the parse error, run on defaults.
            eprintln!("lux: {origin}: {err}");
            return KeyTable::default();
        }
    };
    let defaults = KeyTable::default();

    let mut prefix = defaults.prefix;
    if let Some(value) = doc.get("prefix") {
        match value.as_str().and_then(parse_key_spec) {
            // REQ-CONFIG-005: the configured prefix replaces the default.
            Some(key) => prefix = key,
            None => eprintln!("lux: {origin}: invalid prefix key {value}"),
        }
    }

    let mut entries: Vec<Entry> = defaults
        .bindings
        .into_iter()
        .map(|(key, command)| Entry { key, command, config_idx: None })
        .collect();

    if let Some(value) = doc.get("keys") {
        let Some(keys) = value.as_table() else {
            eprintln!("lux: {origin}: `keys` must be a table");
            return KeyTable { prefix, bindings: resolve(entries, origin) };
        };
        // File order via toml's preserve_order, for REQ-CONFIG-008's
        // last-entry-wins rule.
        for (idx, (name, value)) in keys.iter().enumerate() {
            // REQ-CONFIG-007: unknown command names are reported and
            // ignored.
            let Some(command) = command_by_name(name) else {
                eprintln!("lux: {origin}: unknown command `{name}`");
                continue;
            };
            let Some(key) = value.as_str().and_then(parse_key_spec) else {
                eprintln!("lux: {origin}: invalid key {value} for `{name}`");
                continue;
            };
            // REQ-CONFIG-006: the configured sequence replaces this
            // command's hardcoded default.
            let entry = entries
                .iter_mut()
                .find(|e| e.command == command)
                .expect("every named command has a default entry");
            entry.key = key;
            entry.config_idx = Some(idx);
        }
    }

    KeyTable { prefix, bindings: resolve(entries, origin) }
}

/// Resolve duplicate key sequences: a config entry displaces a default on
/// the same key, and of two config entries the last defined wins, with an
/// error (REQ-CONFIG-008).
fn resolve(entries: Vec<Entry>, origin: &str) -> Vec<(KeyMatch, Command)> {
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
    kept.into_iter().map(|e| (e.key, e.command)).collect()
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
    use crate::server::layout::Dir;

    fn table(text: &str) -> KeyTable {
        from_toml(text, "test")
    }

    fn find(table: &KeyTable, command: Command) -> Vec<KeyMatch> {
        table
            .bindings
            .iter()
            .filter(|(_, c)| *c == command)
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
        assert_eq!(t.bindings.len(), d.bindings.len());
    }

    #[test]
    fn malformed_toml_yields_defaults() {
        let t = table("prefix = [broken");
        assert_eq!(t.prefix, crate::server::keys::DEFAULT_PREFIX);
        assert_eq!(t.bindings.len(), KeyTable::default().bindings.len());
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
        let t = table("[keys]\nfly-to-the-moon = \"m\"");
        assert_eq!(t.bindings.len(), KeyTable::default().bindings.len());
        assert_eq!(t.lookup_char('m'), None);
    }

    #[test]
    fn config_binding_displaces_default_on_same_key() {
        // `n` is next-tab's default; rebinding focus-next to it leaves
        // next-tab unreachable on that key and focus-next off `o`.
        let t = table("[keys]\nfocus-next = \"n\"");
        assert_eq!(find(&t, Command::FocusNext), vec![plain('n')]);
        assert_eq!(find(&t, Command::NextTab), vec![]);
    }

    #[test]
    fn duplicate_config_key_uses_last_entry() {
        // REQ-CONFIG-008: both bound to `g`; the later entry wins.
        let t = table("[keys]\nnew-tab = \"g\"\nnext-tab = \"g\"");
        assert_eq!(find(&t, Command::NextTab), vec![plain('g')]);
        assert_eq!(find(&t, Command::NewTab), vec![]);
    }

    #[test]
    fn ctrl_specs_parse() {
        let t = table("[keys]\nresize-left = \"C-y\"");
        assert_eq!(
            find(&t, Command::ResizeDir(Dir::Left)),
            vec![KeyMatch { code: CtKeyCode::Char('y'), ctrl: true }]
        );
    }

    impl KeyTable {
        fn lookup_char(&self, c: char) -> Option<Command> {
            let m = plain(c);
            self.bindings.iter().find(|(k, _)| *k == m).map(|(_, cmd)| *cmd)
        }
    }
}
