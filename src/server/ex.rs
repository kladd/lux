//! Ex command verbs: parsing and prefix suggestions.

use std::path::PathBuf;

use crate::server::config;

pub const COMMANDS: &[&str] = &[
    "config-open",
    "config-reload",
    "config-set",
    "connect",
    "disconnect",
    "kill-session",
    "new",
    "new-session",
    "rename-session",
    "sp",
    "upgrade",
    "vs",
    "w",
];

#[derive(Debug, PartialEq, Eq)]
pub enum ExCommand {
    SplitSideBySide,
    SplitStacked,
    /// Write the tab's content, scrollback included.
    Write(PathBuf),
    NewSession(Option<String>),
    RenameSession(String),
    /// `None` kills the current session.
    KillSession(Option<String>),
    /// Edit the config file in a new tab.
    ConfigOpen,
    /// Re-read the config file and apply it to every session.
    ConfigReload,
    /// Write one key to the config file, then reload it.
    ConfigSet(String, String),
    /// Adopt the sessions of the host at this ssh alias.
    Connect(String),
    Disconnect(String),
    /// Replace the server with the executable it started from.
    Upgrade,
}

pub fn parse(text: &str) -> Option<ExCommand> {
    match text {
        "vs" => Some(ExCommand::SplitSideBySide),
        "sp" => Some(ExCommand::SplitStacked),
        "new" | "new-session" => Some(ExCommand::NewSession(None)),
        "kill-session" => Some(ExCommand::KillSession(None)),
        "config-open" => Some(ExCommand::ConfigOpen),
        "config-reload" => Some(ExCommand::ConfigReload),
        "upgrade" => Some(ExCommand::Upgrade),
        _ => {
            if let Some(name) = arg(text, "new").or_else(|| arg(text, "new-session")) {
                return Some(ExCommand::NewSession(Some(name.to_string())));
            }
            if let Some(name) = arg(text, "rename-session") {
                return Some(ExCommand::RenameSession(name.to_string()));
            }
            if let Some(name) = arg(text, "kill-session") {
                return Some(ExCommand::KillSession(Some(name.to_string())));
            }
            if let Some(args) = arg(text, "config-set") {
                let mut words = args.split_whitespace();
                return match (words.next(), words.next(), words.next()) {
                    (Some(key), Some(value), None) => {
                        Some(ExCommand::ConfigSet(key.into(), value.into()))
                    }
                    _ => None,
                };
            }
            if let Some(alias) = arg(text, "connect") {
                return Some(ExCommand::Connect(alias.to_string()));
            }
            if let Some(alias) = arg(text, "disconnect") {
                return Some(ExCommand::Disconnect(alias.to_string()));
            }
            let path = text.strip_prefix("w ")?.trim();
            if path.is_empty() {
                return None;
            }
            Some(ExCommand::Write(path.into()))
        }
    }
}

fn arg<'a>(text: &'a str, verb: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(verb)?.strip_prefix(' ')?.trim();
    (!rest.is_empty()).then_some(rest)
}

/// The partial key after `config-set`, before any value is typed.
/// The word being typed, the names it may complete to, and whether a
/// unique completion takes a trailing space.
fn last_word(text: &str) -> (&str, &'static [&'static str], bool) {
    let Some(args) = text.strip_prefix("config-set ") else {
        return (text, COMMANDS, false);
    };
    let args = args.trim_start();
    let Some((key, value)) = args.split_once(char::is_whitespace) else {
        return (args, config::KEYS, true);
    };
    let value = value.trim_start();
    if value.contains(char::is_whitespace) {
        return (value, &[], false);
    }
    (value, config::values(key), false)
}

pub fn suggestions(text: &str) -> Vec<&'static str> {
    let (partial, names, _) = last_word(text);
    let mut matches: Vec<_> = names
        .iter()
        .copied()
        .filter(|name| name.starts_with(partial))
        .collect();
    matches.sort_unstable();
    matches
}

