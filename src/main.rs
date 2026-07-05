//! Lux — terminal multiplexer. Phase 3: windows & tabs.
//!
//! A binary layout tree of windows, each owning its own ordered tab list —
//! tabs belong to the window, not the window to a shared tab set (lux's
//! core differentiator per `docs/lux-features.md`). Hardcoded Ctrl-b prefix
//! dispatching through a single keybinding table for splits, tabs, focus,
//! and resizing. No tab renaming, cross-window tab overview, explicit
//! tab-close keybindings, configuration, ex commands, mouse input, or
//! session attach/detach (REQ-WINDOW-023, REQ-TAB-017, REQ-KEY-007).

mod config;
mod ex;
mod keys;
mod layout;
mod window;

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::thread;

use anyhow::Context;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode as CtKeyCode, KeyEvent,
    KeyEventKind, KeyModifiers as CtMods, MouseButton as CtMouseButton,
    MouseEvent as CtMouseEvent, MouseEventKind as CtMouseKind, read as read_ct_event,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::{DefaultTerminal, Frame};
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::input::{KeyCode, Modifiers as KeyModifiers};
use termwiz::surface::CursorVisibility;
use tui_textarea::TextArea;

use ex::ExCommand;
use keys::{Command, KeyTable};
use layout::{Dir, Node, Separator, SplitKind, WindowId};
use window::{Event, Tab, TabId, Window};

/// Minimum window size a split may produce (REQ-WINDOW-008).
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 3;

