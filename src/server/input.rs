//! Decodes a client's raw terminal input into crossterm events.

use ratatui::crossterm::event::{
    KeyCode as CtKeyCode, KeyEvent, KeyModifiers as CtMods, MouseButton as CtMouseButton,
    MouseEvent as CtMouseEvent, MouseEventKind as CtMouseKind,
};
use termwiz::input::{
    InputEvent, InputParser, KeyCode as TwKey, KeyEvent as TwKeyEvent, Modifiers as TwMods,
    MouseButtons as TwButtons, MouseEvent as TwMouseEvent,
};

use crate::server::palette::ColorSlot;

pub enum DecodedInput {
    Key(KeyEvent),
    Mouse(CtMouseEvent),
    Paste(String),
    /// The terminal's answer to a color query.
    Color(ColorSlot, (u8, u8, u8)),
}

/// Pasted text up to the first control character, since a pasted newline
/// would submit a single-line prompt.
pub fn prompt_paste(text: &str) -> &str {
    let end = text.find(char::is_control).unwrap_or(text.len());
    &text[..end]
}

pub struct InputDecoder {
    parser: InputParser,
    /// Buttons held at the last mouse event, since termwiz reports carry
    /// no press or release state.
    buttons: TwButtons,
    /// Bytes carried to the next read: an unfinished paste, a partial
    /// paste-start marker, or a cut-off color answer.
    held: Vec<u8>,
    in_paste: bool,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Longer than any color answer, so a stray OSC prefix is not held for
/// long.
const OSC_LIMIT: usize = 512;

enum Marker {
    Full(usize),
    /// A prefix of the marker runs from this offset to the end of the input.
    Partial(usize),
    None,
}

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
    /// A paste arrives as one event once its end marker does, however the
    /// reads are chunked.
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
                    _ => {
                        self.held = rest.to_vec();
                        return out;
                    }
                }
            } else {
                match find_marker(rest, PASTE_START) {
                    Marker::Full(pos) => {
                        self.keys(&rest[..pos], &mut out, false);
                        self.in_paste = true;
                        rest = &rest[pos + PASTE_START.len()..];
                    }
                    // Hold the fragment until the next read or an idle
                    // flush resolves it. The bytes before it may be a
                    // color answer whose terminator the fragment begins.
                    Marker::Partial(pos) => {
                        self.keys(&rest[..pos], &mut out, true);
                        self.held.extend_from_slice(&rest[pos..]);
                        return out;
                    }
                    Marker::None => {
                        self.keys(rest, &mut out, true);
                        return out;
                    }
                }
            }
        }
    }

    /// Call when the stream goes idle: a held fragment was really keys,
    /// such as a bare Esc. An unfinished paste stays held.
    pub fn flush(&mut self) -> Vec<DecodedInput> {
        let mut out = Vec::new();
        if !self.in_paste && !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            self.keys(&held, &mut out, false);
        }
        out
    }

    /// The terminal answers color queries with OSC, which no key encoding
    /// uses, so answers are lifted out before the key parser runs. With
    /// `may_continue`, an answer cut off by the end of the read is held
    /// for the next one.
    fn keys(&mut self, bytes: &[u8], out: &mut Vec<DecodedInput>, may_continue: bool) {
        let mut plain = Vec::with_capacity(bytes.len());
        let mut rest = bytes;
        while let Some(start) = rest.windows(2).position(|w| w == b"\x1b]") {
            plain.extend_from_slice(&rest[..start]);
            let osc = &rest[start..];
            match osc_end(osc) {
                Osc::Complete(body_end, len) => {
                    color_answers(&osc[2..body_end], out);
                    rest = &osc[len..];
                }
                Osc::Incomplete if may_continue && osc.len() < OSC_LIMIT => {
                    self.held = osc.to_vec();
                    rest = &[];
                }
                Osc::Incomplete => {
                    plain.extend_from_slice(osc);
                    rest = &[];
                }
                Osc::Aborted => {
                    plain.extend_from_slice(&osc[..2]);
                    rest = &osc[2..];
                }
            }
        }
        plain.extend_from_slice(rest);
        self.plain_keys(&plain, out);
    }

    /// termwiz turns LF into Enter, so LF is split out here to keep Ctrl-J
    /// distinct from Enter.
    fn plain_keys(&mut self, bytes: &[u8], out: &mut Vec<DecodedInput>) {
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

enum Osc {
    /// Where the body ends and the full length, terminator included.
    Complete(usize, usize),
    /// Cut off by the end of the read.
    Incomplete,
    /// An ESC inside the body: Alt-] followed by another sequence.
    Aborted,
}

/// Where the OSC starting at `osc[0]` ends: at BEL or ESC \.
fn osc_end(osc: &[u8]) -> Osc {
    let mut i = 2;
    while i < osc.len() {
        match osc[i] {
            0x07 => return Osc::Complete(i, i + 1),
            0x1b => {
                return match osc.get(i + 1) {
                    Some(b'\\') => Osc::Complete(i, i + 2),
                    Some(_) => Osc::Aborted,
                    None => Osc::Incomplete,
                };
            }
            _ => i += 1,
        }
    }
    Osc::Incomplete
}

/// The colors in one answer: `10;spec`, `11;spec`, or `4;n;spec` pairs.
fn color_answers(body: &[u8], out: &mut Vec<DecodedInput>) {
    let Ok(body) = std::str::from_utf8(body) else {
        return;
    };
    let mut fields = body.split(';');
    let slot = match fields.next() {
        Some("10") => ColorSlot::Foreground,
        Some("11") => ColorSlot::Background,
        Some("4") => {
            while let (Some(n), Some(spec)) = (fields.next(), fields.next()) {
                if let (Ok(n), Some(rgb)) = (n.parse(), parse_color(spec)) {
                    out.push(DecodedInput::Color(ColorSlot::Ansi(n), rgb));
                }
            }
            return;
        }
        _ => return,
    };
    if let Some(rgb) = fields.next().and_then(parse_color) {
        out.push(DecodedInput::Color(slot, rgb));
    }
}

/// An X11 color spec: `rgb:R/G/B` or `#RGB`, with one to four hex digits
/// per channel.
fn parse_color(spec: &str) -> Option<(u8, u8, u8)> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        Some((channel(r)?, channel(g)?, channel(b)?))
    } else {
        let hex = spec.strip_prefix('#')?;
        let n = hex.len() / 3;
        if n == 0 || hex.len() != n * 3 {
            return None;
        }
        Some((
            channel(&hex[..n])?,
            channel(&hex[n..2 * n])?,
            channel(&hex[2 * n..])?,
        ))
    }
}

