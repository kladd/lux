//! Windows and the tabs they own: each tab is one PTY plus a terminal engine.

use std::collections::VecDeque;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;
use termwiz::cell::{CellAttributes, SemanticType};
use termwiz::input::{KeyCode, Modifiers};
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, Clipboard, ClipboardSelection, MouseEvent, Progress, Terminal as Engine,
    TerminalConfiguration, TerminalSize,
};

use crate::server::ServerEvent;
use crate::server::agent::{self, AgentKind, AgentState, Tracker};
use crate::server::anim;
use crate::server::config::OscTitles;
use crate::server::host::HostLink;
use crate::server::layout::WindowId;
use crate::server::modes::ModeTracker;
use crate::server::palette::TermColors;
use crate::server::park;
use crate::server::search;
use crate::server::upgrade::{
    self, AdoptedChild, AdoptedPty, ScreenState as SavedScreen, TabState,
};
use crate::server::wire::{HubMsg, RemoteTabId, ScreenState, TabMeta, TabRef, TabSnap, TabUpdate};

pub type TabId = usize;

/// Server-global, so PTY reader threads can tag events without knowing
/// their session.
static NEXT_TAB_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The adopting hub's terminal colors, which programs' color queries are
/// answered with while it is connected.
static HUB_COLORS: RwLock<Option<ColorPalette>> = RwLock::new(None);

pub fn set_hub_colors(colors: Option<&TermColors>) {
    let palette = colors.map(|colors| {
        let mut palette = ColorPalette::default();
        if let Some(rgb) = colors.fg {
            palette.foreground = rgb.into();
        }
        if let Some(rgb) = colors.bg {
            palette.background = rgb.into();
        }
        for (slot, rgb) in palette.colors.0.iter_mut().zip(colors.ansi) {
            if let Some(rgb) = rgb {
                *slot = rgb.into();
            }
        }
        palette
    });
    *HUB_COLORS.write().unwrap() = palette;
}

#[derive(Debug)]
struct LuxConfig;

impl TerminalConfiguration for LuxConfig {
    fn color_palette(&self) -> ColorPalette {
        HUB_COLORS.read().unwrap().clone().unwrap_or_default()
    }
}

/// Forwards a program's OSC 52 clipboard writes to the server loop. Queries
/// never reach here, so no program can read the clipboard.
struct ClipboardRelay {
    tab: TabId,
    tx: Sender<ServerEvent>,
}

impl Clipboard for ClipboardRelay {
    fn set_contents(&self, _: ClipboardSelection, data: Option<String>) -> anyhow::Result<()> {
        // `None` clears the clipboard, so there's nothing to relay.
        if let Some(text) = data {
            let _ = self.tx.send(ServerEvent::ProgramCopy(self.tab, text));
        }
        Ok(())
    }
}

/// Captures a program's OSC 9 notification text, OSC 0/2 window title, and
/// bell for the tab to read later. OSC 9;4 progress and OSC 1 icon titles
/// never land here.
struct NotificationRelay {
    text: Arc<Mutex<Option<String>>>,
    title: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
}

impl AlertHandler for NotificationRelay {
    fn alert(&mut self, alert: Alert) {
        match alert {
            Alert::ToastNotification { body, .. } => {
                *self.text.lock().unwrap() = Some(body);
            }
            Alert::WindowTitleChanged(title) => {
                *self.title.lock().unwrap() = Some(title);
            }
            Alert::Bell => self.bell.store(true, Ordering::Relaxed),
            _ => {}
        }
    }
}

/// What a tab did while it was not its window's active tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    Output,
    Bell,
}

/// The most recently completed command's output, as far as the shell's
/// command boundaries tell.
#[derive(Debug, PartialEq, Eq)]
pub enum LastOutput {
    Text(String),
    NoneCompleted,
    NoBoundaries,
}

const RATE_WINDOW: Duration = Duration::from_secs(1);
/// Bytes per second at which the rule's shimmer reaches the status text's
/// pace.
const FULL_SPEED_RATE: f32 = 8192.0;

/// Recent writes, whose volume paces the rule's shimmer sweep.
struct RecentOutput {
    writes: VecDeque<Write>,
    phase: f32,
    advanced: Instant,
}

struct Write {
    at: Instant,
    len: usize,
}

impl RecentOutput {
    fn new(now: Instant) -> Self {
        Self {
            writes: VecDeque::new(),
            phase: 0.0,
            advanced: now,
        }
    }

    fn record(&mut self, now: Instant, len: usize) {
        self.prune(now);
        self.writes.push_back(Write { at: now, len });
    }

    fn prune(&mut self, now: Instant) {
        while self
            .writes
            .front()
            .is_some_and(|w| now.duration_since(w.at) > RATE_WINDOW)
        {
            self.writes.pop_front();
        }
    }

    /// Moves the sweep on by the time elapsed at the current output rate,
    /// so it stands still without output unless `sustained`.
    fn advance(&mut self, now: Instant, sustained: bool) -> f32 {
        self.prune(now);
        let bytes: usize = self.writes.iter().map(|w| w.len).sum();
        let rate = bytes as f32 / RATE_WINDOW.as_secs_f32();
        let pace = if sustained {
            1.0
        } else {
            (rate / FULL_SPEED_RATE).min(1.0)
        };
        let sweeps_per_sec = pace / anim::PERIOD;
        let dt = now.duration_since(self.advanced).as_secs_f32();
        self.phase = (self.phase + sweeps_per_sec * dt).rem_euclid(1.0);
        self.advanced = now;
        self.phase
    }
}

/// A tab's agent reaching done or blocked, for the server to raise a
/// desktop notification.
pub struct Notice {
    pub id: TabId,
    pub tab: String,
    pub blocked: bool,
    pub summary: Option<String>,
}

/// A leaf of the layout tree: one rectangle owning a list of tabs.
pub struct Window {
    pub id: WindowId,
    /// Whole window, tab bar row included.
    pub rect: Rect,
    pub tabs: Vec<Tab>,
    pub active: usize,
}

impl Window {
    pub fn with_tab(id: WindowId, rect: Rect, tab: Tab) -> Self {
        Self {
            id,
            rect,
            tabs: vec![tab],
            active: 0,
        }
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    pub fn find_tab_mut(&mut self, id: TabId) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub fn content_rect(&self) -> Rect {
        content_rect(self.rect)
    }

    pub fn tab_bar_rect(&self) -> Rect {
        Rect {
            height: self.rect.height.min(1),
            ..self.rect
        }
    }

    /// Drops any tab that fails to spawn.
    pub fn restore(
        rect: Rect,
        snap: &crate::server::persist::WindowSnapshot,
        tx: &Sender<ServerEvent>,
    ) -> Option<Self> {
        let content = content_rect(rect);
        let mut tabs = Vec::new();
        for tab in &snap.tabs {
            // A missing saved directory falls back to the server's own.
            let cwd = tab.cwd.is_dir().then(|| tab.cwd.clone());
            let spawned = match &tab.claude_session {
                Some(session) => {
                    Tab::spawn_claude_resume(content, cwd.clone(), session, tx.clone())
                        .or_else(|_| Tab::spawn(content, cwd, tx.clone()))
                }
                None => Tab::spawn(content, cwd, tx.clone()),
            };
            if let Ok(mut spawned_tab) = spawned {
                if let Some(name) = &tab.name {
                    spawned_tab.set_name(name.clone());
                }
                tabs.push(spawned_tab);
            }
        }
        if tabs.is_empty() {
            return None;
        }
        let active = snap.active.min(tabs.len() - 1);
        Some(Self {
            id: snap.id,
            rect,
            tabs,
            active,
        })
    }

    /// Resize every tab, not just the active one, so none is stale when it
    /// becomes active.
    pub fn reconcile(&mut self) {
        let content = self.content_rect();
        for tab in &mut self.tabs {
            if tab.rect != content {
                tab.resize(content);
            }
        }
    }
}

pub fn content_rect(rect: Rect) -> Rect {
    // The one chrome row is the tab bar, which also divides stacked windows.
    Rect {
        y: rect.y + rect.height.min(1),
        height: rect.height.saturating_sub(1),
        ..rect
    }
}

/// The foreground command's name from every /proc source: `comm` may be
/// truncated, argv[0] may be a symlink or wrapper, and a launcher run
/// through an interpreter names the agent only in its script argument.
struct Foreground {
    comm: String,
    arg0: String,
    /// Basename of the executable's real path.
    exe: String,
    /// The agent a generic interpreter or shell was given to run.
    wrapped: Option<&'static str>,
}

impl Foreground {
    fn names(&self) -> impl Iterator<Item = &str> {
        [self.comm.as_str(), self.arg0.as_str(), self.exe.as_str()]
            .into_iter()
            .chain(self.wrapped)
    }

