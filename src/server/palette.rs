//! The colors lux draws its own interface in, chosen as one named set so
//! no config can produce a half-themed chrome. Terminal content keeps the
//! host terminal's palette.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;

use crate::server::agent::Status;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
    pub working: Color,
    pub blocked: Color,
    pub done: Color,
    pub idle: Color,
    /// Focused chrome: the bright rule, the active tab's name, hint keys.
    pub text: Color,
    /// Secondary text: the status line, minimized titles, messages.
    pub muted: Color,
    /// Unfocused chrome, inactive tabs, borders, and separators.
    pub dim: Color,
    /// A hovered window control.
    pub bright: Color,
    /// Session names and list highlights.
    pub accent: Color,
    /// The yank star and the scroll label.
    pub mark: Color,
    /// The grid's capture label.
    pub capture: Color,
    pub chrome_bg: Color,
    pub suggestion_bg: Color,
    pub selection_bg: Color,
    /// Stand-ins for the terminal's defaults where darkening needs a value.
    pub fg: Color,
    pub bg: Color,
}

impl Palette {
    pub const DEFAULT: Palette = Palette {
        working: Color::Yellow,
        blocked: Color::Red,
        done: Color::Green,
        idle: Color::DarkGray,
        text: Color::Reset,
        muted: Color::Gray,
        dim: Color::DarkGray,
        bright: Color::White,
        accent: Color::Green,
        mark: Color::Yellow,
        capture: Color::Cyan,
        chrome_bg: Color::Indexed(235),
        suggestion_bg: Color::Indexed(236),
        selection_bg: Color::Indexed(240),
        fg: Color::Rgb(204, 204, 204),
        bg: Color::Rgb(0, 0, 0),
    };

    pub fn named(name: &str) -> Option<Palette> {
        match name {
            "default" => Some(Self::DEFAULT),
            _ => None,
        }
    }

    pub fn status(&self, status: Status) -> Color {
        match status {
            Status::Working => self.working,
            Status::Blocked => self.blocked,
            Status::Done => self.done,
            Status::Idle => self.idle,
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Brightness kept by an unfocused window's cells.
pub const DIM: f32 = 0.6;
/// Brightness kept by the cells under a popover's shadow.
pub const SHADOW: f32 = 0.5;

/// One of the colors a terminal is asked about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorSlot {
    Foreground,
    Background,
    Ansi(u8),
}

/// What the attached terminal answered about its own colors, `None` for
/// each it has not answered.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TermColors {
    pub fg: Option<(u8, u8, u8)>,
    pub bg: Option<(u8, u8, u8)>,
    pub ansi: [Option<(u8, u8, u8)>; 16],
}

impl TermColors {
    pub fn set(&mut self, slot: ColorSlot, rgb: (u8, u8, u8)) {
        match slot {
            ColorSlot::Foreground => self.fg = Some(rgb),
            ColorSlot::Background => self.bg = Some(rgb),
            ColorSlot::Ansi(i) => {
                if let Some(entry) = self.ansi.get_mut(usize::from(i)) {
                    *entry = Some(rgb);
                }
            }
        }
    }

    /// Concrete channels for a color, taking an ANSI color from the
    /// terminal's answer where it gave one; `None` for the terminal default.
    pub fn rgb(&self, color: Color) -> Option<(u8, u8, u8)> {
        index(color)
            .and_then(|i| self.ansi.get(usize::from(i)).copied().flatten())
            .or_else(|| rgb(color))
    }
}

/// Concrete channels for a color, or `None` for the terminal default.
pub fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Reset => None,
        _ => index(color).map(xterm),
    }
}

/// A color's place in the 256-color table.
fn index(color: Color) -> Option<u8> {
    Some(match color {
        Color::Rgb(..) | Color::Reset => return None,
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(i) => i,
    })
}

/// xterm's 256-color table.
fn xterm(index: u8) -> (u8, u8, u8) {
    const ANSI: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => ANSI[index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
            (level(i / 36), level(i / 6 % 6), level(i % 6))
        }
        232..=255 => {
            let grey = 8 + 10 * (index - 232);
            (grey, grey, grey)
        }
    }
}

