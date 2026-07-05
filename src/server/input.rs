//! Server-side terminal input decoding (REQ-SESSION-010/030): raw bytes
//! read from a client's passed stdin descriptor are parsed with termwiz's
//! `InputParser` and converted to the crossterm-typed events the session
//! layer already speaks, reproducing the single-process behavior.

use ratatui::crossterm::event::{
    KeyCode as CtKeyCode, KeyEvent, KeyModifiers as CtMods, MouseButton as CtMouseButton,
    MouseEvent as CtMouseEvent, MouseEventKind as CtMouseKind,
};
use termwiz::input::{
    InputEvent, InputParser, KeyCode as TwKey, KeyEvent as TwKeyEvent, Modifiers as TwMods,
    MouseButtons as TwButtons, MouseEvent as TwMouseEvent,
};

pub enum DecodedInput {
    Key(KeyEvent),
    Mouse(CtMouseEvent),
    /// Bracketed paste from the client terminal.
    Paste(String),
}

pub struct InputDecoder {
    parser: InputParser,
    /// Buttons held as of the previous mouse event, to derive
    /// press/release/drag kinds from termwiz's stateless reports.
    buttons: TwButtons,
}

impl Default for InputDecoder {
    fn default() -> Self {
        Self {
            parser: InputParser::new(),
            buttons: TwButtons::NONE,
        }
    }
}

impl InputDecoder {
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<DecodedInput> {
        let events = self.parser.parse_as_vec(bytes, false);
        events
            .into_iter()
            .filter_map(|event| match event {
                InputEvent::Key(key) => convert_key(key).map(DecodedInput::Key),
                InputEvent::Mouse(mouse) => self.convert_mouse(mouse),
                InputEvent::Paste(text) => Some(DecodedInput::Paste(text)),
                _ => None,
            })
            .collect()
    }

    fn convert_mouse(&mut self, mouse: TwMouseEvent) -> Option<DecodedInput> {
        // termwiz passes SGR coordinates through raw (1-based).
        let column = mouse.x.saturating_sub(1);
        let row = mouse.y.saturating_sub(1);
        let now = mouse.mouse_buttons;

        let kind = if now.contains(TwButtons::VERT_WHEEL) {
            if now.contains(TwButtons::WHEEL_POSITIVE) {
                CtMouseKind::ScrollUp
            } else {
                CtMouseKind::ScrollDown
            }
        } else {
            let pairs = [
                (TwButtons::LEFT, CtMouseButton::Left),
                (TwButtons::RIGHT, CtMouseButton::Right),
                (TwButtons::MIDDLE, CtMouseButton::Middle),
            ];
            let pressed = pairs
                .iter()
                .find(|(tw, _)| now.contains(tw.clone()) && !self.buttons.contains(tw.clone()));
            let released = pairs
                .iter()
                .find(|(tw, _)| !now.contains(tw.clone()) && self.buttons.contains(tw.clone()));
            let held = pairs.iter().find(|(tw, _)| now.contains(tw.clone()));
            let kind = match (pressed, released, held) {
                (Some((_, b)), _, _) => CtMouseKind::Down(*b),
                (None, Some((_, b)), _) => CtMouseKind::Up(*b),
                (None, None, Some((_, b))) => CtMouseKind::Drag(*b),
                (None, None, None) => CtMouseKind::Moved,
            };
            // Wheel events don't change the held-button state.
            self.buttons = now;
            kind
        };

        Some(DecodedInput::Mouse(CtMouseEvent {
            kind,
            column,
            row,
            modifiers: convert_mods(mouse.modifiers),
        }))
    }
}

fn convert_mods(mods: TwMods) -> CtMods {
    let mut out = CtMods::NONE;
    if mods.contains(TwMods::SHIFT) {
        out |= CtMods::SHIFT;
    }
    if mods.contains(TwMods::CTRL) {
        out |= CtMods::CONTROL;
    }
    if mods.contains(TwMods::ALT) {
        out |= CtMods::ALT;
    }
    if mods.contains(TwMods::SUPER) {
        out |= CtMods::SUPER;
    }
    out
}

