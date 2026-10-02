//! One-shot effects drawn over the rendered frame and dropped when they
//! finish.

use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use tachyonfx::{Effect, EffectTimer, Interpolation, RefCount, fx, ref_count};

use crate::server::config::AttachStyle;
use crate::server::layout::WindowId;
use crate::server::palette::{self, Palette, TermColors};

const DIM_FADE: (u32, Interpolation) = (300, Interpolation::QuadOut);
const ZOOM: (u32, Interpolation) = (200, Interpolation::QuadOut);
const MATERIALIZE: (u32, Interpolation) = (400, Interpolation::QuadOut);
const RAIN: (u32, Interpolation) = (2000, Interpolation::Linear);
/// The share of the rain's run a column may wait before its stream starts.
const RAIN_STAGGER: f32 = 0.35;
/// The shares of the rain's run the fastest and slowest streams take to
/// pass down the screen.
const RAIN_FALL: (f32, f32) = (0.35, 0.6);
/// The shortest and longest trails behind a stream's head, in rows.
const RAIN_TRAIL: (f32, f32) = (8.0, 20.0);
/// The most decoy streams falling ahead of a column's revealing one.
const RAIN_DECOYS: u32 = 2;
/// How long a scrambled glyph holds before changing.
const RAIN_GLYPH_MS: f32 = 70.0;
/// The chance a revealed cell flashes bold in any glyph period.
const RAIN_FLICKER: f32 = 0.02;
/// Digits and symbols scrambled glyphs draw from besides half-width
/// katakana.
const RAIN_SYMBOLS: &[u8] = b"0123456789:.=*+-<>|\"";
const KATAKANA: (u32, u32) = (0xFF66, 56);

/// A buffer a transition draws a window from: live for a window growing,
/// a snapshot for one shrinking.
pub type Frame = RefCount<Buffer>;

pub struct Zoom {
    pub window: WindowId,
    from: Rect,
    to: Rect,
    frame: Frame,
    pub live: bool,
    effect: Effect,
}

impl Zoom {
    pub fn rect(&self) -> Rect {
        let alpha = self.effect.timer().map_or(1.0, |t| t.alpha());
        lerp(self.from, self.to, alpha)
    }
}

#[derive(Default)]
pub struct Transitions {
    dims: Vec<(WindowId, Effect)>,
    zoom: Option<Zoom>,
    materialize: Option<Effect>,
    last: Option<Instant>,
}

impl Transitions {
    pub fn running(&self) -> bool {
        !self.dims.is_empty() || self.zoom.is_some() || self.materialize.is_some()
    }

    pub fn tick(&mut self, now: Instant) {
        let delta = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last = Some(now);
        for effect in self.effects_mut() {
            if let Some(timer) = effect.timer_mut() {
                timer.process(delta);
            }
        }
    }

    pub fn prune(&mut self) -> bool {
        let before = self.count();
        self.dims.retain(|(_, e)| !e.done());
        if self.zoom.as_ref().is_some_and(|z| z.effect.done()) {
            self.zoom = None;
        }
        if self.materialize.as_ref().is_some_and(|e| e.done()) {
            self.materialize = None;
        }
        if !self.running() {
            self.last = None;
        }
        self.count() != before
    }

    fn count(&self) -> usize {
        self.dims.len() + self.zoom.iter().count() + self.materialize.iter().count()
    }

    fn effects_mut(&mut self) -> impl Iterator<Item = &mut Effect> {
        self.dims
            .iter_mut()
            .map(|(_, e)| e)
            .chain(self.zoom.iter_mut().map(|z| &mut z.effect))
            .chain(self.materialize.iter_mut())
    }

    /// The clock stops between transitions, so a new one never starts with
    /// a stale delta.
    fn start(&mut self) {
        if !self.running() {
            self.last = Some(Instant::now());
        }
    }

    pub fn dim(&mut self, window: WindowId, palette: Palette, colors: TermColors) {
        self.start();
        self.undim(window);
        let effect = fx::effect_fn_buf((), timer(DIM_FADE), move |_, ctx, buf| {
            let factor = 1.0 - (1.0 - palette::DIM) * ctx.alpha();
            palette::shade(buf, ctx.area, &palette, &colors, factor);
        });
        self.dims.push((window, effect));
    }