fn main() {
    // REQ-CONFIG-002: load the config at startup, before raw mode so its
    // errors are visible on stderr.
    let keys = config::load();
    // REQ-PANE-002/003: raw mode + alternate screen. init() also installs a
    // panic hook that restores the terminal first (REQ-PANE-016).
    let mut tui = ratatui::init();
    // REQ-SCROLL-001: capture mouse events. ratatui's panic hook doesn't
    // know about mouse capture, so chain a release in front of it
    // (REQ-PANE-016: a panic must not leave the host reporting mouse).
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui_hook(info);
    }));
    let result = run(&mut tui, keys);
    // REQ-PANE-014/015: restore the original mode on every exit path.
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("lux: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run(tui: &mut DefaultTerminal, keys: KeyTable) -> anyhow::Result<i32> {
    let size = tui.size()?;
    let (tx, rx) = mpsc::channel();

    let input_tx = tx.clone();
    thread::spawn(move || {
        while let Ok(ev) = read_ct_event() {
            if input_tx.send(Event::Input(ev)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(Rect::new(0, 0, size.width, size.height), tx, keys)?;
    app.draw_frame(tui)?;
    loop {
        let ev = rx.recv().context("event channel closed")?;
        if let Some(code) = app.handle(ev) {
            return Ok(code);
        }
        // Coalesce whatever else is already pending into this frame.
        while let Ok(ev) = rx.try_recv() {
            if let Some(code) = app.handle(ev) {
                return Ok(code);
            }
        }
        // REQ-PANE-006: redraw when any visible engine's state has
        // advanced (or chrome/layout changed, e.g. REQ-TAB-008).
        if app.needs_redraw() {
            app.draw_frame(tui)?;
        }
    }
}

/// A drag selection over one window's content (REQ-SCROLL-014), in
/// content-relative cell coordinates. Linear: the text flows from `start`
/// to `end` through intervening full rows.
struct Selection {
    window: WindowId,
    start: (u16, u16),
    end: (u16, u16),
}

impl Selection {
    /// Endpoints ordered by (row, col).
    fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if (self.start.1, self.start.0) <= (self.end.1, self.end.0) {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

/// The inclusive column span a linear selection covers on `row`.
fn selection_span(row: u16, first: (u16, u16), last: (u16, u16)) -> (u16, u16) {
    let from = if row == first.1 { first.0 } else { 0 };
    let to = if row == last.1 { last.0 } else { u16::MAX };
    (from, to)
}

/// Map an absolute screen position into content-relative cell coordinates,
/// clamped inside the content rectangle (REQ-SCROLL-019).
fn clamp_to_content(pos: Position, content: Rect) -> (u16, u16) {
    if content.width == 0 || content.height == 0 {
        return (0, 0);
    }
    let x = pos.x.clamp(content.left(), content.right() - 1) - content.x;
    let y = pos.y.clamp(content.top(), content.bottom() - 1) - content.y;
    (x, y)
}

/// Forward a mouse event to a tab whose program handles the mouse itself
/// (REQ-SCROLL-020); the engine encodes it per the protocol the program
/// requested, and converts wheel ticks to arrow keys on the alternate
/// screen (REQ-SCROLL-021).
fn forward_mouse(tab: &mut Tab, mouse: &CtMouseEvent, content: Rect) {
    use wezterm_term::{MouseButton as WzButton, MouseEventKind as WzKind};
    let (kind, button) = match mouse.kind {
        CtMouseKind::Down(b) => (WzKind::Press, wz_button(b)),
        CtMouseKind::Up(b) => (WzKind::Release, wz_button(b)),
        CtMouseKind::Drag(b) => (WzKind::Move, wz_button(b)),
        CtMouseKind::Moved => (WzKind::Move, WzButton::None),
        CtMouseKind::ScrollUp => (WzKind::Press, WzButton::WheelUp(1)),
        CtMouseKind::ScrollDown => (WzKind::Press, WzButton::WheelDown(1)),
        CtMouseKind::ScrollLeft | CtMouseKind::ScrollRight => return,
    };
    let (x, y) = clamp_to_content(Position::new(mouse.column, mouse.row), content);
    let _ = tab.engine.mouse_event(wezterm_term::MouseEvent {
        kind,
        x: x as usize,
        y: y as i64,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        button,
        modifiers: convert_mods(mouse.modifiers),
    });
}

fn wz_button(button: CtMouseButton) -> wezterm_term::MouseButton {
    match button {
        CtMouseButton::Left => wezterm_term::MouseButton::Left,
        CtMouseButton::Right => wezterm_term::MouseButton::Right,
        CtMouseButton::Middle => wezterm_term::MouseButton::Middle,
    }
}

/// REQ-SCROLL-016: OSC 52 asks the host terminal to place the text on the
/// system clipboard; it works locally and over SSH without a display
/// server connection.
fn copy_to_system_clipboard(text: &str) {
    use base64::Engine as _;
    use std::io::Write as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}

/// Per-frame chrome geometry for one window, computed into state before
/// drawing (REQ-TAB-010).
struct Chrome {
    window: WindowId,
    tab_bar: Rect,
    tab_count: usize,
    active: usize,
    /// Whether the active tab is in scroll mode (REQ-SCROLL-013).
    scroll: bool,
}

/// Per-frame command line geometry, present while it's open (REQ-EX-002).
struct ExChrome {
    /// The whole bottom row, including the `:` prompt cell.
    line: Rect,
    /// Where the textarea widget renders (after the prompt).
    input: Rect,
    /// Commands matching the typed text (REQ-EX-006), on the row above.
    suggestions: Vec<&'static str>,
    suggestion_row: Option<Rect>,
}

/// Everything the draw pass reads; recomputed once per frame (REQ-TAB-010).
#[derive(Default)]
struct View {
    separators: Vec<Separator>,
    chrome: Vec<Chrome>,
    ex: Option<ExChrome>,
}

struct App {
    tree: Node,
    windows: HashMap<WindowId, Window>,
    /// REQ-WINDOW-014: exactly one focused window at any time.
    focus: WindowId,
    /// True after the prefix key, awaiting the command key (REQ-WINDOW-004).
    prefix_pending: bool,
    /// The open ex command line, if any (REQ-EX-001).
    ex: Option<TextArea<'static>>,
    /// The active prefix and bindings: defaults, or config overrides
    /// (REQ-CONFIG-005/006).
    keys: KeyTable,
    /// The current drag selection, if any (REQ-SCROLL-014).
    selection: Option<Selection>,
    view: View,
    area: Rect,
    next_window_id: WindowId,
    next_tab_id: TabId,
    force_redraw: bool,
    tx: Sender<Event>,
}

impl App {
    fn new(area: Rect, tx: Sender<Event>, keys: KeyTable) -> anyhow::Result<Self> {
        // REQ-PANE-001: the initial window's shell, sized to the viewport.
        let first = Window::new(0, area, 0, tx.clone())?;
        let mut windows = HashMap::new();
        windows.insert(first.id, first);
        Ok(Self {
            tree: Node::Leaf(0),
            windows,
            focus: 0,
            prefix_pending: false,
            ex: None,
            keys,
            selection: None,
            view: View::default(),
            area,
            next_window_id: 1,
            next_tab_id: 1,
            force_redraw: false,
            tx,
        })
    }

    /// Handle one event; returns the app's exit code once the last
    /// window's last tab's child has exited (REQ-WINDOW-022).
    fn handle(&mut self, ev: Event) -> Option<i32> {
        match ev {
            // REQ-PANE-005: advance the owning engine with PTY output,
            // whether or not that tab is currently visible.
            Event::Output(id, bytes) => {
                if let Some(tab) = self.find_tab_mut(id) {
                    tab.engine.advance_bytes(&bytes);
                }
            }
            Event::Exited(id) => return self.tab_exited(id),
            Event::Input(CtEvent::Key(key)) => {
                if key.kind == KeyEventKind::Release {
                    return None;
                }
                self.handle_key(key);
            }
            Event::Input(CtEvent::Mouse(mouse)) => self.handle_mouse(mouse),
            Event::Input(CtEvent::Resize(cols, rows)) => {
                // REQ-WINDOW-019: the next frame's compute pass recomputes
                // the tree and resizes every window's tabs.
                self.area = Rect::new(0, 0, cols, rows);
                self.force_redraw = true;
            }
            Event::Input(_) => {}
        }
        None
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // REQ-EX-003: while the command line is open, every key press edits
        // it instead of reaching the focused window's PTY.
        if self.ex.is_some() {
            self.handle_ex_key(key);
            return;
        }
        if self.prefix_pending {
            self.prefix_pending = false;
            // REQ-KEY-001: every recognized sequence dispatches through the
            // single keybinding table. An unrecognized command key returns
            // None and both keys are discarded, never forwarded
            // (REQ-WINDOW-007).
            if let Some(command) = self.keys.lookup(key) {
                self.execute(command);
            }
            return;
        }
        // REQ-WINDOW-003/004: the prefix key (Ctrl-b by default, config
        // may override it per REQ-CONFIG-005) arms the prefix instead of
        // reaching the focused window's PTY.
        if self.keys.is_prefix(key) {
            self.prefix_pending = true;
            return;
        }
        // REQ-SCROLL-004: in scroll mode every key is consumed by history
        // navigation; nothing reaches the PTY.
        if let Some(win) = self.windows.get_mut(&self.focus)
            && win.active_tab().scroll_mode()
        {
            let page = win.content_rect().height.max(1) as isize;
            let tab = win.active_tab_mut();
            match key.code {
                // REQ-SCROLL-005/006: one line at a time.
                CtKeyCode::Char('k') | CtKeyCode::Up => {
                    tab.scroll_by(-1);
                }
                CtKeyCode::Char('j') | CtKeyCode::Down => {
                    tab.scroll_by(1);
                }
                // REQ-SCROLL-007/008: one page at a time.
                CtKeyCode::PageUp => {
                    tab.scroll_by(-page);
                }
                CtKeyCode::PageDown => {
                    tab.scroll_by(page);
                }
                // REQ-SCROLL-011: back to following live output.
                CtKeyCode::Esc | CtKeyCode::Char('q') => tab.exit_scroll_mode(),
                _ => {}
            }
            self.force_redraw = true;
            return;
        }
        // REQ-WINDOW-015: every other key goes only to the focused window's
        // active tab; the engine encodes it per the live terminal modes and
        // writes it to that tab's PTY (REQ-PANE-011). A write can fail when
        // the child has already exited; Exited follows.
        if let Some((code, mods)) = map_key(key)
            && let Some(win) = self.windows.get_mut(&self.focus)
        {
            let _ = win.active_tab_mut().engine.key_down(code, mods);
        }
    }

    fn handle_mouse(&mut self, mouse: CtMouseEvent) {
        let pos = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            CtMouseKind::Down(button) => {
                let Some(id) = self.window_at(pos) else { return };
                // REQ-SCROLL-002: click-to-focus.
                if self.focus != id {
                    self.focus = id;
                    self.force_redraw = true;
                }
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                // REQ-SCROLL-020: the program owns the mouse.
                if tab.engine.is_mouse_grabbed() {
                    forward_mouse(tab, &mouse, content);
                    return;
                }
                match button {
                    // REQ-SCROLL-014: anchor a selection.
                    CtMouseButton::Left if content.contains(pos) => {
                        let cell = clamp_to_content(pos, content);
                        self.selection = Some(Selection { window: id, start: cell, end: cell });
                        self.force_redraw = true;
                    }
                    // REQ-SCROLL-015: yank + paste.
                    CtMouseButton::Right => self.yank_selection(),
                    _ => {}
                }
            }
            // REQ-SCROLL-014/019: extend the selection, clamped to the
            // window where the drag began.
            CtMouseKind::Drag(CtMouseButton::Left) if self.selection.is_some() => {
                let sel = self.selection.as_mut().expect("checked above");
                let Some(win) = self.windows.get(&sel.window) else { return };
                sel.end = clamp_to_content(pos, win.content_rect());
                self.force_redraw = true;
            }
            CtMouseKind::Up(_) | CtMouseKind::Drag(_) | CtMouseKind::Moved => {
                // A click that never moved selects nothing.
                if matches!(mouse.kind, CtMouseKind::Up(CtMouseButton::Left))
                    && self.selection.as_ref().is_some_and(|s| s.start == s.end)
                {
                    self.selection = None;
                    self.force_redraw = true;
                }
                // REQ-SCROLL-020: releases and motion still reach a
                // grabbed program.
                let Some(id) = self.window_at(pos) else { return };
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                if tab.engine.is_mouse_grabbed() {
                    forward_mouse(tab, &mouse, content);
                }
            }
            CtMouseKind::ScrollUp | CtMouseKind::ScrollDown => {
                let Some(id) = self.window_at(pos) else { return };
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                // REQ-SCROLL-020/021: a grabbed program gets the wheel
                // encoded; on the alternate screen the engine converts
                // wheel ticks to arrow keys itself (alternateScroll).
                if tab.engine.is_mouse_grabbed() || tab.engine.is_alt_screen_active() {
                    forward_mouse(tab, &mouse, content);
                    return;
                }
                // REQ-SCROLL-009: focus, enter scroll mode, scroll 3 lines.
                self.focus = id;
                tab.enter_scroll_mode();
                let delta = if mouse.kind == CtMouseKind::ScrollUp { -3 } else { 3 };
                // Wheeling down to the live bottom resumes following
                // (entering scroll mode just to sit at the tail would trap
                // accidental wheel-downs behind REQ-SCROLL-010).
                if tab.scroll_by(delta) {
                    tab.exit_scroll_mode();
                }
                self.force_redraw = true;
            }
            _ => {}
        }
    }

    fn window_at(&self, pos: Position) -> Option<WindowId> {
        self.windows.values().find(|w| w.rect.contains(pos)).map(|w| w.id)
    }

    /// REQ-SCROLL-015/016: copy the selected text to the system clipboard
    /// and write it to the focused window's active tab's PTY.
    fn yank_selection(&mut self) {
        let Some(text) = self.selection_text() else { return };
        self.selection = None;
        self.force_redraw = true;
        copy_to_system_clipboard(&text);
        if let Some(win) = self.windows.get_mut(&self.focus) {
            // send_paste honors the program's bracketed paste mode.
            let _ = win.active_tab_mut().engine.send_paste(&text);
        }
    }

    /// The text under the current selection, read from the selection
    /// window's active tab's current view.
    fn selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let win = self.windows.get(&sel.window)?;
        let tab = win.active_tab();
        let ((c0, r0), (c1, r1)) = sel.normalized();
        let screen = tab.engine.screen();
        let lines = screen.lines_in_phys_range(tab.view_range());
        let mut rows: Vec<String> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let row = i as u16;
            if row < r0 || row > r1 {
                continue;
            }
            let (from, to) = selection_span(row, (c0, r0), (c1, r1));
            let mut text = String::new();
            for cell in line.visible_cells() {
                let x = cell.cell_index() as u16;
                if x >= from && x <= to {
                    text.push_str(cell.str());
                }
            }
            rows.push(text.trim_end().to_string());
        }
        let text = rows.join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// Apply one dispatched command. Tab commands act only on the focused
    /// window (REQ-TAB-009).
    fn execute(&mut self, command: Command) {
        match command {
            Command::SplitSideBySide => self.split(SplitKind::SideBySide),
            Command::SplitStacked => self.split(SplitKind::Stacked),
            Command::NewTab => self.new_tab(),
            Command::NextTab => self.next_tab(),
            Command::FocusNext => self.focus_next(),
            Command::FocusDir(dir) => self.focus_dir(dir),
            Command::ResizeDir(dir) => self.resize_focused(dir),
            // REQ-KEY-006: recognized, no other action until Phase 8.
            Command::Detach => {}
            Command::OpenEx => self.open_ex(),
            // REQ-SCROLL-003: prefix+[ enters scroll mode.
            Command::ScrollMode => {
                if let Some(win) = self.windows.get_mut(&self.focus) {
                    win.active_tab_mut().enter_scroll_mode();
                    self.force_redraw = true;
                }
            }
        }
    }

    /// REQ-EX-001: open the command line for text input.
    fn open_ex(&mut self) {
        let mut textarea = TextArea::default();
        // The default cursor-line underline reads as stray chrome in a
        // one-line input.
        textarea.set_cursor_line_style(Style::default());
        self.ex = Some(textarea);
        self.force_redraw = true;
    }

    fn handle_ex_key(&mut self, key: KeyEvent) {
        self.force_redraw = true;
        match key.code {
            // REQ-EX-007: close without executing anything.
            CtKeyCode::Esc => {
                self.ex = None;
            }
            // REQ-EX-008/009/010/011: execute (or discard) and close.
            CtKeyCode::Enter => {
                let textarea = self.ex.take().expect("ex mode is open");
                let text = textarea.lines().first().cloned().unwrap_or_default();
                match ex::parse(&text) {
                    Some(ExCommand::SplitSideBySide) => self.split(SplitKind::SideBySide),
                    Some(ExCommand::SplitStacked) => self.split(SplitKind::Stacked),
                    Some(ExCommand::Write(path)) => self.write_visible(&path),
                    // REQ-EX-011: unrecognized text closes with no action.
                    None => {}
                }
            }
            // REQ-EX-004/005 and the rest of line editing via tui-textarea
            // (REQ-EX-018): character insertion, Backspace, cursor motion.
            _ => {
                let textarea = self.ex.as_mut().expect("ex mode is open");
                textarea.input(tui_textarea::Input::from(key));
            }
        }
    }

    /// REQ-EX-010: write the focused window's active tab's visible terminal
    /// content to `path`. There is no error surface yet, so a failed write
    /// is dropped (per REQ-EX-011's justification).
    fn write_visible(&self, path: &std::path::Path) {
        let tab = self.windows[&self.focus].active_tab();
        let screen = tab.engine.screen();
        let visible = screen.phys_range(&(0..screen.physical_rows as i64));
        let mut out = String::new();
        for line in screen.lines_in_phys_range(visible) {
            out.push_str(line.as_str().trim_end());
            out.push('\n');
        }
        let _ = std::fs::write(path, out);
    }

    fn find_tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.windows.values_mut().find_map(|w| w.find_tab_mut(id))
    }

    fn split(&mut self, kind: SplitKind) {
        let rect = self.windows[&self.focus].rect;
        let (first, second, _) = layout::split_areas(kind, 0.5, rect);
        // REQ-WINDOW-008: never create a window under 10 cols or 3 rows.
        for half in [first, second] {
            if half.width < MIN_COLS || half.height < MIN_ROWS {
                return;
            }
        }
        let id = self.next_window_id;
        // REQ-WINDOW-009/010, REQ-TAB-003: the new window gets one active
        // tab with its own shell and engine sized to its content rectangle.
        // If the shell can't spawn, keep the current layout rather than
        // tearing the app down.
        let Ok(win) = Window::new(id, second, self.next_tab_id, self.tx.clone()) else {
            return;
        };
        self.next_window_id += 1;
        self.next_tab_id += 1;
        self.windows.insert(id, win);
        layout::split_leaf(&mut self.tree, self.focus, kind, id);
        self.focus = id;
        self.force_redraw = true;
    }

    /// REQ-TAB-006: append a new tab to the focused window's list and make
    /// it active.
    fn new_tab(&mut self) {
        let id = self.next_tab_id;
        let win = self.windows.get_mut(&self.focus).expect("focused window exists");
        // REQ-TAB-004/005: the tab gets its own shell and engine sized to
        // the window's content rectangle.
        let Ok(tab) = Tab::spawn(id, win.content_rect(), self.tx.clone()) else {
            return;
        };
        self.next_tab_id += 1;
        win.tabs.push(tab);
        win.active = win.tabs.len() - 1;
        self.drop_selection_in(self.focus);
        // REQ-TAB-008: show the switch without waiting on PTY output.
        self.force_redraw = true;
    }

    /// REQ-TAB-007: cycle the focused window's active tab, wrapping.
    fn next_tab(&mut self) {
        let win = self.windows.get_mut(&self.focus).expect("focused window exists");
        win.active = (win.active + 1) % win.tabs.len();
        self.drop_selection_in(self.focus);
        // REQ-TAB-008: show the switch without waiting on PTY output.
        self.force_redraw = true;
    }

    /// A selection describes cells of the window's currently visible tab;
    /// drop it when that content is replaced or the window goes away.
    fn drop_selection_in(&mut self, window: WindowId) {
        if self.selection.as_ref().is_some_and(|s| s.window == window) {
            self.selection = None;
        }
    }

    /// REQ-KEY-004: move focus to the window spatially adjacent in `dir`;
    /// at a screen edge focus stays put (REQ-KEY-005).
    fn focus_dir(&mut self, dir: Dir) {
        let rects: Vec<(WindowId, Rect)> =
            self.windows.values().map(|w| (w.id, w.rect)).collect();
        let from = self.windows[&self.focus].rect;
        if let Some(id) = layout::spatial_neighbor(&rects, from, dir) {
            self.focus = id;
            self.force_redraw = true;
        }
    }

    /// REQ-WINDOW-016: cycle focus through the tree's in-order leaves.
    fn focus_next(&mut self) {
        let ids = layout::leaves(&self.tree);
        if let Some(pos) = ids.iter().position(|id| *id == self.focus) {
            self.focus = ids[(pos + 1) % ids.len()];
            self.force_redraw = true;
        }
    }

    /// REQ-WINDOW-017: move the boundary between the focused window and
    /// its adjacent sibling one cell in `dir`.
    fn resize_focused(&mut self, dir: Dir) {
        if layout::resize_toward(&mut self.tree, self.area, self.focus, dir) {
            self.force_redraw = true;
        }
    }

    fn tab_exited(&mut self, id: TabId) -> Option<i32> {
        let win_id = self
            .windows
            .values()
            .find(|w| w.tabs.iter().any(|t| t.id == id))?
            .id;
        let win = self.windows.get_mut(&win_id).expect("window exists");
        if win.tabs.len() > 1 {
            // REQ-TAB-015: prune the tab and keep the window on a live one.
            let idx = win.tabs.iter().position(|t| t.id == id).expect("tab exists");
            let active_exited = idx == win.active;
            let mut tab = win.tabs.remove(idx);
            tab.wait();
            if idx < win.active || win.active == win.tabs.len() {
                win.active -= 1;
            }
            if active_exited {
                self.drop_selection_in(win_id);
            }
            self.force_redraw = true;
            return None;
        }
        // REQ-TAB-016: a window's last tab exiting collapses the window
        // per REQ-WINDOW-020.
        let mut win = self.windows.remove(&win_id).expect("window exists");
        let status = win.tabs.pop().expect("last tab exists").wait();
        if self.windows.is_empty() {
            // REQ-WINDOW-022: the last window's exit ends the app with that
            // child's status (main restores the terminal mode first).
            return Some(status);
        }
        self.drop_selection_in(win_id);
        // REQ-WINDOW-021: refocus before the leaf disappears from the tree.
        if self.focus == win_id {
            let ids = layout::leaves(&self.tree);
            let pos = ids.iter().position(|i| *i == win_id).unwrap_or(0);
            self.focus = ids[(pos + 1) % ids.len()];
        }
        // REQ-WINDOW-020: the sibling subtree inherits the space.
        let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
        if let Some(tree) = layout::remove_leaf(tree, win_id) {
            self.tree = tree;
        }
        self.force_redraw = true;
        None
    }

    fn needs_redraw(&self) -> bool {
        self.force_redraw
            || self.windows.values().any(|w| {
                let tab = w.active_tab();
                tab.engine.current_seqno() != tab.drawn_seqno
            })
    }

    /// One frame: compute geometry into state, then draw purely from that
    /// state (REQ-TAB-010, per the herdr compute/draw split).
    fn draw_frame(&mut self, tui: &mut DefaultTerminal) -> anyhow::Result<()> {
        self.compute_view();
        tui.draw(|frame| self.render(frame))?;
        self.force_redraw = false;
        for win in self.windows.values_mut() {
            let tab = win.active_tab_mut();
            tab.drawn_seqno = tab.engine.current_seqno();
        }
        Ok(())
    }

    /// Compute this frame's window and tab bar geometry into `self.view`,
    /// reconciling every window's tabs with their rectangles
    /// (REQ-WINDOW-018/019, REQ-TAB-010/011).
    fn compute_view(&mut self) {
        let (rects, separators) = layout::compute(&self.tree, self.area);
        let mut chrome = Vec::with_capacity(rects.len());
        for (id, rect) in rects {
            let Some(win) = self.windows.get_mut(&id) else { continue };
            win.rect = rect;
            win.reconcile();
            chrome.push(Chrome {
                window: id,
                tab_bar: win.tab_bar_rect(),
                tab_count: win.tabs.len(),
                active: win.active,
                scroll: win.active_tab().scroll_mode(),
            });
        }
        self.view = View {
            separators,
            chrome,
            ex: self.compute_ex_chrome(),
        };
    }

    /// Geometry for the open command line: the bottom row (REQ-EX-002) and
    /// the suggestion row above it (REQ-EX-006).
    fn compute_ex_chrome(&self) -> Option<ExChrome> {
        let textarea = self.ex.as_ref()?;
        if self.area.height == 0 || self.area.width < 2 {
            return None;
        }
        let line = Rect::new(self.area.x, self.area.bottom() - 1, self.area.width, 1);
        let input = Rect {
            x: line.x + 1,
            width: line.width - 1,
            ..line
        };
        let text = textarea.lines().first().cloned().unwrap_or_default();
        let suggestions = ex::suggestions(&text);
        let suggestion_row = (!suggestions.is_empty() && self.area.height >= 2)
            .then(|| Rect::new(line.x, line.y - 1, line.width, 1));
        Some(ExChrome {
            line,
            input,
            suggestions,
            suggestion_row,
        })
    }

    /// Draw purely from `self.view` and engine state; no geometry math or
    /// state mutation here (REQ-TAB-010).
    fn render(&self, frame: &mut Frame) {
        let focused_rect = self.windows[&self.focus].rect;
        {
            let buf = frame.buffer_mut();
            // REQ-WINDOW-011: each window confined to its own rectangle;
            // the active tab's content below the tab bar (REQ-TAB-002/012).
            for win in self.windows.values() {
                render_tab(win.active_tab(), buf);
            }
            for chrome in &self.view.chrome {
                render_tab_bar(chrome, self.focus, buf);
            }
            // REQ-SCROLL-017: highlight the selected text.
            if let Some(sel) = &self.selection
                && let Some(win) = self.windows.get(&sel.window)
            {
                render_selection(sel, win.content_rect(), buf);
            }
            // REQ-WINDOW-012/013: separators between windows; the ones
            // touching the focused window are highlighted to mark focus.
            for sep in &self.view.separators {
                render_separator(sep, focused_rect, buf);
            }
            if let Some(chrome) = &self.view.ex {
                render_ex_chrome(chrome, buf);
            }
        }
        if let (Some(textarea), Some(chrome)) = (&self.ex, &self.view.ex) {
            // The textarea draws its own block cursor; the host cursor
            // stays hidden while the command line captures input.
            frame.render_widget(textarea, chrome.input);
            return;
        }
        // REQ-PANE-009/010: the host cursor tracks the focused window's
        // active tab's engine cursor only while it reports it visible. The
        // engine cursor belongs to the live view, so a scrolled tab shows
        // no cursor.
        let win = &self.windows[&self.focus];
        if win.active_tab().scroll_mode() {
            return;
        }
        let content = win.content_rect();
        let cursor = win.active_tab().engine.cursor_pos();
        if cursor.visibility == CursorVisibility::Visible {
            let (x, y) = (cursor.x as u16, cursor.y as u16);
            if x < content.width && y < content.height {
                frame.set_cursor_position(Position::new(content.x + x, content.y + y));
            }
        }
    }
}

fn render_tab(tab: &Tab, buf: &mut Buffer) {
    let rect = tab.rect;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let screen = tab.engine.screen();
    // The scroll-mode anchor or the live tail (REQ-SCROLL-010/012).
    let visible = tab.view_range();
    // Not `with_phys_lines`: at the pinned rev it mis-indexes the second
    // half of a wrapped line deque and panics (its `_mut` twin subtracts
    // `first_len`; the non-mut version forgets to).
    for (y, line) in screen.lines_in_phys_range(visible).iter().enumerate() {
        if y >= rect.height as usize {
            break;
        }
        for cell in line.visible_cells() {
            let x = cell.cell_index();
            if x >= rect.width as usize {
                break;
            }
            let pos = Position::new(rect.x + x as u16, rect.y + y as u16);
            if let Some(dst) = buf.cell_mut(pos) {
                dst.set_symbol(cell.str());
                // REQ-PANE-007/008: colors and text attributes.
                dst.set_style(cell_style(cell.attrs()));
            }
        }
    }
}

/// Draw one window's tab bar: an indicator per tab (REQ-TAB-013), the
/// active one visually distinct (REQ-TAB-014), the remainder ruled to keep
/// the bar readable as chrome rather than content.
fn render_tab_bar(chrome: &Chrome, focus: WindowId, buf: &mut Buffer) {
    let bar = chrome.tab_bar;
    if bar.height == 0 || bar.width == 0 {
        return;
    }
    let focused = chrome.window == focus;
    let mut x = bar.x;
    for i in 0..chrome.tab_count {
        let label = format!(" {} ", i + 1);
        let style = if i == chrome.active {
            let color = if focused { Color::Green } else { Color::Gray };
            Style::default().fg(color).add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        for ch in label.chars() {
            if x >= bar.right() {
                return;
            }
            if let Some(dst) = buf.cell_mut(Position::new(x, bar.y)) {
                dst.set_char(ch);
                dst.set_style(style);
            }
            x += 1;
        }
    }
    let indicators_end = x;
    let rule = Style::default().fg(Color::DarkGray);
    while x < bar.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, bar.y)) {
            dst.set_symbol("─");
            dst.set_style(rule);
        }
        x += 1;
    }
    // REQ-SCROLL-013: mark a scrolled tab so a frozen view isn't mistaken
    // for the live tail. Drawn over the rule, right-aligned.
    if chrome.scroll {
        let label = " scroll ";
        let len = label.len() as u16;
        if bar.width >= len && bar.right() - len >= indicators_end {
            let style = Style::default().fg(Color::Yellow).add_modifier(Modifier::REVERSED);
            let start = bar.right() - len;
            for (i, ch) in label.chars().enumerate() {
                if let Some(dst) = buf.cell_mut(Position::new(start + i as u16, bar.y)) {
                    dst.set_char(ch);
                    dst.set_style(style);
                }
            }
        }
    }
}

/// Invert the selected cells (REQ-SCROLL-017); toggling rather than
/// setting REVERSED keeps the highlight visible over already-reversed
/// content.
fn render_selection(sel: &Selection, content: Rect, buf: &mut Buffer) {
    if content.width == 0 || content.height == 0 {
        return;
    }
    let (first, last) = sel.normalized();
    for row in first.1..=last.1.min(content.height - 1) {
        let (from, to) = selection_span(row, first, last);
        for col in from..=to.min(content.width - 1) {
            let pos = Position::new(content.x + col, content.y + row);
            if let Some(dst) = buf.cell_mut(pos) {
                let mut style = dst.style();
                if style.add_modifier.contains(Modifier::REVERSED) {
                    style.add_modifier.remove(Modifier::REVERSED);
                } else {
                    style.add_modifier.insert(Modifier::REVERSED);
                }
                dst.set_style(style);
            }
        }
    }
}

/// Draw the command line row — cleared, with the `:` prompt (REQ-EX-002) —
/// and the suggestion row above it (REQ-EX-006). The textarea widget itself
/// renders separately, over the cleared input area.
fn render_ex_chrome(chrome: &ExChrome, buf: &mut Buffer) {
    for x in chrome.line.left()..chrome.line.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, chrome.line.y)) {
            dst.reset();
        }
    }
    if let Some(dst) = buf.cell_mut(Position::new(chrome.line.x, chrome.line.y)) {
        dst.set_char(':');
    }
    let Some(row) = chrome.suggestion_row else {
        return;
    };
    let style = Style::default().fg(Color::Gray).bg(Color::Indexed(236));
    let mut x = row.x;
    for name in &chrome.suggestions {
        for ch in format!(" {name} ").chars() {
            if x >= row.right() {
                return;
            }
            if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
                dst.set_char(ch);
                dst.set_style(style);
            }
            x += 1;
        }
    }
}

