//! The CLAUDECOM grid: a pinned, non-attachable session
//! switcher entry showing a live tile for every tab across
//! every session currently identified as running Claude Code. The grid
//! owns no layout tree, windows, or PTYs — each tile resizes an existing
//! tab's engine and PTY to its own interior so the content reflows to
//! fit rather than showing a garbled crop; the next direct render in the
//! tab's home window reconciles it back to its real size. Capturing a
//! tile routes input to the tab it shows.

use std::collections::BTreeMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::server::anim::{self, Anim};
use crate::server::layout::{Dir, WindowId};
use crate::server::session::{Session, cell_style};
use crate::server::window::{Tab, TabId};
use crate::server::{SessionId, clear_region};

/// The pinned switcher entry's display name.
pub const ENTRY_NAME: &str = "*CLAUDECOM*";

/// Every tile is this many rows tall and at least this many columns
/// wide — enough to show a recognizable slice of a Claude Code tab.
/// Deriving tile height from the screen made tiles taller than reads
/// well; width, unlike height, grows past the minimum to consume
/// whatever the packed columns leave unused.
const MIN_TILE_COLS: u16 = 60;
const TILE_ROWS: u16 = 24;

/// A client's view of the grid: the highlighted tile, the first visible
/// tile row, and the captured tab, if any.
#[derive(Clone, Copy, Default)]
pub struct GridState {
    pub highlight: usize,
    scroll: usize,
    /// The tab whose tile is in capture mode: key presses are routed to
    /// its PTY instead of grid navigation.
    pub capture: Option<TabId>,
    /// A prefix key held back pending its follow-up, which selects a
    /// grid command (exit capture, switcher, finder) or discards the
    /// sequence; in capture mode the prefix never reaches the tab.
    pub pending_prefix: bool,
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
/// minimum-width tile columns that fit the screen width, every tile
/// widened evenly to consume the remaining width. Excess items wrap
/// into rows below; rows past the screenful scroll.
struct Layout {
    cols: usize,
    /// Base tile width; the first `wide` columns are one cell wider so
    /// the columns consume the full width.
    tile_w: u16,
    wide: usize,
    tile_h: u16,
    /// Tile rows the items need in total.
    rows: usize,
    /// Whole tile rows the area fits.
    visible: usize,
}

impl Layout {
    /// The rectangle of the tile at grid column `col`, visible row `row`.
    fn tile_rect(&self, area: Rect, col: usize, row: usize) -> Rect {
        Rect::new(
            area.x + col as u16 * self.tile_w + col.min(self.wide) as u16,
            area.y + row as u16 * self.tile_h,
            self.tile_w + (col < self.wide) as u16,
            self.tile_h,
        )
    }
}

fn layout(area: Rect, count: usize) -> Option<Layout> {
    if count == 0 || area.width < MIN_TILE_COLS || area.height < TILE_ROWS {
        return None;
    }
    let cols = (area.width / MIN_TILE_COLS) as usize;
    let rows = count.div_ceil(cols);
    let visible = ((area.height / TILE_ROWS) as usize).min(rows);
    Some(Layout {
        cols,
        tile_w: area.width / cols as u16,
        wide: (area.width % cols as u16) as usize,
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
    sessions: &mut BTreeMap<SessionId, Session>,
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
        state.capture,
    );
}

/// Render the grid with no highlight, scroll, or capture, for the
/// session switcher's preview pane.
pub fn render_preview(buf: &mut Buffer, area: Rect, sessions: &mut BTreeMap<SessionId, Session>) {
    let items = items(sessions);
    draw(buf, area, sessions, &items, None, 0, None);
}

fn draw(
    buf: &mut Buffer,
    area: Rect,
    sessions: &mut BTreeMap<SessionId, Session>,
    items: &[GridItem],
    highlight: Option<usize>,
    scroll: usize,
    capture: Option<TabId>,
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
        let rect = l.tile_rect(area, i % l.cols, row - scroll);
        let Some(session) = sessions.get_mut(&item.session) else {
            continue;
        };
        let name = session.name.clone();
        let Some(tab) = session.tab_at_mut(item.window, item.tab) else {
            continue;
        };
        // Reflow the tab to the tile's interior so its content fits the
        // tile instead of showing a garbled crop of the full-size layout;
        // the home window's reconcile restores the real size on the next
        // direct render.
        let inner = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        if inner.width > 0
            && inner.height > 0
            && (tab.rect.width, tab.rect.height) != (inner.width, inner.height)
        {
            tab.resize(inner);
        }
        let captured = capture == Some(tab.id);
        draw_tile(
            buf,
            rect,
            &name,
            tab,
            highlight == Some(i),
            captured,
            elapsed,
        );
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

/// Render the tail of `tab`'s live content — where fresh output lands —
/// cropped into `area`, without touching the tab's real size.
pub fn render_tail(buf: &mut Buffer, area: Rect, tab: &Tab) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let screen = tab.engine.screen();
    let live_rows = screen.physical_rows as i64;
    let range = screen.phys_range(&((live_rows - area.height as i64).max(0)..live_rows));
    for (y, line) in screen.lines_in_phys_range(range).iter().enumerate() {
        if y >= area.height as usize {
            break;
        }
        for cell in line.visible_cells() {
            let cx = cell.cell_index();
            if cx >= area.width as usize {
                break;
            }
            let pos = Position::new(area.x + cx as u16, area.y + y as u16);
            if let Some(dst) = buf.cell_mut(pos) {
                dst.set_symbol(cell.str());
                dst.set_style(cell_style(cell.attrs()));
            }
        }
    }
}

/// One tile: a border colored to match the tab's status (its animation
/// carried onto the border, so working shimmers and blocked breathes at
/// tile size), the tail of the tab's live content inside, and the tab's
/// bracketed status text, home session name, and tab name drawn over the
/// top edge like a title. The highlighted tile's border is double-lined,
/// marking the highlight through line weight so it collides with neither
/// the status coloring nor the border color; a captured tile carries a
/// right-aligned label on top of that, since capture is entered on the
/// highlighted tile and the border alone can't tell the two apart.
fn draw_tile(
    buf: &mut Buffer,
    rect: Rect,
    session: &str,
    tab: &Tab,
    highlighted: bool,
    captured: bool,
    elapsed: Duration,
) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let (color, border_anim) =
        tab.agent
            .as_ref()
            .map_or((Color::DarkGray, Anim::None), |tracker| {
                let visual = tracker.visual();
                (visual.color, visual.anim)
            });
    draw_border(buf, rect, highlighted, color, border_anim, elapsed);
    render_tail(
        buf,
        Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2),
        tab,
    );
    // Chrome text over the top edge, inside the corner, on the
    // terminal's default background.
    let base = Style::default();
    let mut x = rect.x + 1;
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
    // The capture-mode label, right-aligned on the top edge inside the
    // corner, mirroring the tab bar's scroll label; cyan so it collides
    // with no agent-status color.
    if captured {
        let label = " capture ";
        let len = label.chars().count() as u16;
        if rect.width >= len + 2 {
            let style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::REVERSED);
            let start = rect.right() - 1 - len;
            for (i, ch) in label.chars().enumerate() {
                if let Some(dst) = buf.cell_mut(Position::new(start + i as u16, rect.y)) {
                    dst.set_char(ch);
                    dst.set_style(style);
                }
            }
        }
    }
}