    pub fn undim(&mut self, window: WindowId) {
        self.dims.retain(|(id, _)| *id != window);
    }

    pub fn dim_mut(&mut self, window: WindowId) -> Option<&mut Effect> {
        self.dims
            .iter_mut()
            .find(|(id, _)| *id == window)
            .map(|(_, e)| e)
    }

    pub fn zoom(&mut self, window: WindowId, from: Rect, to: Rect, snapshot: Option<Buffer>) {
        self.start();
        let live = snapshot.is_none();
        let frame = ref_count(snapshot.unwrap_or_else(|| Buffer::empty(to)));
        let effect = {
            let frame = frame.clone();
            fx::effect_fn_buf((), timer(ZOOM), move |_, ctx, buf| {
                let frame = frame.borrow();
                let rect = lerp(from, to, ctx.alpha());
                let anchor = frame.area;
                blit(
                    &frame,
                    buf,
                    rect,
                    i32::from(rect.x) - i32::from(anchor.x),
                    i32::from(rect.y) - i32::from(anchor.y),
                );
            })
        };
        self.zoom = Some(Zoom {
            window,
            from,
            to,
            frame,
            live,
            effect,
        });
    }

    pub fn zoom_state(&self) -> Option<&Zoom> {
        self.zoom.as_ref()
    }

    /// The buffer `window` renders into instead of the screen while a
    /// transition draws it from there.
    pub fn live(&self, window: WindowId) -> Option<Frame> {
        self.zoom
            .as_ref()
            .filter(|z| z.window == window && z.live)
            .map(|z| z.frame.clone())
    }

    pub fn forget(&mut self, window: WindowId) {
        self.undim(window);
        if self.zoom.as_ref().is_some_and(|z| z.window == window) {
            self.zoom = None;
        }
    }

    pub fn overlay(&mut self, buf: &mut Buffer) {
        if let Some(zoom) = &mut self.zoom {
            let area = buf.area;
            zoom.effect.process(Duration::ZERO, buf, area);
        }
    }

    pub fn materialize(&mut self, style: AttachStyle, palette: Palette, colors: TermColors) {
        self.start();
        self.materialize = Some(match style {
            AttachStyle::Coalesce => fx::coalesce_from(Style::reset(), timer(MATERIALIZE)),
            AttachStyle::Rain => rain(palette, colors),
        });
    }

    pub fn materializing(&self) -> bool {
        self.materialize.is_some()
    }

    /// Runs last, over the finished frame, chrome included.
    pub fn reveal(&mut self, buf: &mut Buffer) {
        if let Some(effect) = &mut self.materialize {
            let area = buf.area;
            effect.process(Duration::ZERO, buf, area);
        }
    }
}

fn timer((ms, interpolation): (u32, Interpolation)) -> EffectTimer {
    EffectTimer::from_ms(ms, interpolation)
}

/// A stream of glyphs falling down one column: its head `head` rows below
/// the top, trailed by `trail` rows of glyphs.
struct Stream {
    delay: f32,
    head: f32,
    trail: f32,
}

impl Stream {
    /// `salt` picks the stream; it starts at most `latest` into the run.
    fn new(x: u16, salt: u32, latest: f32, alpha: f32, rows: f32) -> Self {
        let (fastest, slowest) = RAIN_FALL;
        let (shortest, longest) = RAIN_TRAIL;
        let delay = latest * noise(x, 0, salt);
        let fill = fastest + (slowest - fastest) * noise(x, 0, salt + 1);
        let trail = shortest + (longest - shortest) * noise(x, 0, salt + 2);
        Stream {
            delay,
            head: (alpha - delay) / fill * (rows + trail),
            trail,
        }
    }
}

