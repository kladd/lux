//! A session: the server-owned unit of state a client attaches to — one
//! layout tree of windows, each owning its tab list, plus the interaction
//! modes (prefix, ex command line, scroll mode, selection) that Phases 2-7
//! built. Sessions keep running whether or not a client is attached,
//! and reproduce the single-process behavior for
//! whichever client is.

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
use crate::server::ex::{self, ExCommand};
use crate::server::keys::{Command, KeyMatch, KeyTable, KeyTrie};
use crate::server::layout::{self, Dir, Node, Separator, Side, SplitKind, WindowId};
use crate::server::persist;
use crate::server::term::FdBackend;
use crate::server::window::{Notice, Tab, TabId, Window};
use crate::server::{ServerEvent, SessionId};

/// Minimum window size a split may produce.
const MIN_COLS: u16 = 10;
const MIN_ROWS: u16 = 3;

/// The resize submap's repeat deadline; restarted on each repeated
/// resize dispatch.
const RESIZE_REPEAT: Duration = Duration::from_millis(500);

/// The move-tab repeat deadline; restarted on each repeated move-tab
/// dispatch, matching the resize submap's timing.
const MOVE_REPEAT: Duration = Duration::from_millis(500);

/// A session-level consequence the server must act on; everything else is
/// handled inside the session.
pub enum Effect {
    /// Detach the client driving this session.
    Detach,
    /// Enter switcher mode for the driving client.
    OpenSwitcher,
    /// Enter the CLAUDECOM grid for the driving client.
    OpenGrid,
    /// Enter fuzzy tab-find mode for the driving client.
    OpenFinder,
    /// Create a session — named, or auto-named when `None` — and attach
    /// the driving client to it.
    NewSession(Option<String>),
    /// Rename the current session.
    RenameSession(String),
    /// Kill a named session, or the current one if `None`.
    KillSession(Option<String>),
    /// Yanked text for the system clipboard.
    Copy(String),
    /// Paste the system clipboard into this session.
    Paste,
    /// Set the client terminal's mouse pointer shape (an OSC 22 name).
    Pointer(&'static str),
    /// Land on the pending-Claude indicator's tab, which may live in
    /// another session.
    GotoIndicator(Indicator),
    /// The last window's last tab exited.
    Ended,
}

/// An in-progress mouse drag of a split boundary.
struct BorderDrag {
    /// Path from the layout tree's root to the dragged split.
    path: Vec<Side>,
    /// Whether any drag motion arrived; a press released without motion
    /// is a click on the chrome underneath.
    moved: bool,
}

/// A drag selection over one window's content, in
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
/// clamped inside the content rectangle.
fn clamp_to_content(pos: Position, content: Rect) -> (u16, u16) {
    if content.width == 0 || content.height == 0 {
        return (0, 0);
    }
    let x = pos.x.clamp(content.left(), content.right() - 1) - content.x;
    let y = pos.y.clamp(content.top(), content.bottom() - 1) - content.y;
    (x, y)
}

/// Forward a mouse event to a tab whose program handles the mouse itself;
/// the engine encodes it per the protocol the program
/// requested, and converts wheel ticks to arrow keys on the alternate
/// screen.
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

/// A clickable window control in a tab bar's control group.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    Minimize,
    Maximize,
    Exit,
}

/// Width of the tab bar's control group: minimize, maximize/restore, and
/// exit glyphs, each led by one space.
const CONTROLS_WIDTH: u16 = 6;

/// One tab's indicator in the bar: whether it's the active tab, its
/// display name, plus its bracketed status text when the tab
/// runs Claude Code.
struct TabBadge {
    active: bool,
    name: String,
    agent: Option<agent::Visual>,
    /// The x-extent this badge occupies in the bar, for click hit-tests;
    /// mirrors render_tab_bar's layout.
    span: std::ops::Range<u16>,
}

/// Per-frame chrome geometry for one window, computed into state before
/// drawing.
struct Chrome {
    window: WindowId,
    tab_bar: Rect,
    tabs: Vec<TabBadge>,
    /// Whether the active tab is in scroll mode.
    scroll: bool,
    /// The active tab's status animation and status color, carried onto
    /// the bar's rule while the window is focused.
    rule_anim: Anim,
    rule_color: Color,
    /// The six-cell control group at the bar's right edge; absent when
    /// the bar is too narrow to hold it.
    controls: Option<Rect>,
    /// Whether the window is maximized; its maximize control renders as
    /// a restore icon.
    maximized: bool,
    /// The control the mouse hovers over, rendered bright as a hover
    /// cue.
    hover: Option<Control>,
}

/// A minimized window's clickable title in the session status line.
struct MinimizedTitle {
    id: WindowId,
    name: String,
    span: std::ops::Range<u16>,
}

/// The session status line's neutral background — xterm-256
/// grey 235 (#262626), distinct from the default background without a
/// hue.
pub(crate) const CHROME_BG: Color = Color::Indexed(235);

/// The local hostname, fixed for the server's lifetime.
static HOSTNAME: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
});

/// The standing status-line indicator for a Claude Code tab, anywhere on
/// the server, that finished or got blocked while the user wasn't looking
/// at it: the tab it points to and the display text (the tab's name plus
/// its bracketed status). Computed by the server, since the tab may live
/// in another session.
#[derive(Clone, PartialEq)]
pub struct Indicator {
    pub session: SessionId,
    pub window: WindowId,
    pub tab: usize,
    pub text: String,
}

/// Per-frame session status line chrome, absent
/// while the command line owns the bottom row.
struct StatusChrome {
    row: Rect,
    name: String,
    /// Minimized windows' clickable titles, right of the session name,
    /// in minimize order.
    minimized: Vec<MinimizedTitle>,
    host: String,
    clock: String,
    /// The pending-Claude indicator's text and clickable span, shown in
    /// place of the hostname; absent when nothing qualifies or the row
    /// is too narrow to hold it with the clock.
    indicator: Option<(String, std::ops::Range<u16>)>,
}

/// What an open bottom-row prompt collects; both kinds share the same
/// textarea-backed text entry rather than each hand-rolling an input
/// widget.
enum PromptKind {
    /// The ex command line: parse and run the text on Enter.
    Ex,
    /// Rename the focused window's active tab to the text on Enter.
    Rename,
}

/// The open bottom-row text prompt.
struct Prompt {
    kind: PromptKind,
    textarea: TextArea<'static>,
}

impl Prompt {
    /// The label drawn before the input area.
    fn label(&self) -> &'static str {
        match self.kind {
            PromptKind::Ex => ":",
            PromptKind::Rename => "rename: ",
        }
    }

    /// The prompt's current text.
    fn text(&self) -> String {
        self.textarea.lines().first().cloned().unwrap_or_default()
    }
}

/// Per-frame prompt geometry, present while a prompt is open.
struct PromptChrome {
    /// The whole bottom row, including the label.
    line: Rect,
    /// The label drawn before the input area.
    label: &'static str,
    /// Where the textarea widget renders (after the label).
    input: Rect,
    /// Ex commands matching the typed text, on the row above; always
    /// empty for the rename prompt.
    suggestions: Vec<&'static str>,
    suggestion_row: Option<Rect>,
}

