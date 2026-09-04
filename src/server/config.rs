//! Loads the TOML config file at startup and on `:config-reload`.

use std::path::PathBuf;

use ratatui::crossterm::event::KeyCode as CtKeyCode;

use crate::server::keys::{KeyMatch, KeyTable};
use crate::server::palette::Palette;

/// Which tabs may take their name from the program's OSC window title.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OscTitles {
    None,
    #[default]
    Agents,
    All,
}

/// The glyph a window's tab bar rule is drawn with.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RuleStyle {
    #[default]
    Rule,
    Dots,
}

impl RuleStyle {
    pub fn glyph(self) -> char {
        match self {
            RuleStyle::Rule => '─',
            RuleStyle::Dots => '⠶',
        }
    }
}

pub struct Config {
    pub keys: KeyTable,
    /// Restore persisted sessions at startup. Saving happens either way.
    pub restore: bool,
    /// Send a desktop notification when an agent tab reaches done or blocked.
    pub notify: bool,
    /// CLAUDECOM opens auto mode instead of the grid.
    pub automode: bool,
    /// Yank a drag selection to the system clipboard on release.
    pub copy_on_select: bool,
    pub osc_titles: OscTitles,
    pub rule_style: RuleStyle,
    pub palette: Palette,
    /// Darken every window but the focused one.
    pub dim_unfocused: bool,
    /// Popovers cast a shadow on the content beneath them.
    pub shadows: bool,
    /// Animate splits, window removal, and maximize.
    pub layout_transitions: bool,
    /// Materialize the first frame after a client attaches.
    pub attach_transition: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keys: KeyTable::default(),
            restore: true,
            notify: true,
            automode: false,
            copy_on_select: true,
            osc_titles: OscTitles::default(),
            rule_style: RuleStyle::default(),
            palette: Palette::default(),
            dim_unfocused: true,
            shadows: false,
            layout_transitions: true,
            attach_transition: true,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("lux").join("config.toml"))
}

/// The startup load: a file that can't be read or parsed yields defaults.
pub fn load() -> Config {
    reload().unwrap_or_else(|err| {
        eprintln!("lux: {err}");
        Config::default()
    })
}

/// Reads the file afresh, failing rather than falling back so a running
/// server can keep the config it has.
pub fn reload() -> Result<Config, String> {
    let Some(path) = path() else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        // No config file is not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(format!("{}: {err}", path.display())),
        Ok(text) => parse(&text, &path.display().to_string()),
    }
}

