//! The fuzzy tab finder: a floating picker over every tab in every session.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Widget};
use tui_textarea::TextArea;

use crate::server::config::Config;
use crate::server::grid;
use crate::server::layout::WindowId;
use crate::server::palette::{self, Palette};
use crate::server::session::Session;
use crate::server::window::TabId;
use crate::server::{SessionId, clear_region};

pub struct FindItem {
    pub session: SessionId,
    pub window: WindowId,
    pub tab: usize,
    pub id: TabId,
    pub name: String,
    pub session_name: String,
}

/// Per-client finder state. `highlight` indexes the matched list, not all
/// items.
pub struct FinderState {
    pub textarea: TextArea<'static>,
    pub highlight: usize,
    pub backdrop: Buffer,
}

impl FinderState {
    pub fn new(backdrop: Buffer) -> Self {
        let mut textarea = TextArea::default();
        // The default cursor-line underline looks like stray chrome in a
        // one-line input.
        textarea.set_cursor_line_style(Style::default());
        Self {
            textarea,
            highlight: 0,
            backdrop,
        }
    }

    pub fn query(&self) -> String {
        self.textarea.lines().first().cloned().unwrap_or_default()
    }
}

/// Every tab in every session, ordered by session name, then window and
/// tab position.
pub fn items(sessions: &BTreeMap<SessionId, Session>) -> Vec<FindItem> {
    let mut by_name: Vec<(&str, SessionId)> = sessions
        .iter()
        .map(|(&sid, s)| (s.name.as_str(), sid))
        .collect();
    by_name.sort();
    let mut out = Vec::new();
    for (name, sid) in by_name {
        let session = &sessions[&sid];
        for (window, tab) in session.all_tabs() {
            let Some(t) = session.tab_at(window, tab) else {
                continue;
            };
            out.push(FindItem {
                session: sid,
                window,
                tab,
                id: t.id,
                name: t.name.clone(),
                session_name: name.to_string(),
            });
        }
    }
    out
}

/// Indices of the items whose names fuzzy-match `query`, best first. An
/// empty query matches everything.
pub fn matches(items: &[FindItem], query: &str) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| score(query, &item.name).map(|s| (s, i)))
        .collect();
    scored.sort_by_key(|&(s, i)| (std::cmp::Reverse(s), i));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Case-insensitive subsequence match, scored higher for consecutive hits
/// and an early first hit.
fn score(query: &str, name: &str) -> Option<i32> {
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut first = 0usize;
    let mut pos = 0usize;
    let mut prev: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        let found = pos + name.get(pos..)?.iter().position(|&c| c == qc)?;
        score += match prev {
            Some(p) if found == p + 1 => 3,
            _ => 1,
        };
        if prev.is_none() {
            first = found;
        }
        prev = Some(found);
        pos = found + 1;
    }
    Some(score - first as i32)
}

pub fn render(
    buf: &mut Buffer,
    area: Rect,
    sessions: &mut BTreeMap<SessionId, Session>,
    state: &FinderState,
    config: &Config,
) {
    let window = float_rect(area);
    if window.width < 4 || window.height < 4 {
        return;
    }
    let palette = &config.palette;
    clear_region(buf, window);
    Block::bordered()
        .border_style(Style::default().fg(palette.dim))
        .render(window, buf);
    render_panes(
        buf,
        window.inner(Margin::new(1, 1)),
        sessions,
        state,
        palette,
    );
    if config.shadows {
        palette::shadow(buf, window, area, palette);
    }
}

/// The match list beside or above the preview, whichever the shape fits.
fn render_panes(
    buf: &mut Buffer,
    inner: Rect,
    sessions: &mut BTreeMap<SessionId, Session>,
    state: &FinderState,
    palette: &Palette,
) {
    let items = items(sessions);
    let matched = matches(&items, &state.query());
    let highlight = state.highlight.min(matched.len().saturating_sub(1));
    let dim = Style::default().fg(palette.dim);
    if inner.width > inner.height {
        let list_w = 32.min(inner.width);
        render_list(
            buf,
            Rect {
                width: list_w,
                ..inner
            },
            &items,
            &matched,
            highlight,
            state,
            palette,
        );
        if inner.width <= list_w {
            return;
        }
        for y in inner.top()..inner.bottom() {
            if let Some(dst) = buf.cell_mut(Position::new(inner.x + list_w, y)) {
                dst.set_symbol("│");
                dst.set_style(dim);
            }
        }
        let preview = Rect {
            x: inner.x + list_w + 1,
            width: inner.width - list_w - 1,
            ..inner
        };
        render_match_preview(buf, preview, sessions, &items, &matched, highlight);
    } else {
        let list_h = inner.height / 2;
        render_list(
            buf,
            Rect {
                height: list_h,
                ..inner
            },
            &items,
            &matched,
            highlight,
            state,
            palette,
        );
        if inner.height <= list_h + 1 {
            return;
        }
        for x in inner.left()..inner.right() {
            if let Some(dst) = buf.cell_mut(Position::new(x, inner.y + list_h)) {
                dst.set_symbol("─");
                dst.set_style(dim);
            }
        }
        let preview = Rect {
            y: inner.y + list_h + 1,
            height: inner.height - list_h - 1,
            ..inner
        };
        render_match_preview(buf, preview, sessions, &items, &matched, highlight);
    }
}