    fn is_claude(&self) -> bool {
        self.names().any(|name| name == "claude")
    }

    fn is_codex(&self) -> bool {
        self.names().any(|name| name == "codex")
    }

    fn is_kiro(&self) -> bool {
        self.names()
            .any(|name| ["kiro", "kiro-cli"].contains(&name))
    }

    fn agent_kind(&self) -> Option<AgentKind> {
        if self.is_claude() {
            Some(AgentKind::Claude)
        } else if self.is_codex() {
            Some(AgentKind::Codex)
        } else if self.is_kiro() {
            Some(AgentKind::Kiro)
        } else {
            None
        }
    }

    fn display_name(&self) -> &str {
        if self.arg0.is_empty() {
            &self.comm
        } else {
            &self.arg0
        }
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// The agent named by the script or command a generic interpreter or
/// shell was invoked with, as its plain command name.
fn wrapped_agent(argv: &[String]) -> Option<&'static str> {
    let interpreter = basename(argv.first()?);
    let python = interpreter
        .strip_prefix("python")
        .is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'));
    // Inline code (`-e`, python's `-c`) runs no script; a shell's `-c`
    // runs a command whose first word is the program.
    let (code_flags, command_flag, value_flags): (&[&str], Option<&str>, &[&str]) =
        match interpreter {
            "node" | "bun" => (
                &["-e", "--eval", "-p", "--print"],
                None,
                &[
                    "-r",
                    "--require",
                    "--loader",
                    "--import",
                    "--experimental-loader",
                ],
            ),
            "sh" | "bash" | "zsh" | "fish" => (&[], Some("-c"), &[]),
            _ if python => (&["-c"], None, &["-m", "-W", "-X"]),
            _ => return None,
        };
    let mut args = argv.iter().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            return agent_in_path(args.next()?);
        }
        if code_flags.contains(&arg.as_str()) {
            return None;
        }
        if command_flag == Some(arg.as_str()) {
            let command = args.next()?;
            let program = command.split_whitespace().find(|word| *word != "exec")?;
            return agent_in_path(program.trim_matches(['"', '\'']));
        }
        if arg.starts_with('-') {
            if value_flags.contains(&arg.as_str()) {
                args.next();
            }
            continue;
        }
        return agent_in_path(arg);
    }
    None
}

/// The agent a script path names, by its basename or, for Claude Code's
/// npm package entry point, by the package directory.
fn agent_in_path(path: &str) -> Option<&'static str> {
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let file = parts.last()?;
    let stem = [".js", ".mjs", ".cjs", ".py", ".sh"]
        .into_iter()
        .find_map(|ext| file.strip_suffix(ext))
        .unwrap_or(file);
    match stem {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "kiro" | "kiro-cli" => Some("kiro"),
        _ => parts
            .ends_with(&["@anthropic-ai", "claude-code", file])
            .then_some("claude"),
    }
}

pub struct Tab {
    pub id: TabId,
    pub name: String,
    manual_name: bool,
    pub engine: Engine,
    pub rect: Rect,
    pub drawn_seqno: usize,
    /// Top line of the view in scroll mode, as a stable row index so it
    /// survives scrollback trimming. `None` follows live output.
    scroll_top: Option<isize>,
    /// The last submitted search's matches, oldest first; cleared on
    /// leaving scroll mode.
    search: Option<Vec<search::Match>>,
    pub agent: Option<Tracker>,
    /// Seeded on a resume spawn, refreshed at save time, cleared when
    /// claude exits.
    pub claude_session: Option<String>,
    /// Survives an unreadable foreground (mid-exec) and clears only on a
    /// definite non-claude sighting.
    pub running_claude: bool,
    /// Latest OSC 9 text from the program, taken once per desktop
    /// notification so a stale summary never repeats.
    notify_text: Arc<Mutex<Option<String>>>,
    /// Latest OSC 0/2 window title: `None` until set, empty once cleared.
    osc_title: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
    /// Set while the tab is off screen, cleared once it is looked at.
    pub activity: Option<Activity>,
    output: RecentOutput,
    /// Output and bell not yet reported to an adopting hub.
    unsent_bytes: usize,
    unsent_bell: bool,
    modes: ModeTracker,
    io: Io,
}

enum Io {
    /// The engine is the authority on the PTY's screen.
    Local {
        master: Box<dyn MasterPty>,
        child: Box<dyn Child + Send + Sync>,
    },
    /// The engine mirrors a connected host's tab.
    Remote(RemoteIo),
}

struct RemoteIo {
    link: HostLink,
    wire: TabRef,
    id: Option<RemoteTabId>,
    meta: TabMeta,
    /// The host's stable row index minus the engine's, on the current
    /// screen.
    delta: isize,
    /// The primary screen's `delta`, kept while the alternate screen is up.
    primary_delta: isize,
    /// False when the snapshot arrived on the alternate screen, so the
    /// primary screen was never sent.
    primary_known: bool,
    requested: (u16, u16),
}

/// What a remote update did, for activity marks and redraws.
#[derive(Default)]
pub struct RemoteChange {
    pub output: bool,
    pub bell: bool,
    pub display: bool,
}

/// Tracks what an adopting hub has been sent of one tab.
pub struct Synced {
    seqno: usize,
    screen: ScreenState,
    meta: TabMeta,
}

impl Tab {
    pub fn spawn(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        Self::spawn_argv(rect, cwd, &[&shell], tx)
    }

    pub fn spawn_claude_resume(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        session: &str,
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let mut tab = Self::spawn_argv(rect, cwd, &["claude", "--resume", session], tx)?;
        // Placeholder until the save-time refresh reads the new instance's
        // session file.
        tab.claude_session = Some(session.to_string());
        Ok(tab)
    }

    pub fn spawn_argv(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        argv: &[&str],
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let id = NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pty = native_pty_system();
        let pair = pty.openpty(pty_size(rect)).context("open PTY")?;
        let mut cmd = CommandBuilder::from_argv(argv.iter().map(|arg| (*arg).into()).collect());
        // Programs talk to the engine, not the host terminal, so TERM is fixed.
        cmd.env("TERM", "xterm-256color");
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);

