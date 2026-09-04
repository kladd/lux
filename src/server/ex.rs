//! Ex command verbs: parsing and prefix suggestions.

use std::path::PathBuf;

pub const COMMANDS: &[&str] = &[
    "config-open",
    "config-reload",
    "kill-session",
    "new",
    "new-session",
    "rename-session",
    "sp",
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
}

pub fn parse(text: &str) -> Option<ExCommand> {
    match text {
        "vs" => Some(ExCommand::SplitSideBySide),
        "sp" => Some(ExCommand::SplitStacked),
        "new" | "new-session" => Some(ExCommand::NewSession(None)),
        "kill-session" => Some(ExCommand::KillSession(None)),
        "config-open" => Some(ExCommand::ConfigOpen),
        "config-reload" => Some(ExCommand::ConfigReload),
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

pub fn suggestions(text: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(text))
        .collect()
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
                "kill-session",
                "new",
                "new-session",
                "rename-session",
                "sp",
                "vs",
                "w"
            ]
        );
        assert_eq!(suggestions("v"), vec!["vs"]);
        assert_eq!(suggestions("new"), vec!["new", "new-session"]);
        assert_eq!(suggestions("rename"), vec!["rename-session"]);
        assert_eq!(suggestions("kill"), vec!["kill-session"]);
        assert_eq!(suggestions("config"), vec!["config-open", "config-reload"]);
        assert_eq!(suggestions("w"), vec!["w"]);
        assert_eq!(suggestions("w /tmp"), Vec::<&str>::new());
        assert_eq!(suggestions("x"), Vec::<&str>::new());
    }
}
