//! Terminal modes a program set, kept as the sequences that set them, so
//! a fresh engine can be put back in the same state.

use termwiz::escape::csi::{Cursor, DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, CSI, Esc, EscCode};

#[derive(Default)]
pub struct ModeTracker {
    parser: Parser,
    /// Each mode's latest setting, oldest change first, keyed by what it
    /// sets.
    modes: Vec<(String, String)>,
}

impl ModeTracker {
    pub fn with_modes(modes: Vec<(String, String)>) -> Self {
        Self {
            parser: Parser::new(),
            modes,
        }
    }

    pub fn modes(&self) -> &[(String, String)] {
        &self.modes
    }

    /// Parses `bytes` into actions for the engine, noting any mode they set.
    pub fn parse(&mut self, bytes: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let modes = &mut self.modes;
        self.parser.parse(bytes, |action| {
            observe(modes, &action);
            actions.push(action);
        });
        actions
    }

    /// The sequences that restore every mode, in the order they were set.
    pub fn replay(&self) -> String {
        self.modes.iter().map(|(_, seq)| seq.as_str()).collect()
    }
}

fn observe(modes: &mut Vec<(String, String)>, action: &Action) {
    let key = match action {
        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(mode) | Mode::ResetDecPrivateMode(mode))) => {
            // The engine's screens are restored on their own.
            if matches!(
                mode,
                DecPrivateMode::Code(
                    DecPrivateModeCode::EnableAlternateScreen
                        | DecPrivateModeCode::OptEnableAlternateScreen
                        | DecPrivateModeCode::ClearAndEnableAlternateScreen
                )
            ) {
                return;
            }
            format!("dec {mode:?}")
        }
        Action::CSI(CSI::Mode(Mode::SetMode(mode) | Mode::ResetMode(mode))) => {
            format!("ansi {mode:?}")
        }
        Action::CSI(CSI::Mode(Mode::XtermKeyMode { resource, .. })) => {
            format!("key {resource:?}")
        }
        Action::CSI(CSI::Cursor(Cursor::SetTopAndBottomMargins { .. })) => "margins".into(),
        Action::CSI(CSI::Cursor(Cursor::SetLeftAndRightMargins { .. })) => "side margins".into(),
        Action::CSI(CSI::Cursor(Cursor::CursorStyle(_))) => "cursor style".into(),
        Action::Esc(Esc::Code(EscCode::DecApplicationKeyPad | EscCode::DecNormalKeyPad)) => {
            "keypad".into()
        }
        Action::Esc(Esc::Code(EscCode::FullReset)) => {
            modes.clear();
            return;
        }
        Action::CSI(CSI::Device(device)) if matches!(**device, Device::SoftReset) => {
            modes.clear();
            return;
        }
        _ => return,
    };
    modes.retain(|(k, _)| *k != key);
    modes.push((key, action.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_setting_of_each_mode_replays_in_order() {
        let mut tracker = ModeTracker::default();
        tracker.parse(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        tracker.parse(b"hello\x1b[?1000l\x1b=");
        assert_eq!(tracker.replay(), "\x1b[?2004h\x1b[?1006h\x1b[?1000l\x1b=");
    }

    #[test]
    fn a_sequence_split_across_reads_is_still_seen() {
        let mut tracker = ModeTracker::default();
        tracker.parse(b"\x1b[?20");
        tracker.parse(b"04h");
        assert_eq!(tracker.replay(), "\x1b[?2004h");
    }

    #[test]
    fn screen_switches_are_left_out_and_a_reset_forgets_everything() {
        let mut tracker = ModeTracker::default();
        tracker.parse(b"\x1b[?1049h\x1b[?1h");
        assert_eq!(tracker.replay(), "\x1b[?1h");
        tracker.parse(b"\x1bc");
        assert_eq!(tracker.replay(), "");
    }
}