fn parse(text: &str, origin: &str) -> Result<Config, String> {
    let doc: toml::Table = toml::from_str(text).map_err(|err| format!("{origin}: {err}"))?;
    let mut config = Config::default();
    if let Some(value) = doc.get("prefix") {
        match value.as_str().and_then(parse_key_spec) {
            Some(key) => config.keys.set_prefix(key),
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
    if let Some(value) = doc.get("automode") {
        match value.as_bool() {
            Some(automode) => config.automode = automode,
            None => eprintln!("lux: {origin}: invalid automode value {value}"),
        }
    }
    if let Some(value) = doc.get("copy-on-select") {
        match value.as_bool() {
            Some(copy) => config.copy_on_select = copy,
            None => eprintln!("lux: {origin}: invalid copy-on-select value {value}"),
        }
    }
    if let Some(value) = doc.get("osc-titles") {
        match value.as_str() {
            Some("none") => config.osc_titles = OscTitles::None,
            Some("agents") => config.osc_titles = OscTitles::Agents,
            Some("all") => config.osc_titles = OscTitles::All,
            _ => eprintln!("lux: {origin}: invalid osc-titles value {value}"),
        }
    }
    if let Some(value) = doc.get("rule-style") {
        match value.as_str() {
            Some("rule") => config.rule_style = RuleStyle::Rule,
            Some("dots") => config.rule_style = RuleStyle::Dots,
            _ => eprintln!("lux: {origin}: invalid rule-style value {value}"),
        }
    }
    if let Some(value) = doc.get("palette") {
        match value.as_str().and_then(Palette::named) {
            Some(palette) => config.palette = palette,
            None => eprintln!("lux: {origin}: unknown palette {value}"),
        }
    }
    if let Some(value) = doc.get("dim-unfocused") {
        match value.as_bool() {
            Some(dim) => config.dim_unfocused = dim,
            None => eprintln!("lux: {origin}: invalid dim-unfocused value {value}"),
        }
    }
    if let Some(value) = doc.get("shadows") {
        match value.as_bool() {
            Some(shadows) => config.shadows = shadows,
            None => eprintln!("lux: {origin}: invalid shadows value {value}"),
        }
    }
    if let Some(value) = doc.get("layout-transitions") {
        match value.as_bool() {
            Some(animate) => config.layout_transitions = animate,
            None => eprintln!("lux: {origin}: invalid layout-transitions value {value}"),
        }
    }
    if let Some(value) = doc.get("attach-transition") {
        match value.as_bool() {
            Some(animate) => config.attach_transition = animate,
            None => eprintln!("lux: {origin}: invalid attach-transition value {value}"),
        }
    }
    Ok(config)
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

    fn from_toml(text: &str, origin: &str) -> Config {
        parse(text, origin).unwrap_or_default()
    }

    fn table(text: &str) -> KeyTable {
        from_toml(text, "test").keys
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse("prefix = [broken", "test").is_err());
        assert!(parse("", "test").is_ok());
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
        assert!(from_toml("prefix = \"C-a\"", "test").restore);
        assert!(!from_toml("restore = false", "test").restore);
        assert!(from_toml("restore = true", "test").restore);
        assert!(from_toml("restore = \"no\"", "test").restore);
    }

    #[test]
    fn notify_option_parses_and_defaults_on() {
        assert!(from_toml("prefix = \"C-a\"", "test").notify);
        assert!(!from_toml("notify = false", "test").notify);
        assert!(from_toml("notify = true", "test").notify);
        assert!(from_toml("notify = \"no\"", "test").notify);
    }

    #[test]
    fn automode_option_parses_and_defaults_off() {
        assert!(!from_toml("prefix = \"C-a\"", "test").automode);
        assert!(from_toml("automode = true", "test").automode);
        assert!(!from_toml("automode = false", "test").automode);
        assert!(!from_toml("automode = \"yes\"", "test").automode);
    }

    #[test]
    fn copy_on_select_option_parses_and_defaults_on() {
        assert!(from_toml("prefix = \"C-a\"", "test").copy_on_select);
        assert!(!from_toml("copy-on-select = false", "test").copy_on_select);
        assert!(from_toml("copy-on-select = true", "test").copy_on_select);
        assert!(from_toml("copy-on-select = \"no\"", "test").copy_on_select);
    }

    #[test]
    fn osc_titles_option_parses_and_defaults_to_agents() {
        assert_eq!(from_toml("", "test").osc_titles, OscTitles::Agents);
        assert_eq!(
            from_toml("osc-titles = \"none\"", "test").osc_titles,
            OscTitles::None
        );
        assert_eq!(
            from_toml("osc-titles = \"agents\"", "test").osc_titles,
            OscTitles::Agents
        );
        assert_eq!(
            from_toml("osc-titles = \"all\"", "test").osc_titles,
            OscTitles::All
        );
        assert_eq!(
            from_toml("osc-titles = \"shells\"", "test").osc_titles,
            OscTitles::Agents
        );
        assert_eq!(
            from_toml("osc-titles = true", "test").osc_titles,
            OscTitles::Agents
        );
    }

    #[test]
    fn rule_style_option_parses_and_defaults_to_rule() {
        let parse = |text: &str| from_toml(text, "test").rule_style;
        assert_eq!(parse(""), RuleStyle::Rule);
        assert_eq!(parse("rule-style = \"rule\""), RuleStyle::Rule);
        assert_eq!(parse("rule-style = \"dots\""), RuleStyle::Dots);
        assert_eq!(parse("rule-style = \"dashes\""), RuleStyle::Rule);
        assert_eq!(parse("rule-style = 2"), RuleStyle::Rule);
        assert_eq!(RuleStyle::Rule.glyph(), '─');
        assert_eq!(RuleStyle::Dots.glyph(), '\u{2836}');
    }

    #[test]
    fn layout_transitions_option_parses_and_defaults_on() {
        assert!(from_toml("", "test").layout_transitions);
        assert!(from_toml("layout-transitions = true", "test").layout_transitions);
        assert!(!from_toml("layout-transitions = false", "test").layout_transitions);
        assert!(from_toml("layout-transitions = \"no\"", "test").layout_transitions);
    }

    #[test]
    fn attach_transition_option_parses_and_defaults_on() {
        assert!(from_toml("", "test").attach_transition);
        assert!(from_toml("attach-transition = true", "test").attach_transition);
        assert!(!from_toml("attach-transition = false", "test").attach_transition);
        assert!(from_toml("attach-transition = \"no\"", "test").attach_transition);
    }

    #[test]
    fn palette_option_selects_a_named_set_and_defaults_otherwise() {
        assert_eq!(from_toml("", "test").palette, Palette::DEFAULT);
        assert_eq!(
            from_toml("palette = \"default\"", "test").palette,
            Palette::DEFAULT
        );
        assert_eq!(
            from_toml("palette = \"nope\"", "test").palette,
            Palette::DEFAULT
        );
        assert_eq!(from_toml("palette = 3", "test").palette, Palette::DEFAULT);
    }

    #[test]
    fn dim_unfocused_option_parses_and_defaults_on() {
        assert!(from_toml("", "test").dim_unfocused);
        assert!(from_toml("dim-unfocused = true", "test").dim_unfocused);
        assert!(!from_toml("dim-unfocused = false", "test").dim_unfocused);
        assert!(from_toml("dim-unfocused = \"yes\"", "test").dim_unfocused);
    }

    #[test]
    fn shadows_option_parses_and_defaults_off() {
        assert!(!from_toml("", "test").shadows);
        assert!(from_toml("shadows = true", "test").shadows);
        assert!(!from_toml("shadows = false", "test").shadows);
        assert!(!from_toml("shadows = 1", "test").shadows);
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
        let t = table("prefix = \"C-a\"\n[keys]\nnew-tab = \"t\"");
        let prefix = KeyMatch {
            code: CtKeyCode::Char('a'),
            ctrl: true,
            shift: false,
        };
        let mut expected = KeyTable::default();
        expected.set_prefix(prefix);
        assert_eq!(t.root, expected.root);
        assert_eq!(t.prefix, prefix);
    }
}