fn convert_key(key: TwKeyEvent) -> Option<KeyEvent> {
    let mut mods = convert_mods(key.modifiers);
    let code = match key.key {
        // Control bytes may surface as raw control chars; normalize them
        // to what crossterm would have reported so the session layer (and
        // the keybinding table's Ctrl matching) behaves identically.
        TwKey::Char('\r') => CtKeyCode::Enter,
        TwKey::Char('\t') => CtKeyCode::Tab,
        TwKey::Char('\u{7f}') | TwKey::Char('\u{8}') => CtKeyCode::Backspace,
        TwKey::Char(c) if (c as u32) < 0x20 => {
            mods |= CtMods::CONTROL;
            CtKeyCode::Char((((c as u8) | 0x60) as char).to_ascii_lowercase())
        }
        TwKey::Char(c) => CtKeyCode::Char(c),
        TwKey::Enter => CtKeyCode::Enter,
        TwKey::Tab => CtKeyCode::Tab,
        TwKey::Backspace => CtKeyCode::Backspace,
        TwKey::Escape => CtKeyCode::Esc,
        TwKey::LeftArrow => CtKeyCode::Left,
        TwKey::RightArrow => CtKeyCode::Right,
        TwKey::UpArrow => CtKeyCode::Up,
        TwKey::DownArrow => CtKeyCode::Down,
        TwKey::Home => CtKeyCode::Home,
        TwKey::End => CtKeyCode::End,
        TwKey::PageUp => CtKeyCode::PageUp,
        TwKey::PageDown => CtKeyCode::PageDown,
        TwKey::Insert => CtKeyCode::Insert,
        TwKey::Delete => CtKeyCode::Delete,
        TwKey::Function(n) => CtKeyCode::F(n),
        _ => return None,
    };
    Some(KeyEvent::new(code, mods))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(decoder: &mut InputDecoder, bytes: &[u8]) -> Vec<KeyEvent> {
        decoder
            .decode(bytes)
            .into_iter()
            .filter_map(|d| match d {
                DecodedInput::Key(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_and_control_keys_decode() {
        let mut d = InputDecoder::default();
        let evs = keys(&mut d, b"a");
        assert_eq!(evs[0].code, CtKeyCode::Char('a'));
        // Ctrl-B (the default prefix) is byte 0x02.
        let evs = keys(&mut d, b"\x02");
        assert_eq!(evs[0].code, CtKeyCode::Char('b'));
        assert!(evs[0].modifiers.contains(CtMods::CONTROL));
        // Enter arrives as CR in raw mode.
        let evs = keys(&mut d, b"\r");
        assert_eq!(evs[0].code, CtKeyCode::Enter);
        // Arrow key CSI.
        let evs = keys(&mut d, b"\x1b[A");
        assert_eq!(evs[0].code, CtKeyCode::Up);
    }

    #[test]
    fn sgr_mouse_press_drag_release_decode() {
        let mut d = InputDecoder::default();
        let ev = |bytes: &[u8], d: &mut InputDecoder| match d.decode(bytes).pop() {
            Some(DecodedInput::Mouse(m)) => m,
            other => panic!("expected mouse event, got {:?}", other.is_some()),
        };
        // Press left at 1-based (5, 3) → 0-based (4, 2).
        let m = ev(b"\x1b[<0;5;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::Down(CtMouseButton::Left));
        assert_eq!((m.column, m.row), (4, 2));
        // Motion with left held (code 32) → drag.
        let m = ev(b"\x1b[<32;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::Drag(CtMouseButton::Left));
        // Release (lowercase m).
        let m = ev(b"\x1b[<0;6;3m", &mut d);
        assert_eq!(m.kind, CtMouseKind::Up(CtMouseButton::Left));
        // Wheel up (code 64).
        let m = ev(b"\x1b[<64;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::ScrollUp);
        // Wheel down (code 65).
        let m = ev(b"\x1b[<65;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::ScrollDown);
    }
}
