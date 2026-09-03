//! A session: one layout tree of windows plus the interaction modes a
//! client drives. It outlives client attachments.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::Terminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers as CtMods,
    MouseButton as CtMouseButton, MouseEvent as CtMouseEvent, MouseEventKind as CtMouseKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use termwiz::cell::{CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::input::{KeyCode, Modifiers as KeyModifiers};
use termwiz::surface::CursorVisibility;
use tui_textarea::TextArea;

use crate::server::agent;
use crate::server::anim::{self, Anim};
use crate::server::config::OscTitles;
use crate::server::ex::{self, ExCommand};
use crate::server::input;
use crate::server::keys::{Command, KeyMatch, KeyTable, KeyTrie};
use crate::server::layout::{self, Dir, Node, Separator, Side, SplitKind, WindowId};
use crate::server::persist;
use crate::server::term::FdBackend;
use crate::server::window::{Notice, Tab, TabId, Window};
use crate::server::{ServerEvent, SessionId};

/// Minimum window size.
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 3;

const RESIZE_REPEAT: Duration = Duration::from_millis(500);

const MOVE_REPEAT: Duration = Duration::from_millis(500);

const SEND_PREFIX_REPEAT: Duration = Duration::from_millis(500);

/// A consequence the server, not the session, must act on.
pub enum Effect {
    Detach,
    OpenSwitcher,
    OpenGrid,
    OpenFinder,
    /// Create and attach to a session, auto-named when `None`.
    NewSession(Option<String>),
    RenameSession(String),
    /// Kill a named session, or the current one if `None`.
    KillSession(Option<String>),
    Copy(String),
    Paste,
    /// Mouse pointer shape, as an OSC 22 name.
    Pointer(&'static str),
    /// Hold this tab as the client's pending yank.
    YankTab(TabId),
    PasteTab,
    ClearYank,
    /// Go to the indicator's tab, which may live in another session.
    GotoIndicator(Indicator),
    /// Jump to the next done or blocked agent tab, across all sessions.
    CycleAgent,
    /// The last window's last tab exited.
    Ended,
}

struct BorderDrag {
    /// Path from the tree root to the dragged split.
    path: Vec<Side>,
    /// A press released without motion is a click on the chrome underneath.
    moved: bool,
}

/// A linear text selection in content-relative cell coordinates.
struct Selection {
    window: WindowId,
    start: (u16, u16),
    end: (u16, u16),
    /// Cleared on release. The selection itself can outlive the drag.
    dragging: bool,
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

/// Screen position to content-relative cell, clamped inside `content`.
fn clamp_to_content(pos: Position, content: Rect) -> (u16, u16) {
    if content.width == 0 || content.height == 0 {
        return (0, 0);
    }
    let x = pos.x.clamp(content.left(), content.right() - 1) - content.x;
    let y = pos.y.clamp(content.top(), content.bottom() - 1) - content.y;
    (x, y)
}

/// The engine encodes the event for the program's mouse protocol and turns
/// wheel ticks into arrow keys on the alternate screen.
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

/// A window control in the tab bar.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Minimize,
    Maximize,
    Exit,
}

/// Three control glyphs, each led by a space.
const CONTROLS_WIDTH: u16 = 6;

struct TabBadge {
    active: bool,
    name: String,
    yanked: bool,
    agent: Option<agent::Visual>,
    /// Columns in the bar, for hit-tests. Must match render_tab_bar's layout.
    span: std::ops::Range<u16>,
}

/// One window's per-frame chrome geometry.
struct Chrome {
    window: WindowId,
    tab_bar: Rect,
    tabs: Vec<TabBadge>,
    scroll: bool,
    /// The active tab's status animation, drawn on the rule while focused.
    rule_anim: Anim,
    rule_color: Color,
    /// None when the bar is too narrow to hold it.
    controls: Option<Rect>,
    maximized: bool,
    hover: Option<Control>,
}

struct MinimizedTitle {
    id: WindowId,
    name: String,
    span: std::ops::Range<u16>,
}

/// Grey 235 (#262626), a neutral shade with no hue.
pub(crate) const CHROME_BG: Color = Color::Indexed(235);

static HOSTNAME: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
});

/// Status-line pointer to an agent tab that finished or got blocked unseen,
/// possibly in another session.
#[derive(Clone, PartialEq)]
pub struct Indicator {
    pub session: SessionId,
    pub window: WindowId,
    pub tab: usize,
    pub text: String,
}

struct StatusChrome {
    row: Rect,
    name: String,
    minimized: Vec<MinimizedTitle>,
    host: String,
    clock: String,
    /// Replaces the hostname when it fits alongside the clock.
    indicator: Option<(String, std::ops::Range<u16>)>,
}

enum PromptKind {
    Ex,
    Rename,
}

struct Prompt {
    kind: PromptKind,
    textarea: TextArea<'static>,
}

impl Prompt {
    fn label(&self) -> &'static str {
        match self.kind {
            PromptKind::Ex => ":",
            PromptKind::Rename => "rename: ",
        }
    }

    fn text(&self) -> String {
        self.textarea.lines().first().cloned().unwrap_or_default()
    }
}

struct PromptChrome {
    /// The whole row, label included.
    line: Rect,
    label: &'static str,
    input: Rect,
    suggestions: Vec<&'static str>,
    suggestion_row: Option<Rect>,
}

struct HintChrome {
    rect: Rect,
    key_width: u16,
    /// `(keys, description)` per row.
    rows: Vec<(String, &'static str)>,
}

/// Everything the draw pass reads, recomputed each frame.
#[derive(Default)]
struct View {
    separators: Vec<Separator>,
    chrome: Vec<Chrome>,
    status: Option<StatusChrome>,
    message: Option<(Rect, String)>,
    prompt: Option<PromptChrome>,
    hints: Option<HintChrome>,
    /// Animation clock for this frame.
    elapsed: Duration,
}

pub struct Session {
    pub name: String,
    tree: Node,
    windows: HashMap<WindowId, Window>,
    focus: WindowId,
    /// The pending chord's keys, `Some` and empty right after the prefix.
    /// Only the resize repeat deadline expires it.
    chord: Option<Vec<KeyMatch>>,
    /// While armed, a bare direction key resizes again.
    resize_repeat: Option<Instant>,
    /// While armed, a bare `H`/`J`/`K`/`L` moves the tab again.
    move_repeat: Option<Instant>,
    /// While armed, a bare prefix press forwards again.
    send_prefix_repeat: Option<Instant>,
    /// View state only: the tree is untouched, focus leaving clears it, and
    /// it is never persisted.
    maximized: Option<WindowId>,
    /// Out of the layout tree but still running, in minimize order.
    minimized: Vec<WindowId>,
    hover: Option<(WindowId, Control)>,
    prompt: Option<Prompt>,
    keys: Arc<KeyTable>,
    copy_on_select: bool,
    osc_titles: OscTitles,
    /// Shown on the bottom row until the next key press.
    message: Option<String>,
    selection: Option<Selection>,
    border_drag: Option<BorderDrag>,
    /// Set by the server each render pass.
    indicator: Option<Indicator>,
    /// Tabs held as pending yanks, set by the server each render pass.
    yanked: Vec<TabId>,
    view: View,
    area: Rect,
    /// The clock text as of the last computed view.
    clock: String,
    next_window_id: WindowId,
    force_redraw: bool,
    tx: Sender<ServerEvent>,
}

impl Session {
    pub fn new(
        name: String,
        area: Rect,
        keys: Arc<KeyTable>,
        copy_on_select: bool,
        osc_titles: OscTitles,
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let first = Window::new(0, tree_area(area), tx.clone())?;
        let mut windows = HashMap::new();
        windows.insert(first.id, first);
        Ok(Self {
            name,
            tree: Node::Leaf(0),
            windows,
            focus: 0,
            chord: None,
            resize_repeat: None,
            move_repeat: None,
            send_prefix_repeat: None,
            maximized: None,
            minimized: Vec::new(),
            hover: None,
            prompt: None,
            keys,
            copy_on_select,
            osc_titles,
            message: None,
            selection: None,
            border_drag: None,
            indicator: None,
            yanked: Vec::new(),
            view: View::default(),
            area,
            clock: String::new(),
            next_window_id: 1,
            force_redraw: true,
            tx,
        })
    }