        let engine = local_engine(id, term_size(rect), pair.master.as_ref(), tx)?;
        let mut tab = Self::assemble(
            id,
            engine,
            rect,
            Io::Local {
                master: pair.master,
                child,
            },
        );
        tab.name = std::path::Path::new(argv[0])
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| argv[0].to_string());
        Ok(tab)
    }

    fn assemble(id: TabId, mut engine: Engine, rect: Rect, io: Io) -> Self {
        let notify_text = Arc::new(Mutex::new(None));
        let osc_title = Arc::new(Mutex::new(None));
        let bell = Arc::new(AtomicBool::new(false));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: notify_text.clone(),
            title: osc_title.clone(),
            bell: bell.clone(),
        }));
        Self {
            id,
            name: String::new(),
            manual_name: false,
            engine,
            rect,
            drawn_seqno: 0,
            scroll_top: None,
            search: None,
            agent: None,
            claude_session: None,
            running_claude: false,
            notify_text,
            osc_title,
            bell,
            activity: None,
            output: RecentOutput::new(Instant::now()),
            unsent_bytes: 0,
            unsent_bell: false,
            modes: ModeTracker::default(),
            io,
        }
    }

    /// A tab the hub asked a host to spawn, blank until the host answers.
    pub fn remote_placeholder(link: HostLink, token: u64, rect: Rect) -> Self {
        let id = NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let size = (rect.width.max(1), rect.height.max(1));
        let io = Io::Remote(RemoteIo {
            link,
            wire: TabRef::Token(token),
            id: None,
            meta: TabMeta {
                name: String::new(),
                progress: None,
                mouse_grabbed: false,
                agent: None,
            },
            delta: 0,
            primary_delta: 0,
            primary_known: true,
            requested: size,
        });
        Self::assemble(id, mirror_engine(size.0, size.1), rect, io)
    }

    pub fn from_remote(link: HostLink, snap: TabSnap, now: Instant) -> Self {
        let wire = TabRef::Id(snap.id);
        let rect = Rect::new(0, 0, snap.screen.cols, snap.screen.rows);
        let mut tab = Self::remote_placeholder(link, 0, rect);
        if let Io::Remote(io) = &mut tab.io {
            io.wire = wire;
        }
        tab.load_remote(snap, now);
        tab
    }

    /// Replaces the mirror with a host's whole copy of the tab.
    pub fn load_remote(&mut self, snap: TabSnap, now: Instant) {
        let Io::Remote(io) = &mut self.io else {
            return;
        };
        io.id = Some(snap.id);
        let state = snap.screen;
        let mut engine = mirror_engine(state.cols, state.rows);
        if state.alt {
            engine.advance_bytes(b"\x1b[?1049h");
        }
        load_lines(&mut engine, snap.lines);
        io.delta = state.top - engine.screen().visible_row_to_stable_row(0);
        io.primary_known = !state.alt;
        sync_cursor(&mut engine, &state);
        self.engine = engine;
        // The replaced engine's seqno means nothing to the new one.
        self.drawn_seqno = usize::MAX;
        self.scroll_top = None;
        self.search = None;
        self.apply_meta(snap.meta, now);
    }

    pub fn apply_remote(&mut self, update: TabUpdate, now: Instant) -> RemoteChange {
        let Io::Remote(io) = &mut self.io else {
            return RemoteChange::default();
        };
        let state = update.screen;
        let mut resync = false;
        let was_alt = self.engine.is_alt_screen_active();
        let size = self.engine.get_size();
        let resized = size.cols != usize::from(state.cols) || size.rows != usize::from(state.rows);
        if resized {
            self.engine
                .resize(term_size(Rect::new(0, 0, state.cols, state.rows)));
            // The host's primary screen reflowed out of sight.
            if was_alt || state.alt {
                io.primary_known = false;
            }
        }
        if state.alt && !was_alt {
            io.primary_delta = io.delta;
            self.engine.advance_bytes(b"\x1b[?1049h");
        } else if !state.alt && was_alt {
            self.engine.advance_bytes(b"\x1b[?1049l");
            io.delta = io.primary_delta;
            resync |= !io.primary_known;
        }
        let top = self.engine.screen().visible_row_to_stable_row(0);
        if resized || (state.alt && !was_alt) {
            io.delta = state.top - top;
        } else if state.top > top + io.delta {
            scroll_up(&mut self.engine, (state.top - top - io.delta) as usize);
        } else if state.top < top + io.delta {
            resync = true;
        }
        let output = !update.lines.is_empty() || update.bytes > 0;
        let screen = self.engine.screen_mut();
        for (row, line) in update.lines {
            if let Some(phys) = screen.stable_row_to_phys(row - io.delta) {
                *screen.line_mut(phys) = line;
            }
        }
        if !state.alt && screen.scrollback_rows() != state.held {
            // The host erased its scrollback.
            if state.held == usize::from(state.rows) {
                self.engine.erase_scrollback();
            }
            if self.engine.screen().scrollback_rows() != state.held {
                resync = true;
            }
        }
        sync_cursor(&mut self.engine, &state);
        self.engine.increment_seqno();
        if resync && let Some(id) = io.id {
            io.link.send(HubMsg::Resync(id));
        }
        if update.bytes > 0 {
            self.output.record(now, update.bytes);
        }
        let display = self.apply_meta(update.meta, now);
        RemoteChange {
            output,
            bell: update.bell,
            display,
        }
    }

    /// Returns whether the tab bar changes.
    fn apply_meta(&mut self, meta: TabMeta, now: Instant) -> bool {
        let mut changed = false;
        if meta.name != self.name {
            self.name = meta.name.clone();
            changed = true;
        }
        match (&mut self.agent, &meta.agent) {
            (Some(tracker), Some(report)) => changed |= tracker.apply_report(report, now),
            (None, Some(report)) => {
                self.agent = Some(Tracker::reported(report, now));
                changed = true;
            }
            (Some(_), None) => {
                self.agent = None;
                changed = true;
            }
            (None, None) => {}
        }
        if let Io::Remote(io) = &mut self.io {
            changed |= io.meta.progress != meta.progress;
            io.meta = meta;
        }
        changed
    }

    /// How a host knows this tab.
    pub fn wire(&self) -> Option<TabRef> {
        match &self.io {
            Io::Remote(io) => Some(io.wire),
            Io::Local { .. } => None,
        }
    }

    pub fn remote_id(&self) -> Option<RemoteTabId> {
        match &self.io {
            Io::Remote(io) => io.id,
            Io::Local { .. } => None,
        }
    }

    pub fn key_down(&mut self, code: KeyCode, mods: Modifiers) {
        match &self.io {
            Io::Local { .. } => {
                let _ = self.engine.key_down(code, mods);
            }
            Io::Remote(io) => io.link.send(HubMsg::Key {
                tab: io.wire,
                code,
                mods,
            }),
        }
    }

    pub fn send_paste(&mut self, text: &str) {
        match &self.io {
            Io::Local { .. } => {
                let _ = self.engine.send_paste(text);
            }
            Io::Remote(io) => io.link.send(HubMsg::Paste {
                tab: io.wire,
                text: text.to_string(),
            }),
        }
    }

    pub fn mouse_event(&mut self, event: MouseEvent) {
        match &self.io {
            Io::Local { .. } => {
                let _ = self.engine.mouse_event(event);
            }
            Io::Remote(io) => io.link.send(HubMsg::Mouse {
                tab: io.wire,
                event,
            }),
        }
    }

    pub fn mouse_grabbed(&self) -> bool {
        match &self.io {
            Io::Local { .. } => self.engine.is_mouse_grabbed(),
            Io::Remote(io) => io.meta.mouse_grabbed,
        }
    }

    /// A host hears of it too, so its report stops showing done.
    pub fn mark_seen(&mut self) {
        let Some(tracker) = &mut self.agent else {
            return;
        };
        if tracker.mark_seen()
            && let Io::Remote(io) = &self.io
        {
            io.link.send(HubMsg::Seen(io.wire));
        }
    }

    /// Output and bell for an adopting hub to show as activity.
    pub fn note_unsent(&mut self, len: usize, bell: bool) {
        self.unsent_bytes = self.unsent_bytes.saturating_add(len);
        self.unsent_bell |= bell;
    }

    fn wire_meta(&self, now: Instant) -> TabMeta {
        TabMeta {
            name: self.name.clone(),
            progress: self.progress(),
            mouse_grabbed: self.mouse_grabbed(),
            agent: self.agent.as_ref().map(|t| t.report(now)),
        }
    }

    fn wire_screen(&self) -> ScreenState {
        let screen = self.engine.screen();
        let size = self.engine.get_size();
        let cursor = self.engine.cursor_pos();
        ScreenState {
            cols: size.cols as u16,
            rows: size.rows as u16,
            alt: self.engine.is_alt_screen_active(),
            top: screen.visible_row_to_stable_row(0),
            held: screen.scrollback_rows(),
            cursor_x: cursor.x,
            cursor_y: cursor.y,
            cursor_visible: cursor.visibility == termwiz::surface::CursorVisibility::Visible,
        }
    }

    /// The whole tab for a hub, and the baseline for later updates.
    pub fn wire_snapshot(&mut self, now: Instant) -> (TabSnap, Synced) {
        let screen = self.engine.screen();
        let lines = screen.lines_in_phys_range(0..screen.scrollback_rows());
        let synced = Synced {
            seqno: self.engine.current_seqno(),
            screen: self.wire_screen(),
            meta: self.wire_meta(now),
        };
        self.unsent_bytes = 0;
        self.unsent_bell = false;
        let snap = TabSnap {
            id: self.id,
            meta: synced.meta.clone(),
            screen: synced.screen,
            lines,
        };
        (snap, synced)
    }

    /// What changed since `synced`, or `None` when nothing did.
    pub fn wire_update(&mut self, synced: &mut Synced, now: Instant) -> Option<TabUpdate> {
        let seqno = self.engine.current_seqno();
        let state = self.wire_screen();
        let meta = self.wire_meta(now);
        if seqno == synced.seqno
            && state == synced.screen
            && meta.same(&synced.meta)
            && self.unsent_bytes == 0
            && !self.unsent_bell
        {
            return None;
        }
        let screen = self.engine.screen();
        let bottom = state.top + state.rows as isize;
        let reshaped = state.alt != synced.screen.alt
            || state.cols != synced.screen.cols
            || state.rows != synced.screen.rows;
        let rows: Vec<isize> = if reshaped {
            (state.top..bottom).collect()
        } else {
            // Rows that scrolled into history since the last update may
            // have changed on the way.
            let oldest = screen.phys_to_stable_row_index(0);
            let from = synced.screen.top.min(state.top).max(oldest);
            screen.get_changed_stable_rows(from..bottom, synced.seqno)
        };
        let lines = rows
            .into_iter()
            .filter_map(|row| {
                let phys = screen.stable_row_to_phys(row)?;
                Some((row, screen.lines_in_phys_range(phys..phys + 1).pop()?))
            })
            .collect();
        let update = TabUpdate {
            meta: meta.clone(),
            screen: state,
            lines,
            bytes: std::mem::take(&mut self.unsent_bytes),
            bell: std::mem::take(&mut self.unsent_bell),
        };
        *synced = Synced {
            seqno,
            screen: state,
            meta,
        };
        Some(update)
    }

    pub fn note_output(&mut self, len: usize, now: Instant) {
        self.output.record(now, len);
    }

    /// The rule shimmer's phase, moved on by recent output, or at full
    /// pace while `sustained`.
    pub fn advance_shimmer(&mut self, now: Instant, sustained: bool) -> f32 {
        self.output.advance(now, sustained)
    }

    pub fn take_bell(&mut self) -> bool {
        self.bell.swap(false, Ordering::Relaxed)
    }

    /// The percentage the program last reported through OSC 9;4.
    pub fn progress(&self) -> Option<u8> {
        if let Io::Remote(io) = &self.io {
            return io.meta.progress;
        }
        match self.engine.get_progress() {
            Progress::Percentage(p) | Progress::Error(p) => Some(p.min(100)),
            Progress::None | Progress::Indeterminate => None,
        }
    }

    pub fn last_command_output(&mut self) -> LastOutput {
        last_command_output(&mut self.engine)
    }

    /// In scroll mode: the top row's index into history, the view's
    /// height, and history's row count.
    pub fn scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let top = self.scroll_top?;
        let screen = self.engine.screen();
        let phys = screen.stable_range(&(top..top + 1)).start;
        Some((phys, screen.physical_rows, screen.scrollback_rows()))
    }

    /// Removal follows once the PTY closes, like any other exit.
    pub fn kill(&mut self) {
        match &mut self.io {
            Io::Local { child, .. } => {
                let _ = child.kill();
            }
            Io::Remote(io) => io.link.send(HubMsg::Kill(io.wire)),
        }
    }

    pub fn set_name(&mut self, name: String) {
        if let Io::Remote(io) = &self.io {
            io.link.send(HubMsg::SetName {
                tab: io.wire,
                name: Some(name.clone()),
            });
        }
        self.name = name;
        self.manual_name = true;
    }

    pub fn clear_name(&mut self, osc_titles: OscTitles) {
        self.manual_name = false;
        if let Io::Remote(io) = &self.io {
            io.link.send(HubMsg::SetName {
                tab: io.wire,
                name: None,
            });
        }
        // Don't wait for the next PTY output to trigger refresh_identity.
        if let Some(fg) = self.foreground() {
            let name = self.derived_name(&fg, osc_titles);
            if !name.is_empty() {
                self.name = name;
            }
        }
    }

    fn derived_name(&self, fg: &Foreground, osc_titles: OscTitles) -> String {
        let kind = fg.agent_kind();
        // Claude Code churns the OSC title as a status channel, so the
        // session name outranks it.
        let session = (kind == Some(AgentKind::Claude))
            .then(|| self.claude_session_name())
            .flatten();
        automatic_name(
            kind,
            session,
            self.osc_title(),
            fg.display_name(),
            osc_titles,
        )
    }

    fn osc_title(&self) -> Option<String> {
        self.osc_title
            .lock()
            .unwrap()
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string)
    }

    pub fn is_manually_named(&self) -> bool {
        self.manual_name
    }

    /// Re-derive the display name and re-run agent detection after new PTY
    /// output. The bool is whether the display changed.
    pub fn refresh_identity(&mut self, osc_titles: OscTitles) -> (bool, Option<Notice>) {
        let fg = self.foreground();
        let renamed = if self.manual_name {
            false
        } else {
            // Unreadable foreground (mid-exec): keep the current name rather
            // than flicker.
            let new_name = fg
                .as_ref()
                .map_or_else(String::new, |fg| self.derived_name(fg, osc_titles));
            if !new_name.is_empty() && new_name != self.name {
                self.name = new_name;
                true
            } else {
                false
            }
        };
        let kind = fg.as_ref().and_then(Foreground::agent_kind);
        let Some(kind) = kind else {
            // Only a definite non-claude sighting clears session identity.
            // An unreadable foreground (mid-exec) is transient.
            if fg.is_some() {
                self.claude_session = None;
                self.running_claude = false;
            }
            return (self.agent.take().is_some() || renamed, None);
        };
        match kind {
            agent::AgentKind::Claude => self.running_claude = true,
            // Another agent is as definite a non-claude sighting as a plain
            // shell.
            agent::AgentKind::Codex | agent::AgentKind::Kiro => {
                self.claude_session = None;
                self.running_claude = false;
            }
        }
        // A tracker for a different agent belongs to a process that's gone.
        if self.agent.as_ref().is_some_and(|t| t.kind() != kind) {
            self.agent = None;
        }
        let snapshot = agent::Snapshot::capture(&self.engine);
        let raw = agent::evaluate(kind, &snapshot);
        let appeared = self.agent.is_none();
        let tracker = self.agent.get_or_insert_with(|| Tracker::new(kind));
        let entered = tracker.observe(raw, std::time::Instant::now());
        let notice = self.notice_for(entered);
        (appeared || entered.is_some() || renamed, notice)
    }

    fn notice_for(&mut self, entered: Option<AgentState>) -> Option<Notice> {
        let blocked = match entered? {
            AgentState::Idle => false,
            AgentState::Blocked => true,
            AgentState::Working | AgentState::Waiting => return None,
        };
        Some(Notice {
            id: self.id,
            tab: self.name.clone(),
            blocked,
            summary: self.notify_text.lock().unwrap().take(),
        })
    }

    fn process_group_leader(&self) -> Option<i32> {
        match &self.io {
            Io::Local { master, .. } => master.process_group_leader(),
            Io::Remote(_) => None,
        }
    }

    fn foreground(&self) -> Option<Foreground> {
        let pid = self.process_group_leader()?;
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let argv: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let arg0 = argv.first().map(|arg| basename(arg).to_string());
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
            .unwrap_or_default();
        Some(Foreground {
            comm: comm.trim().to_string(),
            arg0: arg0.unwrap_or_default(),
            exe,
            wrapped: wrapped_agent(&argv),
        })
    }

    /// The session file Claude Code keeps for its own pid.
    fn claude_session_file(&self) -> Option<serde_json::Value> {
        let pid = self.process_group_leader()?;
        let home = std::env::var_os("HOME")?;
        let path = std::path::PathBuf::from(home)
            .join(".claude/sessions")
            .join(format!("{pid}.json"));
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn claude_session_name(&self) -> Option<String> {
        let json = self.claude_session_file()?;
        // No nameSource field means the user set the name.
        if json["nameSource"].as_str() == Some("derived") {
            return None;
        }
        let name = json["name"].as_str()?;
        (!name.is_empty()).then(|| name.to_string())
    }

    /// Read fresh each time, since `/clear` changes the id within one
    /// process.
    pub fn claude_session_id(&self) -> Option<String> {
        let id = self.claude_session_file()?["sessionId"]
            .as_str()?
            .to_string();
        (!id.is_empty()).then_some(id)
    }

    pub fn working_dir(&self) -> Option<std::path::PathBuf> {
        let pid = self.process_group_leader()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Commit an elapsed idle debounce, returning whether the display
    /// changed plus any notice.
    pub fn tick_agent(&mut self, now: std::time::Instant) -> (bool, Option<Notice>) {
        let entered = self.agent.as_mut().and_then(|t| t.tick(now));
        let notice = self.notice_for(entered);
        (entered.is_some(), notice)
    }

    pub fn agent_pending_idle(&self) -> bool {
        self.agent.as_ref().is_some_and(|t| t.pending())
    }

    pub fn scroll_mode(&self) -> bool {
        self.scroll_top.is_some()
    }

    pub fn enter_scroll_mode(&mut self) {
        if self.scroll_top.is_none() {
            self.scroll_top = Some(self.engine.screen().visible_row_to_stable_row(0));
        }
    }

    pub fn exit_scroll_mode(&mut self) {
        self.scroll_top = None;
        self.search = None;
    }

    pub fn search(&self) -> Option<&[search::Match]> {
        self.search.as_deref()
    }

    /// Find `query` throughout history, then jump to the nearest match
    /// above the view.
    pub fn search_for(&mut self, query: String) {
        let screen = self.engine.screen();
        let all = 0..screen.scrollback_rows();
        let mut matches = Vec::new();
        for (phys, line) in screen.lines_in_phys_range(all).iter().enumerate() {
            let row = screen.phys_to_stable_row_index(phys);
            let cells: Vec<(usize, usize, String)> = line
                .visible_cells()
                .map(|cell| (cell.cell_index(), cell.width(), cell.str().to_string()))
                .collect();
            let cells = cells
                .iter()
                .map(|(col, width, s)| (*col, *width, s.as_str()));
            for cols in search::spans(cells, &query) {
                matches.push(search::Match { row, cols });
            }
        }
        self.search = Some(matches);
        self.jump_to_match(true);
    }

    /// Move the view to the nearest match above (`older`) or below its
    /// top row. Returns whether there was one.
    pub fn jump_to_match(&mut self, older: bool) -> bool {
        let (Some(top), Some(matches)) = (self.scroll_top, &self.search) else {
            return false;
        };
        let target = if older {
            search::above(matches, top)
        } else {
            search::below(matches, top)
        };
        let Some(target) = target else {
            return false;
        };
        let screen = self.engine.screen();
        let oldest = screen.phys_to_stable_row_index(0);
        let live_top = screen.visible_row_to_stable_row(0);
        self.scroll_top = Some(target.row.clamp(oldest, live_top));
        true
    }

    /// Matches in the scrolled view as `(view row, columns)`.
    pub fn visible_matches(&self) -> impl Iterator<Item = (usize, std::ops::Range<usize>)> + '_ {
        let top = self.scroll_top;
        let rows = self.engine.screen().physical_rows as isize;
        self.search.iter().flatten().filter_map(move |m| {
            let y = m.row - top?;
            (0..rows).contains(&y).then(|| (y as usize, m.cols.clone()))
        })
    }

    /// Negative `delta` scrolls into history. Returns whether the view is
    /// at the live bottom afterwards.
    pub fn scroll_by(&mut self, delta: isize) -> bool {
        let Some(top) = self.scroll_top else {
            return true;
        };
        let screen = self.engine.screen();
        let oldest = screen.phys_to_stable_row_index(0);
        let live_top = screen.visible_row_to_stable_row(0);
        let new_top = (top + delta).clamp(oldest, live_top);
        self.scroll_top = Some(new_top);
        new_top == live_top
    }

    pub fn view_range(&self) -> std::ops::Range<usize> {
        let screen = self.engine.screen();
        let rows = screen.physical_rows as isize;
        match self.scroll_top {
            Some(top) => screen.stable_range(&(top..top + rows)),
            None => screen.phys_range(&(0..rows as i64)),
        }
    }

    /// A remote tab's mirror follows once its host has resized.
    pub fn resize(&mut self, rect: Rect) {
        self.rect = rect;
        match &mut self.io {
            Io::Local { master, .. } => {
                let _ = master.resize(pty_size(rect));
                self.engine.resize(term_size(rect));
            }
            Io::Remote(io) => {
                let size = (rect.width.max(1), rect.height.max(1));
                if io.requested != size {
                    io.requested = size;
                    io.link.send(HubMsg::Resize {
                        tab: io.wire,
                        cols: size.0,
                        rows: size.1,
                    });
                }
            }
        }
    }

    pub fn wait(&mut self) -> i32 {
        let Io::Local { child, .. } = &mut self.io else {
            return 0;
        };
        match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 0,
        }
    }

    /// Feeds the program's output to the engine.
    pub fn advance(&mut self, bytes: &[u8]) {
        let actions = self.modes.parse(bytes);
        self.engine.perform_actions(actions);
    }

    /// What the next image needs to carry the tab on. `None` for a remote
    /// tab, which its host keeps.
    pub fn handoff(&mut self) -> Option<TabState> {
        let Io::Local { master, child } = &self.io else {
            return None;
        };
        let pty = master.as_raw_fd()?;
        let pid = child.process_id();
        let size = self.engine.get_size();
        let cursor_visible =
            self.engine.cursor_pos().visibility == termwiz::surface::CursorVisibility::Visible;
        let alt = self.engine.is_alt_screen_active().then(|| {
            let alt = saved_screen(&self.engine);
            // To the primary screen without clearing either, with the
            // cursor it saved on the way out.
            self.engine.advance_bytes(b"\x1b[?47l\x1b8");
            alt
        });
        let primary = saved_screen(&self.engine);
        if let Some(alt) = &alt {
            self.engine.advance_bytes(b"\x1b[?47h");
            place_cursor(&mut self.engine, (alt.cursor_x, alt.cursor_y));
        }
        Some(TabState {
            pty,
            pid,
            rect: upgrade::rect_state(self.rect),
            name: self.name.clone(),
            manual_name: self.manual_name,
            claude_session: self.claude_session.clone(),
            running_claude: self.running_claude,
            osc_title: self.osc_title.lock().unwrap().clone(),
            modes: self.modes.modes().to_vec(),
            cols: size.cols as u16,
            rows: size.rows as u16,
            cursor_visible,
            primary,
            alt,
        })
    }

    /// Carries on a tab the previous image handed off.
    pub fn resume(state: TabState, tx: Sender<ServerEvent>) -> anyhow::Result<Self> {
        // SAFETY: the previous image passed this descriptor for this tab
        // alone.
        let master = AdoptedPty(unsafe { upgrade::adopt_fd(state.pty) }?);
        let id = NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let size = term_size(Rect::new(0, 0, state.cols, state.rows));
        let mut engine = local_engine(id, size, &master, tx)?;
        let mut cursor = (state.primary.cursor_x, state.primary.cursor_y);
        load_lines(&mut engine, state.primary.lines);
        place_cursor(&mut engine, cursor);
        if let Some(alt) = state.alt {
            // Saves the primary screen's cursor for the program's return.
            engine.advance_bytes(b"\x1b[?1049h");
            cursor = (alt.cursor_x, alt.cursor_y);
            load_lines(&mut engine, alt.lines);
        }
        let pid = state
            .pid
            .and_then(|pid| rustix::process::Pid::from_raw(pid as i32));
        let mut tab = Self::assemble(
            id,
            engine,
            upgrade::state_rect(state.rect),
            Io::Local {
                master: Box::new(master),
                child: Box::new(AdoptedChild(pid)),
            },
        );
        tab.modes = ModeTracker::with_modes(state.modes);
        tab.engine.advance_bytes(tab.modes.replay());
        // Margins and origin mode home the cursor, so it goes last.
        place_cursor(&mut tab.engine, cursor);
        if !state.cursor_visible {
            tab.engine.advance_bytes(b"\x1b[?25l");
        }
        if let Some(title) = state.osc_title {
            tab.engine.advance_bytes(format!("\x1b]2;{title}\x1b\\"));
        }
        tab.name = state.name;
        tab.manual_name = state.manual_name;
        tab.claude_session = state.claude_session;
        tab.running_claude = state.running_claude;
        Ok(tab)
    }

    /// Signals the foreground program that its window changed, which most
    /// programs answer by redrawing.
    pub fn prompt_redraw(&self) {
        if let Some(group) = self
            .process_group_leader()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::WINCH);
        }
    }
}