/// Per-frame key-hint popup geometry, present while
/// the prefix is pending.
struct HintChrome {
    rect: Rect,
    /// Width of the key column, so descriptions align across rows.
    key_width: u16,
    /// `(keys, description)` per row, from `KeyTable::hints`.
    rows: Vec<(String, &'static str)>,
}

/// Everything the draw pass reads; recomputed once per frame.
#[derive(Default)]
struct View {
    separators: Vec<Separator>,
    chrome: Vec<Chrome>,
    status: Option<StatusChrome>,
    prompt: Option<PromptChrome>,
    hints: Option<HintChrome>,
    /// The animation clock this frame renders at.
    elapsed: Duration,
}

pub struct Session {
    /// Stable handle for `-s`/`-t`/`ls` and the switcher.
    pub name: String,
    tree: Node,
    windows: HashMap<WindowId, Window>,
    /// Exactly one focused window at any time.
    focus: WindowId,
    /// The accumulated keys of a pending chord, walking the keybinding
    /// tree from its root: `Some` while awaiting the next key,
    /// empty directly after the prefix.
    /// No timer expires it, except the resize submap's repeat deadline.
    chord: Option<Vec<KeyMatch>>,
    /// The resize submap's repeat deadline: armed while the submap is
    /// held pending after a resize dispatch, so a bare direction key
    /// resizes again; elapsing closes the submap.
    resize_repeat: Option<Instant>,
    /// The move-tab repeat deadline: armed after a move-tab dispatch, so
    /// a bare `H`/`J`/`K`/`L` moves the tab again without the prefix;
    /// elapsing (or any other key) closes the window.
    move_repeat: Option<Instant>,
    /// The maximized window, rendered over the whole layout area while
    /// set. Purely a view state: the layout tree is untouched, focus
    /// leaving the window clears it, and it is never persisted.
    maximized: Option<WindowId>,
    /// Minimized windows in minimize order: removed from the layout tree
    /// with their processes still running, each shown as a clickable
    /// title in the session status line.
    minimized: Vec<WindowId>,
    /// The window control under the mouse with no button held; the
    /// hovered control renders bright.
    hover: Option<(WindowId, Control)>,
    /// The open bottom-row prompt (ex command line or tab rename), if
    /// any.
    prompt: Option<Prompt>,
    /// The server's prefix and bindings (config may override both).
    keys: Arc<KeyTable>,
    /// The current drag selection, if any.
    selection: Option<Selection>,
    /// The boundary drag in progress, if any.
    border_drag: Option<BorderDrag>,
    /// The pending-Claude indicator to show in the status line's
    /// hostname block, set by the server each render pass.
    indicator: Option<Indicator>,
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
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        // The initial window's shell, sized to the viewport
        // minus the session status row.
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
            maximized: None,
            minimized: Vec::new(),
            hover: None,
            prompt: None,
            keys,
            selection: None,
            border_drag: None,
            indicator: None,
            view: View::default(),
            area,
            clock: String::new(),
            next_window_id: 1,
            force_redraw: true,
            tx,
        })
    }

    /// Rebuild a session from its persisted snapshot: the saved layout
    /// tree and tab lists, each tab getting a fresh shell in its saved
    /// working directory — or a resumed Claude Code session where one was
    /// saved. Windows whose tabs all fail to spawn collapse out of the
    /// tree; a session with none left is `None`.
    pub fn restore(
        snap: &persist::SessionSnapshot,
        area: Rect,
        keys: Arc<KeyTable>,
        tx: Sender<ServerEvent>,
    ) -> Option<Self> {
        let mut tree = Some(persist::restore_node(&snap.tree));
        // Lay the tree out up front so each window's PTYs spawn at their
        // final size rather than a placeholder.
        let rects: HashMap<WindowId, Rect> = layout::compute(tree.as_ref()?, tree_area(area))
            .0
            .into_iter()
            .collect();
        let mut windows = HashMap::new();
        for wsnap in &snap.windows {
            let Some(&rect) = rects.get(&wsnap.id) else {
                // Not a leaf of the saved tree; nowhere to put it.
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
        // Leaves with no window snapshot at all collapse the same way.
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
            maximized: None,
            minimized: Vec::new(),
            hover: None,
            prompt: None,
            keys,
            selection: None,
            border_drag: None,
            indicator: None,
            view: View::default(),
            area,
            clock: String::new(),
            next_window_id,
            force_redraw: true,
            tx,
        })
    }

    /// Capture this session's persistable state: name, layout tree, each
    /// window's tab list and active tab, each tab's working directory,
    /// and — for tabs identified as running Claude Code — a session
    /// reference to resume it by.
    pub fn snapshot(&mut self) -> persist::SessionSnapshot {
        self.assign_claude_sessions();
        let windows = layout::leaves(&self.tree)
            .into_iter()
            .filter_map(|id| {
                let win = self.windows.get(&id)?;
                let tabs = win
                    .tabs
                    .iter()
                    .map(|tab| {
                        let cwd = tab.working_dir().unwrap_or_else(|| {
                            // No readable foreground process; the home
                            // directory beats losing the tab.
                            std::env::var_os("HOME")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|| "/".into())
                        });
                        persist::TabSnapshot {
                            cwd,
                            claude_session: tab.claude_session.clone(),
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

    /// Give every Claude Code tab that doesn't yet know its session id a
    /// distinct one, by matching each project directory's transcripts
    /// (by creation time) against when each tab was first seen running
    /// claude. Ids already owned — resumed tabs, earlier matches — are
    /// never taken, so concurrent claude tabs in the same directory keep
    /// distinct sessions instead of all saving the newest one. An
    /// assignment sticks until that tab's claude exits.
    fn assign_claude_sessions(&mut self) {
        let mut claimed: std::collections::HashSet<String> = self
            .windows
            .values()
            .flat_map(|w| w.tabs.iter().filter_map(|t| t.claude_session.clone()))
            .collect();
        // Unassigned claude tabs grouped by working directory, in
        // first-seen order within each group.
        let mut groups: std::collections::HashMap<
            std::path::PathBuf,
            Vec<(WindowId, usize, std::time::SystemTime)>,
        > = std::collections::HashMap::new();
        for (&wid, win) in &self.windows {
            for (idx, tab) in win.tabs.iter().enumerate() {
                if tab.agent.is_none() || tab.claude_session.is_some() {
                    continue;
                }
                let (Some(since), Some(cwd)) = (tab.claude_since, tab.working_dir()) else {
                    continue;
                };
                groups.entry(cwd).or_default().push((wid, idx, since));
            }
        }
        for (cwd, mut tabs) in groups {
            tabs.sort_by_key(|&(_, _, since)| since);
            let transcripts = persist::claude_sessions(&cwd);
            let since: Vec<std::time::SystemTime> = tabs.iter().map(|&(_, _, s)| s).collect();
            let ids = persist::match_claude_sessions(&since, &transcripts, &mut claimed);
            for (&(wid, idx, _), id) in tabs.iter().zip(ids) {
                if let Some(win) = self.windows.get_mut(&wid) {
                    win.tabs[idx].claude_session = id;
                }
            }
        }
    }

    pub fn has_tab(&self, id: TabId) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.id == id))
    }

    /// Advance the owning engine with PTY output, whether or
    /// not that tab is currently visible, then re-derive the tab's name
    /// and re-evaluate agent detection against the new
    /// content. Returns a notice when the tab's agent reached done or
    /// blocked.
    pub fn pty_output(&mut self, id: TabId, bytes: &[u8]) -> Option<Notice> {
        let tab = self.find_tab_mut(id)?;
        tab.engine.advance_bytes(bytes);
        let (changed, notice) = tab.refresh_identity();
        if changed {
            self.force_redraw = true;
        }
        notice
    }

    /// Any tab waiting out the idle debounce? The server
    /// switches to a timed wait while one is.
    pub fn has_pending_idle(&self) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.agent_pending_idle()))
    }

    /// Whether a repeat deadline (resize submap or move-tab) is armed.
    /// The server wakes on a timer while one is, so the deadline can
    /// close its repeat window.
    pub fn has_pending_repeat(&self) -> bool {
        self.resize_repeat.is_some() || self.move_repeat.is_some()
    }

    /// Close repeat windows whose deadline elapsed with no keypress: the
    /// resize submap (requiring prefix+r again) and the move-tab window
    /// (requiring the prefix again).
    pub fn tick_repeats(&mut self, now: Instant) {
        if self.resize_repeat.is_some_and(|deadline| now >= deadline) {
            self.resize_repeat = None;
            self.chord = None;
            self.force_redraw = true;
        }
        if self.move_repeat.is_some_and(|deadline| now >= deadline) {
            self.move_repeat = None;
        }
    }

    /// Commit idle debounces whose window elapsed without further output.
    /// Returns a notice per tab whose agent landed in done.
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

    /// Resize everything to the attached client's
    /// terminal; the tabs reconcile on the next
    /// compute pass.
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

    /// Whether any tab is currently identified as running Claude Code.
    pub fn has_claude_tab(&self) -> bool {
        self.windows
            .values()
            .any(|w| w.tabs.iter().any(|t| t.agent.is_some()))
    }

    /// The tabs currently identified as running Claude Code, in window
    /// layout order then tab order: each as its window id and position in
    /// that window's tab list.
    pub fn claude_tabs(&self) -> Vec<(WindowId, usize)> {
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

    /// The Claude Code tabs whose agent is in the done or blocked
    /// state — on-screen windows in layout order, then minimized windows
    /// in minimize order — each as its window id and position in that
    /// window's tab list.
    pub fn attention_tabs(&self) -> Vec<(WindowId, usize)> {
        let mut ids = layout::leaves(&self.tree);
        ids.extend(self.minimized.iter().copied());
        let mut out = Vec::new();
        for id in ids {
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

    /// The focused window and its active tab's position — what the
    /// attached client is looking at.
    pub fn focused_active(&self) -> (WindowId, usize) {
        let active = self.windows.get(&self.focus).map_or(0, |w| w.active);
        (self.focus, active)
    }

    /// Set the pending-Claude indicator the status line shows; a change
    /// forces a redraw.
    pub fn set_indicator(&mut self, indicator: Option<Indicator>) {
        if self.indicator != indicator {
            self.indicator = indicator;
            self.force_redraw = true;
        }
    }

    /// Every tab in window layout order then tab order, each as its
    /// window id and position in that window's tab list.
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

    /// The tab at `index` in window `window`'s tab list.
    pub fn tab_at(&self, window: WindowId, index: usize) -> Option<&Tab> {
        self.windows.get(&window)?.tabs.get(index)
    }

    /// Mutable access to that same tab, for views that resize it to their
    /// own geometry (the CLAUDECOM grid's tiles).
    pub fn tab_at_mut(&mut self, window: WindowId, index: usize) -> Option<&mut Tab> {
        self.windows.get_mut(&window)?.tabs.get_mut(index)
    }

    /// Encode `key` to the PTY of the tab at `index` in window `window`,
    /// regardless of focus — the delivery path for a tab captured from
    /// the CLAUDECOM grid.
    pub fn key_to_tab(&mut self, window: WindowId, index: usize, key: KeyEvent) {
        if let Some((code, mods)) = map_key(key)
            && let Some(win) = self.windows.get_mut(&window)
            && let Some(tab) = win.tabs.get_mut(index)
        {
            let _ = tab.engine.key_down(code, mods);
        }
    }

    /// Write pasted text to that same tab's PTY, honoring bracketed
    /// paste.
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

    /// Land on `window`'s tab at `index`: a minimized window is
    /// restored, focused, and maximized — in that order, so focusing
    /// doesn't immediately un-maximize — landing the user on the tab
    /// full-screen; any other window is simply focused with the tab made
    /// active. A restore that fails (minimum window size) leaves the
    /// window minimized and changes nothing else.
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

    /// Focus `window` and make its tab at `index` active.
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
        // While a prompt is open, every key press edits
        // it instead of reaching the focused window's PTY.
        if self.prompt.is_some() {
            return self.handle_prompt_key(key);
        }
        // An elapsed repeat deadline has already closed its repeat
        // window; the tick normally does this on time, but a key racing
        // the timer must not land in a window that should be gone.
        self.tick_repeats(Instant::now());
        if let Some(mut path) = self.chord.take() {
            // Any key either dispatches (re-arming the deadline below) or
            // ends the sequence, so the armed deadline never outlives it.
            self.resize_repeat = None;
            // Whatever this key does, the hint popup changes or closes.
            self.force_redraw = true;
            // Escape discards the pending sequence.
            if key.code == CtKeyCode::Esc {
                return None;
            }
            // Every recognized sequence
            // dispatches through the single, server-side keybinding table.
            let node = self
                .keys
                .node_at(&path)
                .expect("pending chord path resolves to a node");
            match node.get(KeyMatch::from_event(key)) {
                // A command at any depth dispatches and
                // ends the sequence — except a resize, which holds its
                // submap pending and arms the repeat deadline, so a bare
                // direction key resizes again without prefix+r.
                Some(&KeyTrie::Command(command)) => {
                    if matches!(command, Command::ResizeDir(_)) {
                        self.chord = Some(path);
                        self.resize_repeat = Some(Instant::now() + RESIZE_REPEAT);
                    }
                    return self.execute(command);
                }
                // A deeper node — keep waiting, scoped one
                // level down.
                Some(KeyTrie::Node(_)) => {
                    path.push(KeyMatch::from_event(key));
                    self.chord = Some(path);
                }
                // A dead-end key discards the whole
                // accumulated sequence, dispatching nothing and writing
                // nothing to the PTY.
                None => {}
            }
            return None;
        }
        // While the move-repeat deadline is armed, a bare `H`/`J`/`K`/`L`
        // moves the tab again (a successful move re-arms the deadline);
        // any other key is discarded — never reaching the PTY — and
        // closes the window.
        if self.move_repeat.take().is_some() {
            if let Some(dir) = move_repeat_dir(key) {
                return self.execute(Command::MoveTabDir(dir));
            }
            return None;
        }
        // The prefix key (Ctrl-b by default, config
        // may override it) arms the prefix instead of
        // reaching the focused window's PTY.
        if self.keys.is_prefix(key) {
            self.chord = Some(Vec::new());
            // Show the hint popup without waiting on output.
            self.force_redraw = true;
            return None;
        }
        // In scroll mode every key is consumed by history
        // navigation; nothing reaches the PTY.
        if let Some(win) = self.windows.get_mut(&self.focus)
            && win.active_tab().scroll_mode()
        {
            let page = win.content_rect().height.max(1) as isize;
            let tab = win.active_tab_mut();
            match key.code {
                // One line at a time.
                CtKeyCode::Char('k') | CtKeyCode::Up => {
                    tab.scroll_by(-1);
                }
                CtKeyCode::Char('j') | CtKeyCode::Down => {
                    tab.scroll_by(1);
                }
                // One page at a time.
                CtKeyCode::PageUp => {
                    tab.scroll_by(-page);
                }
                CtKeyCode::PageDown => {
                    tab.scroll_by(page);
                }
                // Back to following live output.
                CtKeyCode::Esc | CtKeyCode::Char('q') => tab.exit_scroll_mode(),
                _ => {}
            }
            self.force_redraw = true;
            return None;
        }
        // Every other key goes only to the focused window's
        // active tab; the engine encodes it per the live terminal modes and
        // writes it to that tab's PTY. A write can fail when
        // the child has already exited; the exit event follows.
        if let Some((code, mods)) = map_key(key)
            && let Some(win) = self.windows.get_mut(&self.focus)
        {
            let _ = win.active_tab_mut().engine.key_down(code, mods);
        }
        None
    }

    /// Write text to the focused window's active tab's PTY, honoring
    /// bracketed paste, including client-initiated pastes.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(win) = self.windows.get_mut(&self.focus) {
            let _ = win.active_tab_mut().engine.send_paste(text);
        }
    }

    pub fn handle_mouse(&mut self, mouse: CtMouseEvent) -> Option<Effect> {
        let pos = Position::new(mouse.column, mouse.row);
        // Shift bypasses a program's mouse grab, keeping selection,
        // yank, and paste reachable inside mouse-aware programs.
        let shift = mouse.modifiers.contains(CtMods::SHIFT);
        // An active boundary drag consumes every mouse event until the
        // button is released.
        if self.border_drag.is_some() {
            return self.drag_border(&mouse, pos);
        }
        match mouse.kind {
            CtMouseKind::Down(button) => {
                // A left click on a minimized window's title in the
                // status line restores it.
                if button == CtMouseButton::Left
                    && let Some(id) = self.minimized_title_at(pos)
                {
                    self.restore_window(id);
                    return None;
                }
                // A left click on the pending-Claude indicator lands on
                // its tab; the server resolves it, since the tab may
                // live in another session.
                if button == CtMouseButton::Left
                    && let Some(indicator) = self.indicator_at(pos)
                {
                    return Some(Effect::GotoIndicator(indicator));
                }
                // A left press on a draggable boundary — a separator
                // column, or the tab bar row bordering the window above —
                // starts a boundary drag. Boundaries are lux chrome, so
                // the press never reaches a mouse-grabbed program; click
                // behavior on the row underneath happens on release when
                // no drag follows.
                if button == CtMouseButton::Left
                    && self.maximized.is_none()
                    && let Some((path, _)) =
                        layout::boundary_at(&self.tree, tree_area(self.area), pos)
                {
                    self.border_drag = Some(BorderDrag { path, moved: false });
                    return None;
                }
                let id = self.window_at(pos)?;
                // Click-to-focus.
                if self.focus != id {
                    self.set_focus(id);
                    self.force_redraw = true;
                }
                // A left click on the bar's window controls minimizes,
                // toggles maximize, or closes. The bar is lux chrome, so
                // the click never reaches a mouse-grabbed program.
                if button == CtMouseButton::Left
                    && let Some(control) = self.control_at(id, pos)
                {
                    self.click_control(id, control);
                    return None;
                }
                // A left click on a tab's indicator makes
                // that tab active.
                if button == CtMouseButton::Left
                    && let Some(index) = self.tab_badge_at(id, pos)
                {
                    self.select_tab(index);
                    return None;
                }
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                // The program owns the mouse, unless Shift bypasses it.
                if tab.engine.is_mouse_grabbed() && !shift {
                    forward_mouse(tab, &mouse, content);
                    return None;
                }
                match button {
                    // Anchor a selection.
                    CtMouseButton::Left if content.contains(pos) => {
                        let cell = clamp_to_content(pos, content);
                        self.selection = Some(Selection {
                            window: id,
                            start: cell,
                            end: cell,
                        });
                        self.force_redraw = true;
                    }
                    // With a selection, right-click yanks;
                    // Without one, it pastes.
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
            // Extend the selection, clamped to the
            // window where the drag began.
            CtMouseKind::Drag(CtMouseButton::Left) if self.selection.is_some() => {
                let sel = self.selection.as_mut().expect("checked above");
                let win = self.windows.get(&sel.window)?;
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
                // Releases and motion still reach a
                // grabbed program, unless Shift bypasses it.
                if let Some(id) = self.window_at(pos) {
                    let win = self.windows.get_mut(&id).expect("window exists");
                    let content = win.content_rect();
                    let tab = win.active_tab_mut();
                    if tab.engine.is_mouse_grabbed() && !shift {
                        forward_mouse(tab, &mouse, content);
                    }
                }
                // Hovering a window control brightens it; moving off
                // restores its resting shade.
                if mouse.kind == CtMouseKind::Moved {
                    let hover = self
                        .window_at(pos)
                        .and_then(|id| self.control_at(id, pos).map(|c| (id, c)));
                    if hover != self.hover {
                        self.hover = hover;
                        self.force_redraw = true;
                    }
                    // Hovering a control shows a hand pointer, a
                    // draggable boundary a resize pointer.
                    return Some(Effect::Pointer(self.pointer_shape(pos)));
                }
            }
            CtMouseKind::ScrollUp | CtMouseKind::ScrollDown => {
                let id = self.window_at(pos)?;
                let win = self.windows.get_mut(&id).expect("window exists");
                let content = win.content_rect();
                let tab = win.active_tab_mut();
                // A grabbed program gets the wheel
                // encoded; on the alternate screen the engine converts
                // wheel ticks to arrow keys itself (alternateScroll).
                if tab.engine.is_mouse_grabbed() || tab.engine.is_alt_screen_active() {
                    forward_mouse(tab, &mouse, content);
                    return None;
                }
                // Focus, enter scroll mode, scroll 3 lines.
                tab.enter_scroll_mode();
                let delta = if mouse.kind == CtMouseKind::ScrollUp {
                    -3
                } else {
                    3
                };
                // Wheeling down to the live bottom resumes following
                // (entering scroll mode just to sit at the tail would trap
                // accidental wheel-downs).
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

    /// Continue or finish the boundary drag in progress: motion moves the
    /// boundary to track the mouse, stopping at the minimum window size;
    /// a release without motion is a plain click on the chrome
    /// underneath (focus, tab select).
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

    /// The mouse pointer shape for `pos`: a hand pointer over a window
    /// control, a resize shape over a draggable boundary, matching the
    /// axis the boundary moves on, and the default anywhere else. Shapes
    /// are OSC 22 names; terminals without pointer-shape support ignore
    /// the sequence.
    fn pointer_shape(&self, pos: Position) -> &'static str {
        // The control wins where a tab bar doubles as a drag boundary;
        // it stays clickable there via the motionless-release path.
        if self
            .window_at(pos)
            .is_some_and(|id| self.control_at(id, pos).is_some())
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
        // While a window is maximized it is the only one on screen, so
        // hidden windows' stale rectangles never take a click.
        if let Some(id) = self.maximized {
            return self
                .windows
                .get(&id)
                .filter(|w| w.rect.contains(pos))
                .map(|w| w.id);
        }
        self.windows
            .values()
            // A minimized window's stale rectangle never takes a click.
            .find(|w| w.rect.contains(pos) && !self.minimized.contains(&w.id))
            .map(|w| w.id)
    }

    /// Yank the selected text and clear the selection;
    /// yanking never writes to a PTY. The server puts the text on the
    /// system clipboard.
    fn yank_selection(&mut self) -> Option<Effect> {
        let text = self.selection_text();
        self.selection = None;
        self.force_redraw = true;
        text.map(Effect::Copy)
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
    /// window.
    fn execute(&mut self, command: Command) -> Option<Effect> {
        match command {
            Command::SplitSideBySide => self.split(SplitKind::SideBySide),
            Command::SplitStacked => self.split(SplitKind::Stacked),
            Command::NewTab => self.new_tab(),
            Command::NextTab => self.cycle_tab(1),
            // Previous tab, wrapping.
            Command::PrevTab => self.cycle_tab(-1),
            // Direct selection by displayed index.
            Command::SelectTab(index) => self.select_tab(index),
            Command::OnlyWindow => self.only_window(),
            Command::FocusDir(dir) => self.focus_dir(dir),
            Command::ResizeDir(dir) => self.resize_focused(dir),
            // Reset every split to an even ratio.
            Command::Rebalance => {
                layout::rebalance(&mut self.tree);
                self.force_redraw = true;
            }
            // Prefix+H/J/K/L moves the active tab into the adjacent
            // window; a successful move opens the repeat window so a bare
            // direction key moves again. A press with no adjacent window
            // arms nothing.
            Command::MoveTabDir(dir) => {
                if self.move_tab_dir(dir) {
                    self.move_repeat = Some(Instant::now() + MOVE_REPEAT);
                }
            }
            // Prefix+m then a direction key exchanges the focused window
            // with the spatially adjacent one; at a screen edge the
            // sequence is discarded.
            Command::SwapDir(dir) => self.swap_dir(dir),
            // Prefix+z toggles the focused window's maximized state.
            Command::Maximize => {
                self.maximized = (self.maximized != Some(self.focus)).then_some(self.focus);
                self.force_redraw = true;
            }
            // Prefix+i flips the orientation of the split immediately
            // containing the focused window; a lone window has none.
            Command::Rotate => {
                if layout::rotate(&mut self.tree, self.focus) {
                    self.force_redraw = true;
                }
            }
            // Real detach, dispatched server-side.
            Command::Detach => return Some(Effect::Detach),
            // Switcher mode is the server's to run.
            Command::Switcher => return Some(Effect::OpenSwitcher),
            // So is the grid.
            Command::Grid => return Some(Effect::OpenGrid),
            // And the fuzzy tab finder.
            Command::FindTab => return Some(Effect::OpenFinder),
            Command::OpenEx => self.open_prompt(PromptKind::Ex, String::new()),
            // Prefix+, prompts for the active tab's new name.
            Command::RenameTab => {
                let name = self.windows[&self.focus].active_tab().name.clone();
                self.open_prompt(PromptKind::Rename, name);
            }
            // Prefix+x closes the focused window outright.
            Command::CloseWindow => self.close_window(),
            // Prefix+[ enters scroll mode.
            Command::ScrollMode => {
                if let Some(win) = self.windows.get_mut(&self.focus) {
                    win.active_tab_mut().enter_scroll_mode();
                    self.force_redraw = true;
                }
            }
        }
        None
    }

    /// Open a bottom-row prompt pre-filled with `text`, cursor at the
    /// end.
    fn open_prompt(&mut self, kind: PromptKind, text: String) {
        let mut textarea = TextArea::from([text]);
        textarea.move_cursor(tui_textarea::CursorMove::End);
        // The default cursor-line underline reads as stray chrome in a
        // one-line input.
        textarea.set_cursor_line_style(Style::default());
        self.prompt = Some(Prompt { kind, textarea });
        self.force_redraw = true;
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Option<Effect> {
        self.force_redraw = true;
        match key.code {
            // Close without executing anything.
            CtKeyCode::Esc => {
                self.prompt = None;
            }
            // Commit (or discard) and close.
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
                        // Unrecognized text closes with no action.
                        None => {}
                    },
                    PromptKind::Rename => {
                        let win = self
                            .windows
                            .get_mut(&self.focus)
                            .expect("focused window exists");
                        win.active_tab_mut().set_name(text);
                    }
                }
            }
            // The rest of line editing goes via tui-textarea: character
            // insertion, Backspace, cursor motion.
            _ => {
                let prompt = self.prompt.as_mut().expect("prompt is open");
                prompt.textarea.input(tui_textarea::Input::from(key));
            }
        }
        None
    }

    /// Write the focused window's active tab's entire terminal content,
    /// scrollback included, to `path`. A leading `~/` expands to the
    /// user's home directory. There is no error surface yet, so a failed
    /// write is dropped.
    fn write_tab_content(&self, path: &std::path::Path) {
        let tab = self.windows[&self.focus].active_tab();
        let screen = tab.engine.screen();
        // Every physical row: scrollback plus the visible grid.
        // (`scrollback_rows` counts all rows, not just the scrolled-off
        // ones.)
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
        let _ = std::fs::write(expand_tilde(path), out);
    }

    fn find_tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.windows.values_mut().find_map(|w| w.find_tab_mut(id))
    }

    fn split(&mut self, kind: SplitKind) {
        // The tree's rectangle, not the window's — a maximized window's
        // full-area override must not change what fits.
        let Some(&(_, rect)) = self.layout_rects().iter().find(|(id, _)| *id == self.focus) else {
            return;
        };
        let (first, second, _) = layout::split_areas(kind, 0.5, rect);
        // Never create a window under 10 cols or 3 rows.
        for half in [first, second] {
            if half.width < MIN_COLS || half.height < MIN_ROWS {
                return;
            }
        }
        let id = self.next_window_id;
        // The new window gets one active
        // tab with its own shell and engine sized to its content rectangle.
        // If the shell can't spawn, keep the current layout rather than
        // tearing the session down.
        let Ok(win) = Window::new(id, second, self.tx.clone()) else {
            return;
        };
        self.next_window_id += 1;
        self.windows.insert(id, win);
        layout::split_leaf(&mut self.tree, self.focus, kind, id);
        self.set_focus(id);
        self.force_redraw = true;
    }

    /// Append a new tab to the focused window's list and make
    /// it active.
    fn new_tab(&mut self) {
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        // The new shell starts in the working directory of
        // the tab that was active until now.
        let cwd = win.active_tab().working_dir();
        // The tab gets its own shell and engine sized to
        // the window's content rectangle.
        let Ok(tab) = Tab::spawn(win.content_rect(), cwd, self.tx.clone()) else {
            return;
        };
        win.tabs.push(tab);
        win.active = win.tabs.len() - 1;
        self.drop_selection_in(self.focus);
        // Show the switch without waiting on PTY output.
        self.force_redraw = true;
    }

    /// Cycle the focused window's active tab, wrapping in
    /// either direction.
    fn cycle_tab(&mut self, step: isize) {
        let win = self
            .windows
            .get_mut(&self.focus)
            .expect("focused window exists");
        let len = win.tabs.len() as isize;
        win.active = (win.active as isize + step).rem_euclid(len) as usize;
        self.drop_selection_in(self.focus);
        // Show the switch without waiting on PTY output.
        self.force_redraw = true;
    }

    /// Make the focused window's tab at `index` active; an
    /// out-of-range index is discarded silently.
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
        // Show the switch without waiting on PTY output.
        self.force_redraw = true;
    }

    /// The index of the tab badge at `pos` in window `id`'s tab bar, from
    /// the last computed view's geometry.
    fn tab_badge_at(&self, id: WindowId, pos: Position) -> Option<usize> {
        let chrome = self.view.chrome.iter().find(|c| c.window == id)?;
        let bar = chrome.tab_bar;
        if bar.height == 0 || pos.y != bar.y {
            return None;
        }
        chrome.tabs.iter().position(|b| b.span.contains(&pos.x))
    }

    /// The control under `pos` in window `id`'s tab bar, from the last
    /// computed view's geometry. Each control's click target is its
    /// glyph cell plus the space leading it.
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

    /// The minimized window whose status-line title is under `pos`, from
    /// the last computed view's geometry.
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

    /// The pending-Claude indicator, when its status-line span is under
    /// `pos`, from the last computed view's geometry.
    fn indicator_at(&self, pos: Position) -> Option<Indicator> {
        let status = self.view.status.as_ref()?;
        let (_, span) = status.indicator.as_ref()?;
        if pos.y != status.row.y || !span.contains(&pos.x) {
            return None;
        }
        self.indicator.clone()
    }

    /// The minimized windows' status-line titles: each window's active
    /// tab's display name, in minimize order, laid out after `x` with
    /// the status line's two-space separation.
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

    /// Apply a clicked window control.
    fn click_control(&mut self, id: WindowId, control: Control) {
        match control {
            Control::Minimize => self.minimize_window(id),
            // Same toggle as prefix+z.
            Control::Maximize => {
                self.maximized = (self.maximized != Some(id)).then_some(id);
                self.force_redraw = true;
            }
            // Same close as prefix+x, scoped to the clicked window; the
            // exit events collapse it through the ordinary removal path.
            Control::Exit => self.kill_window(id),
        }
    }

    /// Remove window `id` from the layout tree, giving its space to its
    /// sibling, with its tabs' processes left running; the window
    /// reappears as a clickable title in the session status line. A lone
    /// window has nowhere to give its space, so the click is discarded.
    fn minimize_window(&mut self, id: WindowId) {
        let ids = layout::leaves(&self.tree);
        if ids.len() <= 1 || !ids.contains(&id) {
            return;
        }
        // A minimized window can no longer fill the layout area.
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

    /// Reinsert minimized window `id` by splitting the focused window —
    /// side by side if it is wider than it is tall, stacked otherwise —
    /// and focusing the restored window. A restore that would violate
    /// the minimum window size fails silently, leaving the window
    /// minimized.
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

    /// Terminate every tab's child process in window `id`, with no
    /// confirmation.
    fn kill_window(&mut self, id: WindowId) {
        if let Some(win) = self.windows.get_mut(&id) {
            for tab in &mut win.tabs {
                tab.kill();
            }
        }
    }

    /// A selection describes cells of the window's currently visible tab;
    /// drop it when that content is replaced or the window goes away.
    fn drop_selection_in(&mut self, window: WindowId) {
        if self.selection.as_ref().is_some_and(|s| s.window == window) {
            self.selection = None;
        }
    }

    /// Move focus to `id`, exiting the maximized state when focus leaves
    /// the maximized window.
    fn set_focus(&mut self, id: WindowId) {
        self.focus = id;
        if self.maximized.is_some_and(|m| m != id) {
            self.maximized = None;
        }
    }

    /// Every window's rectangle computed from the layout tree — the
    /// geometry directional commands navigate by, unaffected by the
    /// maximized window's full-area override.
    fn layout_rects(&self) -> Vec<(WindowId, Rect)> {
        layout::compute(&self.tree, tree_area(self.area)).0
    }

    /// Move focus to the window spatially adjacent in `dir`;
    /// at a screen edge focus stays put.
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

    /// Move the focused window's active tab into the
    /// window spatially adjacent in `dir`, appended as that window's active
    /// tab. Focus follows the
    /// moved tab, keeping exactly one focused window
    /// whether or not the source window survives. Returns whether a tab
    /// moved.
    fn move_tab_dir(&mut self, dir: Dir) -> bool {
        let rects = self.layout_rects();
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return false;
        };
        // No adjacent window — discard, move nothing.
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
        // Both windows' visible content changes.
        self.drop_selection_in(source);
        self.drop_selection_in(dest);
        let dest_win = self.windows.get_mut(&dest).expect("adjacent window exists");
        dest_win.tabs.push(tab);
        dest_win.active = dest_win.tabs.len() - 1;
        // The tab renders into a new rectangle now.
        let content = dest_win.content_rect();
        dest_win.active_tab_mut().resize(content);
        self.set_focus(dest);
        if emptied {
            // A window left with no tabs collapses — the
            // sibling subtree inherits its space.
            self.windows.remove(&source);
            let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
            if let Some(tree) = layout::remove_leaf(tree, source) {
                self.tree = tree;
            }
        }
        self.force_redraw = true;
        true
    }

    /// Exchange the focused window with the window spatially adjacent in
    /// `dir`; focus stays with the moved window. Both windows' PTYs and
    /// engines resize to their new rectangles on the next frame's
    /// reconcile.
    fn swap_dir(&mut self, dir: Dir) {
        let rects = self.layout_rects();
        let Some(&(_, from)) = rects.iter().find(|(id, _)| *id == self.focus) else {
            return;
        };
        // No adjacent window — discard, swap nothing.
        let Some(other) = layout::spatial_neighbor(&rects, from, dir) else {
            return;
        };
        if layout::swap_leaves(&mut self.tree, self.focus, other) {
            self.force_redraw = true;
        }
    }

    /// Terminate the focused window's child processes,
    /// with no confirmation; the resulting exit events collapse the
    /// window through the ordinary removal path.
    fn close_window(&mut self) {
        self.kill_window(self.focus);
    }

    /// Vim's "only" — terminate every other window's
    /// child processes; the resulting exit events collapse the tree
    /// through the ordinary removal path.
    fn only_window(&mut self) {
        let focus = self.focus;
        for (_, win) in self.windows.iter_mut().filter(|(id, _)| **id != focus) {
            for tab in &mut win.tabs {
                tab.kill();
            }
        }
    }

    /// Move the boundary between the focused window and
    /// its adjacent sibling one cell in `dir`.
    fn resize_focused(&mut self, dir: Dir) {
        if layout::resize_toward(&mut self.tree, tree_area(self.area), self.focus, dir) {
            self.force_redraw = true;
        }
    }

    /// A tab's PTY hit EOF. Returns `Effect::Ended` when this was the
    /// session's last window's last tab.
    pub fn pty_exited(&mut self, id: TabId) -> Option<Effect> {
        let win_id = self
            .windows
            .values()
            .find(|w| w.tabs.iter().any(|t| t.id == id))?
            .id;
        let win = self.windows.get_mut(&win_id).expect("window exists");
        if win.tabs.len() > 1 {
            // Prune the tab and keep the window on a live one.
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
        // A window's last tab exiting collapses the window.
        let mut win = self.windows.remove(&win_id).expect("window exists");
        win.tabs.pop().expect("last tab exists").wait();
        if self.windows.is_empty() {
            // The session's last process is gone.
            return Some(Effect::Ended);
        }
        self.drop_selection_in(win_id);
        // A minimized window collapsing just drops its status-line
        // title; the layout tree never held it.
        if let Some(pos) = self.minimized.iter().position(|m| *m == win_id) {
            self.minimized.remove(pos);
            self.force_redraw = true;
            return None;
        }
        let ids = layout::leaves(&self.tree);
        // The last on-screen window collapsed while minimized windows
        // keep running; the oldest minimized window takes the whole
        // tree rather than the session ending under it.
        if ids == [win_id] {
            let restored = self.minimized.remove(0);
            self.tree = Node::Leaf(restored);
            self.set_focus(restored);
            self.force_redraw = true;
            return None;
        }
        // Refocus before the leaf disappears from the tree; a maximized
        // window collapsing this way also exits the maximized state.
        if self.focus == win_id {
            let pos = ids.iter().position(|i| *i == win_id).unwrap_or(0);
            self.set_focus(ids[(pos + 1) % ids.len()]);
        }
        // The sibling subtree inherits the space.
        let tree = std::mem::replace(&mut self.tree, Node::Leaf(self.focus));
        if let Some(tree) = layout::remove_leaf(tree, win_id) {
            self.tree = tree;
        }
        self.force_redraw = true;
        None
    }

    pub fn needs_redraw(&self) -> bool {
        self.force_redraw
            // The status line's clock rolled over a minute.
            || self.clock != clock_now()
            || self.has_animation()
            || self.windows.values().any(|w| {
                // A minimized window's output draws nothing.
                if self.minimized.contains(&w.id) {
                    return false;
                }
                let tab = w.active_tab();
                tab.engine.current_seqno() != tab.drawn_seqno
            })
    }

    /// Any badge in a tab bar currently animated — or the status line's
    /// pending-Claude indicator, which always shimmers? While one is on
    /// screen, the server redraws on its timer tick so the
    /// animation advances without waiting on PTY output.
    pub fn has_animation(&self) -> bool {
        self.indicator.is_some()
            || self.windows.values().any(|w| {
                !self.minimized.contains(&w.id)
                    && w.tabs.iter().any(|t| {
                        t.agent
                            .as_ref()
                            .is_some_and(|a| a.visual().anim != Anim::None)
                    })
            })
    }

    /// One frame to an attached client's terminal: compute geometry into
    /// state, then draw purely from that state (a compute/draw split).
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

    /// Render this session cropped into `area` of `buf` for the session
    /// switcher's live preview. Doesn't disturb the
    /// seqno bookkeeping an attached client's redraws rely on.
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

    /// Compute this frame's window and tab bar geometry into `self.view`,
    /// reconciling every window's tabs with their rectangles.
    fn compute_view(&mut self) {
        // A maximized window takes the whole layout area by itself; the
        // tree is untouched, so toggling back simply resumes computing
        // from it. Hidden windows keep their engines as they were.
        let (rects, separators) = match self.maximized {
            Some(id) => (vec![(id, tree_area(self.area))], Vec::new()),
            None => layout::compute(&self.tree, tree_area(self.area)),
        };
        let mut chrome = Vec::with_capacity(rects.len());
        for (id, rect) in rects {
            let Some(win) = self.windows.get_mut(&id) else {
                continue;
            };
            win.rect = rect;
            win.reconcile();
            // The focused window's displayed tab counts as
            // seen the moment it's rendered.
            if id == self.focus
                && let Some(tracker) = &mut win.active_tab_mut().agent
            {
                tracker.mark_seen();
            }
            let active = win.active;
            let bar = win.tab_bar_rect();
            // The right-edge control group, when the bar can hold it
            // beyond the two-cell rule lead-in.
            let controls = (bar.height > 0 && bar.width >= CONTROLS_WIDTH + 2)
                .then(|| Rect::new(bar.right() - CONTROLS_WIDTH, bar.y, CONTROLS_WIDTH, 1));
            let badges_end = controls.map_or(bar.right(), |c| c.x);
            // Badge spans track render_tab_bar's layout: the two-cell
            // rule lead-in, then per badge " i:name", the
            // agent text when present, and the trailing separator space,
            // stopping short of the controls.
            let mut next_x = bar.x.saturating_add(2).min(badges_end);
            let tabs: Vec<TabBadge> = win
                .tabs
                .iter()
                .enumerate()
                .map(|(i, tab)| {
                    let agent = tab.agent.as_ref().map(|t| t.visual());
                    let mut width = format!(" {}:{}", i, tab.name).chars().count() as u16;
                    if let Some(visual) = &agent {
                        width += 1 + visual.text.chars().count() as u16;
                    }
                    width += 1;
                    let start = next_x;
                    next_x = next_x.saturating_add(width).min(badges_end);
                    TabBadge {
                        active: i == active,
                        name: tab.name.clone(),
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
        // The reserved bottom row, unless a prompt owns it this frame.
        let status = (prompt.is_none() && self.area.height > 0 && self.area.width > 0).then(|| {
            let row = Rect::new(self.area.x, self.area.bottom() - 1, self.area.width, 1);
            let name_end = row
                .x
                .saturating_add(1 + self.name.chars().count() as u16)
                .min(row.right());
            // The pending-Claude indicator takes the hostname's place
            // when it fits alongside the clock; too narrow a row falls
            // back to the hostname block as usual.
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
            prompt,
            hints: self.compute_hint_chrome(),
            elapsed: anim::elapsed(),
        };
    }

    /// Geometry for the key-hint popup while a chord is pending:
    /// rows from the pending chord's current node, at any
    /// depth, sized to those rows, in the bottom-right
    /// corner one row above the reserved status row,
    /// clipped to the viewport.
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

    /// Geometry for the open prompt: the bottom row — label then input —
    /// and, for the ex command line, the suggestion row above it.
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

    /// Draw purely from `self.view` and engine state into a buffer; no
    /// geometry math or state mutation here.
    fn render_to_buffer(&self, buf: &mut Buffer) {
        // Each window confined to its own rectangle;
        // the active tab's content below the tab bar. While a window is
        // maximized, it is the only one drawn; minimized windows have no
        // rectangle at all.
        for win in self.windows.values() {
            if self.maximized.is_some_and(|id| id != win.id) || self.minimized.contains(&win.id) {
                continue;
            }
            render_tab(win.active_tab(), buf);
        }
        for chrome in &self.view.chrome {
            render_tab_bar(chrome, self.focus, buf, self.view.elapsed);
        }
        // Highlight the selected text, unless its window is hidden behind
        // a maximized one.
        if let Some(sel) = &self.selection
            && self.maximized.is_none_or(|id| id == sel.window)
            && let Some(win) = self.windows.get(&sel.window)
        {
            render_selection(sel, win.content_rect(), buf);
        }
        // Separators between side-by-side windows,
        // uniformly dim.
        for sep in &self.view.separators {
            render_separator(sep, buf);
        }
        // The session status line on the reserved bottom
        // row; absent while a prompt renders there instead.
        if let Some(status) = &self.view.status {
            render_status(status, self.view.elapsed, buf);
        }
        if let Some(chrome) = &self.view.prompt {
            render_prompt_chrome(chrome, buf);
            if let Some(prompt) = &self.prompt {
                prompt.textarea.render(chrome.input, buf);
            }
        }
        // The key-hint popup draws over everything else.
        if let Some(hints) = &self.view.hints {
            render_hints(hints, buf);
        }
    }

    fn render(&self, frame: &mut Frame) {
        self.render_to_buffer(frame.buffer_mut());
        // While a prompt is open its textarea draws its own block
        // cursor; the host cursor stays hidden.
        if self.prompt.is_some() {
            return;
        }
        // The host cursor tracks the focused window's
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
    // The scroll-mode anchor or the live tail.
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
                // Colors and text attributes.
                dst.set_style(cell_style(cell.attrs()));
            }
        }
    }
}

/// Draw one window's tab bar: a two-cell rule lead-in, an
/// indicator per tab (the active one visually distinct), and the
/// remainder ruled. The rule is uniformly thin,
/// its brightness marking window focus;
/// the bar doubles as the boundary with a stacked window above.
fn render_tab_bar(chrome: &Chrome, focus: WindowId, buf: &mut Buffer, elapsed: Duration) {
    let bar = chrome.tab_bar;
    if bar.height == 0 || bar.width == 0 {
        return;
    }
    let focused = chrome.window == focus;
    // Badges and the rule stop short of the control group's reserved
    // width.
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
    // One thin rule weight; brightness signals focus.
    // Focused inherits the terminal's default foreground rather than
    // hardcoding white. The focused bar's rule also carries the active
    // tab's status animation in its status color — working shimmers,
    // blocked breathes — indexed by bar position so the effect sweeps
    // the whole width.
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
    // Badges stop where the bar runs out; the controls still draw.
    'badges: {
        // Two cells of rule anchor the bar's left edge.
        for _ in 0..2 {
            let style = rule_at(x);
            if !put(&mut x, '─', style) {
                break 'badges;
            }
        }
        for (i, badge) in chrome.tabs.iter().enumerate() {
            // Active is bright, inactive dimmed, no
            // background fill — neutral shades only, matching the
            // brightness-only chrome convention.
            let style = if badge.active {
                // Focused+active inherits the terminal's default foreground
                // instead of hardcoding white, so it respects the user's
                // terminal theme.
                let color = if focused { Color::Reset } else { Color::Gray };
                Style::default().fg(color)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            // `<index>:<name>`, indexed from 0.
            for ch in format!(" {}:{}", i, badge.name).chars() {
                if !put(&mut x, ch, style) {
                    break 'badges;
                }
            }
            // The bracketed status text, in its
            // state's color, only for tabs identified as running Claude Code;
            // working shimmers and blocked breathes.
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
    // The unused width up to the controls, same thin rule.
    let indicators_end = x;
    while x < badges_end {
        if let Some(dst) = buf.cell_mut(Position::new(x, bar.y)) {
            dst.set_symbol("─");
            dst.set_style(rule_at(x));
        }
        x += 1;
    }
    // Mark a scrolled tab so a frozen view isn't mistaken
    // for the live tail. Drawn over the rule, right-aligned against the
    // controls.
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
    // The control group: minimize, maximize/restore, exit, in standard
    // Unicode glyphs any monospace font covers, brightness following
    // window focus like the rest of the bar.
    if let Some(controls) = chrome.controls {
        // A hovered control brightens one step above its resting shade,
        // per the bar's bright/dim convention.
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

/// The direction a key repeats a move-tab dispatch in while the
/// move-repeat deadline is armed: the same shifted `H`/`J`/`K`/`L` or
/// Shift-Arrow the prefixed bindings use, without the prefix.
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

/// The layout tree's area: the viewport minus the bottom row reserved
/// for the session status line.
fn tree_area(area: Rect) -> Rect {
    Rect {
        height: area.height.saturating_sub(1),
        ..area
    }
}

/// The status line's clock text, formatted `%H:%M`.
fn clock_now() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

/// Expand a leading `~/` to the user's home directory; the server has no
/// shell to do it. Any other path passes through unchanged.
fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

/// Draw the session status line: name left, clock right,
/// on the neutral chrome background.
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
    let name_style = fill.fg(Color::Green);
    for (i, ch) in format!(" {}", status.name).chars().enumerate() {
        let x = row.x + i as u16;
        if x >= row.right() {
            break;
        }
        if let Some(dst) = buf.cell_mut(Position::new(x, row.y)) {
            dst.set_char(ch);
            dst.set_style(name_style);
        }
    }
    // Minimized windows' titles, clickable to restore.
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
    // Hostname two spaces left of the clock — or, in its place, the
    // pending-Claude indicator, shimmering in the same neutral
    // foreground.
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

/// Draw the key-hint popup: a bordered box on the neutral
/// chrome background, one row per table entry — keys bright, description
/// dimmed — matching the brightness-only chrome convention.
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
        // Border plus one margin cell, keys padded to the shared column.
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

/// Invert the selected cells; toggling rather than
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

/// Draw the prompt row — cleared, with its label —
/// and the suggestion row above it. The textarea widget itself
/// renders separately, over the cleared input area.
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

/// The vertical separator between side-by-side windows —
/// the only separator kind left. Always dimmed;
/// the tab bar's rule brightness alone marks focus.
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

/// Map an engine color to ratatui, deferring palette resolution to the
/// client's terminal so default and indexed colors follow the user's theme.
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
