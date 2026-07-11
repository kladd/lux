//! Server-side terminal input decoding: raw bytes
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
    /// Bytes carried across reads: an in-flight paste's content while its
    /// end marker hasn't arrived, or a partial paste-start marker cut off
    /// by the end of a read.
    held: Vec<u8>,
    /// Whether the byte stream is inside a bracketed paste.
    in_paste: bool,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Where a paste marker sits in a chunk of input, if anywhere.
enum Marker {
    /// The marker occurs whole at this offset.
    Full(usize),
    /// A prefix of the marker runs from this offset to the end of the
    /// input; the next read may complete it.
    Partial(usize),
    None,
}

/// The first occurrence of `marker` in `hay`, complete or cut off by the
/// end of the input.
fn find_marker(hay: &[u8], marker: &[u8]) -> Marker {
    for i in 0..hay.len() {
        if hay[i] != marker[0] {
            continue;
        }
        let rest = &hay[i..];
        if rest.len() >= marker.len() {
            if rest.starts_with(marker) {
                return Marker::Full(i);
            }
        } else if marker.starts_with(rest) {
            return Marker::Partial(i);
        }
    }
    Marker::None
}

impl Default for InputDecoder {
    fn default() -> Self {
        Self {
            parser: InputParser::new(),
            buttons: TwButtons::NONE,
            held: Vec::new(),
            in_paste: false,
        }
    }
}