/// The text with its last word extended to the longest prefix shared by
/// every suggestion, or `None` when nothing matches.
pub fn complete(text: &str) -> Option<String> {
    let matches = suggestions(text);
    let (first, rest) = matches.split_first()?;
    let len = rest.iter().fold(first.len(), |len, m| {
        first[..len]
            .bytes()
            .zip(m.bytes())
            .take_while(|(a, b)| a == b)
            .count()
    });
    let (partial, _, space) = last_word(text);
    let mut out = format!("{}{}", &text[..text.len() - partial.len()], &first[..len]);
    if space && rest.is_empty() {
        out.push(' ');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_verbs_parse() {
        assert_eq!(parse("vs"), Some(ExCommand::SplitSideBySide));
        assert_eq!(parse("sp"), Some(ExCommand::SplitStacked));
        assert_eq!(
            parse("w /tmp/out.txt"),
            Some(ExCommand::Write("/tmp/out.txt".into()))
        );
        assert_eq!(parse("w   spaced"), Some(ExCommand::Write("spaced".into())));
        assert_eq!(parse("config-open"), Some(ExCommand::ConfigOpen));
        assert_eq!(parse("config-reload"), Some(ExCommand::ConfigReload));
        assert_eq!(parse("upgrade"), Some(ExCommand::Upgrade));
    }

    #[test]
    fn new_session_parses_with_and_without_a_name() {
        assert_eq!(parse("new"), Some(ExCommand::NewSession(None)));
        assert_eq!(parse("new-session"), Some(ExCommand::NewSession(None)));
        assert_eq!(
            parse("new work"),
            Some(ExCommand::NewSession(Some("work".into())))
        );
        assert_eq!(
            parse("new-session work"),
            Some(ExCommand::NewSession(Some("work".into())))
        );
        assert_eq!(parse("new "), None);
        assert_eq!(parse("new-session  "), None);
    }

    #[test]
    fn connect_and_disconnect_take_an_alias() {
        assert_eq!(parse("connect dev"), Some(ExCommand::Connect("dev".into())));
        assert_eq!(
            parse("disconnect dev"),
            Some(ExCommand::Disconnect("dev".into()))
        );
        assert_eq!(parse("connect"), None);
        assert_eq!(parse("disconnect "), None);
    }

    #[test]
    fn config_set_takes_exactly_a_key_and_a_value() {
        assert_eq!(
            parse("config-set sidebar true"),
            Some(ExCommand::ConfigSet("sidebar".into(), "true".into()))
        );
        assert_eq!(parse("config-set sidebar"), None);
        assert_eq!(parse("config-set"), None);
        assert_eq!(parse("config-set a b c"), None);
    }

    #[test]
    fn unrecognized_text_parses_to_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("vsp"), None);
        assert_eq!(parse("vs "), None);
        assert_eq!(parse(" vs"), None);
        assert_eq!(parse("q"), None);
        assert_eq!(parse("news"), None);
        assert_eq!(parse("w"), None);
        assert_eq!(parse("w   "), None);
        assert_eq!(parse("config"), None);
        assert_eq!(parse("config-open x"), None);
        assert_eq!(parse("config-reload "), None);
    }

    #[test]
    fn suggestions_narrow_with_the_text() {
        assert_eq!(
            suggestions(""),
            vec![
                "config-open",
                "config-reload",
                "config-set",
                "connect",
                "disconnect",
                "kill-session",
                "new",
                "new-session",
                "rename-session",
                "sp",
                "upgrade",
                "vs",
                "w"
            ]
        );
        assert_eq!(suggestions("v"), vec!["vs"]);
        assert_eq!(suggestions("new"), vec!["new", "new-session"]);
        assert_eq!(suggestions("rename"), vec!["rename-session"]);
        assert_eq!(suggestions("kill"), vec!["kill-session"]);
        assert_eq!(
            suggestions("config"),
            vec!["config-open", "config-reload", "config-set"]
        );
        assert_eq!(suggestions("w"), vec!["w"]);
        assert_eq!(suggestions("w /tmp"), Vec::<&str>::new());
        assert_eq!(suggestions("x"), Vec::<&str>::new());
    }

    #[test]
    fn complete_extends_to_the_common_prefix() {
        assert_eq!(complete("v").as_deref(), Some("vs"));
        assert_eq!(complete("ren").as_deref(), Some("rename-session"));
        assert_eq!(complete("con").as_deref(), Some("con"));
        assert_eq!(complete("config-").as_deref(), Some("config-"));
        assert_eq!(complete("config-r").as_deref(), Some("config-reload"));
        assert_eq!(complete("ne").as_deref(), Some("new"));
        assert_eq!(complete("").as_deref(), Some(""));
    }

    #[test]
    fn config_set_suggests_and_completes_keys() {
        assert_eq!(suggestions("config-set s"), vec!["shadows", "sidebar"]);
        assert!(suggestions("config-set ").contains(&"prefix"));
        assert_eq!(complete("config-set s").as_deref(), Some("config-set s"));
        assert_eq!(
            complete("config-set gr").as_deref(),
            Some("config-set grid-mouse ")
        );
        assert_eq!(
            complete("config-set  at").as_deref(),
            Some("config-set  attach-style ")
        );
        assert_eq!(complete("config-set x"), None);
    }

    #[test]
    fn config_set_suggests_and_completes_fixed_values() {
        assert_eq!(
            suggestions("config-set attach-style c"),
            vec!["coalesce", "crt"]
        );
        assert_eq!(suggestions("config-set sidebar "), vec!["false", "true"]);
        assert_eq!(suggestions("config-set prefix "), Vec::<&str>::new());
        assert_eq!(suggestions("config-set sidebar true x"), Vec::<&str>::new());
        assert_eq!(
            complete("config-set attach-style g").as_deref(),
            Some("config-set attach-style glitch")
        );
        assert_eq!(
            complete("config-set osc-titles a").as_deref(),
            Some("config-set osc-titles a")
        );
        assert_eq!(complete("config-set rule-style x"), None);
    }

    #[test]
    fn complete_without_matches_is_none() {
        assert_eq!(complete("x"), None);
        assert_eq!(complete("w /tmp"), None);
    }
}