/// Each column's cells stay in place while a stream falls down it: a
/// bright head, then a fading trail of changing glyphs that settle into
/// the cells beneath. Decoy streams fall ahead of it over blank cells.
fn rain(palette: Palette, colors: TermColors) -> Effect {
    let (ms, _) = RAIN;
    fx::effect_fn_buf((), timer(RAIN), move |_, ctx, buf| {
        let area = ctx.area;
        let alpha = ctx.alpha();
        let rows = f32::from(area.height);
        let period = alpha * ms as f32 / RAIN_GLYPH_MS;
        let accent = colors.rgb(palette.accent).unwrap_or((0, 205, 0));
        let bg = colors
            .bg
            .or_else(|| colors.rgb(palette.bg))
            .unwrap_or((0, 0, 0));
        let lead = mix(accent, (255, 255, 255), 0.8);
        let source = buf.clone();
        for x in area.left()..area.right() {
            let reveal = Stream::new(x, 0, RAIN_STAGGER, alpha, rows);
            let decoys: Vec<Stream> = (0..(noise(x, 0, 10) * (RAIN_DECOYS + 1) as f32) as u32)
                .map(|i| Stream::new(x, 20 + 3 * i, reveal.delay, alpha, rows))
                .collect();
            let glyph_color = |behind: f32, trail: f32| {
                if behind < 1.0 {
                    (lead, true)
                } else {
                    (mix(accent, bg, 0.85 * behind / trail), false)
                }
            };
            for y in area.top()..area.bottom() {
                let pos = Position::new(x, y);
                let row = f32::from(y - area.top());
                let scrambles = scrambles(&source, pos);
                let behind = reveal.head - row;
                if behind >= reveal.trail {
                    if alpha < 1.0 && noise(x, y, 2000 + period as u32) < RAIN_FLICKER {
                        buf[pos].modifier.insert(Modifier::BOLD);
                    }
                } else if behind > 0.0 {
                    let settle = reveal.trail * (0.3 + 0.6 * noise(x, y, 4));
                    if scrambles && behind < settle {
                        let (color, bold) = glyph_color(behind, reveal.trail);
                        draw_glyph(&mut buf[pos], glyph(x, y, period), color, bold);
                    } else {
                        let settled = ((behind - settle) / (reveal.trail - settle)).max(0.0);
                        dim_cell(buf, pos, &palette, &colors, 0.25 + 0.75 * settled);
                    }
                } else {
                    buf[pos].reset();
                    let decoy = decoys
                        .iter()
                        .map(|d| (d.head - row, d.trail))
                        .find(|&(behind, trail)| behind > 0.0 && behind < trail);
                    if let Some((behind, trail)) = decoy.filter(|_| scrambles) {
                        let (color, bold) = glyph_color(behind, trail);
                        draw_glyph(&mut buf[pos], glyph(x, y, period), color, bold);
                    }
                }
            }
        }
    })
}

/// Wide characters, and the cells they cover, keep their place in the
/// grid.
fn scrambles(buf: &Buffer, pos: Position) -> bool {
    let narrow = |pos: Position| Span::raw(buf[pos].symbol()).width() <= 1;
    narrow(pos) && (pos.x == buf.area.left() || narrow(Position::new(pos.x - 1, pos.y)))
}

fn draw_glyph(cell: &mut Cell, ch: char, color: Color, bold: bool) {
    cell.reset();
    cell.set_char(ch).set_fg(color);
    if bold {
        cell.modifier.insert(Modifier::BOLD);
    }
}

/// The glyph at a cell `period` glyph periods into the run; each cell
/// changes on its own beat.
fn glyph(x: u16, y: u16, period: f32) -> char {
    let step = (period + noise(x, y, 3)) as u32;
    let count = KATAKANA.1 as usize + RAIN_SYMBOLS.len();
    nth_glyph((noise(x, y, 1000 + step) * count as f32) as usize % count)
}

fn nth_glyph(i: usize) -> char {
    let (first, count) = KATAKANA;
    match RAIN_SYMBOLS.get(i.wrapping_sub(count as usize)) {
        Some(&symbol) => char::from(symbol),
        None => char::from_u32(first + i as u32).unwrap_or('0'),
    }
}

fn mix((r, g, b): (u8, u8, u8), (r2, g2, b2): (u8, u8, u8), t: f32) -> Color {
    let m = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t) as u8;
    Color::Rgb(m(r, r2), m(g, g2), m(b, b2))
}