/// The current screen's lines and cursor.
fn saved_screen(engine: &Engine) -> SavedScreen {
    let screen = engine.screen();
    let cursor = engine.cursor_pos();
    SavedScreen {
        lines: screen.lines_in_phys_range(0..screen.scrollback_rows()),
        cursor_x: cursor.x,
        cursor_y: cursor.y,
    }
}

fn place_cursor(engine: &mut Engine, (x, y): (usize, i64)) {
    engine.advance_bytes(format!("\x1b[{};{}H", y + 1, x + 1));
}

/// An engine that answers the program on `master`, whose output a new
/// reader thread feeds back as events.
fn local_engine(
    id: TabId,
    size: TerminalSize,
    master: &dyn MasterPty,
    tx: Sender<ServerEvent>,
) -> anyhow::Result<Engine> {
    let fd = master.as_raw_fd().context("PTY has no descriptor")?;
    // SAFETY: `master` keeps the descriptor open for this call.
    let reader = rustix::io::fcntl_dupfd_cloexec(unsafe { BorrowedFd::borrow_raw(fd) }, 0)?;
    let writer = master.take_writer()?;
    spawn_pty_reader(id, reader, tx.clone());
    let mut engine = Engine::new(
        size,
        Arc::new(LuxConfig),
        "lux",
        env!("CARGO_PKG_VERSION"),
        Box::new(writer),
    );
    let clipboard: Arc<dyn Clipboard> = Arc::new(ClipboardRelay { tab: id, tx });
    engine.set_clipboard(&clipboard);
    Ok(engine)
}

