//! Ex command verbs: parsing typed command text and prefix-matching
//! suggestions. The recognized verb set is exactly `vs`, `sp`, `w`,
//! `new`, and `new-session`.

use std::path::PathBuf;

pub const COMMANDS: &[&str] = &["new", "new-session", "sp", "vs", "w"];

#[derive(Debug, PartialEq, Eq)]
pub enum ExCommand {
    /// `vs`: split side-by-side.
    SplitSideBySide,
    /// `sp`: split stacked.
    SplitStacked,
    /// `w <path>`: write the tab's entire terminal content, scrollback
    /// included.
    Write(PathBuf),
    /// `new`/`new-session [name]`: create a session — named, or
    /// auto-named when no name is given — and attach to it.
    NewSession(Option<String>),
}

/// Parse the command line's text on Enter. `None` means unrecognized —
/// including `w` with no path argument — and nothing runs.
pub fn parse(text: &str) -> Option<ExCommand> {
    match text {
        "vs" => Some(ExCommand::SplitSideBySide),
        "sp" => Some(ExCommand::SplitStacked),
        "new" | "new-session" => Some(ExCommand::NewSession(None)),
        _ => {
            if let Some(name) = arg(text, "new").or_else(|| arg(text, "new-session")) {
                return Some(ExCommand::NewSession(Some(name.to_string())));
            }
            let path = text.strip_prefix("w ")?.trim();
            if path.is_empty() {
                return None;
            }
            Some(ExCommand::Write(path.into()))
        }
    }
}

/// The non-empty argument following `verb `, if the text is that form.
fn arg<'a>(text: &'a str, verb: &str) -> Option<&'a str> {
    let rest = text.strip_prefix(verb)?.strip_prefix(' ')?.trim();
    (!rest.is_empty()).then_some(rest)
}

/// The recognized commands whose names start with the text typed so far.
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
        // A trailing space with no name is unrecognized, like `vs `.
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
        // `w` with no path argument.
        assert_eq!(parse("w"), None);
        assert_eq!(parse("w   "), None);
    }

    #[test]
    fn suggestions_narrow_with_the_text() {
        assert_eq!(suggestions(""), vec!["new", "new-session", "sp", "vs", "w"]);
        assert_eq!(suggestions("v"), vec!["vs"]);
        assert_eq!(suggestions("new"), vec!["new", "new-session"]);
        assert_eq!(suggestions("w"), vec!["w"]);
        assert_eq!(suggestions("w /tmp"), Vec::<&str>::new());
        assert_eq!(suggestions("x"), Vec::<&str>::new());
    }
}