    /// Windows whose tabs all fail to spawn drop out of the tree. `None`
    /// when no window is left.
    pub fn restore(
        snap: &persist::SessionSnapshot,
        area: Rect,
        keys: Arc<KeyTable>,
        copy_on_select: bool,
        osc_titles: OscTitles,
        tx: Sender<ServerEvent>,
    ) -> Option<Self> {
        let mut tree = Some(persist::restore_node(&snap.tree));
        // Lay out first so PTYs spawn at their final size.
        let rects: HashMap<WindowId, Rect> = layout::compute(tree.as_ref()?, tree_area(area))
            .0
            .into_iter()
            .collect();
        let mut windows = HashMap::new();
        for wsnap in &snap.windows {
            let Some(&rect) = rects.get(&wsnap.id) else {
                // Not a leaf of the saved tree.
                continue;
            };
            if windows.contains_key(&wsnap.id) {
                continue;
            }
            match Window::restore(rect, wsnap, &tx) {
                Some(win) => {
                    windows.insert(wsnap.id, win);
                }
                None => tree = layout::remove_leaf(tree.take()?, wsnap.id),
            }
        }
        // Leaves with no snapshot drop out too.
        let mut tree = tree?;
        for id in layout::leaves(&tree) {
            if !windows.contains_key(&id) {
                tree = layout::remove_leaf(tree, id)?;
            }
        }
        let focus = layout::leaves(&tree).first().copied()?;
        let next_window_id = windows.keys().max().copied().unwrap_or(0) + 1;
        Some(Self {
            name: snap.name.clone(),
            tree,
            windows,
            focus,
            chord: None,
            resize_repeat: None,
            move_repeat: None,
            send_prefix_repeat: None,
            maximized: None,
            minimized: Vec::new(),
            hover: None,
            prompt: None,
            keys,
            copy_on_select,
            osc_titles,
            message: None,
            selection: None,
            border_drag: None,
            indicator: None,
            yanked: Vec::new(),
            view: View::default(),
            area,
            clock: String::new(),
            next_window_id,
            force_redraw: true,
            tx,
        })
    }

    pub fn snapshot(&mut self) -> persist::SessionSnapshot {
        self.refresh_claude_sessions();
        let windows = layout::leaves(&self.tree)
            .into_iter()
            .filter_map(|id| {
                let win = self.windows.get(&id)?;
                let tabs = win
                    .tabs
                    .iter()
                    .map(|tab| {
                        let cwd = tab.working_dir().unwrap_or_else(|| {
                            // No readable cwd. Home beats losing the tab.
                            std::env::var_os("HOME")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|| "/".into())
                        });
                        persist::TabSnapshot {
                            cwd,
                            claude_session: tab.claude_session.clone(),
                            name: tab.is_manually_named().then(|| tab.name.clone()),
                        }
                    })
                    .collect();
                Some(persist::WindowSnapshot {
                    id,
                    active: win.active,
                    tabs,
                })
            })
            .collect();
        persist::SessionSnapshot {
            name: self.name.clone(),
            tree: persist::capture_node(&self.tree),
            windows,
        }
    }

    /// Re-read per save so the id tracks the live session across `/clear`.
    /// A tab with no readable session file keeps the id it has.
    fn refresh_claude_sessions(&mut self) {
        for win in self.windows.values_mut() {
            for tab in win.tabs.iter_mut() {
                if !tab.running_claude {
                    continue;
                }
                if let Some(id) = tab.claude_session_id() {
                    tab.claude_session = Some(id);
                }
            }
        }
    }

