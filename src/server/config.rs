//! Config file loading (Phase 6): a TOML file overriding the prefix key,
//! the session-restore toggle, and the desktop-notification toggle. The
//! keybinding table itself is hardcoded and non-configurable. No other
//! settings, and no live reloading.
//!
//! ```toml
//! # ~/.config/lux/config.toml
//! prefix = "C-a"    # "C-" prefix means Ctrl is held
//! restore = false   # skip restoring persisted sessions at startup
//! notify = false    # no desktop notifications for Claude Code tabs
//! ```
//!
//! The key spec is a single character, optionally prefixed with `C-`.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode as CtKeyCode;

use crate::server::keys::{KeyMatch, KeyTable};

/// The loaded settings. Every field has a default, so a missing or
/// broken config file still yields a working server.
pub struct Config {
    pub keys: KeyTable,
    /// Whether the server restores persisted session state at startup;
    /// saving is unconditional either way. Absent means restore.
    pub restore: bool,
    /// Whether the server raises desktop notifications when a Claude
    /// Code tab reaches done or blocked. Absent means notify.
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: KeyTable::default(),
            restore: true,
            notify: true,
        }
    }
}

/// `$XDG_CONFIG_HOME/lux/config.toml`, falling back to
/// `~/.config/lux/config.toml`.
fn config_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("lux").join("config.toml"))
}

/// Load the settings at startup. Every failure path
/// falls back to the hardcoded defaults.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        // No config file is not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(err) => {
            eprintln!("lux: {}: {err}", path.display());
            Config::default()
        }
        Ok(text) => from_toml(&text, &path.display().to_string()),
    }
}

fn from_toml(text: &str, origin: &str) -> Config {
    let doc: toml::Table = match toml::from_str(text) {
        Ok(doc) => doc,
        Err(err) => {
            // Report the parse error, run on defaults.
            eprintln!("lux: {origin}: {err}");
            return Config::default();
        }
    };
    let mut config = Config::default();
    if let Some(value) = doc.get("prefix") {
        match value.as_str().and_then(parse_key_spec) {
            // The configured prefix replaces the default.
            Some(key) => config.keys.prefix = key,
            None => eprintln!("lux: {origin}: invalid prefix key {value}"),
        }
    }
    if let Some(value) = doc.get("restore") {
        match value.as_bool() {
            Some(restore) => config.restore = restore,
            None => eprintln!("lux: {origin}: invalid restore value {value}"),
        }
    }
    if let Some(value) = doc.get("notify") {
        match value.as_bool() {
            Some(notify) => config.notify = notify,
            None => eprintln!("lux: {origin}: invalid notify value {value}"),
        }
    }
    config
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
    Some(KeyMatch {
        code: CtKeyCode::Char(c),
        ctrl,
        shift: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> KeyTable {
        from_toml(text, "test").keys
    }

    #[test]
    fn empty_config_yields_defaults() {
        let t = table("");
        let d = KeyTable::default();
        assert_eq!(t.prefix, d.prefix);
        assert_eq!(t.root, d.root);
        assert!(from_toml("", "test").restore);
    }

    #[test]
    fn malformed_toml_yields_defaults() {
        let t = table("prefix = [broken");
        assert_eq!(t.prefix, crate::server::keys::DEFAULT_PREFIX);
        assert_eq!(t.root, KeyTable::default().root);
    }

    #[test]
    fn restore_option_parses_and_defaults_on() {
        // Absent means restore.
        assert!(from_toml("prefix = \"C-a\"", "test").restore);
        assert!(!from_toml("restore = false", "test").restore);
        assert!(from_toml("restore = true", "test").restore);
        // A non-boolean value keeps the default.
        assert!(from_toml("restore = \"no\"", "test").restore);
    }

    #[test]
    fn notify_option_parses_and_defaults_on() {
        // Absent means notify.
        assert!(from_toml("prefix = \"C-a\"", "test").notify);
        assert!(!from_toml("notify = false", "test").notify);
        assert!(from_toml("notify = true", "test").notify);
        // A non-boolean value keeps the default.
        assert!(from_toml("notify = \"no\"", "test").notify);
    }

    #[test]
    fn configured_prefix_replaces_default() {
        let t = table("prefix = \"C-a\"");
        assert_eq!(
            t.prefix,
            KeyMatch {
                code: CtKeyCode::Char('a'),
                ctrl: true,
                shift: false
            }
        );
    }

    #[test]
    fn invalid_prefix_keeps_default() {
        assert_eq!(
            table("prefix = \"C-\"").prefix,
            crate::server::keys::DEFAULT_PREFIX
        );
        assert_eq!(
            table("prefix = \"abc\"").prefix,
            crate::server::keys::DEFAULT_PREFIX
        );
        assert_eq!(
            table("prefix = 5").prefix,
            crate::server::keys::DEFAULT_PREFIX
        );
    }

    #[test]
    fn keybinding_overrides_are_not_a_setting() {
        // Per-command keybinding overrides are prohibited: a `[keys]` table changes nothing.
        let t = table("prefix = \"C-a\"\n[keys]\nnew-tab = \"t\"");
        assert_eq!(t.root, KeyTable::default().root);
        assert_eq!(
            t.prefix,
            KeyMatch {
                code: CtKeyCode::Char('a'),
                ctrl: true,
                shift: false
            }
        );
    }
}
