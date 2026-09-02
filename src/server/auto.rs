//! Auto mode: attaches to one done-or-blocked agent tab at a time. The
//! hand-off itself runs in the server tick.

use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Style};

use crate::server::anim::{self, Anim};
use crate::server::grid;
use crate::server::session::Session;
use crate::server::window::TabId;
use crate::server::{SessionId, clear_region};

const MESSAGE: &str = "Claude doesn't need you right now";

const ALL_IDLE: &str = "All agents are idle";

const LIST_WIDTH: u16 = 80;

#[derive(Clone, Copy, Default)]
pub struct AutoState {
    pub presented: Option<TabId>,
    /// Set while the fallback screen waits for a prefix key's follow-up.
    pub pending_prefix: bool,
}

type Run = Vec<(char, Style)>;

/// Draws the fallback screen. The working list grows up from the bottom
/// and drops lines before it reaches the message.
pub fn render_blank(buf: &mut Buffer, area: Rect, sessions: &BTreeMap<SessionId, Session>) {
    clear_region(buf, area);
    if area.width == 0 || area.height == 0 {
        return;
    }
    let msg_y = area.y + area.height / 2;
    let len = MESSAGE.chars().count() as u16;
    let mut x = area.x + area.width.saturating_sub(len) / 2;
    for ch in MESSAGE.chars() {
        if x >= area.right() {
            break;
        }
        if let Some(dst) = buf.cell_mut(Position::new(x, msg_y)) {
            dst.set_char(ch);
            dst.set_style(Style::default());
        }
        x += 1;
    }
    let width = area.width.min(LIST_WIDTH);
    let x0 = area.x + (area.width - width) / 2;
    let mut entries = working_entries(sessions);
    if entries.is_empty() {
        entries = vec![
            ALL_IDLE
                .chars()
                .map(|c| (c, Style::default().fg(Color::DarkGray)))
                .collect(),
        ];
    }
    let lines = pack(entries, width as usize);
    let avail = usize::from(area.bottom().saturating_sub(msg_y + 2));
    let shown = lines.len().min(avail);
    let y0 = area.bottom() - shown as u16;
    for (i, line) in lines.into_iter().take(shown).enumerate() {
        let y = y0 + i as u16;
        for (j, (ch, style)) in line.into_iter().enumerate() {
            if let Some(dst) = buf.cell_mut(Position::new(x0 + j as u16, y)) {
                dst.set_char(ch);
                dst.set_style(style);
            }
        }
    }
}

/// Working agent tabs in grid order, formatted like the grid's tile headers
/// but with the status uncolored.
fn working_entries(sessions: &BTreeMap<SessionId, Session>) -> Vec<Run> {
    let now = std::time::Instant::now();
    let elapsed = anim::elapsed();
    grid::items(sessions)
        .into_iter()
        .filter_map(|item| {
            let session = sessions.get(&item.session)?;
            let tab = session.tab_at(item.window, item.tab)?;
            let tracker = tab.agent.as_ref().filter(|t| t.working())?;
            let visual = tracker.visual(now);
            let mut run = Run::new();
            let len = visual.text.chars().count();
            for (j, ch) in visual.text.chars().enumerate() {
                let color = match visual.anim {
                    Anim::None => Color::Reset,
                    Anim::Shimmer => anim::shimmer(Color::Reset, j, len, elapsed),
                    Anim::Breathe => anim::breathe(Color::Reset, elapsed),
                };
                run.push((ch, Style::default().fg(color)));
            }
            run.push((' ', Style::default()));
            for ch in session.name.chars() {
                run.push((ch, Style::default().fg(Color::Gray)));
            }
            run.push((':', Style::default().fg(Color::DarkGray)));
            for ch in tab.name.chars() {
                run.push((ch, Style::default().fg(Color::DarkGray)));
            }
            Some(run)
        })
        .collect()
}

/// Join entries with commas into lines at most `width` wide, hard-wrapping
/// any entry wider than that.
fn pack(entries: Vec<Run>, width: usize) -> Vec<Run> {
    let last = entries.len().saturating_sub(1);
    let mut lines: Vec<Run> = Vec::new();
    let mut line = Run::new();
    for (i, mut entry) in entries.into_iter().enumerate() {
        if i < last {
            entry.push((',', Style::default()));
        }
        if !line.is_empty() {
            if line.len() + 1 + entry.len() <= width {
                line.push((' ', Style::default()));
            } else {
                lines.push(std::mem::take(&mut line));
            }
        }
        line.extend(entry);
        while line.len() > width {
            let rest = line.split_off(width);
            lines.push(std::mem::replace(&mut line, rest));
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Run {
        text.chars().map(|c| (c, Style::default())).collect()
    }

    fn text(line: &Run) -> String {
        line.iter().map(|&(c, _)| c).collect()
    }

    #[test]
    fn entries_join_on_one_line_while_they_fit() {
        let lines = pack(
            vec![run("[working 5s] a:claude"), run("[working 2s] b:codex")],
            80,
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(
            text(&lines[0]),
            "[working 5s] a:claude, [working 2s] b:codex"
        );
    }

    #[test]
    fn overflow_breaks_between_entries_with_the_comma_kept() {
        let lines = pack(vec![run("aaaa"), run("bbbb"), run("cccc")], 11);
        assert_eq!(
            lines.iter().map(text).collect::<Vec<_>>(),
            vec!["aaaa, bbbb,", "cccc"],
        );
    }

    #[test]
    fn an_entry_wider_than_the_column_hard_wraps() {
        let lines = pack(vec![run("abcdefghij"), run("kl")], 4);
        assert_eq!(
            lines.iter().map(text).collect::<Vec<_>>(),
            vec!["abcd", "efgh", "ij,", "kl"],
        );
    }
}
