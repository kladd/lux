//! Windows and tabs. A window is a leaf of the layout tree owning an
//! ordered list of tabs; a tab is one PTY running $SHELL plus
//! one terminal engine instance, with a reader thread feeding PTY output
//! into the app's event channel.

use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Alert, AlertHandler, Clipboard, ClipboardSelection, Terminal as Engine, TerminalConfiguration,
    TerminalSize,
};

use crate::server::ServerEvent;
use crate::server::agent::{self, AgentState, Tracker};
use crate::server::layout::WindowId;

pub type TabId = usize;

/// Tab ids are unique across all sessions on the server, so PTY reader
/// threads can tag events without knowing which session owns them.
static NEXT_TAB_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Debug)]
struct LuxConfig;

impl TerminalConfiguration for LuxConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Routes a tab program's OSC 52 clipboard writes out of the engine to
/// the server loop; without a hook the engine drops them silently.
/// Clipboard queries never reach this hook — the engine discards them,
/// so no program can read the clipboard.
struct ClipboardRelay {
    tab: TabId,
    tx: Sender<ServerEvent>,
}

impl Clipboard for ClipboardRelay {
    fn set_contents(&self, _: ClipboardSelection, data: Option<String>) -> anyhow::Result<()> {
        // `None` clears the clipboard; there's nothing to relay.
        if let Some(text) = data {
            let _ = self.tx.send(ServerEvent::ProgramCopy(self.tab, text));
        }
        Ok(())
    }
}

/// Captures a tab program's plain OSC 9 system-notification text (the
/// engine's toast alert) into a slot the tab reads when it next raises a
/// desktop notification. The OSC 9;4 progress sequence takes a different
/// engine path and never lands here.
struct NotificationRelay {
    text: Arc<Mutex<Option<String>>>,
}

impl AlertHandler for NotificationRelay {
    fn alert(&mut self, alert: Alert) {
        if let Alert::ToastNotification { body, .. } = alert {
            *self.text.lock().unwrap() = Some(body);
        }
    }
}

/// A tab's transition into done or blocked, surfaced upward so the server
/// can raise a desktop notification: which tab by display name, which of
/// the two states it reached, and the task summary its program offered,
/// if any.
pub struct Notice {
    pub tab: String,
    pub blocked: bool,
    pub summary: Option<String>,
}

/// A leaf of the layout tree: one rectangle of screen space owning an
/// ordered tab list with exactly one active tab.
pub struct Window {
    pub id: WindowId,
    /// Last rectangle the window was laid out into (tab bar + content).
    pub rect: Rect,
    pub tabs: Vec<Tab>,
    /// Index of the active tab; the only one rendered.
    pub active: usize,
}

