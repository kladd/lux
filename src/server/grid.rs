//! The CLAUDECOM grid: a pinned, non-attachable session
//! switcher entry showing a live, read-only tile for every tab across
//! every session currently identified as running Claude Code. The grid
//! owns no layout tree, windows, or PTYs — each tile crops an existing
//! tab's live engine content without touching that tab's real size, and
//! selecting a tile re-attaches the client to the tab's home session.

use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Widget};

use crate::server::anim::{self, Anim};
use crate::server::layout::{Dir, WindowId};
use crate::server::session::{Session, cell_style};
use crate::server::window::Tab;
use crate::server::{SessionId, clear_region};

/// The pinned switcher entry's display name.
pub const ENTRY_NAME: &str = "*CLAUDECOM*";

/// Fixed size of every grid tile — enough to show a recognizable slice
/// of a Claude Code tab. Tiles never grow past this to fill leftover
/// screen space; deriving their shape from the screen made them taller
/// and wider than reads well.
const TILE_COLS: u16 = 60;
const TILE_ROWS: u16 = 24;

/// A client's view of the grid: the highlighted tile and the first
/// visible tile row.
#[derive(Clone, Copy, Default)]
pub struct GridState {
    pub highlight: usize,
    scroll: usize,
}

/// One tile's target: a Claude Code tab addressed by its home session,
/// window, and position in that window's tab list.
#[derive(Clone, Copy)]
pub struct GridItem {
    pub session: SessionId,
    pub window: WindowId,
    pub tab: usize,
}

/// Every Claude Code tab across every session, ordered by home session
/// name, then window and tab position within the session — a stable
/// order, so a tile stays put as unrelated tabs elsewhere update.
pub fn items(sessions: &BTreeMap<SessionId, Session>) -> Vec<GridItem> {
    let mut by_name: Vec<(&str, SessionId)> = sessions
        .iter()
        .map(|(&sid, s)| (s.name.as_str(), sid))
        .collect();
    by_name.sort();
    by_name
        .into_iter()
        .flat_map(|(_, sid)| {
            sessions[&sid]
                .claude_tabs()
                .into_iter()
                .map(move |(window, tab)| GridItem {
                    session: sid,
                    window,
                    tab,
                })
        })
        .collect()
}

/// Tile geometry for `count` items in `area`: the largest number of
/// fixed-size tile columns that fit the screen width, leftover space
/// left blank. Excess items wrap into rows below; rows past the
/// screenful scroll.
struct Layout {
    cols: usize,
    tile_w: u16,
    tile_h: u16,
    /// Tile rows the items need in total.
    rows: usize,
    /// Whole tile rows the area fits.
    visible: usize,
}

fn layout(area: Rect, count: usize) -> Option<Layout> {
    if count == 0 || area.width < TILE_COLS || area.height < TILE_ROWS {
        return None;
    }
    let cols = (area.width / TILE_COLS) as usize;
    let rows = count.div_ceil(cols);
    let visible = ((area.height / TILE_ROWS) as usize).min(rows);
    Some(Layout {
        cols,
        tile_w: TILE_COLS,
        tile_h: TILE_ROWS,
        rows,
        visible,
    })
}

/// Move the highlight to the tile spatially adjacent in `dir`, if one
/// exists; at a grid edge the highlight stays put.
pub fn navigate(state: &mut GridState, area: Rect, count: usize, dir: Dir) {
    let Some(l) = layout(area, count) else {
        return;
    };
    let i = state.highlight.min(count - 1);
    let col = i % l.cols;
    let target = match dir {
        Dir::Left => (col > 0).then(|| i - 1),
        Dir::Right => (col + 1 < l.cols && i + 1 < count).then(|| i + 1),
        Dir::Up => i.checked_sub(l.cols),
        Dir::Down => (i + l.cols < count).then(|| i + l.cols),
    };
    state.highlight = target.unwrap_or(i);
}

/// Keep the highlighted tile's row inside the visible rows, carrying the
/// view along as the highlight moves past either edge.
fn ensure_visible(state: &mut GridState, l: &Layout) {
    let row = state.highlight / l.cols;
    state.scroll = state.scroll.min(l.rows - l.visible).min(row);
    if row >= state.scroll + l.visible {
        state.scroll = row + 1 - l.visible;
    }
}

