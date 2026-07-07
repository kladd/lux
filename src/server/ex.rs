//! Ex command verbs: parsing typed command text and prefix-matching
//! suggestions. The recognized verb set is exactly `vs`, `sp`, and `w`.

use std::path::PathBuf;

pub const COMMANDS: &[&str] = &["sp", "vs", "w"];

#[derive(Debug, PartialEq, Eq)]
pub enum ExCommand {
    /// `vs`: split side-by-side.
    SplitSideBySide,
    /// `sp`: split stacked.
    SplitStacked,
    /// `w <path>`: write the tab's entire terminal content, scrollback
    /// included.
    Write(PathBuf),
}

/// Parse the command line's text on Enter. `None` means unrecognized —
/// including `w` with no path argument — and nothing runs.
pub fn parse(text: &str) -> Option<ExCommand> {
    match text {
        "vs" => Some(ExCommand::SplitSideBySide),
        "sp" => Some(ExCommand::SplitStacked),
        _ => {
            let path = text.strip_prefix("w ")?.trim();
            if path.is_empty() {
                return None;
            }
            Some(ExCommand::Write(path.into()))
        }
    }
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
    fn unrecognized_text_parses_to_none() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("vsp"), None);
        assert_eq!(parse("vs "), None);
        assert_eq!(parse(" vs"), None);
        assert_eq!(parse("q"), None);
        // `w` with no path argument.
        assert_eq!(parse("w"), None);
        assert_eq!(parse("w   "), None);
    }

    #[test]
    fn suggestions_narrow_with_the_text() {
        assert_eq!(suggestions(""), vec!["sp", "vs", "w"]);
        assert_eq!(suggestions("v"), vec!["vs"]);
        assert_eq!(suggestions("w"), vec!["w"]);
        assert_eq!(suggestions("w /tmp"), Vec::<&str>::new());
        assert_eq!(suggestions("x"), Vec::<&str>::new());
    }
}