impl Window {
    /// Create a window whose tab list holds exactly one active tab,
    /// with the tab's shell and engine sized to the content
    /// rectangle below the tab bar row.
    pub fn new(id: WindowId, rect: Rect, tx: Sender<ServerEvent>) -> anyhow::Result<Self> {
        let tab = Tab::spawn(content_rect(rect), None, tx)?;
        Ok(Self {
            id,
            rect,
            tabs: vec![tab],
            active: 0,
        })
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

    /// The rectangle the active tab's content renders into.
    pub fn content_rect(&self) -> Rect {
        content_rect(self.rect)
    }

    /// The tab bar row reserved at the top of the window.
    pub fn tab_bar_rect(&self) -> Rect {
        Rect {
            height: self.rect.height.min(1),
            ..self.rect
        }
    }

    /// Rebuild a window from its persisted snapshot: a fresh shell per
    /// tab in its saved working directory, or a resumed Claude Code
    /// session where one was saved. Tabs that fail to spawn are dropped;
    /// a window left with no tabs is `None`.
    pub fn restore(
        rect: Rect,
        snap: &crate::server::persist::WindowSnapshot,
        tx: &Sender<ServerEvent>,
    ) -> Option<Self> {
        let content = content_rect(rect);
        let mut tabs = Vec::new();
        for tab in &snap.tabs {
            // A saved directory that no longer exists falls back to the
            // server's own, rather than losing the tab.
            let cwd = tab.cwd.is_dir().then(|| tab.cwd.clone());
            let spawned = match &tab.claude_session {
                // A failed resume spawn still gets its shell back.
                Some(session) => {
                    Tab::spawn_claude_resume(content, cwd.clone(), session, tx.clone())
                        .or_else(|_| Tab::spawn(content, cwd, tx.clone()))
                }
                None => Tab::spawn(content, cwd, tx.clone()),
            };
            if let Ok(tab) = spawned {
                tabs.push(tab);
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

    /// Bring every tab's PTY and engine in sync with the window's current
    /// rectangle (across all of this window's tabs, so
    /// no tab is stale when it later becomes active).
    pub fn reconcile(&mut self) {
        let content = self.content_rect();
        for tab in &mut self.tabs {
            if tab.rect != content {
                tab.resize(content);
            }
        }
    }
}

fn content_rect(rect: Rect) -> Rect {
    // One chrome row: the tab bar, which also serves as
    // the boundary with a stacked window above.
    Rect {
        y: rect.y + rect.height.min(1),
        height: rect.height.saturating_sub(1),
        ..rect
    }
}

/// A tab's foreground process command name, from both /proc sources:
/// `comm` is the kernel's command name; argv[0]'s basename covers the
/// same name when comm is truncated or wrapped.
struct Foreground {
    comm: String,
    arg0: String,
}

impl Foreground {
    /// The foreground command name must be `claude`,
    /// under either reading.
    fn is_claude(&self) -> bool {
        self.comm == "claude" || self.arg0 == "claude"
    }

    /// The foreground command name must be `codex`,
    /// under either reading.
    fn is_codex(&self) -> bool {
        self.comm == "codex" || self.arg0 == "codex"
    }

    /// The tab's display name: argv[0]'s basename, with
    /// comm covering processes that rewrite their argv.
    fn display_name(&self) -> &str {
        if self.arg0.is_empty() {
            &self.comm
        } else {
            &self.arg0
        }
    }
}

pub struct Tab {
    pub id: TabId,
    /// Display name derived from the PTY's foreground process command
    /// name, re-derived as that process changes — until a manual rename
    /// pins it.
    pub name: String,
    /// Whether the name was set by the rename prompt; a pinned name no
    /// longer tracks the foreground process.
    manual_name: bool,
    pub engine: Engine,
    /// Last rectangle the tab's PTY and engine were sized to.
    pub rect: Rect,
    /// Engine seqno at the last draw, to skip redraws with no changes.
    pub drawn_seqno: usize,
    /// Scroll mode: the stable row index of the view's
    /// top line. `None` means following live output.
    /// Stable indices survive scrollback growth and trimming, so the view
    /// stays anchored to content while output arrives.
    scroll_top: Option<isize>,
    /// Present while the tab is identified as running Claude Code.
    pub agent: Option<Tracker>,
    /// Present while the tab is identified as running Codex. The state
    /// is classified but deliberately surfaced nowhere — no status
    /// text, overview entry, desktop notification, or persistence.
    pub codex: Option<Tracker>,
    /// The Claude Code session id this tab's claude instance owns:
    /// seeded when the tab was spawned as a resume (Claude Code keeps
    /// the id across resumes), or assigned at save time by matching
    /// transcript creation times. Cleared when claude exits.
    pub claude_session: Option<String>,
    /// When this tab was first seen running Claude Code; the matching
    /// anchor for transcripts. Cleared when claude exits.
    pub claude_since: Option<std::time::SystemTime>,
    /// The latest OSC 9 system-notification text the tab's program wrote,
    /// shared with the engine's alert handler. Taken (once) when a desktop
    /// notification is raised, so a later notification without a fresh
    /// summary says nothing rather than repeating a stale one.
    notify_text: Arc<Mutex<Option<String>>>,
    master: Box<dyn MasterPty>,
    child: Box<dyn Child + Send + Sync>,
}

impl Tab {
    /// Spawn a PTY running $SHELL sized to `rect` with an engine to match
    /// and a reader thread feeding `tx`.
    /// `cwd` sets the shell's working directory; `None`
    /// leaves the server's own.
    pub fn spawn(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        Self::spawn_argv(rect, cwd, &[&shell], tx)
    }

    /// Spawn `claude --resume <session>` in place of the shell, picking a
    /// persisted Claude Code session back up.
    pub fn spawn_claude_resume(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        session: &str,
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let mut tab = Self::spawn_argv(rect, cwd, &["claude", "--resume", session], tx)?;
        // The resumed instance keeps this id, so the tab's owner is known
        // without transcript matching.
        tab.claude_session = Some(session.to_string());
        Ok(tab)
    }

    fn spawn_argv(
        rect: Rect,
        cwd: Option<std::path::PathBuf>,
        argv: &[&str],
        tx: Sender<ServerEvent>,
    ) -> anyhow::Result<Self> {
        let id = NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pty = native_pty_system();
        let pair = pty.openpty(pty_size(rect)).context("open PTY")?;
        let mut cmd = CommandBuilder::from_argv(argv.iter().map(|arg| (*arg).into()).collect());
        // The engine speaks xterm's protocol regardless of the host terminal.
        cmd.env("TERM", "xterm-256color");
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let relay_tx = tx.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    // EOF or EIO: child side of the PTY closed.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
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

        // The engine writes encoded key input back through `writer` into
        // the PTY.
        let mut engine = Engine::new(
            term_size(rect),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(writer),
        );
        let clipboard: Arc<dyn Clipboard> = Arc::new(ClipboardRelay {
            tab: id,
            tx: relay_tx,
        });
        engine.set_clipboard(&clipboard);
        let notify_text = Arc::new(Mutex::new(None));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: notify_text.clone(),
        }));

        // Until the first foreground read, the name is the spawned
        // command's.
        let name = std::path::Path::new(argv[0])
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| argv[0].to_string());

        Ok(Self {
            id,
            name,
            manual_name: false,
            engine,
            rect,
            drawn_seqno: 0,
            scroll_top: None,
            agent: None,
            codex: None,
            claude_session: None,
            claude_since: None,
            notify_text,
            master: pair.master,
            child,
        })
    }