/// A default background stays as it is, matching the blank cells below.
fn dim_cell(buf: &mut Buffer, pos: Position, palette: &Palette, colors: &TermColors, factor: f32) {
    let default_bg = buf.cell(pos).is_some_and(|cell| cell.bg == Color::Reset);
    palette::shade(buf, Rect::new(pos.x, pos.y, 1, 1), palette, colors, factor);
    if default_bg && let Some(cell) = buf.cell_mut(pos) {
        cell.bg = Color::Reset;
    }
}

/// In `0..1`, spread so neighboring cells land far apart, and unrelated
/// from one `salt` to the next.
fn noise(x: u16, y: u16, salt: u32) -> f32 {
    let mut hash = (u32::from(x) | u32::from(y) << 16).wrapping_mul(0x9E37_79B1)
        ^ salt.wrapping_mul(0x85EB_CA77);
    hash ^= hash >> 15;
    hash = hash.wrapping_mul(0x85EB_CA6B);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xC2B2_AE35);
    hash ^= hash >> 16;
    (hash >> 8) as f32 / (1u32 << 24) as f32
}

fn blit(src: &Buffer, buf: &mut Buffer, within: Rect, dx: i32, dy: i32) {
    for pos in src.area.positions() {
        let (x, y) = (i32::from(pos.x) + dx, i32::from(pos.y) + dy);
        let Ok(to) = u16::try_from(x).and_then(|x| u16::try_from(y).map(|y| Position::new(x, y)))
        else {
            continue;
        };
        if !within.contains(to) {
            continue;
        }
        if let Some(cell) = buf.cell_mut(to) {
            *cell = src[pos].clone();
        }
    }
}