/// `color` at `factor` of its brightness, with the terminal default read
/// as `default`.
pub fn darken(color: Color, default: Color, colors: &TermColors, factor: f32) -> Color {
    let (r, g, b) = colors
        .rgb(color)
        .or_else(|| colors.rgb(default))
        .unwrap_or((170, 170, 170));
    let scale = |c: u8| (c as f32 * factor) as u8;
    Color::Rgb(scale(r), scale(g), scale(b))
}

/// Darkens every cell in `rect`.
pub fn shade(buf: &mut Buffer, rect: Rect, palette: &Palette, colors: &TermColors, factor: f32) {
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            shade_cell(buf, Position::new(x, y), palette, colors, factor);
        }
    }
}

/// The shadow `panel` casts one cell down and right, clipped to `area`.
pub fn shadow(buf: &mut Buffer, panel: Rect, area: Rect, palette: &Palette, colors: &TermColors) {
    let cast = Rect::new(
        panel.x.saturating_add(1),
        panel.y.saturating_add(1),
        panel.width,
        panel.height,
    )
    .intersection(area);
    for y in cast.top()..cast.bottom() {
        for x in cast.left()..cast.right() {
            let pos = Position::new(x, y);
            if !panel.contains(pos) {
                shade_cell(buf, pos, palette, colors, SHADOW);
            }
        }
    }
}