/// One to four hex digits, scaled to eight bits.
fn channel(hex: &str) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len())) - 1;
    Some((value * 255 / max) as u8)
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
        // termwiz may surface control bytes as raw chars. Normalize them to
        // crossterm's form so Ctrl bindings match.
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
        // 0x02 is Ctrl-B.
        let evs = keys(&mut d, b"\x02");
        assert_eq!(evs[0].code, CtKeyCode::Char('b'));
        assert!(evs[0].modifiers.contains(CtMods::CONTROL));
        // Enter arrives as CR in raw mode.
        let evs = keys(&mut d, b"\r");
        assert_eq!(evs[0].code, CtKeyCode::Enter);
        let evs = keys(&mut d, b"\x1b[A");
        assert_eq!(evs[0].code, CtKeyCode::Up);
    }

    #[test]
    fn shifted_arrows_keep_the_shift_modifier() {
        let mut d = InputDecoder::default();
        // CSI 1;2D is Shift-Left.
        let evs = keys(&mut d, b"\x1b[1;2D");
        assert_eq!(evs[0].code, CtKeyCode::Left);
        assert!(evs[0].modifiers.contains(CtMods::SHIFT));
        let evs = keys(&mut d, b"\x1b[D");
        assert_eq!(evs[0].code, CtKeyCode::Left);
        assert!(!evs[0].modifiers.contains(CtMods::SHIFT));
    }

    #[test]
    fn bs_and_del_both_decode_as_backspace() {
        let mut d = InputDecoder::default();
        assert_eq!(keys(&mut d, b"\x08")[0].code, CtKeyCode::Backspace);
        assert_eq!(keys(&mut d, b"\x7f")[0].code, CtKeyCode::Backspace);
    }

    #[test]
    fn lf_decodes_as_ctrl_j_not_enter() {
        let mut d = InputDecoder::default();
        let evs = keys(&mut d, b"\n");
        assert_eq!(evs[0].code, CtKeyCode::Char('j'));
        assert!(evs[0].modifiers.contains(CtMods::CONTROL));
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
    fn prompt_paste_stops_at_the_first_control_character() {
        assert_eq!(prompt_paste("plain text"), "plain text");
        assert_eq!(prompt_paste("first\nsecond"), "first");
        assert_eq!(prompt_paste("tab\there"), "tab");
        assert_eq!(prompt_paste("\nleading"), "");
        assert_eq!(prompt_paste(""), "");
    }

    #[test]
    fn paste_spanning_reads_arrives_as_one_event() {
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b[200~one").is_empty());
        assert!(d.decode(b"\ntwo").is_empty());
        let evs = d.decode(b"\x1b[201~");
        assert_eq!(paste_of(&evs), "one\ntwo");
    }

    #[test]
    fn paste_markers_split_across_reads_still_frame_the_paste() {
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
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b").is_empty());
        let evs = d.flush();
        assert!(matches!(evs.as_slice(), [DecodedInput::Key(k)] if k.code == CtKeyCode::Esc));
        assert!(d.flush().is_empty());
    }

    #[test]
    fn flush_never_releases_an_unterminated_paste() {
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
        // Left press at 1-based (5, 3).
        let m = ev(b"\x1b[<0;5;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::Down(CtMouseButton::Left));
        assert_eq!((m.column, m.row), (4, 2));
        // Code 32 is motion with left held.
        let m = ev(b"\x1b[<32;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::Drag(CtMouseButton::Left));
        // Lowercase m is a release.
        let m = ev(b"\x1b[<0;6;3m", &mut d);
        assert_eq!(m.kind, CtMouseKind::Up(CtMouseButton::Left));
        // Code 64 is wheel up.
        let m = ev(b"\x1b[<64;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::ScrollUp);
        // Code 65 is wheel down.
        let m = ev(b"\x1b[<65;6;3M", &mut d);
        assert_eq!(m.kind, CtMouseKind::ScrollDown);
    }

    fn colors(evs: &[DecodedInput]) -> Vec<(ColorSlot, (u8, u8, u8))> {
        evs.iter()
            .filter_map(|d| match d {
                DecodedInput::Color(slot, rgb) => Some((*slot, *rgb)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn color_answers_decode_and_never_reach_the_key_parser() {
        let mut d = InputDecoder::default();
        let evs = d.decode(b"a\x1b]10;rgb:8080/c0c0/8080\x1b\\b\x1b]11;rgb:00/10/00\x07");
        assert_eq!(
            colors(&evs),
            vec![
                (ColorSlot::Foreground, (128, 192, 128)),
                (ColorSlot::Background, (0, 16, 0))
            ]
        );
        let keys: Vec<_> = evs
            .iter()
            .filter_map(|d| match d {
                DecodedInput::Key(k) => Some(k.code),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![CtKeyCode::Char('a'), CtKeyCode::Char('b')]);

        let evs = d.decode(b"\x1b]4;2;rgb:6464/c8c8/6464;9;#ff0000\x1b\\");
        assert_eq!(
            colors(&evs),
            vec![
                (ColorSlot::Ansi(2), (100, 200, 100)),
                (ColorSlot::Ansi(9), (255, 0, 0))
            ]
        );
        assert_eq!(evs.len(), 2);
    }

    #[test]
    fn an_answer_split_across_reads_still_decodes() {
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b]10;rgb:ff").is_empty());
        assert!(d.decode(b"ff/ffff/ffff\x1b").is_empty());
        let evs = d.decode(b"\\c");
        assert_eq!(colors(&evs), vec![(ColorSlot::Foreground, (255, 255, 255))]);
        assert!(
            matches!(evs.as_slice(), [_, DecodedInput::Key(k)] if k.code == CtKeyCode::Char('c'))
        );
    }

    #[test]
    fn an_unfinished_osc_resolves_as_keys_on_flush_or_another_escape() {
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b]").is_empty());
        let evs = d.flush();
        assert!(matches!(evs.as_slice(), [DecodedInput::Key(k)] if k.code == CtKeyCode::Char(']')));

        let evs = keys(&mut d, b"\x1b]\x1b[A");
        assert!(evs.iter().any(|k| k.code == CtKeyCode::Up));
        assert!(evs.iter().any(|k| k.code == CtKeyCode::Char(']')));
    }

    #[test]
    fn other_osc_answers_are_dropped() {
        let mut d = InputDecoder::default();
        assert!(d.decode(b"\x1b]52;c;Zm9v\x07").is_empty());
        assert!(d.decode(b"\x1b]10;bogus\x1b\\").is_empty());
    }

    #[test]
    fn color_specs_scale_to_eight_bits() {
        assert_eq!(parse_color("rgb:ffff/8080/0000"), Some((255, 128, 0)));
        assert_eq!(parse_color("rgb:c/8/0"), Some((204, 136, 0)));
        assert_eq!(parse_color("rgb:cc/88/00"), Some((204, 136, 0)));
        assert_eq!(parse_color("#ff8000"), Some((255, 128, 0)));
        assert_eq!(parse_color("#f80"), Some((255, 136, 0)));
        assert_eq!(parse_color("#ffff80800000"), Some((255, 128, 0)));
        assert_eq!(parse_color("rgb:ff/ff"), None);
        assert_eq!(parse_color("rgb:fffff/ff/ff"), None);
        assert_eq!(parse_color("#ff80"), None);
        assert_eq!(parse_color("red"), None);
    }
}