fn render_separator(sep: &Separator, focused: Rect, buf: &mut Buffer) {
    let (symbol, adjacent) = match sep.kind {
        SplitKind::SideBySide => (
            "│",
            (focused.right() == sep.rect.x || focused.x == sep.rect.right())
                && focused.y < sep.rect.bottom()
                && sep.rect.y < focused.bottom(),
        ),
        SplitKind::Stacked => (
            "─",
            (focused.bottom() == sep.rect.y || focused.y == sep.rect.bottom())
                && focused.x < sep.rect.right()
                && sep.rect.x < focused.right(),
        ),
    };
    let style = if adjacent {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    for y in sep.rect.top()..sep.rect.bottom() {
        for x in sep.rect.left()..sep.rect.right() {
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.set_symbol(symbol);
                dst.set_style(style);
            }
        }
    }
}

/// Map a crossterm key event to the engine's key type. Returns `None` for
/// keys that have no terminal input encoding.
fn map_key(key: KeyEvent) -> Option<(KeyCode, KeyModifiers)> {
    let mut mods = convert_mods(key.modifiers);

    let code = match key.code {
        CtKeyCode::Char(c) => KeyCode::Char(c),
        CtKeyCode::Enter => KeyCode::Enter,
        CtKeyCode::Backspace => KeyCode::Backspace,
        CtKeyCode::Tab => KeyCode::Tab,
        CtKeyCode::BackTab => {
            mods |= KeyModifiers::SHIFT;
            KeyCode::Tab
        }
        CtKeyCode::Esc => KeyCode::Escape,
        CtKeyCode::Left => KeyCode::LeftArrow,
        CtKeyCode::Right => KeyCode::RightArrow,
        CtKeyCode::Up => KeyCode::UpArrow,
        CtKeyCode::Down => KeyCode::DownArrow,
        CtKeyCode::Home => KeyCode::Home,
        CtKeyCode::End => KeyCode::End,
        CtKeyCode::PageUp => KeyCode::PageUp,
        CtKeyCode::PageDown => KeyCode::PageDown,
        CtKeyCode::Insert => KeyCode::Insert,
        CtKeyCode::Delete => KeyCode::Delete,
        CtKeyCode::F(n) => KeyCode::Function(n),
        _ => return None,
    };
    Some((code, mods))
}