fn spawn_pty_reader(id: TabId, pty: OwnedFd, tx: Sender<ServerEvent>) {
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match park::when_readable(&pty, park::WAIT, || rustix::io::read(&pty, &mut buf)) {
                Ok(None)
                | Err(rustix::io::Errno::INTR)
                | Ok(Some(Err(rustix::io::Errno::INTR))) => {}
                // EOF or EIO: child side of the PTY closed.
                Ok(Some(Ok(0) | Err(_))) | Err(_) => break,
                Ok(Some(Ok(n))) => {
                    if tx
                        .send(ServerEvent::PtyOutput(id, buf[..n].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let _ = tx.send(ServerEvent::PtyExited(id));
    });
}

/// Writes nowhere: the host's own engine answers the program.
fn mirror_engine(cols: u16, rows: u16) -> Engine {
    Engine::new(
        term_size(Rect::new(0, 0, cols, rows)),
        Arc::new(LuxConfig),
        "lux",
        env!("CARGO_PKG_VERSION"),
        Box::new(std::io::sink()),
    )
}

/// Fills the current screen from the bottom up, history included, with as
/// many of `lines` as it holds.
fn load_lines(engine: &mut Engine, lines: Vec<wezterm_term::Line>) {
    let rows = engine.screen().physical_rows.max(1);
    let seqno = engine.current_seqno();
    let screen = engine.screen_mut();
    let mut missing = lines.len().saturating_sub(screen.scrollback_rows());
    while missing > 0 {
        let step = missing.min(rows);
        screen.scroll_up(
            &(0..rows as i64),
            step,
            seqno,
            CellAttributes::default(),
            LuxConfig.bidi_mode(),
        );
        missing -= step;
    }
    let held = screen.scrollback_rows();
    let skip = lines.len().saturating_sub(held);
    let first = held - (lines.len() - skip);
    for (i, line) in lines.into_iter().skip(skip).enumerate() {
        *screen.line_mut(first + i) = line;
    }
}

/// Pushes `count` blank rows in at the bottom, moving the top rows into
/// history.
fn scroll_up(engine: &mut Engine, mut count: usize) {
    let rows = engine.screen().physical_rows;
    let seqno = engine.current_seqno();
    while count > 0 {
        let step = count.min(rows);
        engine.screen_mut().scroll_up(
            &(0..rows as i64),
            step,
            seqno,
            CellAttributes::default(),
            LuxConfig.bidi_mode(),
        );
        count -= step;
    }
}

fn sync_cursor(engine: &mut Engine, state: &ScreenState) {
    let visibility = if state.cursor_visible { 'h' } else { 'l' };
    engine.advance_bytes(format!(
        "\x1b[{};{}H\x1b[?25{visibility}",
        state.cursor_y + 1,
        state.cursor_x + 1
    ));
}

/// Session name, then the OSC title where the scope allows it, then the
/// command name.
fn automatic_name(
    kind: Option<AgentKind>,
    session: Option<String>,
    title: Option<String>,
    command: &str,
    osc_titles: OscTitles,
) -> String {
    let title = match osc_titles {
        OscTitles::All => title,
        OscTitles::Agents if kind.is_some() => title,
        _ => None,
    };
    session.or(title).unwrap_or_else(|| command.to_string())
}

/// The last output zone that a prompt or input zone follows. Without
/// shell integration every cell is output, so there are no boundaries.
fn last_command_output(engine: &mut Engine) -> LastOutput {
    let zones = engine.get_semantic_zones().unwrap_or_default();
    if zones
        .iter()
        .all(|zone| zone.semantic_type == SemanticType::Output)
    {
        return LastOutput::NoBoundaries;
    }
    let completed = zones.windows(2).rev().find_map(|pair| {
        (pair[0].semantic_type == SemanticType::Output
            && pair[1].semantic_type != SemanticType::Output)
            .then_some(pair[0])
    });
    let Some(zone) = completed else {
        return LastOutput::NoneCompleted;
    };
    let screen = engine.screen();
    let lines = screen.lines_in_phys_range(screen.stable_range(&(zone.start_y..zone.end_y + 1)));
    let last = lines.len().saturating_sub(1);
    let mut rows: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let from = if i == 0 { zone.start_x } else { 0 };
            // `end_x` is the last cell's index, not one past it.
            let to = if i == last {
                zone.end_x + 1
            } else {
                usize::MAX
            };
            line.visible_cells()
                .filter(|cell| (from..to).contains(&cell.cell_index()))
                .fold(String::new(), |mut text, cell| {
                    text.push_str(cell.str());
                    text
                })
                .trim_end()
                .to_string()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    LastOutput::Text(rows.join("\n"))
}

/// A rect can shrink to zero, but the PTY and engine need at least 1x1.
fn pty_size(rect: Rect) -> PtySize {
    PtySize {
        rows: rect.height.max(1),
        cols: rect.width.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn term_size(rect: Rect) -> TerminalSize {
    TerminalSize {
        rows: rect.height.max(1) as usize,
        cols: rect.width.max(1) as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(comm: &str, arg0: &str) -> Foreground {
        Foreground {
            comm: comm.into(),
            arg0: arg0.into(),
            exe: String::new(),
            wrapped: None,
        }
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn display_name_prefers_argv0_basename() {
        assert_eq!(fg("vim", "vim").display_name(), "vim");
        assert_eq!(fg("node", "claude").display_name(), "claude");
        assert_eq!(fg("bash", "").display_name(), "bash");
    }

    #[test]
    fn claude_is_identified_under_either_reading() {
        assert!(fg("claude", "node").is_claude());
        assert!(fg("node", "claude").is_claude());
        assert!(!fg("node", "node").is_claude());
    }

    #[test]
    fn a_wrapped_or_symlinked_binary_is_identified_by_its_real_path() {
        let mut wrapped = fg(".codex-wrapped", ".codex-wrapped");
        assert_eq!(wrapped.agent_kind(), None);
        wrapped.exe = "codex".into();
        assert_eq!(wrapped.agent_kind(), Some(AgentKind::Codex));
        let mut launcher = fg("node", "node");
        launcher.wrapped = Some("claude");
        assert_eq!(launcher.agent_kind(), Some(AgentKind::Claude));
        let mut kiro = fg("bash", "bash");
        kiro.wrapped = Some("kiro");
        assert_eq!(kiro.agent_kind(), Some(AgentKind::Kiro));
    }

    #[test]
    fn interpreter_script_arguments_name_the_agent() {
        let wrapped = |args: &[&str]| wrapped_agent(&argv(args));
        assert_eq!(
            wrapped(&[
                "node",
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
            ]),
            Some("claude")
        );
        assert_eq!(
            wrapped(&["/usr/bin/node", "--no-warnings", "/opt/codex/bin/codex.js"]),
            Some("codex")
        );
        assert_eq!(
            wrapped(&["bun", "-r", "./preload.js", "/home/u/.bun/bin/claude"]),
            Some("claude")
        );
        assert_eq!(wrapped(&["python3.12", "/opt/kiro-cli"]), Some("kiro"));
        assert_eq!(
            wrapped(&["sh", "/nix/store/abc/bin/claude"]),
            Some("claude")
        );
        assert_eq!(
            wrapped(&["bash", "-c", "exec claude --resume"]),
            Some("claude")
        );
        assert_eq!(
            wrapped(&["zsh", "-c", "'/opt/codex/bin/codex' \"$@\""]),
            Some("codex")
        );
        assert_eq!(wrapped(&["node", "--", "/x/kiro.js"]), Some("kiro"));
    }

    #[test]
    fn unrelated_arguments_name_no_agent() {
        let wrapped = |args: &[&str]| wrapped_agent(&argv(args));
        assert_eq!(wrapped(&["node", "/x/app/index.js"]), None);
        assert_eq!(wrapped(&["node", "-e", "claude"]), None);
        assert_eq!(wrapped(&["python3", "-m", "claude"]), None);
        assert_eq!(wrapped(&["bash", "-c", "ls claude"]), None);
        assert_eq!(wrapped(&["vim", "claude"]), None);
        assert_eq!(wrapped(&["node"]), None);
        assert_eq!(wrapped(&[]), None);
    }

    #[test]
    fn osc_title_scope_gates_which_tabs_use_it() {
        let title = || Some("kyle@host: ~/src".to_string());
        let name = |kind, scope| automatic_name(kind, None, title(), "bash", scope);
        assert_eq!(name(None, OscTitles::All), "kyle@host: ~/src");
        assert_eq!(name(None, OscTitles::Agents), "bash");
        assert_eq!(name(None, OscTitles::None), "bash");
        assert_eq!(
            name(Some(AgentKind::Codex), OscTitles::Agents),
            "kyle@host: ~/src"
        );
        assert_eq!(name(Some(AgentKind::Codex), OscTitles::None), "bash");
    }

    #[test]
    fn session_name_outranks_title_and_title_outranks_command() {
        let session = Some("refactor".to_string());
        let title = Some("✻ Thinking".to_string());
        let claude = Some(AgentKind::Claude);
        assert_eq!(
            automatic_name(
                claude,
                session.clone(),
                title.clone(),
                "claude",
                OscTitles::Agents
            ),
            "refactor"
        );
        assert_eq!(
            automatic_name(claude, session, None, "claude", OscTitles::None),
            "refactor"
        );
        assert_eq!(
            automatic_name(claude, None, title, "claude", OscTitles::Agents),
            "✻ Thinking"
        );
        assert_eq!(
            automatic_name(claude, None, None, "claude", OscTitles::All),
            "claude"
        );
    }

    #[test]
    fn osc52_copies_reach_the_relay_and_queries_go_unanswered() {
        use std::sync::Mutex;

        struct SharedWriter(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let replies = Arc::new(Mutex::new(Vec::new()));
        let mut engine = Engine::new(
            term_size(Rect::new(0, 0, 80, 24)),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(SharedWriter(replies.clone())),
        );
        let clipboard: Arc<dyn Clipboard> = Arc::new(ClipboardRelay { tab: 7, tx });
        engine.set_clipboard(&clipboard);

        // OSC 52 copy of base64 "hello".
        engine.advance_bytes(b"\x1b]52;c;aGVsbG8=\x07");
        match rx.try_recv() {
            Ok(ServerEvent::ProgramCopy(tab, text)) => {
                assert_eq!(tab, 7);
                assert_eq!(text, "hello");
            }
            _ => panic!("expected a ProgramCopy event"),
        }

        // OSC 52 query: no event and no reply.
        engine.advance_bytes(b"\x1b]52;c;?\x07");
        assert!(rx.try_recv().is_err());
        assert!(replies.lock().unwrap().is_empty());
    }

    #[test]
    fn plain_osc9_text_is_captured_and_progress_is_not() {
        struct NullWriter;
        impl std::io::Write for NullWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut engine = Engine::new(
            term_size(Rect::new(0, 0, 80, 24)),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(NullWriter),
        );
        let text = Arc::new(Mutex::new(None));
        let title = Arc::new(Mutex::new(None));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: text.clone(),
            title: title.clone(),
            bell: Arc::new(AtomicBool::new(false)),
        }));

        // OSC 9;4 is progress, not notification text.
        engine.advance_bytes(b"\x1b]9;4;1;40\x07");
        assert_eq!(*text.lock().unwrap(), None);

        engine.advance_bytes(b"\x1b]9;finished the refactor\x07");
        assert_eq!(
            text.lock().unwrap().as_deref(),
            Some("finished the refactor")
        );
        engine.advance_bytes(b"\x1b]9;ran the tests\x1b\\");
        assert_eq!(text.lock().unwrap().as_deref(), Some("ran the tests"));
    }

    #[test]
    fn osc_titles_land_in_the_title_slot() {
        struct NullWriter;
        impl std::io::Write for NullWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut engine = Engine::new(
            term_size(Rect::new(0, 0, 80, 24)),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(NullWriter),
        );
        let text = Arc::new(Mutex::new(None));
        let title = Arc::new(Mutex::new(None));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: text.clone(),
            title: title.clone(),
            bell: Arc::new(AtomicBool::new(false)),
        }));

        // OSC 2 sets the window title, OSC 0 sets icon and window title.
        engine.advance_bytes(b"\x1b]2;kyle@host: ~/src\x07");
        assert_eq!(title.lock().unwrap().as_deref(), Some("kyle@host: ~/src"));
        engine.advance_bytes(b"\x1b]0;notes.txt - vim\x1b\\");
        assert_eq!(title.lock().unwrap().as_deref(), Some("notes.txt - vim"));

        // OSC 1 is icon title only.
        engine.advance_bytes(b"\x1b]1;icon-only\x07");
        assert_eq!(title.lock().unwrap().as_deref(), Some("notes.txt - vim"));

        // Empty means cleared, not unset.
        engine.advance_bytes(b"\x1b]2;\x07");
        assert_eq!(title.lock().unwrap().as_deref(), Some(""));
    }

    fn bare_engine() -> Engine {
        struct NullWriter;
        impl std::io::Write for NullWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        Engine::new(
            term_size(Rect::new(0, 0, 40, 6)),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(NullWriter),
        )
    }

    #[test]
    fn the_bell_is_flagged_by_the_relay() {
        let mut engine = bare_engine();
        let bell = Arc::new(AtomicBool::new(false));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: Arc::new(Mutex::new(None)),
            title: Arc::new(Mutex::new(None)),
            bell: bell.clone(),
        }));
        engine.advance_bytes(b"hello");
        assert!(!bell.load(Ordering::Relaxed));
        engine.advance_bytes(b"\x07");
        assert!(bell.load(Ordering::Relaxed));
    }

    #[test]
    fn shimmer_speed_follows_output_and_stops_without_it() {
        let start = Instant::now();
        let mut output = RecentOutput::new(start);
        assert_eq!(
            output.advance(start + Duration::from_millis(500), false),
            0.0
        );
        // Full speed sweeps at the status text's pace: 8 KiB within the
        // window moves half a sweep per second.
        output.record(start + Duration::from_millis(500), 8192);
        let phase = output.advance(start + Duration::from_millis(1500), false);
        assert!((phase - 0.5).abs() < 0.01, "phase {phase}");
        // Half the rate moves half as far.
        let mut output = RecentOutput::new(start);
        output.record(start, 4096);
        let phase = output.advance(start + Duration::from_millis(1000), false);
        assert!((phase - 0.25).abs() < 0.01, "phase {phase}");
        // Once the write ages out the sweep stands still.
        let held = output.advance(start + Duration::from_millis(3000), false);
        assert_eq!(held, phase);
    }

    #[test]
    fn sustained_shimmer_sweeps_at_full_speed_without_output() {
        let start = Instant::now();
        let mut output = RecentOutput::new(start);
        let phase = output.advance(start + Duration::from_millis(1000), true);
        assert!((phase - 0.5).abs() < 0.01, "phase {phase}");
        // Output on top never pushes it past the status text's pace.
        output.record(start + Duration::from_millis(1000), 65536);
        let phase = output.advance(start + Duration::from_millis(1500), true);
        assert!((phase - 0.75).abs() < 0.01, "phase {phase}");
    }

    /// A local tab whose engine the test feeds directly.
    fn source_tab(cols: u16, rows: u16) -> Tab {
        let (tx, _) = std::sync::mpsc::channel();
        Tab::spawn_argv(Rect::new(0, 0, cols, rows), None, &["sleep", "30"], tx).unwrap()
    }

    fn all_text(engine: &Engine) -> Vec<String> {
        let screen = engine.screen();
        screen
            .lines_in_phys_range(0..screen.scrollback_rows())
            .iter()
            .map(|line| line.as_str().trim_end().to_string())
            .collect()
    }

    fn visible_text(engine: &Engine) -> Vec<String> {
        let screen = engine.screen();
        let rows = screen.physical_rows as i64;
        screen
            .lines_in_phys_range(screen.phys_range(&(0..rows)))
            .iter()
            .map(|line| line.as_str().trim_end().to_string())
            .collect()
    }

    /// A handoff as the next image would take it: its own descriptor on
    /// the same PTY.
    fn handed_off(tab: &mut Tab) -> TabState {
        let mut state = tab.handoff().unwrap();
        // SAFETY: `tab` keeps the descriptor open for this call.
        let pty = unsafe { BorrowedFd::borrow_raw(state.pty) };
        state.pty = std::os::fd::IntoRawFd::into_raw_fd(rustix::io::dup(pty).unwrap());
        state
    }

    #[test]
    fn a_resumed_tab_keeps_both_screens_its_modes_and_cursors() {
        let mut tab = source_tab(20, 5);
        tab.advance(b"one\r\ntwo\r\n\x1b[?2004h\x1b[?1h\x1b[?1049h\x1b[2;3Hfull screen");
        let (tx, _) = std::sync::mpsc::channel();
        let mut resumed = Tab::resume(handed_off(&mut tab), tx).unwrap();

        assert!(resumed.engine.is_alt_screen_active());
        assert!(resumed.engine.bracketed_paste_enabled());
        assert_eq!(visible_text(&resumed.engine)[1], "  full screen");
        assert_eq!(
            (resumed.engine.cursor_pos().x, resumed.engine.cursor_pos().y),
            (13, 1)
        );
        resumed.advance(b"\x1b[?1049l");
        assert_eq!(&visible_text(&resumed.engine)[..3], ["one", "two", ""]);
        assert_eq!(resumed.engine.cursor_pos().y, 2);

        // The handoff leaves the source as it was, for an exec that fails.
        assert!(tab.engine.is_alt_screen_active());
        assert_eq!(visible_text(&tab.engine)[1], "  full screen");
        tab.kill();
    }

    #[test]
    fn a_resumed_tab_keeps_its_history_and_names() {
        let mut tab = source_tab(20, 3);
        for i in 0..10 {
            tab.advance(format!("line {i}\r\n").as_bytes());
        }
        tab.set_name("work".into());
        let (tx, _) = std::sync::mpsc::channel();
        let resumed = Tab::resume(handed_off(&mut tab), tx).unwrap();
        assert_eq!(all_text(&resumed.engine), all_text(&tab.engine));
        assert_eq!(resumed.name, "work");
        assert!(resumed.is_manually_named());
        tab.kill();
    }

    fn assert_mirrors(mirror: &Tab, source: &Tab) {
        assert_eq!(all_text(&mirror.engine), all_text(&source.engine));
        let (m, s) = (mirror.engine.cursor_pos(), source.engine.cursor_pos());
        assert_eq!((m.x, m.y, m.visibility), (s.x, s.y, s.visibility));
        assert_eq!(
            mirror.engine.is_alt_screen_active(),
            source.engine.is_alt_screen_active()
        );
    }

    fn sync(source: &mut Tab, synced: &mut Synced, mirror: &mut Tab) {
        let now = Instant::now();
        if let Some(update) = source.wire_update(synced, now) {
            mirror.apply_remote(update, now);
        }
    }

    #[test]
    fn a_mirror_matches_its_source_through_scrolling_output() {
        let now = Instant::now();
        let mut source = source_tab(40, 6);
        for i in 0..50 {
            source.engine.advance_bytes(format!("line {i}\r\n"));
        }
        let (snap, mut synced) = source.wire_snapshot(now);
        let mut mirror = Tab::from_remote(HostLink::default(), snap, now);
        assert_mirrors(&mirror, &source);

        for i in 50..120 {
            source
                .engine
                .advance_bytes(format!("\x1b[1mmore\x1b[0m {i}\r\n"));
        }
        source.engine.advance_bytes(b"partial");
        sync(&mut source, &mut synced, &mut mirror);
        assert_mirrors(&mirror, &source);

        // A rewrite in place, with nothing scrolling.
        source
            .engine
            .advance_bytes(b"\r\x1b[2Kreplaced\x1b[3;1Hmiddle");
        sync(&mut source, &mut synced, &mut mirror);
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_mirror_matches_its_source_past_the_scrollback_limit() {
        let now = Instant::now();
        let mut source = source_tab(20, 5);
        let (snap, mut synced) = source.wire_snapshot(now);
        let mut mirror = Tab::from_remote(HostLink::default(), snap, now);
        for batch in 0..10 {
            for i in 0..800 {
                source.engine.advance_bytes(format!("{batch}-{i}\r\n"));
            }
            sync(&mut source, &mut synced, &mut mirror);
        }
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_mirror_follows_the_alternate_screen_and_back() {
        let now = Instant::now();
        let mut source = source_tab(30, 5);
        for i in 0..20 {
            source.engine.advance_bytes(format!("shell {i}\r\n"));
        }
        let (snap, mut synced) = source.wire_snapshot(now);
        let mut mirror = Tab::from_remote(HostLink::default(), snap, now);

        source
            .engine
            .advance_bytes(b"\x1b[?1049h\x1b[H\x1b[2Jeditor\x1b[?25l");
        sync(&mut source, &mut synced, &mut mirror);
        assert_mirrors(&mirror, &source);

        source.engine.advance_bytes(b"\x1b[?1049l\x1b[?25hback\r\n");
        sync(&mut source, &mut synced, &mut mirror);
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_snapshot_taken_on_the_alternate_screen_asks_for_the_primary_later() {
        let now = Instant::now();
        let mut source = source_tab(30, 5);
        source
            .engine
            .advance_bytes(b"history\r\n\x1b[?1049hfull screen");
        let (snap, mut synced) = source.wire_snapshot(now);
        let (tx, rx) = std::sync::mpsc::channel();
        let mut mirror = Tab::from_remote(HostLink::connected_to(tx), snap, now);
        assert_mirrors(&mirror, &source);

        source.engine.advance_bytes(b"\x1b[?1049l");
        sync(&mut source, &mut synced, &mut mirror);
        assert!(matches!(rx.try_recv(), Ok(HubMsg::Resync(id)) if id == source.id));
        let (snap, _) = source.wire_snapshot(now);
        mirror.load_remote(snap, now);
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_mirror_drops_scrollback_the_source_erased() {
        let now = Instant::now();
        let mut source = source_tab(30, 5);
        for i in 0..40 {
            source.engine.advance_bytes(format!("old {i}\r\n"));
        }
        let (snap, mut synced) = source.wire_snapshot(now);
        let mut mirror = Tab::from_remote(HostLink::default(), snap, now);
        source.engine.advance_bytes(b"\x1b[H\x1b[2J\x1b[3Jclean");
        sync(&mut source, &mut synced, &mut mirror);
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_mirror_follows_its_source_through_a_resize() {
        let now = Instant::now();
        let mut source = source_tab(40, 6);
        for i in 0..30 {
            source
                .engine
                .advance_bytes(format!("a fairly long line number {i} that wraps\r\n"));
        }
        let (snap, mut synced) = source.wire_snapshot(now);
        let mut mirror = Tab::from_remote(HostLink::default(), snap, now);
        source.resize(Rect::new(0, 0, 25, 8));
        sync(&mut source, &mut synced, &mut mirror);
        assert_eq!(visible_text(&mirror.engine), visible_text(&source.engine));
        assert_mirrors(&mirror, &source);
        source.kill();
    }

    #[test]
    fn a_remote_tab_sends_input_to_its_host_instead_of_its_mirror() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut tab =
            Tab::remote_placeholder(HostLink::connected_to(tx), 9, Rect::new(0, 0, 10, 3));
        tab.key_down(KeyCode::Char('x'), Modifiers::NONE);
        tab.send_paste("hi");
        tab.resize(Rect::new(0, 0, 12, 4));
        tab.resize(Rect::new(0, 0, 12, 4));
        assert!(matches!(
            rx.try_recv(),
            Ok(HubMsg::Key {
                tab: TabRef::Token(9),
                code: KeyCode::Char('x'),
                ..
            })
        ));
        assert!(matches!(rx.try_recv(), Ok(HubMsg::Paste { text, .. }) if text == "hi"));
        assert!(matches!(
            rx.try_recv(),
            Ok(HubMsg::Resize {
                cols: 12,
                rows: 4,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
        // The mirror resizes only once the host has.
        assert_eq!(tab.engine.get_size().cols, 10);
    }

    #[test]
    fn a_disconnected_link_drops_input() {
        let mut tab = Tab::remote_placeholder(HostLink::default(), 1, Rect::new(0, 0, 10, 3));
        tab.key_down(KeyCode::Char('x'), Modifiers::NONE);
        assert!(
            tab.engine.screen().lines_in_phys_range(0..1)[0]
                .as_str()
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn last_output_comes_from_the_shells_command_boundaries() {
        let mut engine = bare_engine();
        engine.advance_bytes(b"plain output\r\nmore\r\n");
        assert_eq!(last_command_output(&mut engine), LastOutput::NoBoundaries);

        let mut engine = bare_engine();
        engine.advance_bytes(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(last_command_output(&mut engine), LastOutput::NoneCompleted);

        engine.advance_bytes(b"ls\r\n\x1b]133;C\x07a.txt\r\nb.txt  \r\n\x1b]133;D;0\x07");
        engine.advance_bytes(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(
            last_command_output(&mut engine),
            LastOutput::Text("a.txt\nb.txt".into())
        );

        // A command still running leaves the previous one as the last
        // completed.
        engine.advance_bytes(b"sleep\r\n\x1b]133;C\x07partial");
        assert_eq!(
            last_command_output(&mut engine),
            LastOutput::Text("a.txt\nb.txt".into())
        );
    }
}