/// The terminal's defaults are what it reported, else the palette's
/// stand-ins.
fn shade_cell(
    buf: &mut Buffer,
    pos: Position,
    palette: &Palette,
    colors: &TermColors,
    factor: f32,
) {
    let Some(cell) = buf.cell_mut(pos) else {
        return;
    };
    let fg = colors
        .fg
        .map_or(palette.fg, |(r, g, b)| Color::Rgb(r, g, b));
    let bg = colors
        .bg
        .map_or(palette.bg, |(r, g, b)| Color::Rgb(r, g, b));
    cell.fg = darken(cell.fg, fg, colors, factor);
    cell.bg = darken(cell.bg, bg, colors, factor);
    // Reset means "same as the foreground", which is already darkened.
    if cell.underline_color != Color::Reset {
        cell.underline_color = darken(cell.underline_color, fg, colors, factor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_default_palette_is_named() {
        assert_eq!(Palette::named("default"), Some(Palette::DEFAULT));
        assert_eq!(Palette::named("solarized"), None);
    }

    #[test]
    fn status_colors_come_from_the_palette() {
        let p = Palette::DEFAULT;
        assert_eq!(p.status(Status::Working), p.working);
        assert_eq!(p.status(Status::Blocked), p.blocked);
        assert_eq!(p.status(Status::Done), p.done);
        assert_eq!(p.status(Status::Idle), p.idle);
    }

    #[test]
    fn indexed_colors_follow_the_xterm_table() {
        assert_eq!(rgb(Color::Indexed(1)), Some((205, 0, 0)));
        assert_eq!(rgb(Color::Indexed(16)), Some((0, 0, 0)));
        assert_eq!(rgb(Color::Indexed(21)), Some((0, 0, 255)));
        assert_eq!(rgb(Color::Indexed(196)), Some((255, 0, 0)));
        assert_eq!(rgb(Color::Indexed(231)), Some((255, 255, 255)));
        assert_eq!(rgb(Color::Indexed(232)), Some((8, 8, 8)));
        assert_eq!(rgb(Color::Indexed(255)), Some((238, 238, 238)));
        assert_eq!(rgb(Color::Indexed(235)), Some((38, 38, 38)));
        assert_eq!(rgb(Color::White), Some((255, 255, 255)));
        assert_eq!(rgb(Color::Reset), None);
    }

    #[test]
    fn darkening_resolves_the_default_first() {
        let none = TermColors::default();
        assert_eq!(
            darken(Color::Rgb(200, 100, 50), Color::Black, &none, 0.5),
            Color::Rgb(100, 50, 25)
        );
        assert_eq!(
            darken(Color::Reset, Color::Rgb(200, 200, 200), &none, 0.5),
            Color::Rgb(100, 100, 100)
        );
    }

    #[test]
    fn reported_ansi_colors_replace_the_xterm_table() {
        let mut colors = TermColors::default();
        assert_eq!(colors.rgb(Color::Green), Some((0, 205, 0)));
        colors.set(ColorSlot::Ansi(2), (100, 200, 100));
        assert_eq!(colors.rgb(Color::Green), Some((100, 200, 100)));
        assert_eq!(colors.rgb(Color::Indexed(2)), Some((100, 200, 100)));
        assert_eq!(colors.rgb(Color::Red), Some((205, 0, 0)));
        assert_eq!(colors.rgb(Color::Indexed(196)), Some((255, 0, 0)));
        assert_eq!(colors.rgb(Color::Reset), None);
        assert_eq!(
            darken(Color::Green, Color::Black, &colors, 0.5),
            Color::Rgb(50, 100, 50)
        );
        // Only the sixteen ANSI colors are kept.
        colors.set(ColorSlot::Ansi(16), (1, 1, 1));
        assert_eq!(colors.rgb(Color::Indexed(16)), Some((0, 0, 0)));
    }

    #[test]
    fn shade_darkens_every_cell_and_resolves_defaults() {
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf.cell_mut(Position::new(1, 0)).unwrap().fg = Color::Rgb(100, 100, 100);
        let none = TermColors::default();
        shade(&mut buf, area, &Palette::DEFAULT, &none, 0.5);
        let p = Palette::DEFAULT;
        assert_eq!(
            buf.cell(Position::new(0, 0)).unwrap().fg,
            darken(p.fg, p.fg, &none, 0.5)
        );
        assert_eq!(
            buf.cell(Position::new(0, 0)).unwrap().bg,
            darken(p.bg, p.bg, &none, 0.5)
        );
        assert_eq!(
            buf.cell(Position::new(1, 0)).unwrap().fg,
            Color::Rgb(50, 50, 50)
        );
    }

    #[test]
    fn shade_darkens_the_reported_defaults_over_the_stand_ins() {
        let area = Rect::new(0, 0, 1, 1);
        let mut buf = Buffer::empty(area);
        buf.cell_mut(Position::new(0, 0)).unwrap().underline_color = Color::Reset;
        let mut colors = TermColors::default();
        colors.set(ColorSlot::Foreground, (100, 200, 100));
        colors.set(ColorSlot::Background, (0, 20, 0));
        shade(&mut buf, area, &Palette::DEFAULT, &colors, 0.5);
        let cell = buf.cell(Position::new(0, 0)).unwrap();
        assert_eq!(cell.fg, Color::Rgb(50, 100, 50));
        assert_eq!(cell.bg, Color::Rgb(0, 10, 0));
        assert_eq!(cell.underline_color, Color::Reset);
    }

    #[test]
    fn shadow_covers_the_offset_edge_and_clips_to_the_area() {
        let area = Rect::new(0, 0, 5, 4);
        let mut buf = Buffer::empty(area);
        for cell in &mut buf.content {
            cell.fg = Color::Rgb(100, 100, 100);
        }
        let none = TermColors::default();
        // A 3x2 panel at the origin casts on column 3 (rows 1-2) and row 2
        // (columns 1-3).
        shadow(
            &mut buf,
            Rect::new(0, 0, 3, 2),
            area,
            &Palette::DEFAULT,
            &none,
        );
        let shaded = |x, y| buf.cell(Position::new(x, y)).unwrap().fg == Color::Rgb(50, 50, 50);
        assert!(shaded(3, 1) && shaded(3, 2));
        assert!(shaded(1, 2) && shaded(2, 2));
        assert!(
            !shaded(0, 0) && !shaded(2, 1),
            "the panel itself is untouched"
        );
        assert!(!shaded(0, 2), "nothing left of the panel's shadow");
        assert!(!shaded(4, 1) && !shaded(3, 3));

        // Flush against the right and bottom edges, the shadow is clipped.
        let mut buf = Buffer::empty(area);
        for cell in &mut buf.content {
            cell.fg = Color::Rgb(100, 100, 100);
        }
        shadow(
            &mut buf,
            Rect::new(2, 2, 3, 2),
            area,
            &Palette::DEFAULT,
            &none,
        );
        assert!(
            buf.content
                .iter()
                .all(|c| c.fg == Color::Rgb(100, 100, 100))
        );
    }
}