fn convert_mods(mods: CtMods) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    if mods.contains(CtMods::SHIFT) {
        out |= KeyModifiers::SHIFT;
    }
    if mods.contains(CtMods::CONTROL) {
        out |= KeyModifiers::CTRL;
    }
    if mods.contains(CtMods::ALT) {
        out |= KeyModifiers::ALT;
    }
    if mods.contains(CtMods::SUPER) {
        out |= KeyModifiers::SUPER;
    }
    out
}

fn cell_style(attrs: &CellAttributes) -> Style {
    let mut style = Style::default()
        .fg(cell_color(attrs.foreground()))
        .bg(cell_color(attrs.background()));
    match attrs.intensity() {
        Intensity::Bold => style = style.add_modifier(Modifier::BOLD),
        Intensity::Half => style = style.add_modifier(Modifier::DIM),
        Intensity::Normal => {}
    }
    if attrs.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if attrs.underline() != Underline::None {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if attrs.reverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

/// Map an engine color to ratatui, deferring palette resolution to the host
/// terminal so default and indexed colors follow the user's theme.
fn cell_color(attr: ColorAttribute) -> Color {
    match attr {
        ColorAttribute::Default => Color::Reset,
        ColorAttribute::PaletteIndex(i) => Color::Indexed(i),
        ColorAttribute::TrueColorWithPaletteFallback(c, _)
        | ColorAttribute::TrueColorWithDefaultFallback(c) => {
            let (r, g, b, _) = c.to_srgb_u8();
            Color::Rgb(r, g, b)
        }
    }
}