    pub fn has_tab(&self, id: TabId) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.id == id))
    }

    pub fn locate_tab(&self, id: TabId) -> Option<(WindowId, usize)> {
        self.windows
            .iter()
            .find_map(|(&wid, w)| w.tabs.iter().position(|t| t.id == id).map(|i| (wid, i)))
    }

    /// Feed PTY output to the tab and re-run name and agent detection.
    pub fn pty_output(&mut self, id: TabId, bytes: &[u8]) -> Option<Notice> {
        let osc_titles = self.osc_titles;
        let tab = self.find_tab_mut(id)?;
        tab.engine.advance_bytes(bytes);
        let (changed, notice) = tab.refresh_identity(osc_titles);
        if changed {
            self.force_redraw = true;
        }
        notice
    }

    pub fn has_pending_idle(&self) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.agent_pending_idle()))
    }

    pub fn has_pending_repeat(&self) -> bool {
        self.resize_repeat.is_some()
            || self.move_repeat.is_some()
            || self.send_prefix_repeat.is_some()
    }

    /// Close any repeat window whose deadline passed.
    pub fn tick_repeats(&mut self, now: Instant) {
        if self.resize_repeat.is_some_and(|deadline| now >= deadline) {
            self.resize_repeat = None;
            self.chord = None;
            self.force_redraw = true;
        }
        if self.move_repeat.is_some_and(|deadline| now >= deadline) {
            self.move_repeat = None;
        }
        if self
            .send_prefix_repeat
            .is_some_and(|deadline| now >= deadline)
        {
            self.send_prefix_repeat = None;
        }
    }

    /// Commit idle debounces that elapsed without more output.
    pub fn tick_agents(&mut self, now: std::time::Instant) -> Vec<Notice> {
        let mut notices = Vec::new();
        for win in self.windows.values_mut() {
            for tab in &mut win.tabs {
                let (changed, notice) = tab.tick_agent(now);
                if changed {
                    self.force_redraw = true;
                }
                notices.extend(notice);
            }
        }
        notices
    }

    /// Tabs resize on the next compute pass.
    pub fn set_area(&mut self, area: Rect) {
        self.area = area;
        self.force_redraw = true;
    }

    pub fn request_redraw(&mut self) {
        self.force_redraw = true;
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn has_agent_tab(&self) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.agent.is_some()))
    }

    /// In layout order, then tab order.
    pub fn agent_tabs(&self) -> Vec<(WindowId, usize)> {
        let mut out = Vec::new();
        for id in layout::leaves(&self.tree) {
            let Some(win) = self.windows.get(&id) else {
                continue;
            };
            for (i, tab) in win.tabs.iter().enumerate() {
                if tab.agent.is_some() {
                    out.push((id, i));
                }
            }
        }
        out
    }

    /// The most pressing agent state across on-screen tabs.
    pub fn urgency(&self) -> Option<agent::Urgency> {
        self.agent_tabs()
            .into_iter()
            .filter_map(|(id, i)| self.tab_at(id, i)?.agent.as_ref()?.urgency())
            .max()
    }

    /// On-screen windows in layout order, then minimized ones.
    pub fn window_order(&self) -> Vec<WindowId> {
        let mut ids = layout::leaves(&self.tree);
        ids.extend(self.minimized.iter().copied());
        ids
    }

    /// Done or blocked agent tabs, in window order.
    pub fn attention_tabs(&self) -> Vec<(WindowId, usize)> {
        let mut out = Vec::new();
        for id in self.window_order() {
            let Some(win) = self.windows.get(&id) else {
                continue;
            };
            for (i, tab) in win.tabs.iter().enumerate() {
                if tab.agent.as_ref().is_some_and(|t| t.needs_attention()) {
                    out.push((id, i));
                }
            }
        }
        out
    }

    pub fn focused_active(&self) -> (WindowId, usize) {
        let active = self.windows.get(&self.focus).map_or(0, |w| w.active);
        (self.focus, active)
    }

    pub fn set_indicator(&mut self, indicator: Option<Indicator>) {
        if self.indicator != indicator {
            self.indicator = indicator;
            self.force_redraw = true;
        }
    }

    pub fn set_yanked(&mut self, yanked: Vec<TabId>) {
        if self.yanked != yanked {
            self.yanked = yanked;
            self.force_redraw = true;
        }
    }

    /// In layout order, then tab order.
    pub fn all_tabs(&self) -> Vec<(WindowId, usize)> {
        let mut out = Vec::new();
        for id in layout::leaves(&self.tree) {
            let Some(win) = self.windows.get(&id) else {
                continue;
            };
            out.extend((0..win.tabs.len()).map(|i| (id, i)));
        }
        out
    }

    pub fn tab_at(&self, window: WindowId, index: usize) -> Option<&Tab> {
        self.windows.get(&window)?.tabs.get(index)
    }

    pub fn tab_at_mut(&mut self, window: WindowId, index: usize) -> Option<&mut Tab> {
        self.windows.get_mut(&window)?.tabs.get_mut(index)
    }

    pub fn key_to_tab(&mut self, window: WindowId, index: usize, key: KeyEvent) {
        if let Some((code, mods)) = map_key(key)
            && let Some(win) = self.windows.get_mut(&window)
            && let Some(tab) = win.tabs.get_mut(index)
        {
            let _ = tab.engine.key_down(code, mods);
        }
    }

    pub fn paste_to_tab(&mut self, window: WindowId, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(win) = self.windows.get_mut(&window)
            && let Some(tab) = win.tabs.get_mut(index)
        {
            let _ = tab.engine.send_paste(text);
        }
    }

    pub fn goto_tab(&mut self, window: WindowId, index: usize) {
        if self.minimized.contains(&window) {
            self.restore_window(window);
            if self.minimized.contains(&window) {
                return;
            }
            self.focus_tab(window, index);
            self.maximized = Some(window);
            return;
        }
        self.focus_tab(window, index);
    }

    pub fn focus_tab(&mut self, window: WindowId, index: usize) {
        let Some(win) = self.windows.get_mut(&window) else {
            return;
        };
        if index < win.tabs.len() && win.active != index {
            win.active = index;
            self.drop_selection_in(window);
        }
        self.set_focus(window);
        self.force_redraw = true;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if self.message.take().is_some() {
            self.force_redraw = true;
        }
        if self.prompt.is_some() {
            return self.handle_prompt_key(key);
        }
        // A key racing the timer must not land in a repeat window that
        // already expired.
        self.tick_repeats(Instant::now());
        if let Some(mut path) = self.chord.take() {
            // Re-armed below if this key resizes again.
            self.resize_repeat = None;
            // The hint popup changes or closes either way.
            self.force_redraw = true;
            // Escape right after the prefix also clears the pending yank.
            if key.code == CtKeyCode::Esc {
                return path.is_empty().then_some(Effect::ClearYank);
            }
            let node = self
                .keys
                .node_at(&path)
                .expect("pending chord path resolves to a node");
            match node.get(KeyMatch::from_event(key)) {
                // A resize keeps the submap open so a bare direction key
                // resizes again.
                Some(&KeyTrie::Command(command)) => {
                    if matches!(command, Command::ResizeDir(_)) {
                        self.chord = Some(path);
                        self.resize_repeat = Some(Instant::now() + RESIZE_REPEAT);
                    }
                    return self.execute(command);
                }
                Some(KeyTrie::Node(_)) => {
                    path.push(KeyMatch::from_event(key));
                    self.chord = Some(path);
                }
                // An unbound key ends the chord and goes nowhere.
                None => {}
            }
            return None;
        }
        // Any other key closes the move-repeat window and goes nowhere.
        if self.move_repeat.take().is_some() {
            if let Some(dir) = move_repeat_dir(key) {
                return self.execute(Command::MoveTabDir(dir));
            }
            return None;
        }
        if self.send_prefix_repeat.take().is_some() && self.keys.is_prefix(key) {
            return self.execute(Command::SendPrefix);
        }
        if self.keys.is_prefix(key) {
            self.chord = Some(Vec::new());
            self.force_redraw = true;
            return None;
        }
        // Scroll mode swallows every key.
        if let Some(win) = self.windows.get_mut(&self.focus)
            && win.active_tab().scroll_mode()
        {
            let page = win.content_rect().height.max(1) as isize;
            let tab = win.active_tab_mut();
            match key.code {
                CtKeyCode::Char('k') | CtKeyCode::Up => {
                    tab.scroll_by(-1);
                }
                CtKeyCode::Char('j') | CtKeyCode::Down => {
                    tab.scroll_by(1);
                }
                CtKeyCode::PageUp => {
                    tab.scroll_by(-page);
                }
                CtKeyCode::PageDown => {
                    tab.scroll_by(page);
                }
                CtKeyCode::Esc | CtKeyCode::Char('q') => tab.exit_scroll_mode(),
                _ => {}
            }
            self.force_redraw = true;
            return None;
        }
        // A write fails once the child exits. The exit event follows.
        if let Some((code, mods)) = map_key(key)
            && let Some(win) = self.windows.get_mut(&self.focus)
        {
            let _ = win.active_tab_mut().engine.key_down(code, mods);
        }
        None
    }

    fn write_prefix_key(&mut self) {
        let prefix = self.keys.prefix;
        let mut mods = CtMods::NONE;
        if prefix.ctrl {
            mods |= CtMods::CONTROL;
        }
        if prefix.shift {
            mods |= CtMods::SHIFT;
        }
        if let Some((code, mods)) = map_key(KeyEvent::new(prefix.code, mods))
            && let Some(win) = self.windows.get_mut(&self.focus)
        {
            let _ = win.active_tab_mut().engine.key_down(code, mods);
        }
    }

    /// Both terminal and right-click pastes come through here.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.textarea.insert_str(input::prompt_paste(text));
            self.force_redraw = true;
            return;
        }
        if let Some(win) = self.windows.get_mut(&self.focus) {
            let _ = win.active_tab_mut().engine.send_paste(text);
        }
    }

    pub fn handle_mouse(&mut self, mouse: CtMouseEvent) -> Option<Effect> {
        let pos = Position::new(mouse.column, mouse.row);
        // Shift bypasses a program's mouse grab.
        let shift = mouse.modifiers.contains(CtMods::SHIFT);
        if self.border_drag.is_some() {
            return self.drag_border(&mouse, pos);
        }
        match mouse.kind {
            CtMouseKind::Down(button) => {
                if button == CtMouseButton::Left
                    && let Some(id) = self.minimized_title_at(pos)
                {
                    self.restore_window(id);
                    return None;
                }
                if button == CtMouseButton::Left
                    && let Some(indicator) = self.indicator_at(pos)
                {
                    return Some(Effect::GotoIndicator(indicator));
                }
                if button == CtMouseButton::Left && self.menu_icon_at(pos) {
                    return Some(Effect::OpenSwitcher);
                }
                // A press on a boundary starts a drag. A click on the chrome
                // underneath resolves on release if nothing moved.
                if button == CtMouseButton::Left
                    && self.maximized.is_none()
                    && let Some((path, _)) =
                        layout::boundary_at(&self.tree, tree_area(self.area), pos)
                {
                    self.border_drag = Some(BorderDrag { path, moved: false });
                    return None;
                }
                let id = self.window_at(pos)?;
                if self.focus != id {
                    self.set_focus(id);
                    self.force_redraw = true;
                }
                if button == CtMouseButton::Left
                    && let Some(control) = self.control_at(id, pos)
                {
                    self.click_control(id, control);
                    return None;
                }
                if button == CtMouseButton::Left
                    && let Some(index) = self.tab_badge_at(id, pos)
                {
                    self.select_tab(index);
                    return None;
                }
                if button == CtMouseButton::Middle
                    && let Some(index) = self.tab_badge_at(id, pos)
                {
                    if let Some(tab) = self.tab_at_mut(id, index) {
                        tab.kill();
                    }
                    return None;
                }
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                if tab.engine.is_mouse_grabbed() && !shift {
                    forward_mouse(tab, &mouse, content);
                    return None;
                }
                match button {
                    CtMouseButton::Left if content.contains(pos) => {
                        let cell = clamp_to_content(pos, content);
                        self.selection = Some(Selection {
                            window: id,
                            start: cell,
                            end: cell,
                            dragging: true,
                        });
                        self.force_redraw = true;
                    }
                    CtMouseButton::Right => {
                        return if self.selection.is_some() {
                            self.yank_selection()
                        } else {
                            Some(Effect::Paste)
                        };
                    }
                    _ => {}
                }
            }
            CtMouseKind::Drag(CtMouseButton::Left) if self.selection.is_some() => {
                let sel = self.selection.as_mut().expect("checked above");
                let win = self.windows.get(&sel.window)?;
                sel.end = clamp_to_content(pos, win.content_rect());
                self.force_redraw = true;
            }
            CtMouseKind::Up(_) | CtMouseKind::Drag(_) | CtMouseKind::Moved => {
                // A click without motion selects nothing.
                if matches!(mouse.kind, CtMouseKind::Up(CtMouseButton::Left))
                    && self.selection.as_ref().is_some_and(|s| s.start == s.end)
                {
                    self.selection = None;
                    self.force_redraw = true;
                }
                if let Some(id) = self.window_at(pos) {
                    let win = self.windows.get_mut(&id).expect("window exists");
                    let content = win.content_rect();
                    let tab = win.active_tab_mut();
                    if tab.engine.is_mouse_grabbed() && !shift {
                        forward_mouse(tab, &mouse, content);
                    }
                }
                // Copy-on-select yanks on release but keeps the highlight.
                if matches!(mouse.kind, CtMouseKind::Up(CtMouseButton::Left))
                    && let Some(sel) = self.selection.as_mut()
                    && sel.dragging
                {
                    sel.dragging = false;
                    if self.copy_on_select
                        && let Some(text) = self.selection_text()
                    {
                        self.message = Some(format!(
                            "copied {}",
                            count(text.chars().count(), "character")
                        ));
                        self.force_redraw = true;
                        return Some(Effect::Copy(text));
                    }
                }
                if mouse.kind == CtMouseKind::Moved {
                    let hover = self
                        .window_at(pos)
                        .and_then(|id| self.control_at(id, pos).map(|c| (id, c)));
                    if hover != self.hover {
                        self.hover = hover;
                        self.force_redraw = true;
                    }
                    return Some(Effect::Pointer(self.pointer_shape(pos)));
                }
            }
            CtMouseKind::ScrollUp | CtMouseKind::ScrollDown => {
                let id = self.window_at(pos)?;
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                // On the alternate screen the engine turns wheel ticks into
                // arrow keys.
                if tab.engine.is_mouse_grabbed() || tab.engine.is_alt_screen_active() {
                    forward_mouse(tab, &mouse, content);
                    return None;
                }
                tab.enter_scroll_mode();
                let delta = if mouse.kind == CtMouseKind::ScrollUp {
                    -3
                } else {
                    3
                };
                // Reaching the live bottom resumes following, so a stray
                // wheel-down can't trap the view.
                if tab.scroll_by(delta) {
                    tab.exit_scroll_mode();
                }
                self.set_focus(id);
                self.force_redraw = true;
            }
            _ => {}
        }
        None
    }

    /// A release without motion is a plain click on the chrome underneath.
    fn drag_border(&mut self, mouse: &CtMouseEvent, pos: Position) -> Option<Effect> {
        let drag = self.border_drag.as_mut().expect("drag in progress");
        match mouse.kind {
            CtMouseKind::Drag(CtMouseButton::Left) => {
                drag.moved = true;
                let path = drag.path.clone();
                if layout::drag_boundary(
                    &mut self.tree,
                    tree_area(self.area),
                    &path,
                    pos,
                    (MIN_COLS, MIN_ROWS),
                ) {
                    self.force_redraw = true;
                }
            }
            CtMouseKind::Up(CtMouseButton::Left) => {
                let moved = drag.moved;
                self.border_drag = None;
                if !moved && let Some(id) = self.window_at(pos) {
                    if self.focus != id {
                        self.set_focus(id);
                        self.force_redraw = true;
                    }
                    if let Some(control) = self.control_at(id, pos) {
                        self.click_control(id, control);
                    } else if let Some(index) = self.tab_badge_at(id, pos) {
                        self.select_tab(index);
                    }
                }
            }
            _ => {}
        }
        None
    }

    /// An OSC 22 pointer name. Terminals without pointer support ignore it.
    fn pointer_shape(&self, pos: Position) -> &'static str {
        // Controls and badges win over a tab bar's drag boundary.
        if self.window_at(pos).is_some_and(|id| {
            self.control_at(id, pos).is_some() || self.tab_badge_at(id, pos).is_some()
        }) {
            return "pointer";
        }
        if self.menu_icon_at(pos)
            || self.minimized_title_at(pos).is_some()
            || self.indicator_at(pos).is_some()
        {
            return "pointer";
        }
        if self.maximized.is_some() {
            return "default";
        }
        match layout::boundary_at(&self.tree, tree_area(self.area), pos) {
            Some((_, SplitKind::SideBySide)) => "ew-resize",
            Some((_, SplitKind::Stacked)) => "ns-resize",
            None => "default",
        }
    }

    fn window_at(&self, pos: Position) -> Option<WindowId> {
        // Only the maximized window is on screen. The rest keep stale rects.
        if let Some(id) = self.maximized {
            return self
                .windows
                .get(&id)
                .filter(|w| w.rect.contains(pos))
                .map(|w| w.id);
        }
        self.windows
            .values()
            // Minimized windows keep stale rects too.
            .find(|w| w.rect.contains(pos) && !self.minimized.contains(&w.id))
            .map(|w| w.id)
    }

    fn yank_selection(&mut self) -> Option<Effect> {
        let text = self.selection_text();
        self.selection = None;
        self.force_redraw = true;
        text.map(Effect::Copy)
    }

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

    fn execute(&mut self, command: Command) -> Option<Effect> {
        match command {
            Command::SplitSideBySide => self.split(SplitKind::SideBySide),
            Command::SplitStacked => self.split(SplitKind::Stacked),
            Command::NewTab => self.new_tab(),
            Command::NextTab => self.cycle_tab(1),
            Command::PrevTab => self.cycle_tab(-1),
            Command::SelectTab(index) => self.select_tab(index),
            Command::OnlyWindow => self.only_window(),
            Command::FocusDir(dir) => self.focus_dir(dir),
            Command::ResizeDir(dir) => self.resize_focused(dir),
            Command::Rebalance => {
                layout::rebalance(&mut self.tree);
                self.force_redraw = true;
            }
            Command::MoveTabDir(dir) => {
                if self.move_tab_dir(dir) {
                    self.move_repeat = Some(Instant::now() + MOVE_REPEAT);
                }
            }
            Command::SwapDir(dir) => self.swap_dir(dir),
            Command::Maximize => {
                self.maximized = (self.maximized != Some(self.focus)).then_some(self.focus);
                self.force_redraw = true;
            }
            Command::Rotate => {
                if layout::rotate(&mut self.tree, self.focus) {
                    self.force_redraw = true;
                }
            }
            Command::Detach => return Some(Effect::Detach),
            Command::CycleAgent => return Some(Effect::CycleAgent),
            Command::Switcher => return Some(Effect::OpenSwitcher),
            Command::Grid => return Some(Effect::OpenGrid),
            Command::FindTab => return Some(Effect::OpenFinder),
            Command::OpenEx => self.open_prompt(PromptKind::Ex, String::new()),
            Command::RenameTab => {
                let name = self.windows[&self.focus].active_tab().name.clone();
                self.open_prompt(PromptKind::Rename, name);
            }
            // The exit event removes the tab.
            Command::CloseTab => {
                if let Some(win) = self.windows.get_mut(&self.focus) {
                    win.active_tab_mut().kill();
                }
            }
            Command::CloseWindow => self.close_window(),
            Command::YankTab => {
                let id = self.windows[&self.focus].active_tab().id;
                return Some(Effect::YankTab(id));
            }
            Command::PasteTab => return Some(Effect::PasteTab),
            Command::SendPrefix => {
                self.write_prefix_key();
                self.send_prefix_repeat = Some(Instant::now() + SEND_PREFIX_REPEAT);
            }
            Command::ScrollMode => {
                if let Some(win) = self.windows.get_mut(&self.focus) {
                    win.active_tab_mut().enter_scroll_mode();
                    self.force_redraw = true;
                }
            }
        }
        None
    }

    fn open_prompt(&mut self, kind: PromptKind, text: String) {
        let mut textarea = TextArea::from([text]);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        // The default cursor-line underline looks like stray chrome here.
        textarea.set_cursor_line_style(Style::default());
        self.prompt = Some(Prompt { kind, textarea });
        self.force_redraw = true;
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.force_redraw = true;
        match key.code {
            CtKeyCode::Esc => {
                self.prompt = None;
            }
            CtKeyCode::Enter => {
                let prompt = self.prompt.take().expect("prompt is open");
                let text = prompt.text();
                match prompt.kind {
                    PromptKind::Ex => match ex::parse(&text) {
                        Some(ExCommand::SplitSideBySide) => self.split(SplitKind::SideBySide),
                        Some(ExCommand::SplitStacked) => self.split(SplitKind::Stacked),
                        Some(ExCommand::Write(path)) => self.write_tab_content(&path),
                        Some(ExCommand::NewSession(name)) => {
                            return Some(Effect::NewSession(name));
                        }
                        Some(ExCommand::RenameSession(name)) => {
                            return Some(Effect::RenameSession(name));
                        }
                        Some(ExCommand::KillSession(name)) => {
                            return Some(Effect::KillSession(name));
                        }
                        None => {}
                    },
                    PromptKind::Rename => {
                        let osc_titles = self.osc_titles;
                        let win = self
                            .windows
                            .get_mut(&self.focus)
                            .expect("focused window exists");
                        let tab = win.active_tab_mut();
                        if text.is_empty() {
                            tab.clear_name(osc_titles);
                        } else {
                            tab.set_name(text);
                        }
                    }
                }
            }
            _ => {
                let prompt = self.prompt.as_mut().expect("prompt is open");
                prompt.textarea.input(tui_textarea::Input::from(key));
            }
        }
        None
    }

    /// Scrollback included. A failed write is dropped, since there is no
    /// error surface yet.
    fn write_tab_content(&mut self, path: &std::path::Path) {
        let tab = self.windows[&self.focus].active_tab();
        let screen = tab.engine.screen();
        // `scrollback_rows` counts every row, visible grid included.
        let all = 0..screen.scrollback_rows();
        let mut out = String::new();
        for line in screen.lines_in_phys_range(all) {
            out.push_str(line.as_str().trim_end());
            out.push('\n');
        }
        // The grid's blank tail rows aren't content.
        let trimmed = out.trim_end_matches('\n');
        let out = if trimmed.is_empty() {
            String::new()
        } else {
            format!("{trimmed}\n")
        };
        let path = expand_tilde(path);
        if std::fs::write(&path, &out).is_err() {
            return;
        }
        // A relative path resolves against the server's directory, so show
        // where it really landed.
        let full = std::fs::canonicalize(&path).unwrap_or(path);
        self.message = Some(format!(
            "wrote {} ({})",
            full.display(),
            count(out.len(), "byte")
        ));
        self.force_redraw = true;
    }

    fn find_tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.windows.values_mut().find_map(|w| w.find_tab_mut(id))
    }

    fn split(&mut self, kind: SplitKind) {
        // The tree's rectangle, not the maximized override, decides what
        // fits.
        let Some(&(_, rect)) = self.layout_rects().iter().find(|(id, _)| *id == self.focus) else {
            return;
        };
        let (first, second, _) = layout::split_areas(kind, 0.5, rect);
        for half in [first, second] {
            if half.width < MIN_COLS || half.height < MIN_ROWS {
                return;
            }
        }
        let id = self.next_window_id;
        // If the shell can't spawn, keep the current layout.
        let Ok(win) = Window::new(id, second, self.tx.clone()) else {
            return;
        };
        self.next_window_id += 1;
        self.windows.insert(id, win);
        layout::split_leaf(&mut self.tree, self.focus, kind, id);
        self.set_focus(id);
        self.force_redraw = true;
    }

    fn new_tab(&mut self) {
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        let cwd = win.active_tab().working_dir();
        let Ok(tab) = Tab::spawn(win.content_rect(), cwd, self.tx.clone()) else {
            return;
        };
        win.tabs.push(tab);
        win.active = win.tabs.len() - 1;
        self.drop_selection_in(self.focus);
        self.force_redraw = true;
    }

    fn cycle_tab(&mut self, step: isize) {
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        let len = win.tabs.len() as isize;
        win.active = (win.active as isize + step).rem_euclid(len) as usize;
        self.drop_selection_in(self.focus);
        self.force_redraw = true;
    }

    fn select_tab(&mut self, index: usize) {
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        if index >= win.tabs.len() || index == win.active {
            return;
        }
        win.active = index;
        self.drop_selection_in(self.focus);
        self.force_redraw = true;
    }

    fn tab_badge_at(&self, id: WindowId, pos: Position) -> Option<usize> {
        let chrome = self.view.chrome.iter().find(|c| c.window == id)?;
        let bar = chrome.tab_bar;
        if bar.height == 0 || pos.y != bar.y {
            return None;
        }
        chrome.tabs.iter().position(|b| b.span.contains(&pos.x))
    }

    /// Each control's click target is its glyph plus the space before it.
    fn control_at(&self, id: WindowId, pos: Position) -> Option<Control> {
        let chrome = self.view.chrome.iter().find(|c| c.window == id)?;
        let controls = chrome.controls?;
        if !controls.contains(pos) {
            return None;
        }
        Some(match (pos.x - controls.x) / 2 {
            0 => Control::Minimize,
            1 => Control::Maximize,
            _ => Control::Exit,
        })
    }

    fn minimized_title_at(&self, pos: Position) -> Option<WindowId> {
        let status = self.view.status.as_ref()?;
        if pos.y != status.row.y {
            return None;
        }
        status
            .minimized
            .iter()
            .find(|t| t.span.contains(&pos.x))
            .map(|t| t.id)
    }

    fn menu_icon_at(&self, pos: Position) -> bool {
        self.view
            .status
            .as_ref()
            .is_some_and(|s| pos.y == s.row.y && pos.x == s.row.x)
    }

    fn indicator_at(&self, pos: Position) -> Option<Indicator> {
        let status = self.view.status.as_ref()?;
        let (_, span) = status.indicator.as_ref()?;
        if pos.y != status.row.y || !span.contains(&pos.x) {
            return None;
        }
        self.indicator.clone()
    }

    fn minimized_titles(&self, row: Rect, mut x: u16) -> Vec<MinimizedTitle> {
        let mut titles = Vec::new();
        for &id in &self.minimized {
            let Some(win) = self.windows.get(&id) else {
                continue;
            };
            let name = win.active_tab().name.clone();
            let start = x.saturating_add(2).min(row.right());
            let end = start
                .saturating_add(name.chars().count() as u16)
                .min(row.right());
            x = end;
            titles.push(MinimizedTitle {
                id,
                name,
                span: start..end,
            });
        }
        titles
    }

    fn click_control(&mut self, id: WindowId, control: Control) {
        match control {
            Control::Minimize => self.minimize_window(id),
            Control::Maximize => {
                self.maximized = (self.maximized != Some(id)).then_some(id);
                self.force_redraw = true;
            }
            Control::Exit => self.kill_window(id),
        }
    }

    fn minimize_window(&mut self, id: WindowId) {
        let ids = layout::leaves(&self.tree);
        if ids.len() <= 1 || !ids.contains(&id) {
            return;
        }
        if self.maximized == Some(id) {
            self.maximized = None;
        }
        self.drop_selection_in(id);
        if self.focus == id {
            let pos = ids.iter().position(|i| *i == id).unwrap_or(0);
            self.set_focus(ids[(pos + 1) % ids.len()]);
        }
        let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
        if let Some(tree) = layout::remove_leaf(tree, id) {
            self.tree = tree;
        }
        self.minimized.push(id);
        self.force_redraw = true;
    }

    /// Splits the focused window. Fails silently if the halves would be
    /// too small.
    fn restore_window(&mut self, id: WindowId) {
        let Some(pos) = self.minimized.iter().position(|m| *m == id) else {
            return;
        };
        let Some(&(_, rect)) = self.layout_rects().iter().find(|(w, _)| *w == self.focus) else {
            return;
        };
        let kind = if rect.width > rect.height {
            SplitKind::SideBySide
        } else {
            SplitKind::Stacked
        };
        let (first, second, _) = layout::split_areas(kind, 0.5, rect);
        for half in [first, second] {
            if half.width < MIN_COLS || half.height < MIN_ROWS {
                return;
            }
        }
        self.minimized.remove(pos);
        layout::split_leaf(&mut self.tree, self.focus, kind, id);
        self.set_focus(id);
        self.force_redraw = true;
    }

    fn kill_window(&mut self, id: WindowId) {
        if let Some(win) = self.windows.get_mut(&id) {
            for tab in &mut win.tabs {
                tab.kill();
            }
        }
    }

    /// Call when the window's visible content changes.
    fn drop_selection_in(&mut self, window: WindowId) {
        if self.selection.as_ref().is_some_and(|s| s.window == window) {
            self.selection = None;
        }
    }

    fn set_focus(&mut self, id: WindowId) {
        self.focus = id;
        if self.maximized.is_some_and(|m| m != id) {
            self.maximized = None;
        }
    }

    /// From the tree, ignoring the maximized override.
    fn layout_rects(&self) -> Vec<(WindowId, Rect)> {
        layout::compute(&self.tree, tree_area(self.area)).0
    }

    fn focus_dir(&mut self, dir: Dir) {
        let rects = self.layout_rects();
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return;
        };
        if let Some(id) = layout::spatial_neighbor(&rects, from, dir) {
            self.set_focus(id);
            self.force_redraw = true;
        }
    }

    /// Focus follows the moved tab.
    fn move_tab_dir(&mut self, dir: Dir) -> bool {
        let rects = self.layout_rects();
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return false;
        };
        let Some(dest) = layout::spatial_neighbor(&rects, from, dir) else {
            return false;
        };
        let source = self.focus;
        let win = self
            .windows
            .get_mut(&source)
            .expect("focused window exists");
        let tab = win.tabs.remove(win.active);
        if win.active == win.tabs.len() && win.active > 0 {
            win.active -= 1;
        }
        let emptied = win.tabs.is_empty();
        self.drop_selection_in(source);
        self.drop_selection_in(dest);
        let dest_win = self.windows.get_mut(&dest).expect("adjacent window exists");
        dest_win.tabs.push(tab);
        dest_win.active = dest_win.tabs.len() - 1;
        let content = dest_win.content_rect();
        dest_win.active_tab_mut().resize(content);
        self.set_focus(dest);
        if emptied {
            self.windows.remove(&source);
            let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
            if let Some(tree) = layout::remove_leaf(tree, source) {
                self.tree = tree;
            }
        }
        self.force_redraw = true;
        true
    }

    fn swap_dir(&mut self, dir: Dir) {
        let rects = self.layout_rects();
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return;
        };
        let Some(other) = layout::spatial_neighbor(&rects, from, dir) else {
            return;
        };
        if layout::swap_leaves(&mut self.tree, self.focus, other) {
            self.force_redraw = true;
        }
    }

    fn close_window(&mut self) {
        self.kill_window(self.focus);
    }

    /// Kill every other window's processes. The exit events collapse the tree.
    fn only_window(&mut self) {
        let focus = self.focus;
        for (_, win) in self.windows.iter_mut().filter(|(id, _)| **id != focus) {
            for tab in &mut win.tabs {
                tab.kill();
            }
        }
    }

    /// Move the focused window's boundary one cell in `dir`.
    fn resize_focused(&mut self, dir: Dir) {
        if layout::resize_toward(&mut self.tree, tree_area(self.area), self.focus, dir) {
            self.force_redraw = true;
        }
    }

    pub fn pty_exited(&mut self, id: TabId) -> Option<Effect> {
        let win_id = self
            .windows
            .values()
            .find(|w| w.tabs.iter().any(|t| t.id == id))?
            .id;
        let win = self.windows.get_mut(&win_id).expect("window exists");
        if win.tabs.len() > 1 {
            let idx = win
                .tabs
                .iter()
                .position(|t| t.id == id)
                .expect("tab exists");
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
        let mut win = self.windows.remove(&win_id).expect("window exists");
        win.tabs.pop().expect("last tab exists").wait();
        self.collapse_window(win_id)
    }

    /// The window must already be out of `windows`.
    fn collapse_window(&mut self, win_id: WindowId) -> Option<Effect> {
        if self.windows.is_empty() {
            return Some(Effect::Ended);
        }
        self.drop_selection_in(win_id);
        // The tree never held a minimized window.
        if let Some(pos) = self.minimized.iter().position(|m| *m == win_id) {
            self.minimized.remove(pos);
            self.force_redraw = true;
            return None;
        }
        let ids = layout::leaves(&self.tree);
        // The oldest minimized window takes over rather than the session
        // ending under it.
        if ids == [win_id] {
            let restored = self.minimized.remove(0);
            self.tree = Node::Leaf(restored);
            self.set_focus(restored);
            self.force_redraw = true;
            return None;
        }
        // Refocus before the leaf leaves the tree.
        if self.focus == win_id {
            let pos = ids.iter().position(|i| *i == win_id).unwrap_or(0);
            self.set_focus(ids[(pos + 1) % ids.len()]);
        }
        let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
        if let Some(tree) = layout::remove_leaf(tree, win_id) {
            self.tree = tree;
        }
        self.force_redraw = true;
        None
    }

    /// The flag is true when removing the tab emptied the session.
    pub fn extract_tab(&mut self, id: TabId) -> Option<(Tab, bool)> {
        let (win_id, idx) = self.locate_tab(id)?;
        let win = self.windows.get_mut(&win_id).expect("window exists");
        if win.tabs.len() > 1 {
            let active_removed = idx == win.active;
            let tab = win.tabs.remove(idx);
            if idx < win.active || win.active == win.tabs.len() {
                win.active -= 1;
            }
            if active_removed {
                self.drop_selection_in(win_id);
            }
            self.force_redraw = true;
            return Some((tab, false));
        }
        let mut win = self.windows.remove(&win_id).expect("window exists");
        let tab = win.tabs.pop().expect("last tab exists");
        let ended = matches!(self.collapse_window(win_id), Some(Effect::Ended));
        self.force_redraw = true;
        Some((tab, ended))
    }

    pub fn insert_tab(&mut self, tab: Tab) {
        self.drop_selection_in(self.focus);
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        win.tabs.push(tab);
        win.active = win.tabs.len() - 1;
        let content = win.content_rect();
        win.active_tab_mut().resize(content);
        self.force_redraw = true;
    }

    pub fn needs_redraw(&self) -> bool {
        self.force_redraw
            || self.clock != clock_now()
            || self.has_animation()
            || self.windows.values().any(|w| {
                // Minimized windows don't draw.
                if self.minimized.contains(&w.id) {
                    return false;
                }
                let tab = w.active_tab();
                tab.engine.current_seqno() != tab.drawn_seqno
            })
    }

    /// The indicator always shimmers, so it counts on its own.
    pub fn has_animation(&self) -> bool {
        self.indicator.is_some()
            || self.windows.values().any(|w| {
                !self.minimized.contains(&w.id)
                    && w.tabs
                        .iter()
                        .any(|t| t.agent.as_ref().is_some_and(agent::Tracker::animated))
            })
    }

    pub fn draw_frame(&mut self, tui: &mut Terminal<FdBackend>) -> anyhow::Result<()> {
        self.compute_view();
        tui.draw(|frame| self.render(frame))?;
        self.force_redraw = false;
        for win in self.windows.values_mut() {
            let tab = win.active_tab_mut();
            tab.drawn_seqno = tab.engine.current_seqno();
        }
        Ok(())
    }

    /// Unlike `draw_frame`, leaves the seqno bookkeeping alone.
    pub fn render_preview(&mut self, buf: &mut Buffer, area: Rect) {
        self.compute_view();
        let full = Rect::new(0, 0, self.area.width, self.area.height);
        if full.width == 0 || full.height == 0 {
            return;
        }
        let mut tmp = Buffer::empty(full);
        self.render_to_buffer(&mut tmp);
        for y in 0..area.height.min(full.height) {
            for x in 0..area.width.min(full.width) {
                if let (Some(dst), Some(src)) = (
                    buf.cell_mut(Position::new(area.x + x, area.y + y)),
                    tmp.cell(Position::new(x, y)),
                ) {
                    *dst = src.clone();
                }
            }
        }
    }

    fn compute_view(&mut self) {
        let (rects, separators) = match self.maximized {
            Some(id) => (vec![(id, tree_area(self.area))], Vec::new()),
            None => layout::compute(&self.tree, tree_area(self.area)),
        };
        let now = Instant::now();
        let yanked = self.yanked.clone();
        let mut chrome = Vec::with_capacity(rects.len());
        for (id, rect) in rects {
            let Some(win) = self.windows.get_mut(&id) else {
                continue;
            };
            win.rect = rect;
            win.reconcile();
            // The focused tab counts as seen once rendered.
            if id == self.focus
                && let Some(tracker) = &mut win.active_tab_mut().agent
            {
                tracker.mark_seen();
            }
            let active = win.active;
            let bar = win.tab_bar_rect();
            // Room for the controls past the two-cell rule lead-in.
            let controls = (bar.height > 0 && bar.width >= CONTROLS_WIDTH + 2)
                .then(|| Rect::new(bar.right() - CONTROLS_WIDTH, bar.y, CONTROLS_WIDTH, 1));
            let badges_end = controls.map_or(bar.right(), |c| c.x);
            let visuals: Vec<Option<agent::Visual>> = win
                .tabs
                .iter()
                .map(|t| t.agent.as_ref().map(|a| a.visual(now)))
                .collect();
            let marks: Vec<bool> = win.tabs.iter().map(|t| yanked.contains(&t.id)).collect();
            let fixed: usize = visuals
                .iter()
                .zip(&marks)
                .enumerate()
                .map(|(i, (v, &mark))| {
                    format!(" {}:", i).chars().count()
                        + v.as_ref().map_or(0, |v| 1 + v.text.chars().count())
                        + mark as usize
                        + 1
                })
                .sum();
            let name_lens: Vec<usize> = win.tabs.iter().map(|t| t.name.chars().count()).collect();
            let avail = badges_end.saturating_sub(bar.x.saturating_add(2)) as usize;
            let widths = allocate_name_widths(&name_lens, avail.saturating_sub(fixed));
            // Spans must match render_tab_bar's layout.
            let mut next_x = bar.x.saturating_add(2).min(badges_end);
            let tabs: Vec<TabBadge> = win
                .tabs
                .iter()
                .zip(visuals)
                .enumerate()
                .map(|(i, (tab, agent))| {
                    let name = truncate_name(&tab.name, widths[i]);
                    let mut width = format!(" {}:{}", i, name).chars().count() as u16;
                    width += marks[i] as u16;
                    if let Some(visual) = &agent {
                        width += 1 + visual.text.chars().count() as u16;
                    }
                    width += 1;
                    let start = next_x;
                    next_x = next_x.saturating_add(width).min(badges_end);
                    TabBadge {
                        active: i == active,
                        name,
                        yanked: marks[i],
                        agent,
                        span: start..next_x,
                    }
                })
                .collect();
            let (rule_anim, rule_color) = tabs
                .get(active)
                .and_then(|badge| badge.agent.as_ref())
                .map_or((Anim::None, Color::Reset), |visual| {
                    (visual.anim, visual.color)
                });
            chrome.push(Chrome {
                window: id,
                tab_bar: bar,
                tabs,
                scroll: win.active_tab().scroll_mode(),
                rule_anim,
                rule_color,
                controls,
                maximized: self.maximized == Some(id),
                hover: self
                    .hover
                    .and_then(|(win, control)| (win == id).then_some(control)),
            });
        }
        self.clock = clock_now();
        let prompt = self.compute_prompt_chrome();
        // A prompt outranks a message, which outranks the status line.
        let message = (prompt.is_none() && self.area.height > 0 && self.area.width > 0)
            .then(|| {
                self.message.as_ref().map(|text| {
                    let row = Rect::new(self.area.x, self.area.bottom() - 1, self.area.width, 1);
                    (row, text.clone())
                })
            })
            .flatten();
        let status =
            (prompt.is_none() && message.is_none() && self.area.height > 0 && self.area.width > 0)
                .then(|| {
                    let row = Rect::new(self.area.x, self.area.bottom() - 1, self.area.width, 1);
                    let name_end = row
                        .x
                        .saturating_add(2 + self.name.chars().count() as u16)
                        .min(row.right());
                    let indicator = self.indicator.as_ref().and_then(|ind| {
                        let ind_len = ind.text.chars().count() as u16;
                        let len = ind_len + 2 + self.clock.chars().count() as u16 + 1;
                        (row.width >= len).then(|| {
                            let start = row.right() - len;
                            (ind.text.clone(), start..start + ind_len)
                        })
                    });
                    StatusChrome {
                        row,
                        name: self.name.clone(),
                        minimized: self.minimized_titles(row, name_end),
                        host: HOSTNAME.clone(),
                        clock: self.clock.clone(),
                        indicator,
                    }
                });
        self.view = View {
            separators,
            chrome,
            status,
            message,
            prompt,
            hints: self.compute_hint_chrome(),
            elapsed: anim::elapsed(),
        };
    }

    /// Bottom-right corner, one row above the status row.
    fn compute_hint_chrome(&self) -> Option<HintChrome> {
        let path = self.chord.as_ref()?;
        let rows = self.keys.node_at(path)?.hints();
        let width = |s: &str| s.chars().count() as u16;
        let key_width = rows.iter().map(|(keys, _)| width(keys)).max()?;
        let body = rows
            .iter()
            .map(|(_, desc)| key_width + 2 + width(desc))
            .max()?;
        // One cell of border plus one of margin on each side.
        let w = body + 4;
        let h = rows.len() as u16 + 2;
        let rect = self.area.intersection(Rect::new(
            self.area.right().saturating_sub(w),
            self.area.bottom().saturating_sub(h + 1),
            w,
            h,
        ));
        Some(HintChrome {
            rect,
            key_width,
            rows,
        })
    }

    fn compute_prompt_chrome(&self) -> Option<PromptChrome> {
        let prompt = self.prompt.as_ref()?;
        let label = prompt.label();
        let label_len = label.chars().count() as u16;
        if self.area.height == 0 || self.area.width <= label_len {
            return None;
        }
        let line = Rect::new(self.area.x, self.area.bottom() - 1, self.area.width, 1);
        let input = Rect {
            x: line.x + label_len,
            width: line.width - label_len,
            ..line
        };
        let suggestions = match prompt.kind {
            PromptKind::Ex => ex::suggestions(&prompt.text()),
            PromptKind::Rename => Vec::new(),
        };
        let suggestion_row = (!suggestions.is_empty() && self.area.height >= 2)
            .then(|| Rect::new(line.x, line.y - 1, line.width, 1));
        Some(PromptChrome {
            line,
            label,
            input,
            suggestions,
            suggestion_row,
        })
    }

    /// Draws from `self.view` and engine state only, with no geometry math.
    fn render_to_buffer(&self, buf: &mut Buffer) {
        for win in self.windows.values() {
            if self.maximized.is_some_and(|id| id != win.id) || self.minimized.contains(&win.id) {
                continue;
            }
            render_tab(win.active_tab(), buf);
        }
        for chrome in &self.view.chrome {
            render_tab_bar(chrome, self.focus, buf, self.view.elapsed);
        }
        if let Some(sel) = &self.selection
            && self.maximized.is_none_or(|id| id == sel.window)
            && let Some(win) = self.windows.get(&sel.window)
        {
            render_selection(sel, win.content_rect(), buf);
        }
        for sep in &self.view.separators {
            render_separator(sep, buf);
        }
        if let Some(status) = &self.view.status {
            render_status(status, self.view.elapsed, buf);
        }
        if let Some((row, text)) = &self.view.message {
            render_message(*row, text, buf);
        }
        if let Some(chrome) = &self.view.prompt {
            render_prompt_chrome(chrome, buf);
            if let Some(prompt) = &self.prompt {
                prompt.textarea.render(chrome.input, buf);
            }
        }
        if let Some(hints) = &self.view.hints {
            render_hints(hints, buf);
        }
    }

    fn render(&self, frame: &mut Frame) {
        self.render_to_buffer(frame.buffer_mut());
        // The prompt's textarea draws its own cursor.
        if self.prompt.is_some() {
            return;
        }
        // The engine cursor belongs to the live view, so a scrolled tab
        // shows none.
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
    let visible = tab.view_range();
    // Not `with_phys_lines`: at the pinned rev it mis-indexes the second
    // half of a wrapped line deque and panics.
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
                dst.set_style(cell_style(cell.attrs()));
            }
        }
    }
}