impl InputDecoder {
    /// Decode one read's bytes, carrying paste state across reads: a
    /// bracketed paste is delivered as a single event once its end marker
    /// arrives, never as key events, however the stream is chunked.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<DecodedInput> {
        let mut buf = std::mem::take(&mut self.held);
        buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut rest = buf.as_slice();
        loop {
            if self.in_paste {
                match find_marker(rest, PASTE_END) {
                    Marker::Full(pos) => {
                        let text = String::from_utf8_lossy(&rest[..pos]).into_owned();
                        out.push(DecodedInput::Paste(text));
                        self.in_paste = false;
                        rest = &rest[pos + PASTE_END.len()..];
                    }
                    // Without the end marker the content is withheld —
                    // it may even stop mid-marker.
                    _ => {
                        self.held = rest.to_vec();
                        return out;
                    }
                }
            } else {
                match find_marker(rest, PASTE_START) {
                    Marker::Full(pos) => {
                        self.keys(&rest[..pos], &mut out);
                        self.in_paste = true;
                        rest = &rest[pos + PASTE_START.len()..];
                    }
                    // The read stops mid-marker: hold the fragment until
                    // the next read (or an idle flush) resolves it.
                    Marker::Partial(pos) => {
                        self.keys(&rest[..pos], &mut out);
                        self.held = rest[pos..].to_vec();
                        return out;
                    }
                    Marker::None => {
                        self.keys(rest, &mut out);
                        return out;
                    }
                }
            }
        }
    }

    /// Resolve held bytes once the stream has gone idle: a paste-start
    /// fragment that never completed was ordinary keys (e.g. a bare Esc).
    /// An in-flight paste's content stays withheld — a paste is delivered
    /// complete or not at all.
    pub fn flush(&mut self) -> Vec<DecodedInput> {
        let mut out = Vec::new();
        if !self.in_paste && !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            self.keys(&held, &mut out);
        }
        out
    }

    /// Parse non-paste bytes into key and mouse events. termwiz maps LF
    /// and CR both to Enter, which would re-encode Ctrl-J (LF) as CR on
    /// the pane pty; intercept LF so the two stay distinct keys.
    fn keys(&mut self, bytes: &[u8], out: &mut Vec<DecodedInput>) {
        let mut start = 0;
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\n' {
                // ESC-prefixed LF is the legacy Alt encoding.
                let alt = i > start && bytes[i - 1] == 0x1b;
                let seg_end = if alt { i - 1 } else { i };
                self.parse_into(&bytes[start..seg_end], out);
                let mods = if alt {
                    CtMods::CONTROL | CtMods::ALT
                } else {
                    CtMods::CONTROL
                };
                out.push(DecodedInput::Key(KeyEvent::new(CtKeyCode::Char('j'), mods)));
                start = i + 1;
                i = start;
            } else {
                i += 1;
            }
        }
        self.parse_into(&bytes[start..], out);
    }

    fn parse_into(&mut self, bytes: &[u8], out: &mut Vec<DecodedInput>) {
        if bytes.is_empty() {
            return;
        }
        let events = self.parser.parse_as_vec(bytes, false);
        out.extend(events.into_iter().filter_map(|event| match event {
            InputEvent::Key(key) => convert_key(key).map(DecodedInput::Key),
            InputEvent::Mouse(mouse) => self.convert_mouse(mouse),
            InputEvent::Paste(text) => Some(DecodedInput::Paste(text)),
            _ => None,
        }));
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
    fn shifted_arrows_keep_the_shift_modifier() {
        // Shift-Arrow (CSI 1;2) must stay distinct from the bare arrow:
        // the keybinding table binds focus to one and move-tab to the
        // other.
        let mut d = InputDecoder::default();
        let evs = keys(&mut d, b"\x1b[1;2D");
        assert_eq!(evs[0].code, CtKeyCode::Left);
        assert!(evs[0].modifiers.contains(CtMods::SHIFT));
        let evs = keys(&mut d, b"\x1b[D");
        assert_eq!(evs[0].code, CtKeyCode::Left);
        assert!(!evs[0].modifiers.contains(CtMods::SHIFT));
    }

    #[test]
    fn bs_and_del_both_decode_as_backspace() {
        // No binding distinguishes Ctrl-H from Backspace anymore; both bytes
        // pass through as Backspace, matching termwiz's own collapsing.
        let mut d = InputDecoder::default();
        assert_eq!(keys(&mut d, b"\x08")[0].code, CtKeyCode::Backspace);
        assert_eq!(keys(&mut d, b"\x7f")[0].code, CtKeyCode::Backspace);
    }

    #[test]
    fn lf_decodes_as_ctrl_j_not_enter() {
        let mut d = InputDecoder::default();
        // Ctrl-J arrives as LF; it must stay distinct from Enter (CR) so
        // apps that treat Ctrl-J as newline-insert don't see a submit.
        let evs = keys(&mut d, b"\n");
        assert_eq!(evs[0].code, CtKeyCode::Char('j'));
        assert!(evs[0].modifiers.contains(CtMods::CONTROL));
        // Surrounding bytes keep their order.
        let evs = keys(&mut d, b"a\nb");
        let codes: Vec<_> = evs.iter().map(|e| e.code).collect();
        assert_eq!(
            codes,
            vec![
                CtKeyCode::Char('a'),
                CtKeyCode::Char('j'),
                CtKeyCode::Char('b')
            ]
        );
        // Alt-Ctrl-J arrives ESC-prefixed.
        let evs = keys(&mut d, b"\x1b\n");
        assert_eq!(evs[0].code, CtKeyCode::Char('j'));
        assert!(evs[0].modifiers.contains(CtMods::CONTROL | CtMods::ALT));
    }

    fn paste_of(evs: &[DecodedInput]) -> &str {
        match evs {
            [DecodedInput::Paste(text)] => text,
            _ => panic!("expected a single paste event"),
        }
    }

    #[test]
    fn paste_content_keeps_newlines() {
        let mut d = InputDecoder::default();
        let evs = d.decode(b"\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(paste_of(&evs), "one\ntwo");
    }

    #[test]
    fn paste_spanning_reads_arrives_as_one_event() {
        // Content is withheld until the end marker arrives, then the
        // whole paste is a single event, with the mid-paste LF as content
        // rather than a keypress.
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b[200~one").is_empty());
        assert!(d.decode(b"\ntwo").is_empty());
        let evs = d.decode(b"\x1b[201~");
        assert_eq!(paste_of(&evs), "one\ntwo");
    }

    #[test]
    fn paste_markers_split_across_reads_still_frame_the_paste() {
        // The start marker's tail and the end marker's head each arrive
        // in a later read; neither fragment may leak as key events or
        // literal text.
        let mut d = InputDecoder::default();
        let evs = d.decode(b"a\x1b[2");
        assert!(matches!(evs.as_slice(), [DecodedInput::Key(k)] if k.code == CtKeyCode::Char('a')));
        assert!(d.decode(b"00~hi\x1b[20").is_empty());
        let evs = d.decode(b"1~b");
        match evs.as_slice() {
            [DecodedInput::Paste(text), DecodedInput::Key(k)] => {
                assert_eq!(text, "hi");
                assert_eq!(k.code, CtKeyCode::Char('b'));
            }
            _ => panic!("expected a paste then a key"),
        }
    }

    #[test]
    fn two_pastes_in_one_read_stay_separate() {
        let mut d = InputDecoder::default();
        let evs = d.decode(b"\x1b[200~one\x1b[201~\x1b[200~two\x1b[201~");
        match evs.as_slice() {
            [DecodedInput::Paste(one), DecodedInput::Paste(two)] => {
                assert_eq!(one, "one");
                assert_eq!(two, "two");
            }
            _ => panic!("expected two paste events"),
        }
    }

    #[test]
    fn flush_resolves_a_held_marker_prefix_as_keys() {
        // A bare Esc is indistinguishable from a paste marker's first
        // byte until the stream goes idle; flush delivers it as the Esc
        // key it was.
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b").is_empty());
        let evs = d.flush();
        assert!(matches!(evs.as_slice(), [DecodedInput::Key(k)] if k.code == CtKeyCode::Esc));
        // Idle with nothing held stays silent.
        assert!(d.flush().is_empty());
    }

    #[test]
    fn flush_never_releases_an_unterminated_paste() {
        // An in-flight paste survives idle: its content is delivered
        // complete once the end marker arrives, or not at all.
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b[200~held\n").is_empty());
        assert!(d.flush().is_empty());
        let evs = d.decode(b"tight\x1b[201~");
        assert_eq!(paste_of(&evs), "held\ntight");
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