    /// Terminate the tab's child process; the tab's
    /// removal follows the ordinary exit path once its PTY closes.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Manually set the tab's display name, pinning it against automatic
    /// renaming.
    pub fn set_name(&mut self, name: String) {
        self.name = name;
        self.manual_name = true;
    }

    /// Re-identify the tab after new PTY output: re-derive its display
    /// name from the foreground command and re-evaluate agent detection.
    /// Returns whether the displayed name or agent
    /// state (including the status text appearing or disappearing)
    /// changed, plus a notice when the agent reached done or blocked.
    pub fn refresh_identity(&mut self) -> (bool, Option<Notice>) {
        let fg = self.foreground();
        let renamed = if self.manual_name {
            // A manually renamed tab keeps its name.
            false
        } else {
            match fg.as_ref().map(Foreground::display_name) {
                Some(name) if !name.is_empty() && name != self.name => {
                    self.name = name.to_string();
                    true
                }
                // No readable foreground process (e.g. mid-exec): keep the
                // current name rather than flickering through a fallback.
                _ => false,
            }
        };
        if fg.as_ref().is_some_and(|fg| fg.is_codex()) {
            // Codex: classify the tab's state, but surface nothing —
            // status text, overview, notifications, and persistence
            // stay Claude Code-only.
            let snapshot = agent::Snapshot::capture(&self.engine);
            let raw = agent::evaluate(agent::AgentKind::Codex, &snapshot);
            self.codex
                .get_or_insert_default()
                .observe(raw, std::time::Instant::now());
            self.claude_session = None;
            self.claude_since = None;
            return (self.agent.take().is_some() || renamed, None);
        }
        if !fg.as_ref().is_some_and(|fg| fg.is_claude()) {
            // Not Claude Code — no status text, drop
            // any stale state. Session identity is cleared only on a
            // definite non-claude sighting (a later claude in this tab is
            // a different session); an unreadable foreground (e.g.
            // mid-exec) is transient and must not wipe it.
            if fg.is_some() {
                self.claude_session = None;
                self.claude_since = None;
            }
            self.codex = None;
            return (self.agent.take().is_some() || renamed, None);
        }
        self.codex = None;
        let snapshot = agent::Snapshot::capture(&self.engine);
        let raw = agent::evaluate(agent::AgentKind::Claude, &snapshot);
        let appeared = self.agent.is_none();
        // First sighting of this claude instance; kept across tracker
        // flicker so the transcript-matching anchor doesn't drift.
        if self.claude_since.is_none() {
            self.claude_since = Some(std::time::SystemTime::now());
        }
        let tracker = self.agent.get_or_insert_default();
        let entered = tracker.observe(raw, std::time::Instant::now());
        let notice = self.notice_for(entered);
        (appeared || entered.is_some() || renamed, notice)
    }

    /// The notice a just-entered displayed state warrants: a commit into
    /// idle always lands as done, blocked is blocked, and working is
    /// nobody's business.
    fn notice_for(&mut self, entered: Option<AgentState>) -> Option<Notice> {
        let blocked = match entered? {
            AgentState::Idle => false,
            AgentState::Blocked => true,
            AgentState::Working => return None,
        };
        Some(Notice {
            tab: self.name.clone(),
            blocked,
            summary: self.notify_text.lock().unwrap().take(),
        })
    }

    /// The PTY foreground process group leader's identity, from /proc.
    fn foreground(&self) -> Option<Foreground> {
        let pid = self.master.process_group_leader()?;
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let arg0 = cmdline.split(|b| *b == 0).next().unwrap_or(b"");
        let arg0 = std::path::Path::new(&String::from_utf8_lossy(arg0).into_owned())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(Foreground {
            comm: comm.trim().to_string(),
            arg0,
        })
    }