/// Names that fit keep their full length, and the rest share what's left.
fn allocate_name_widths(lens: &[usize], budget: usize) -> Vec<usize> {
    let total: usize = lens.iter().sum();
    if total <= budget {
        return lens.to_vec();
    }
    // Binary search for the largest cap that fits.
    let fits = |cap: usize| lens.iter().map(|&l| l.min(cap)).sum::<usize>() <= budget;
    let (mut lo, mut hi) = (0, budget);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut alloc: Vec<usize> = lens.iter().map(|&l| l.min(lo)).collect();
    // Leftover cells go one each to the capped names, left to right.
    let mut spare = budget - alloc.iter().sum::<usize>();
    for (a, &l) in alloc.iter_mut().zip(lens) {
        if spare == 0 {
            break;
        }
        if *a < l {
            *a += 1;
            spare -= 1;
        }
    }
    alloc
}

fn truncate_name(name: &str, width: usize) -> String {
    if name.chars().count() <= width {
        return name.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = name.chars().take(width - 1).collect();
    out.push('…');
    out
}

/// Rule brightness marks window focus.
fn render_tab_bar(chrome: &Chrome, focus: WindowId, buf: &mut Buffer, elapsed: Duration) {
    let bar = chrome.tab_bar;
    if bar.height == 0 || bar.width == 0 {
        return;
    }
    let focused = chrome.window == focus;
    let badges_end = chrome.controls.map_or(bar.right(), |c| c.x);
    let mut x = bar.x;
    let mut put = |x: &mut u16, ch: char, style: Style| -> bool {
        if *x >= badges_end {
            return false;
        }
        if let Some(dst) = buf.cell_mut(Position::new(*x, bar.y)) {
            dst.set_char(ch);
            dst.set_style(style);
        }
        *x += 1;
        true
    };
    // Focused uses the terminal's default foreground, not hardcoded white.
    // The animation indexes by bar position so it sweeps the whole width.
    let rule_at = |x: u16| -> Style {
        let base = if focused {
            Color::Reset
        } else {
            Color::DarkGray
        };
        let color = match (focused, chrome.rule_anim) {
            (false, _) | (_, Anim::None) => base,
            (true, Anim::Shimmer) => anim::shimmer(
                chrome.rule_color,
                (x - bar.x) as usize,
                bar.width as usize,
                elapsed,
            ),
            (true, Anim::Breathe) => anim::breathe(chrome.rule_color, elapsed),
        };
        Style::default().fg(color)
    };
    // Badges stop where the bar runs out, but the controls still draw.
    'badges: {
        for _ in 0..2 {
            let style = rule_at(x);
            if !put(&mut x, '─', style) {
                break 'badges;
            }
        }
        for (i, badge) in chrome.tabs.iter().enumerate() {
            let style = if badge.active {
                let color = if focused { Color::Reset } else { Color::Gray };
                Style::default().fg(color)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            for ch in format!(" {}:{}", i, badge.name).chars() {
                if !put(&mut x, ch, style) {
                    break 'badges;
                }
            }
            if badge.yanked && !put(&mut x, '*', style.fg(Color::Yellow)) {
                break 'badges;
            }
            if let Some(visual) = &badge.agent {
                if !put(&mut x, ' ', style) {
                    break 'badges;
                }
                let len = visual.text.chars().count();
                for (j, ch) in visual.text.chars().enumerate() {
                    let color = match visual.anim {
                        Anim::None => visual.color,
                        Anim::Shimmer => anim::shimmer(visual.color, j, len, elapsed),
                        Anim::Breathe => anim::breathe(visual.color, elapsed),
                    };
                    if !put(&mut x, ch, style.fg(color)) {
                        break 'badges;
                    }
                }
            }
            if !put(&mut x, ' ', style) {
                break 'badges;
            }
        }
    }
    let indicators_end = x;
    while x < badges_end {
        if let Some(dst) = buf.cell_mut(Position::new(x, bar.y)) {
            dst.set_symbol("─");
            dst.set_style(rule_at(x));
        }
        x += 1;
    }
    // So a frozen view isn't mistaken for the live tail.
    if chrome.scroll {
        let label = " scroll ";
        let len = label.len() as u16;
        if badges_end >= bar.x + len && badges_end - len >= indicators_end {
            let style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::REVERSED);
            let start = badges_end - len;
            for (i, ch) in label.chars().enumerate() {
                if let Some(dst) = buf.cell_mut(Position::new(start + i as u16, bar.y)) {
                    dst.set_char(ch);
                    dst.set_style(style);
                }
            }
        }
    }
    // Glyphs any monospace font covers.
    if let Some(controls) = chrome.controls {
        let (rest, bright) = if focused {
            (Color::Reset, Color::White)
        } else {
            (Color::DarkGray, Color::Gray)
        };
        let toggle = if chrome.maximized { '❐' } else { '□' };
        let glyphs = [
            (' ', Control::Minimize),
            ('−', Control::Minimize),
            (' ', Control::Maximize),
            (toggle, Control::Maximize),
            (' ', Control::Exit),
            ('×', Control::Exit),
        ];
        for (i, (ch, control)) in glyphs.into_iter().enumerate() {
            let color = if chrome.hover == Some(control) {
                bright
            } else {
                rest
            };
            if let Some(dst) = buf.cell_mut(Position::new(controls.x + i as u16, controls.y)) {
                dst.set_char(ch);
                dst.set_style(Style::default().fg(color));
            }
        }
    }
}

