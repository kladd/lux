//! One-shot transitions: bounded effects that run once over the rendered
//! frame and remove themselves when done.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use tachyonfx::{Effect, EffectTimer, Interpolation, RefCount, fx, ref_count};

use crate::server::layout::{Side, SplitKind, WindowId};
use crate::server::palette::{self, Palette, TermColors};

const DIM_FADE: (u32, Interpolation) = (300, Interpolation::QuadOut);
const SLIDE_IN: (u32, Interpolation) = (200, Interpolation::QuadOut);
const SLIDE_OUT: (u32, Interpolation) = (200, Interpolation::QuadIn);
const ZOOM: (u32, Interpolation) = (200, Interpolation::QuadOut);
const MATERIALIZE: (u32, Interpolation) = (400, Interpolation::QuadOut);

/// A buffer a transition draws a window from: rendered afresh each frame
/// while the window slides in or grows, captured once when it leaves or
/// shrinks.
pub type Frame = RefCount<Buffer>;

pub struct Slide {
    pub window: WindowId,
    pub frame: Frame,
    effect: Effect,
}

pub struct Zoom {
    pub window: WindowId,
    from: Rect,
    to: Rect,
    frame: Frame,
    /// The frame is re-rendered every frame rather than a snapshot.
    pub live: bool,
    effect: Effect,
}

impl Zoom {
    /// The window's rectangle at this point of the transition.
    pub fn rect(&self) -> Rect {
        let alpha = self.effect.timer().map_or(1.0, |t| t.alpha());
        lerp(self.from, self.to, alpha)
    }
}

#[derive(Default)]
pub struct Transitions {
    dims: Vec<(WindowId, Effect)>,
    slides: Vec<Slide>,
    departures: Vec<Effect>,
    zoom: Option<Zoom>,
    materialize: Option<Effect>,
    last: Option<Instant>,
}

impl Transitions {
    pub fn running(&self) -> bool {
        !self.dims.is_empty()
            || !self.slides.is_empty()
            || !self.departures.is_empty()
            || self.zoom.is_some()
            || self.materialize.is_some()
    }

    /// Advances every effect by the time since the last frame.
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

    /// Drops finished effects after their last frame, returning whether any
    /// ended.
    pub fn prune(&mut self) -> bool {
        let before = self.count();
        self.dims.retain(|(_, e)| !e.done());
        self.slides.retain(|s| !s.effect.done());
        self.departures.retain(|e| !e.done());
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
        self.dims.len()
            + self.slides.len()
            + self.departures.len()
            + self.zoom.iter().count()
            + self.materialize.iter().count()
    }

    fn effects_mut(&mut self) -> impl Iterator<Item = &mut Effect> {
        self.dims
            .iter_mut()
            .map(|(_, e)| e)
            .chain(self.slides.iter_mut().map(|s| &mut s.effect))
            .chain(self.departures.iter_mut())
            .chain(self.zoom.iter_mut().map(|z| &mut z.effect))
            .chain(self.materialize.iter_mut())
    }

    /// The clock only runs while something is in flight, so a transition
    /// never starts with a stale delta.
    fn start(&mut self) {
        if !self.running() {
            self.last = Some(Instant::now());
        }
    }

    /// Fades `window` from full brightness to the dimmed shade.
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

    /// Slides `window`, the second half of a fresh split, in from the far
    /// edge.
    pub fn slide_in(&mut self, window: WindowId, kind: SplitKind) {
        self.start();
        self.slides.retain(|s| s.window != window);
        let frame = ref_count(Buffer::empty(Rect::default()));
        let effect = slide(frame.clone(), kind, Side::Second, true, timer(SLIDE_IN));
        self.slides.push(Slide {
            window,
            frame,
            effect,
        });
    }

    /// Slides a removed window's last frame out of its rectangle, away
    /// from the sibling that takes its space.
    pub fn slide_out(&mut self, snapshot: Buffer, kind: SplitKind, side: Side) {
        self.start();
        let effect = slide(ref_count(snapshot), kind, side, false, timer(SLIDE_OUT));
        self.departures.push(effect);
    }

    /// Animates `window`'s rectangle from `from` to `to`. With a snapshot
    /// the window shrinks showing its last frame; without one it grows
    /// showing its live content rendered at `to`.
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

    /// The buffer `window` renders into this frame instead of the screen,
    /// while a transition draws it from there.
    pub fn live(&self, window: WindowId) -> Option<Frame> {
        if let Some(zoom) = &self.zoom
            && zoom.window == window
            && zoom.live
        {
            return Some(zoom.frame.clone());
        }
        self.slides
            .iter()
            .find(|s| s.window == window)
            .map(|s| s.frame.clone())
    }