/// Render the grid over `area`, first clamping `state` to the current
/// items and scrolling the highlighted tile's row into view.
pub fn render(
    buf: &mut Buffer,
    area: Rect,
    sessions: &BTreeMap<SessionId, Session>,
    state: &mut GridState,
) {
    let items = items(sessions);
    state.highlight = state.highlight.min(items.len().saturating_sub(1));
    match layout(area, items.len()) {
        Some(l) => ensure_visible(state, &l),
        None => state.scroll = 0,
    }
    draw(
        buf,
        area,
        sessions,
        &items,
        Some(state.highlight),
        state.scroll,
    );
}

/// Render the grid with no highlight or scroll, for the session
/// switcher's preview pane.
pub fn render_preview(buf: &mut Buffer, area: Rect, sessions: &BTreeMap<SessionId, Session>) {
    let items = items(sessions);
    draw(buf, area, sessions, &items, None, 0);
}

fn draw(
    buf: &mut Buffer,
    area: Rect,
    sessions: &BTreeMap<SessionId, Session>,
    items: &[GridItem],
    highlight: Option<usize>,
    scroll: usize,
) {
    clear_region(buf, area);
    if items.is_empty() {
        draw_empty(buf, area);
        return;
    }
    let Some(l) = layout(area, items.len()) else {
        return;
    };
    let elapsed = anim::elapsed();
    for (i, item) in items.iter().enumerate() {
        let row = i / l.cols;
        if row < scroll {
            continue;
        }
        if row >= scroll + l.visible {
            break;
        }
        let rect = Rect::new(
            area.x + (i % l.cols) as u16 * l.tile_w,
            area.y + (row - scroll) as u16 * l.tile_h,
            l.tile_w,
            l.tile_h,
        );
        let Some(session) = sessions.get(&item.session) else {
            continue;
        };
        let Some(tab) = session.tab_at(item.window, item.tab) else {
            continue;
        };
        draw_tile(buf, rect, &session.name, tab, highlight == Some(i), elapsed);
    }
}

/// The grid with no items left stays on screen rather than ejecting its
/// viewer; a dim notice marks it deliberate.
fn draw_empty(buf: &mut Buffer, area: Rect) {
    let msg = "no Claude Code tabs";
    let len = msg.chars().count() as u16;
    if area.width < len || area.height == 0 {
        return;
    }
    let x = area.x + (area.width - len) / 2;
    let y = area.y + area.height / 2;
    let style = Style::default().fg(Color::DarkGray);
    for (i, ch) in msg.chars().enumerate() {
        if let Some(dst) = buf.cell_mut(Position::new(x + i as u16, y)) {
            dst.set_char(ch);
            dst.set_style(style);
        }
    }
}

/// One tile: the tab's bracketed status text, home session name, and tab
/// name on a top chrome row, the tail of the tab's live content below,
/// cropped to the tile without touching the tab's real size. The
/// rightmost cell stays blank so adjacent tiles' content doesn't run
/// together. The highlighted tile is framed by a border, its chrome text
/// sitting on the top edge like a title so the border can't collide with
/// the status coloring.
fn draw_tile(
    buf: &mut Buffer,
    rect: Rect,
    session: &str,
    tab: &Tab,
    highlighted: bool,
    elapsed: Duration,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    // Chrome carries no background of its own: the text, status coloring,
    // and border glyphs sit on the terminal's default background.
    let base = Style::default();
    for x in rect.left()..rect.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, rect.y)) {
            dst.set_char(' ');
            dst.set_style(base);
        }
    }
    let content_h = rect.height - 1;
    let content_w = rect.width.saturating_sub(1);
    if content_h > 0 && content_w > 0 {
        // The tail of the live view, where fresh output lands.
        let screen = tab.engine.screen();
        let live_rows = screen.physical_rows as i64;
        let range = screen.phys_range(&((live_rows - content_h as i64).max(0)..live_rows));
        for (y, line) in screen.lines_in_phys_range(range).iter().enumerate() {
            if y >= content_h as usize {
                break;
            }
            for cell in line.visible_cells() {
                let cx = cell.cell_index();
                if cx >= content_w as usize {
                    break;
                }
                let pos = Position::new(rect.x + cx as u16, rect.y + 1 + y as u16);
                if let Some(dst) = buf.cell_mut(pos) {
                    dst.set_symbol(cell.str());
                    dst.set_style(cell_style(cell.attrs()));
                }
            }
        }
    }
    if highlighted {
        Block::bordered()
            .border_style(base.fg(HIGHLIGHT))
            .render(rect, buf);
    }
    // Chrome text last, over the highlighted tile's top edge, starting
    // inside its corner.
    let mut x = rect.x + highlighted as u16;
    let mut put = |x: &mut u16, ch: char, style: Style| -> bool {
        if *x + 1 >= rect.right() {
            return false;
        }
        if let Some(dst) = buf.cell_mut(Position::new(*x, rect.y)) {
            dst.set_char(ch);
            dst.set_style(style);
        }
        *x += 1;
        true
    };
    // The same status coloring and animation the tab's home tab bar shows.
    if let Some(tracker) = &tab.agent {
        let visual = tracker.visual();
        let len = visual.text.chars().count();
        for (j, ch) in visual.text.chars().enumerate() {
            let color = match visual.anim {
                Anim::None => visual.color,
                Anim::Shimmer => anim::shimmer(visual.color, j, len, elapsed),
                Anim::Breathe => anim::breathe(visual.color, elapsed),
            };
            if !put(&mut x, ch, base.fg(color)) {
                break;
            }
        }
        put(&mut x, ' ', base);
    }
    for ch in session.chars() {
        if !put(&mut x, ch, base.fg(Color::Gray)) {
            break;
        }
    }
    // The tab name after the session name, tmux-target style
    // (`session:tab`), dimmer so the session name stays dominant.
    put(&mut x, ':', base.fg(Color::DarkGray));
    for ch in tab.name.chars() {
        if !put(&mut x, ch, base.fg(Color::DarkGray)) {
            break;
        }
    }
}