/// The same keys the prefixed move-tab bindings use.
fn move_repeat_dir(key: KeyEvent) -> Option<Dir> {
    let m = KeyMatch::from_event(key);
    if m.ctrl {
        return None;
    }
    match (m.code, m.shift) {
        (CtKeyCode::Char('H'), _) | (CtKeyCode::Left, true) => Some(Dir::Left),
        (CtKeyCode::Char('J'), _) | (CtKeyCode::Down, true) => Some(Dir::Down),
        (CtKeyCode::Char('K'), _) | (CtKeyCode::Up, true) => Some(Dir::Up),
        (CtKeyCode::Char('L'), _) | (CtKeyCode::Right, true) => Some(Dir::Right),
        _ => None,
    }
}

/// The viewport minus the status row.
fn tree_area(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

fn clock_now() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// The server has no shell to expand `~`.
fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

fn count(n: usize, unit: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{s}")
}

fn render_message(row: Rect, text: &str, buf: &mut Buffer) {
    if row.height == 0 || row.width == 0 {
        return;
    }
    let fill = Style::default().bg(CHROME_BG);
    for x in row.x..row.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
            dst.set_char(' ');
            dst.set_style(fill);
        }
    }
    let style = fill.fg(Color::Gray);
    for (i, ch) in text.chars().enumerate() {
        let x = row.x + 1 + i as u16;
        if x >= row.right() {
            break;
        }
        if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
            dst.set_char(ch);
            dst.set_style(style);
        }
    }
}