/// Draw a tile's border in `color`, double-lined when highlighted. The
/// status animation is applied per border cell, indexed clockwise around
/// the perimeter so a shimmer band sweeps the tile's edge.
fn draw_border(
    buf: &mut Buffer,
    rect: Rect,
    double: bool,
    color: Color,
    border_anim: Anim,
    elapsed: Duration,
) {
    let (h, v, tl, tr, bl, br) = if double {
        ('═', '║', '╔', '╗', '╚', '╝')
    } else {
        ('─', '│', '┌', '┐', '└', '┘')
    };
    let (left, right) = (rect.left(), rect.right() - 1);
    let (top, bottom) = (rect.top(), rect.bottom() - 1);
    let horizontal = |x: u16, y: u16| {
        let ch = match (x == left, x == right, y == top) {
            (true, _, true) => tl,
            (_, true, true) => tr,
            (true, _, false) => bl,
            (_, true, false) => br,
            _ => h,
        };
        (x, y, ch)
    };
    // The perimeter walked clockwise from the top-left corner; corners
    // belong to the horizontal edges.
    let cells: Vec<(u16, u16, char)> = (left..=right)
        .map(|x| horizontal(x, top))
        .chain((top + 1..bottom).map(|y| (right, y, v)))
        .chain((left..=right).rev().map(|x| horizontal(x, bottom)))
        .chain((top + 1..bottom).rev().map(|y| (left, y, v)))
        .collect();
    let len = cells.len();
    for (i, (x, y, ch)) in cells.into_iter().enumerate() {
        let color = match border_anim {
            Anim::None => color,
            Anim::Shimmer => anim::shimmer(color, i, len, elapsed),
            Anim::Breathe => anim::breathe(color, elapsed),
        };
        if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
            dst.set_char(ch);
            dst.set_style(Style::default().fg(color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn columns_pack_at_minimum_width_and_tiles_grow_to_fill() {
        // 150 wide fits two minimum-width columns; each tile widens to
        // 75 so no width is left over. Height stays fixed at 24, and
        // neither depends on the item count.
        for count in [1, 3, 9] {
            let l = layout(area(150, 70), count).unwrap();
            assert_eq!((l.cols, l.tile_w, l.wide, l.tile_h), (2, 75, 0, 24));
        }
        // 300 wide fits five columns exactly at the minimum width.
        let l = layout(area(300, 150), 3).unwrap();
        assert_eq!((l.cols, l.tile_w, l.wide), (5, 60, 0));
        // Smaller than one minimum tile lays out nothing.
        assert!(layout(area(59, 70), 3).is_none());
        assert!(layout(area(120, 23), 3).is_none());
        assert!(layout(area(120, 70), 0).is_none());
    }

    #[test]
    fn leftover_width_spreads_across_the_columns() {
        // 131 wide: two columns, base width 65, the first one cell
        // wider — the tiles consume all 131 columns between them.
        let l = layout(area(131, 70), 4).unwrap();
        assert_eq!((l.cols, l.tile_w, l.wide), (2, 65, 1));
        let a = area(131, 70);
        let first = l.tile_rect(a, 0, 0);
        let second = l.tile_rect(a, 1, 0);
        assert_eq!((first.x, first.width), (0, 66));
        assert_eq!((second.x, second.width), (66, 65));
        assert_eq!(second.right(), 131);
        // The second row sits one tile height down.
        assert_eq!(l.tile_rect(a, 0, 1).y, 24);
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

    #[test]
    fn border_cells_cover_the_perimeter_once() {
        // Every border cell drawn exactly once, corners included, for
        // both line weights.
        for double in [false, true] {
            let rect = Rect::new(2, 1, 6, 4);
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 8));
            draw_border(
                &mut buf,
                rect,
                double,
                Color::Red,
                Anim::None,
                Duration::ZERO,
            );
            let (tl, tr, bl, br) = if double {
                ("╔", "╗", "╚", "╝")
            } else {
                ("┌", "┐", "└", "┘")
            };
            assert_eq!(buf.cell(Position::new(2, 1)).unwrap().symbol(), tl);
            assert_eq!(buf.cell(Position::new(7, 1)).unwrap().symbol(), tr);
            assert_eq!(buf.cell(Position::new(2, 4)).unwrap().symbol(), bl);
            assert_eq!(buf.cell(Position::new(7, 4)).unwrap().symbol(), br);
            // Edges take the status color; the interior is untouched.
            assert_eq!(
                buf.cell(Position::new(4, 1)).unwrap().style().fg,
                Some(Color::Red)
            );
            assert_eq!(buf.cell(Position::new(4, 2)).unwrap().symbol(), " ");
        }
    }
}
