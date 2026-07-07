//! Config file loading (Phase 6): a TOML file overriding the prefix key.
//! The keybinding table itself is hardcoded and non-configurable.
//! No other settings, and no live reloading.
//!
//! ```toml
//! # ~/.config/lux/config.toml
//! prefix = "C-a"   # "C-" prefix means Ctrl is held
//! ```
//!
//! The key spec is a single character, optionally prefixed with `C-`.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode as CtKeyCode;

use crate::server::keys::{KeyMatch, KeyTable};

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

fn from_toml(text: &str, origin: &str) -> KeyTable {
    let doc: toml::Table = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(err) => {
            // Report the parse error, run on defaults.
            eprintln!("lux: {origin}: {err}");
            return KeyTable::default();
        }
    };
    let mut table = KeyTable::default();
    if let Some(value) = doc.get("prefix") {
        match value.as_str().and_then(parse_key_spec) {
            // The configured prefix replaces the default.
            Some(key) => table.prefix = key,
            None => eprintln!("lux: {origin}: invalid prefix key {value}"),
        }
    }
    table
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

    fn table(text: &str) -> KeyTable {
        from_toml(text, "test")
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
    fn keybinding_overrides_are_not_a_setting() {
        // Per-command keybinding overrides are prohibited: a `[keys]` table changes nothing.
        let t = table("prefix = \"C-a\"\n[keys]\nnew-tab = \"t\"");
        assert_eq!(t.root, KeyTable::default().root);
        assert_eq!(t.prefix, KeyMatch { code: CtKeyCode::Char('a'), ctrl: true });
    }
}
