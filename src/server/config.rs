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

/// How the first frame after attaching is revealed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AttachStyle {
    Coalesce,
    #[default]
    Rain,
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
    /// Animate maximize.
    pub layout_transitions: bool,
    /// Reveal the first frame gradually after a client attaches.
    pub attach_transition: bool,
    pub attach_style: AttachStyle,
    /// Keep the session list visible beside the layout.
    pub sidebar: bool,
    /// Clicks and the wheel work in the CLAUDECOM grid.
    pub grid_mouse: bool,
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
            attach_style: AttachStyle::default(),
            sidebar: false,
            grid_mouse: false,
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
        Ok(text) => {
            let (config, invalid) = parse(&text, &path.display().to_string())?;
            for (_, message) in invalid {
                eprintln!("lux: {message}");
            }
            Ok(config)
        }
    }
}

/// Writes `key = value` to the file, creating it if needed, and returns
/// the config it now holds. The file is left alone if the result wouldn't
/// parse or the value isn't valid for the key.
pub fn set(key: &str, value: &str) -> Result<Config, String> {
    if !KEYS.contains(&key) {
        return Err(format!("unknown config key {key}"));
    }
    let path = path().ok_or("no config path: HOME is unset")?;
    let origin = path.display().to_string();
    let text = match std::fs::read_to_string(&path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(format!("{origin}: {err}")),
        Ok(text) => text,
    };
    let text = with_key(&text, key, &parse_value(value));
    let (config, invalid) = parse(&text, &origin)?;
    if let Some((_, message)) = invalid.iter().find(|(k, _)| *k == key) {
        return Err(message.clone());
    }
    for (_, message) in invalid {
        eprintln!("lux: {message}");
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    std::fs::write(&path, text).map_err(|err| format!("{origin}: {err}"))?;
    Ok(config)
}

/// A TOML literal as written, or else a bare string.
fn parse_value(text: &str) -> toml::Value {
    toml::from_str::<toml::Table>(&format!("v = {text}"))
        .ok()
        .and_then(|mut table| table.remove("v"))
        .unwrap_or_else(|| toml::Value::String(text.into()))
}

/// Replaces the top-level line for `key`, or adds one ahead of the first
/// table.
fn with_key(text: &str, key: &str, value: &toml::Value) -> String {
    let line = format!("{key} = {value}");
    let mut lines: Vec<&str> = text.lines().collect();
    let top_end = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    let existing = lines[..top_end].iter().position(|l| {
        l.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });
    match existing {
        Some(i) => lines[i] = &line,
        None => {
            let mut at = top_end;
            while at > 0 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.insert(at, &line);
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Every top-level key the file may set.
const KEYS: &[&str] = &[
    "prefix",
    "restore",
    "notify",
    "automode",
    "copy-on-select",
    "osc-titles",
    "rule-style",
    "palette",
    "dim-unfocused",
    "shadows",
    "layout-transitions",
    "attach-transition",
    "attach-style",
    "sidebar",
    "grid-mouse",
];

/// A key whose value was skipped, and why.
type Invalid = (&'static str, String);

/// Invalid values are skipped, each reported with its key.
fn parse(text: &str, origin: &str) -> Result<(Config, Vec<Invalid>), String> {
    let doc: toml::Table = toml::from_str(text).map_err(|err| format!("{origin}: {err}"))?;
    let mut config = Config::default();
    let mut invalid = Vec::new();
    for &key in KEYS {
        if let Some(value) = doc.get(key)
            && !apply(&mut config, key, value)
        {
            invalid.push((key, format!("{origin}: invalid {key} value {value}")));
        }
    }
    Ok((config, invalid))
}

/// False when the value isn't valid for the key.
fn apply(config: &mut Config, key: &str, value: &toml::Value) -> bool {
    let flag = |field: &mut bool| value.as_bool().map(|b| *field = b).is_some();
    match (key, value.as_str()) {
        ("prefix", spec) => match spec.and_then(parse_key_spec) {
            Some(prefix) => config.keys.set_prefix(prefix),
            None => return false,
        },
        ("restore", _) => return flag(&mut config.restore),
        ("notify", _) => return flag(&mut config.notify),
        ("automode", _) => return flag(&mut config.automode),
        ("copy-on-select", _) => return flag(&mut config.copy_on_select),
        ("dim-unfocused", _) => return flag(&mut config.dim_unfocused),
        ("shadows", _) => return flag(&mut config.shadows),
        ("layout-transitions", _) => return flag(&mut config.layout_transitions),
        ("attach-transition", _) => return flag(&mut config.attach_transition),
        ("sidebar", _) => return flag(&mut config.sidebar),
        ("grid-mouse", _) => return flag(&mut config.grid_mouse),
        ("osc-titles", Some("none")) => config.osc_titles = OscTitles::None,
        ("osc-titles", Some("agents")) => config.osc_titles = OscTitles::Agents,
        ("osc-titles", Some("all")) => config.osc_titles = OscTitles::All,
        ("rule-style", Some("rule")) => config.rule_style = RuleStyle::Rule,
        ("rule-style", Some("dots")) => config.rule_style = RuleStyle::Dots,
        ("palette", name) => match name.and_then(Palette::named) {
            Some(palette) => config.palette = palette,
            None => return false,
        },
        ("attach-style", Some("coalesce")) => config.attach_style = AttachStyle::Coalesce,
        ("attach-style", Some("rain")) => config.attach_style = AttachStyle::Rain,
        _ => return false,
    }
    true
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
        parse(text, origin).map(|(c, _)| c).unwrap_or_default()
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
    fn attach_style_option_parses_and_defaults_to_rain() {
        let parse = |text: &str| from_toml(text, "test").attach_style;
        assert_eq!(parse(""), AttachStyle::Rain);
        assert_eq!(parse("attach-style = \"rain\""), AttachStyle::Rain);
        assert_eq!(parse("attach-style = \"coalesce\""), AttachStyle::Coalesce);
        assert_eq!(parse("attach-style = \"snow\""), AttachStyle::Rain);
        assert_eq!(parse("attach-style = true"), AttachStyle::Rain);
    }

    #[test]
    fn grid_mouse_option_parses_and_defaults_off() {
        assert!(!from_toml("", "test").grid_mouse);
        assert!(from_toml("grid-mouse = true", "test").grid_mouse);
        assert!(!from_toml("grid-mouse = false", "test").grid_mouse);
        assert!(!from_toml("grid-mouse = \"yes\"", "test").grid_mouse);
    }

    #[test]
    fn sidebar_option_parses_and_defaults_off() {
        assert!(!from_toml("", "test").sidebar);
        assert!(from_toml("sidebar = true", "test").sidebar);
        assert!(!from_toml("sidebar = false", "test").sidebar);
        assert!(!from_toml("sidebar = \"yes\"", "test").sidebar);
    }

    #[test]
    fn invalid_values_are_reported_by_key() {
        let (_, invalid) = parse("shadows = 1\nnotify = false\nrule-style = \"x\"", "t").unwrap();
        let keys: Vec<&str> = invalid.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["rule-style", "shadows"]);
    }

    #[test]
    fn set_values_parse_as_toml_or_fall_back_to_strings() {
        assert_eq!(parse_value("false"), toml::Value::Boolean(false));
        assert_eq!(parse_value("rain"), toml::Value::String("rain".into()));
        assert_eq!(parse_value("\"C-a\""), toml::Value::String("C-a".into()));
        assert_eq!(parse_value("C-a"), toml::Value::String("C-a".into()));
    }

    #[test]
    fn with_key_replaces_a_top_level_line_in_place() {
        let text = "# mine\nshadows = false # off\nnotify = true\n";
        assert_eq!(
            with_key(text, "shadows", &toml::Value::Boolean(true)),
            "# mine\nshadows = true\nnotify = true\n"
        );
    }

    #[test]
    fn with_key_adds_missing_keys_ahead_of_tables() {
        let bool = toml::Value::Boolean(true);
        assert_eq!(with_key("", "sidebar", &bool), "sidebar = true\n");
        assert_eq!(
            with_key(
                "notify = true\n\n[keys]\nsidebar = false\n",
                "sidebar",
                &bool
            ),
            "notify = true\nsidebar = true\n\n[keys]\nsidebar = false\n"
        );
        assert_eq!(
            with_key("shadows-extra = 1\n", "shadows", &bool),
            "shadows-extra = 1\nshadows = true\n"
        );
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