fn render_status(status: &StatusChrome, elapsed: Duration, buf: &mut Buffer) {
    let row = status.row;
    if row.height == 0 || row.width == 0 {
        return;
    }
    let fill = Style::default().bg(CHROME_BG);
    for x in row.x..row.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
            dst.set_char(' ');
            dst.set_style(fill);
        }
    }
    // The menu icon.
    if let Some(dst) = buf.cell_mut(Position::new(row.x, row.y)) {
        dst.set_char('☢');
        dst.set_style(fill.fg(Color::Gray));
    }
    let name_style = fill.fg(Color::Green);
    for (i, ch) in format!(" {}", status.name).chars().enumerate() {
        let x = row.x + 1 + i as u16;
        if x >= row.right() {
            break;
        }
        if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
            dst.set_char(ch);
            dst.set_style(name_style);
        }
    }
    let title_style = fill.fg(Color::Gray);
    for title in &status.minimized {
        for (i, ch) in title.name.chars().enumerate() {
            let x = title.span.start + i as u16;
            if x >= title.span.end {
                break;
            }
            if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
                dst.set_char(ch);
                dst.set_style(title_style);
            }
        }
    }
    let clock_style = fill.fg(Color::Gray);
    let (text, ind_len) = match &status.indicator {
        Some((ind, _)) => (format!("{}  {} ", ind, status.clock), ind.chars().count()),
        None => (format!("{}  {} ", status.host, status.clock), 0),
    };
    let len = text.chars().count() as u16;
    if row.width >= len {
        let start = row.right() - len;
        for (i, ch) in text.chars().enumerate() {
            if let Some(dst) = buf.cell_mut(Position::new(start + i as u16, row.y)) {
                dst.set_char(ch);
                dst.set_style(if i < ind_len {
                    clock_style.fg(anim::shimmer(Color::Gray, i, ind_len, elapsed))
                } else {
                    clock_style
                });
            }
        }
    }
}