    /// The foreground process's working directory, via the
    /// same /proc inspection `foreground` uses.
    pub fn working_dir(&self) -> Option<std::path::PathBuf> {
        let pid = self.master.process_group_leader()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// Commit a pending idle debounce whose window has elapsed with no
    /// further output; returns whether display changed, plus a notice
    /// when the commit landed the agent in done.
    pub fn tick_agent(&mut self, now: std::time::Instant) -> (bool, Option<Notice>) {
        // A Codex commit changes nothing visible, so it neither redraws
        // nor notifies.
        if let Some(codex) = self.codex.as_mut() {
            codex.tick(now);
        }
        let entered = self.agent.as_mut().and_then(|t| t.tick(now));
        let notice = self.notice_for(entered);
        (entered.is_some(), notice)
    }

    pub fn agent_pending_idle(&self) -> bool {
        self.agent.as_ref().is_some_and(|t| t.pending())
            || self.codex.as_ref().is_some_and(|t| t.pending())
    }

    pub fn scroll_mode(&self) -> bool {
        self.scroll_top.is_some()
    }

    /// Enter scroll mode anchored at the current live view.
    pub fn enter_scroll_mode(&mut self) {
        if self.scroll_top.is_none() {
            self.scroll_top = Some(self.engine.screen().visible_row_to_stable_row(0));
        }
    }

    /// Exit scroll mode and resume following live output.
    pub fn exit_scroll_mode(&mut self) {
        self.scroll_top = None;
    }

    /// Scroll the view by `delta` lines (negative = up into history),
    /// clamped to the oldest scrollback line and the live view.
    /// Returns true if the view is at the live
    /// bottom afterwards.
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

    /// The physical rows the tab's view shows: the scroll-mode anchor
    /// or the live tail.
    pub fn view_range(&self) -> std::ops::Range<usize> {
        let screen = self.engine.screen();
        let rows = screen.physical_rows as isize;
        match self.scroll_top {
            Some(top) => screen.stable_range(&(top..top + rows)),
            None => screen.phys_range(&(0..rows as i64)),
        }
    }

    /// Resize the PTY and engine to a new content rectangle.
    pub fn resize(&mut self, rect: Rect) {
        self.rect = rect;
        let _ = self.master.resize(pty_size(rect));
        self.engine.resize(term_size(rect));
    }

    /// Reap the exited child and return its exit status.
    pub fn wait(&mut self) -> i32 {
        match self.child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 0,
        }
    }
}

/// A zero-sized rect can occur transiently during extreme shrink; the PTY
/// and engine still need sane minimum dimensions.
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
        }
    }

    #[test]
    fn display_name_prefers_argv0_basename() {
        // argv[0] first, comm when argv is rewritten/unreadable.
        assert_eq!(fg("vim", "vim").display_name(), "vim");
        assert_eq!(fg("node", "claude").display_name(), "claude");
        assert_eq!(fg("bash", "").display_name(), "bash");
    }

    #[test]
    fn claude_is_identified_under_either_reading() {
        // Comm or argv[0] basename.
        assert!(fg("claude", "node").is_claude());
        assert!(fg("node", "claude").is_claude());
        assert!(!fg("node", "node").is_claude());
    }

    #[test]
    fn osc52_copies_reach_the_relay_and_queries_go_unanswered() {
        use std::sync::Mutex;

        // Captures what the engine writes back toward the program.
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

        // A program's copy: OSC 52 with base64 content ("hello").
        engine.advance_bytes(b"\x1b]52;c;aGVsbG8=\x07");
        match rx.try_recv() {
            Ok(ServerEvent::ProgramCopy(tab, text)) => {
                assert_eq!(tab, 7);
                assert_eq!(text, "hello");
            }
            _ => panic!("expected a ProgramCopy event"),
        }

        // A clipboard query is discarded: no event, and no reply handing
        // the program the clipboard's contents.
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
        engine.set_notification_handler(Box::new(NotificationRelay { text: text.clone() }));

        // The OSC 9;4 progress sequence is not notification text.
        engine.advance_bytes(b"\x1b]9;4;1;40\x07");
        assert_eq!(*text.lock().unwrap(), None);

        // A plain OSC 9 lands in the slot; a later one replaces it.
        engine.advance_bytes(b"\x1b]9;finished the refactor\x07");
        assert_eq!(
            text.lock().unwrap().as_deref(),
            Some("finished the refactor")
        );
        engine.advance_bytes(b"\x1b]9;ran the tests\x1b\\");
        assert_eq!(text.lock().unwrap().as_deref(), Some("ran the tests"));
    }
}