/// The highlight border's color, matching the switcher's highlight.
const HIGHLIGHT: Color = Color::Green;

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn tiles_keep_their_fixed_size() {
        // 150 wide fits two 60-wide columns; tiles stay 60×24 however
        // much screen is left over and however many items there are.
        for count in [1, 3, 9] {
            let l = layout(area(150, 70), count).unwrap();
            assert_eq!((l.cols, l.tile_w, l.tile_h), (2, 60, 24));
        }
        // 300 wide fits five columns of the same fixed-size tiles.
        let l = layout(area(300, 150), 3).unwrap();
        assert_eq!((l.cols, l.tile_w, l.tile_h), (5, 60, 24));
        // Smaller than one tile lays out nothing.
        assert!(layout(area(59, 70), 3).is_none());
        assert!(layout(area(120, 23), 3).is_none());
        assert!(layout(area(120, 70), 0).is_none());
    }

    #[test]
    fn rows_wrap_the_overflow_and_cap_at_the_screenful() {
        // Two columns: three items need two rows, both fitting in 80
        // screen rows.
        let l = layout(area(150, 80), 3).unwrap();
        assert_eq!((l.rows, l.visible), (2, 2));
        // A single row of items shows just that row.
        let l = layout(area(150, 80), 2).unwrap();
        assert_eq!((l.rows, l.visible), (1, 1));
        // Ten items need five rows; only three fit.
        let l = layout(area(150, 80), 10).unwrap();
        assert_eq!((l.rows, l.visible), (5, 3));
    }

    #[test]
    fn navigation_moves_to_spatial_neighbors_and_holds_at_edges() {
        // 4 columns, 6 items: two rows, the second row partial.
        let a = area(240, 70);
        let mut s = GridState::default();
        navigate(&mut s, a, 6, Dir::Left);
        assert_eq!(s.highlight, 0, "left edge holds");
        navigate(&mut s, a, 6, Dir::Right);
        assert_eq!(s.highlight, 1);
        navigate(&mut s, a, 6, Dir::Down);
        assert_eq!(s.highlight, 5);
        navigate(&mut s, a, 6, Dir::Down);
        assert_eq!(s.highlight, 5, "no row below");
        navigate(&mut s, a, 6, Dir::Right);
        assert_eq!(s.highlight, 5, "no tile to the right in a partial row");
        navigate(&mut s, a, 6, Dir::Up);
        assert_eq!(s.highlight, 1);
        // Item 2 has no tile directly below it (the second row holds two).
        s.highlight = 2;
        navigate(&mut s, a, 6, Dir::Down);
        assert_eq!(s.highlight, 2);
    }

    #[test]
    fn scrolling_follows_the_highlight() {
        // 4 columns × 2 visible rows, 20 items: 5 rows.
        let l = layout(area(240, 70), 20).unwrap();
        assert_eq!((l.cols, l.rows, l.visible), (4, 5, 2));
        let mut s = GridState::default();
        s.highlight = 19; // last row
        ensure_visible(&mut s, &l);
        assert_eq!(s.scroll, 3, "view carried down to the highlight");
        s.highlight = 0;
        ensure_visible(&mut s, &l);
        assert_eq!(s.scroll, 0, "and back up");
    }
}
