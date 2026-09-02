//! Windows and the tabs they own: each tab is one PTY plus a terminal engine.

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

/// Server-global, so PTY reader threads can tag events without knowing
/// their session.
static NEXT_TAB_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Debug)]
struct LuxConfig;

impl TerminalConfiguration for LuxConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
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

/// Captures a program's OSC 9 notification text and OSC 0/2 window title
/// for the tab to read later. OSC 9;4 progress and OSC 1 icon titles never
/// land here.
struct NotificationRelay {
    text: Arc<Mutex<Option<String>>>,
    title: Arc<Mutex<Option<String>>>,
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
            _ => {}
        }
    }
}

/// A tab's agent reaching done or blocked, for the server to raise a
/// desktop notification.
pub struct Notice {
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

fn content_rect(rect: Rect) -> Rect {
    // The one chrome row is the tab bar, which also divides stacked windows.
    Rect {
        y: rect.y + rect.height.min(1),
        height: rect.height.saturating_sub(1),
        ..rect
    }
}

/// The foreground command name from both /proc sources, since `comm` may be
/// truncated or wrapped.
struct Foreground {
    comm: String,
    arg0: String,
}

impl Foreground {
    fn is_claude(&self) -> bool {
        self.comm == "claude" || self.arg0 == "claude"
    }

    fn is_codex(&self) -> bool {
        self.comm == "codex" || self.arg0 == "codex"
    }

    fn is_kiro(&self) -> bool {
        ["kiro", "kiro-cli"].contains(&self.comm.as_str())
            || ["kiro", "kiro-cli"].contains(&self.arg0.as_str())
    }

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
    pub name: String,
    manual_name: bool,
    pub engine: Engine,
    pub rect: Rect,
    pub drawn_seqno: usize,
    /// Top line of the view in scroll mode, as a stable row index so it
    /// survives scrollback trimming. `None` follows live output.
    scroll_top: Option<isize>,
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
    master: Box<dyn MasterPty>,
    child: Box<dyn Child + Send + Sync>,
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
        // Programs talk to the engine, not the host terminal, so TERM is fixed.
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
        let osc_title = Arc::new(Mutex::new(None));
        engine.set_notification_handler(Box::new(NotificationRelay {
            text: notify_text.clone(),
            title: osc_title.clone(),
        }));

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
            claude_session: None,
            running_claude: false,
            notify_text,
            osc_title,
            master: pair.master,
            child,
        })
    }

    /// Removal follows once the PTY closes, like any other exit.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
        self.manual_name = true;
    }

    pub fn clear_name(&mut self) {
        self.manual_name = false;
        // Don't wait for the next PTY output to trigger refresh_identity.
        if let Some(name) = self.claude_session_name().or_else(|| self.osc_title()) {
            self.name = name;
        }
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
    pub fn refresh_identity(&mut self) -> (bool, Option<Notice>) {
        let fg = self.foreground();
        let renamed = if self.manual_name {
            false
        } else {
            let new_name = match fg.as_ref() {
                // Claude Code churns the OSC title as a status channel, so
                // the session name outranks it.
                Some(fg) if fg.is_claude() => self
                    .claude_session_name()
                    .or_else(|| self.osc_title())
                    .unwrap_or_else(|| fg.display_name().to_string()),
                Some(fg) => self
                    .osc_title()
                    .unwrap_or_else(|| fg.display_name().to_string()),
                // Unreadable foreground (mid-exec): keep the current name
                // rather than flicker.
                None => String::new(),
            };
            if !new_name.is_empty() && new_name != self.name {
                self.name = new_name;
                true
            } else {
                false
            }
        };
        let kind = match fg.as_ref() {
            Some(fg) if fg.is_claude() => Some(agent::AgentKind::Claude),
            Some(fg) if fg.is_codex() => Some(agent::AgentKind::Codex),
            Some(fg) if fg.is_kiro() => Some(agent::AgentKind::Kiro),
            _ => None,
        };
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
            AgentState::Working => return None,
        };
        Some(Notice {
            tab: self.name.clone(),
            blocked,
            summary: self.notify_text.lock().unwrap().take(),
        })
    }

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

    /// The session file Claude Code keeps for its own pid.
    fn claude_session_file(&self) -> Option<serde_json::Value> {
        let pid = self.master.process_group_leader()?;
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
        let pid = self.master.process_group_leader()?;
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

    pub fn resize(&mut self, rect: Rect) {
        self.rect = rect;
        let _ = self.master.resize(pty_size(rect));
        self.engine.resize(term_size(rect));
    }

    pub fn wait(&mut self) -> i32 {
        match self.child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 0,
        }
    }
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
        }
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
}