fn render_hints(chrome: &HintChrome, buf: &mut Buffer) {
    let rect = chrome.rect;
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let fill = Style::default().bg(CHROME_BG);
    let border = fill.fg(Color::DarkGray);
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            let Some(dst) = buf.cell_mut(Position::new(x, y)) else {
                continue;
            };
            let on_top = y == rect.top();
            let on_bottom = y == rect.bottom() - 1;
            let on_left = x == rect.left();
            let on_right = x == rect.right() - 1;
            let ch = match (on_top, on_bottom, on_left, on_right) {
                (true, _, true, _) => '┌',
                (true, _, _, true) => '┐',
                (_, true, true, _) => '└',
                (_, true, _, true) => '┘',
                (true, ..) | (_, true, ..) => '─',
                (_, _, true, _) | (_, _, _, true) => '│',
                _ => ' ',
            };
            dst.set_char(ch);
            dst.set_style(if ch == ' ' { fill } else { border });
        }
    }
    let keys_style = fill.fg(Color::Reset);
    let desc_style = fill.fg(Color::Gray);
    for (i, (keys, desc)) in chrome.rows.iter().enumerate() {
        let y = rect.y + 1 + i as u16;
        if y >= rect.bottom() - 1 {
            break;
        }
        // Border plus one margin cell on each side.
        let text = format!("{:width$}  {desc}", keys, width = chrome.key_width as usize);
        let styled = keys.chars().count() as u16 + 2;
        for (x, (j, ch)) in (rect.x + 2..rect.right() - 2).zip(text.chars().enumerate()) {
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.set_char(ch);
                dst.set_style(if (j as u16) < styled {
                    keys_style
                } else {
                    desc_style
                });
            }
        }
    }
}