fn float_rect(area: Rect) -> Rect {
    let width = (area.width as u32 * 9 / 10) as u16;
    let height = (area.height as u32 * 9 / 10) as u16;
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn render_list(
    buf: &mut Buffer,
    area: Rect,
    items: &[FindItem],
    matched: &[usize],
    highlight: usize,
    state: &FinderState,
    palette: &Palette,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if let Some(dst) = buf.cell_mut(Position::new(area.x, area.y)) {
        dst.set_char('>');
        dst.set_style(Style::default().fg(palette.accent));
    }
    let input = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(2),
        area.height.min(1),
    );
    state.textarea.render(input, buf);
    // Without scroll state, the visible window slides to keep the highlight
    // on its last row.
    let visible = area.height.saturating_sub(1) as usize;
    let start = (highlight + 1).saturating_sub(visible);
    for (row, &idx) in matched.iter().enumerate().skip(start) {
        let y = area.y + 1 + (row - start) as u16;
        if y >= area.bottom() {
            break;
        }
        let item = &items[idx];
        let selected = row == highlight;
        let (name_style, session_style) = if selected {
            let style = Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::REVERSED);
            (style, style)
        } else {
            (Style::default(), Style::default().fg(palette.dim))
        };
        let mut x = area.x;
        let mut put = |x: &mut u16, ch: char, style: Style| -> bool {
            if *x >= area.right() {
                return false;
            }
            if let Some(dst) = buf.cell_mut(Position::new(*x, y)) {
                dst.set_char(ch);
                dst.set_style(style);
            }
            *x += 1;
            true
        };
        for ch in format!(" {} ", item.name).chars() {
            if !put(&mut x, ch, name_style) {
                break;
            }
        }
        for ch in item.session_name.chars() {
            if !put(&mut x, ch, session_style) {
                break;
            }
        }
        put(&mut x, ' ', session_style);
    }
}

/// Resizes the previewed tab to fit the pane. The home window restores the
/// real size on its next render.
fn render_match_preview(
    buf: &mut Buffer,
    area: Rect,
    sessions: &mut BTreeMap<SessionId, Session>,
    items: &[FindItem],
    matched: &[usize],
    highlight: usize,
) {
    let Some(&idx) = matched.get(highlight) else {
        return;
    };
    let item = &items[idx];
    let Some(tab) = sessions
        .get_mut(&item.session)
        .and_then(|s| s.tab_at_mut(item.window, item.tab))
    else {
        return;
    };
    if area.width > 0
        && area.height > 0
        && (tab.rect.width, tab.rect.height) != (area.width, area.height)
    {
        tab.resize(area);
    }
    grid::render_tail(buf, area, tab);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str) -> FindItem {
        FindItem {
            session: 0,
            window: 0,
            tab: 0,
            id: 0,
            name: name.to_string(),
            session_name: String::new(),
        }
    }

    #[test]
    fn every_query_character_must_appear_in_order() {
        assert!(score("cl", "claude").is_some());
        assert!(score("cde", "claude").is_some(), "gaps are allowed");
        assert!(score("lc", "claude").is_none(), "order matters");
        assert!(score("z", "claude").is_none());
        assert!(score("claudee", "claude").is_none());
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(score("CL", "Claude"), score("cl", "claude"));
        assert!(score("cl", "CLAUDE").is_some());
    }

    #[test]
    fn floating_window_takes_ninety_percent_centered() {
        let win = float_rect(Rect::new(0, 0, 100, 40));
        assert_eq!((win.x, win.y, win.width, win.height), (5, 2, 90, 36));
        let win = float_rect(Rect::new(10, 4, 100, 40));
        assert_eq!((win.x, win.y), (15, 6));
        // Too small to draw, so render bails instead of panicking.
        let win = float_rect(Rect::new(0, 0, 4, 3));
        assert!(win.width < 4);
    }

    #[test]
    fn empty_query_matches_everything_in_stable_order() {
        let items = [item("zsh"), item("claude"), item("vim")];
        assert_eq!(matches(&items, ""), vec![0, 1, 2]);
    }

    #[test]
    fn consecutive_and_earlier_matches_rank_higher() {
        let items = [item("calc"), item("claude")];
        assert_eq!(matches(&items, "cl"), vec![1, 0]);
        let items = [item("watch-vim"), item("vim")];
        assert_eq!(matches(&items, "vim"), vec![1, 0]);
        let items = [item("zsh"), item("claude"), item("vim")];
        assert_eq!(matches(&items, "cd"), vec![1]);
    }
}