fn lerp(from: Rect, to: Rect, t: f32) -> Rect {
    let mix = |a: u16, b: u16| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u16;
    Rect::new(
        mix(from.x, to.x),
        mix(from.y, to.y),
        mix(from.width, to.width),
        mix(from.height, to.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(area: Rect, ch: char) -> Buffer {
        let mut buf = Buffer::empty(area);
        for pos in area.positions() {
            buf[pos].set_char(ch);
        }
        buf
    }

    fn row(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf[Position::new(x, y)].symbol().chars().next().unwrap())
            .collect()
    }

    fn materialize(t: &mut Transitions, style: AttachStyle) {
        t.materialize(style, Palette::DEFAULT, TermColors::default());
    }

    /// Each cell's symbol names its row.
    fn lettered(area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        for pos in area.positions() {
            buf[pos].set_char(char::from_u32(0x41 + u32::from(pos.y)).unwrap());
        }
        buf
    }

    /// The row a lettered cell came from, if it isn't blank or a glyph.
    fn source_row(cell: &Cell) -> Option<u16> {
        let ch = cell.symbol().chars().next()?;
        (ch != ' ' && !is_glyph(cell)).then(|| (u32::from(ch) - 0x41) as u16)
    }

    fn is_glyph(cell: &Cell) -> bool {
        let count = KATAKANA.1 as usize + RAIN_SYMBOLS.len();
        let ch = cell.symbol().chars().next();
        (0..count).any(|i| Some(nth_glyph(i)) == ch)
    }

    /// Cells in each column showing their own content, asserting none
    /// sits below a blank one.
    fn revealed(buf: &Buffer) -> Vec<u16> {
        let area = buf.area;
        (area.left()..area.right())
            .map(|x| {
                let mut blank = false;
                let mut shown = 0;
                for y in area.top()..area.bottom() {
                    let cell = &buf[Position::new(x, y)];
                    if cell.symbol() == " " {
                        blank = true;
                    } else if source_row(cell) == Some(y) {
                        assert!(!blank, "{x},{y} revealed below a blank cell");
                        shown += 1;
                    } else {
                        assert!(is_glyph(cell), "{x},{y} is {:?}", cell.symbol());
                    }
                }
                shown
            })
            .collect()
    }

    fn advance(t: &mut Transitions, ms: u64) {
        let last = t.last.expect("running");
        t.tick(last + Duration::from_millis(ms));
    }

    #[test]
    fn a_zoom_grows_from_its_place_anchored_at_its_corner() {
        let screen = Rect::new(0, 0, 6, 4);
        let mut t = Transitions::default();
        t.zoom(1, Rect::new(3, 2, 3, 2), screen, None);
        let live = t.live(1).unwrap();
        let mut frame = Buffer::empty(screen);
        for pos in screen.positions() {
            frame[pos].set_char(char::from(b'0' + pos.y as u8));
        }
        *live.borrow_mut() = frame;
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 2), "...000", "the top row moves with the rect");
        assert_eq!(row(&buf, 3), "...111");
        assert_eq!(row(&buf, 0), "......");
        advance(&mut t, 500);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "000000");
        assert_eq!(row(&buf, 3), "333333");
    }

    #[test]
    fn a_zoom_shrinks_showing_its_snapshot() {
        let screen = Rect::new(0, 0, 6, 4);
        let mut t = Transitions::default();
        t.zoom(1, screen, Rect::new(3, 2, 3, 2), Some(filled(screen, 's')));
        assert!(t.live(1).is_none());
        advance(&mut t, 500);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "......");
        assert_eq!(row(&buf, 2), "...sss");
        assert_eq!(row(&buf, 3), "...sss");
    }

    #[test]
    fn the_dim_fade_ends_at_the_steady_shade() {
        let rect = Rect::new(0, 0, 2, 1);
        let mut t = Transitions::default();
        let colors = TermColors::default();
        t.dim(1, Palette::DEFAULT, colors);
        let mut buf = Buffer::empty(rect);
        buf[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        t.dim_mut(1)
            .unwrap()
            .process(Duration::ZERO, &mut buf, rect);
        assert_eq!(buf[Position::new(0, 0)].fg, Color::Rgb(100, 100, 100));
        advance(&mut t, 1000);
        let mut buf = Buffer::empty(rect);
        buf[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        t.dim_mut(1)
            .unwrap()
            .process(Duration::ZERO, &mut buf, rect);
        let mut steady = Buffer::empty(rect);
        steady[Position::new(0, 0)].fg = Color::Rgb(100, 100, 100);
        palette::shade(&mut steady, rect, &Palette::DEFAULT, &colors, palette::DIM);
        assert_eq!(buf, steady);
        t.undim(1);
        assert!(!t.running());
    }

    #[test]
    fn an_attaching_frame_materializes_cell_by_cell() {
        let screen = Rect::new(0, 0, 8, 4);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Coalesce);
        let mut buf = filled(screen, 'x');
        buf[Position::new(0, 0)].bg = Color::Red;
        t.reveal(&mut buf);
        assert!(
            screen
                .positions()
                .all(|p| buf[p].symbol() == " " && buf[p].bg == Color::Reset),
            "starts blank, backgrounds included"
        );
        advance(&mut t, 200);
        let mut buf = filled(screen, 'x');
        t.reveal(&mut buf);
        let shown = screen
            .positions()
            .filter(|&p| buf[p].symbol() == "x")
            .count();
        assert!(shown > 0 && shown < 32, "part way through: {shown} shown");
        advance(&mut t, 300);
        let mut buf = filled(screen, 'x');
        t.reveal(&mut buf);
        assert!(screen.positions().all(|p| buf[p].symbol() == "x"));
        assert!(t.prune());
        assert!(!t.running());
    }

    #[test]
    fn rain_reveals_each_column_behind_falling_glyphs() {
        let screen = Rect::new(0, 0, 8, 20);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let bg = |x: u16| {
            if x.is_multiple_of(2) {
                Color::Red
            } else {
                Color::Reset
            }
        };
        let painted = || {
            let mut buf = lettered(screen);
            for pos in screen.positions() {
                buf[pos].fg = Color::Green;
                buf[pos].bg = bg(pos.x);
            }
            buf
        };
        let mut buf = painted();
        t.reveal(&mut buf);
        assert!(
            screen
                .positions()
                .all(|p| buf[p].symbol() == " " && buf[p].bg == Color::Reset),
            "starts blank, backgrounds included"
        );
        advance(&mut t, 800);
        let mut buf = painted();
        t.reveal(&mut buf);
        let shown = revealed(&buf);
        assert!(shown.windows(2).any(|w| w[0] != w[1]), "columns stagger");
        let glyphs: Vec<&Cell> = screen
            .positions()
            .map(|p| &buf[p])
            .filter(|c| is_glyph(c))
            .collect();
        assert!(glyphs.len() > 1, "glyphs falling: {shown:?}");
        assert!(
            glyphs.iter().any(|c| c.modifier.contains(Modifier::BOLD)),
            "bold heads"
        );
        assert!(
            glyphs
                .iter()
                .all(|c| c.fg != Color::Green && c.bg == Color::Reset),
            "glyphs in the rain's colors"
        );
        advance(&mut t, 1500);
        let mut buf = painted();
        t.reveal(&mut buf);
        assert_eq!(buf, painted());
        assert!(t.prune());
        assert!(!t.running());
    }

    #[test]
    fn a_rain_head_falls_through_changing_glyphs() {
        let screen = Rect::new(0, 0, 1, 40);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let mut heads = Vec::new();
        let mut changed = false;
        let mut last = lettered(screen);
        for _ in 0..150 {
            advance(&mut t, 16);
            let mut buf = lettered(screen);
            t.reveal(&mut buf);
            revealed(&buf);
            let mut trail = (screen.top()..screen.bottom())
                .map(|y| Position::new(0, y))
                .filter(|&p| is_glyph(&buf[p]));
            heads.extend(
                trail
                    .clone()
                    .find(|&p| buf[p].modifier.contains(Modifier::BOLD)),
            );
            changed |= trail.any(|p| {
                !buf[p].modifier.contains(Modifier::BOLD)
                    && is_glyph(&last[p])
                    && !last[p].modifier.contains(Modifier::BOLD)
                    && last[p].symbol() != buf[p].symbol()
            });
            last = buf;
        }
        heads.dedup();
        assert!(heads.len() >= 3, "frames falling: {heads:?}");
        assert!(heads.is_sorted_by_key(|p| p.y), "moves down: {heads:?}");
        assert!(changed, "trail glyphs change in place");
    }

    #[test]
    fn rain_columns_reveal_at_different_speeds() {
        let screen = Rect::new(0, 0, 40, 40);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let mut at = |ms: u64| {
            advance(&mut t, ms);
            let mut buf = lettered(screen);
            t.reveal(&mut buf);
            revealed(&buf)
        };
        let (before, after) = (at(900), at(200));
        let risen: Vec<u16> = before
            .iter()
            .zip(&after)
            .filter(|&(&a, &b)| a > 0 && b < screen.height)
            .map(|(&a, &b)| b - a)
            .collect();
        assert!(risen.len() > 1, "columns part way down: {risen:?}");
        assert!(
            risen.iter().max() > risen.iter().min(),
            "rows revealed in the same time: {risen:?}"
        );
    }

    #[test]
    fn rain_leaves_wide_characters_unscrambled() {
        let screen = Rect::new(0, 0, 4, 30);
        let mut t = Transitions::default();
        materialize(&mut t, AttachStyle::Rain);
        let mut glyphs = [0; 4];
        for _ in 0..100 {
            advance(&mut t, 16);
            let mut buf = Buffer::empty(screen);
            for y in screen.top()..screen.bottom() {
                buf.set_string(0, y, "日ab", Style::default());
            }
            t.reveal(&mut buf);
            for pos in screen.positions() {
                glyphs[usize::from(pos.x)] += usize::from(is_glyph(&buf[pos]));
            }
        }
        assert_eq!(glyphs[..2], [0, 0], "glyphs over the wide cell");
        assert!(glyphs[2] > 0, "glyphs beside it");
    }

    #[test]
    fn forgetting_a_window_drops_everything_pinned_to_it() {
        let mut t = Transitions::default();
        t.dim(1, Palette::DEFAULT, TermColors::default());
        t.zoom(1, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 2, 2), None);
        t.forget(1);
        assert!(t.dim_mut(1).is_none() && t.live(1).is_none() && t.zoom_state().is_none());
        assert!(!t.running());
    }
}