/// Toggles REVERSED rather than setting it, so already-reversed content
/// stays visible.
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

/// The textarea renders separately, over the cleared input area.
fn render_prompt_chrome(chrome: &PromptChrome, buf: &mut Buffer) {
    for x in chrome.line.left()..chrome.line.right() {
        if let Some(dst) = buf.cell_mut(Position::new(x, chrome.line.y)) {
            dst.reset();
        }
    }
    for (i, ch) in chrome.label.chars().enumerate() {
        if let Some(dst) = buf.cell_mut(Position::new(chrome.line.x + i as u16, chrome.line.y)) {
            dst.set_char(ch);
        }
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

/// Always dim, since the tab bar's rule marks focus.
fn render_separator(sep: &Separator, buf: &mut Buffer) {
    let style = Style::default().fg(Color::DarkGray);
    for y in sep.rect.top()..sep.rect.bottom() {
        for x in sep.rect.left()..sep.rect.right() {
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.set_symbol("│");
                dst.set_style(style);
            }
        }
    }
}

/// `None` for keys with no terminal input encoding.
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

pub(crate) fn cell_style(attrs: &CellAttributes) -> Style {
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

/// Palette resolution is left to the client's terminal so colors follow
/// its theme.
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

#[cfg(test)]
mod tests {
    use super::{allocate_name_widths, truncate_name};

    #[test]
    fn names_that_fit_keep_full_length() {
        assert_eq!(allocate_name_widths(&[4, 20, 3], 17), vec![4, 10, 3]);
    }

    #[test]
    fn no_truncation_when_everything_fits() {
        assert_eq!(allocate_name_widths(&[4, 5, 6], 15), vec![4, 5, 6]);
    }

    #[test]
    fn long_names_share_the_budget_evenly() {
        assert_eq!(allocate_name_widths(&[20, 20], 11), vec![6, 5]);
    }

    #[test]
    fn zero_budget_allocates_nothing() {
        assert_eq!(allocate_name_widths(&[5, 5], 0), vec![0, 0]);
    }

    #[test]
    fn truncated_name_ends_in_ellipsis() {
        assert_eq!(truncate_name("long-tab-name", 5), "long…");
    }

    #[test]
    fn fitting_name_is_untouched() {
        assert_eq!(truncate_name("vim", 3), "vim");
        assert_eq!(truncate_name("vim", 10), "vim");
    }

    #[test]
    fn zero_width_name_is_empty() {
        assert_eq!(truncate_name("vim", 0), "");
    }

    #[test]
    fn truncation_counts_chars_not_bytes() {
        assert_eq!(truncate_name("héllo wörld", 6), "héllo…");
    }
}