    /// Drops everything pinned to a window that no longer exists.
    pub fn forget(&mut self, window: WindowId) {
        self.undim(window);
        self.slides.retain(|s| s.window != window);
        if self.zoom.as_ref().is_some_and(|z| z.window == window) {
            self.zoom = None;
        }
    }

    /// Draws every departing, sliding, and zooming window over the frame.
    pub fn overlay(&mut self, buf: &mut Buffer) {
        let area = buf.area;
        for effect in &mut self.departures {
            effect.process(Duration::ZERO, buf, area);
        }
        for slide in &mut self.slides {
            slide.effect.process(Duration::ZERO, buf, area);
        }
        if let Some(zoom) = &mut self.zoom {
            zoom.effect.process(Duration::ZERO, buf, area);
        }
    }

    /// Reveals the next frames cell by cell, from blank to fully drawn.
    pub fn materialize(&mut self) {
        self.start();
        self.materialize = Some(fx::coalesce_from(Style::reset(), timer(MATERIALIZE)));
    }

    pub fn materializing(&self) -> bool {
        self.materialize.is_some()
    }

    /// Blanks the cells that haven't materialized yet. Runs over the
    /// finished frame, chrome included, so it comes after everything else.
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

/// Draws `frame` shifted along the split axis: fully off its rectangle
/// toward `side`'s edge at one end of the timer, in place at the other.
fn slide(frame: Frame, kind: SplitKind, side: Side, entering: bool, timer: EffectTimer) -> Effect {
    fx::effect_fn_buf((), timer, move |_, ctx, buf| {
        let frame = frame.borrow();
        let rect = frame.area;
        let out = if entering {
            1.0 - ctx.alpha()
        } else {
            ctx.alpha()
        };
        let sign = match side {
            Side::First => -1.0,
            Side::Second => 1.0,
        };
        let (dx, dy) = match kind {
            SplitKind::SideBySide => ((f32::from(rect.width) * out * sign).round() as i32, 0),
            SplitKind::Stacked => (0, (f32::from(rect.height) * out * sign).round() as i32),
        };
        blit(&frame, buf, rect, dx, dy);
    })
}

/// Copies `src` into `buf` shifted by (`dx`, `dy`), keeping only what lands
/// inside `within`.
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
    use ratatui::style::Color;

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

    fn advance(t: &mut Transitions, ms: u64) {
        let last = t.last.expect("running");
        t.tick(last + Duration::from_millis(ms));
    }

    #[test]
    fn a_new_split_window_slides_in_from_the_far_edge() {
        let screen = Rect::new(0, 0, 8, 1);
        let mut t = Transitions::default();
        t.slide_in(1, SplitKind::SideBySide);
        let rect = Rect::new(4, 0, 4, 1);
        *t.live(1).unwrap().borrow_mut() = filled(rect, 'x');
        let mut buf = Buffer::empty(screen);
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "        ", "starts fully off the right edge");
        advance(&mut t, 100);
        let mut buf = Buffer::empty(screen);
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "     xxx", "never spills left of its rect");
        advance(&mut t, 200);
        let mut buf = Buffer::empty(screen);
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "    xxxx");
        assert!(t.prune());
        assert!(!t.running());
    }

    #[test]
    fn a_removed_window_slides_out_away_from_its_sibling() {
        let screen = Rect::new(0, 0, 4, 4);
        let mut t = Transitions::default();
        t.slide_out(
            filled(Rect::new(0, 0, 4, 2), 'a'),
            SplitKind::Stacked,
            Side::First,
        );
        advance(&mut t, 100);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert_eq!(row(&buf, 0), "aaaa", "top half exits upward");
        assert_eq!(row(&buf, 1), "....");
        assert_eq!(row(&buf, 2), "....", "never spills onto the sibling");
        advance(&mut t, 200);
        let mut buf = filled(screen, '.');
        t.overlay(&mut buf);
        assert!((0..4).all(|y| row(&buf, y) == "...."));
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
        t.materialize();
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
    fn forgetting_a_window_drops_everything_pinned_to_it() {
        let mut t = Transitions::default();
        t.dim(1, Palette::DEFAULT, TermColors::default());
        t.slide_in(1, SplitKind::Stacked);
        t.zoom(1, Rect::new(0, 0, 1, 1), Rect::new(0, 0, 2, 2), None);
        t.slide_out(
            Buffer::empty(Rect::new(0, 0, 1, 1)),
            SplitKind::Stacked,
            Side::Second,
        );
        t.forget(1);
        assert!(t.dim_mut(1).is_none() && t.live(1).is_none() && t.zoom_state().is_none());
        assert!(t.running(), "a departure belongs to no window");
    }
}
