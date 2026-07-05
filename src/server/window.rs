//! Windows and tabs. A window is a leaf of the layout tree owning an
//! ordered list of tabs (REQ-TAB-001); a tab is one PTY running $SHELL plus
//! one terminal engine instance, with a reader thread feeding PTY output
//! into the app's event channel.

use std::io::Read;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;

use anyhow::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::layout::Rect;
use wezterm_term::color::ColorPalette;
use wezterm_term::{Terminal as Engine, TerminalConfiguration, TerminalSize};

use crate::server::ServerEvent;
use crate::server::agent::{self, Tracker};
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

/// A leaf of the layout tree: one rectangle of screen space owning an
/// ordered tab list with exactly one active tab (REQ-TAB-001/002).
pub struct Window {
    pub id: WindowId,
    /// Last rectangle the window was laid out into (tab bar + content).
    pub rect: Rect,
    pub tabs: Vec<Tab>,
    /// Index of the active tab; the only one rendered (REQ-TAB-002).
    pub active: usize,
}

impl Window {
    /// Create a window whose tab list holds exactly one active tab
    /// (REQ-TAB-003), with the tab's shell and engine sized to the content
    /// rectangle below the tab bar row (REQ-WINDOW-009/010, REQ-TAB-005).
    pub fn new(id: WindowId, rect: Rect, tx: Sender<ServerEvent>) -> anyhow::Result<Self> {
        let tab = Tab::spawn(content_rect(rect), tx)?;
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

    /// The rectangle the active tab's content renders into (REQ-TAB-012).
    pub fn content_rect(&self) -> Rect {
        content_rect(self.rect)
    }

    /// The tab bar row reserved at the top of the window (REQ-TAB-011).
    pub fn tab_bar_rect(&self) -> Rect {
        Rect {
            height: self.rect.height.min(1),
            ..self.rect
        }
    }

    /// Bring every tab's PTY and engine in sync with the window's current
    /// rectangle (REQ-WINDOW-018/019 across all of this window's tabs, so
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
    Rect {
        y: rect.y + rect.height.min(1),
        height: rect.height.saturating_sub(1),
        ..rect
    }
}

pub struct Tab {
    pub id: TabId,
    pub engine: Engine,
    /// Last rectangle the tab's PTY and engine were sized to.
    pub rect: Rect,
    /// Engine seqno at the last draw, to skip redraws with no changes.
    pub drawn_seqno: usize,
    /// Scroll mode (REQ-SCROLL-003): the stable row index of the view's
    /// top line. `None` means following live output (REQ-SCROLL-012).
    /// Stable indices survive scrollback growth and trimming, so the view
    /// stays anchored to content while output arrives (REQ-SCROLL-010).
    scroll_top: Option<isize>,
    /// Present while the tab is identified as running Claude Code
    /// (REQ-AGENT-002/016).
    pub agent: Option<Tracker>,
    master: Box<dyn MasterPty>,
    child: Box<dyn Child + Send + Sync>,
}

impl Tab {
    /// Spawn a PTY running $SHELL sized to `rect` with an engine to match
    /// (REQ-TAB-004/005) and a reader thread feeding `tx` (REQ-PANE-005).
    pub fn spawn(rect: Rect, tx: Sender<ServerEvent>) -> anyhow::Result<Self> {
        let id = NEXT_TAB_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pty = native_pty_system();
        let pair = pty.openpty(pty_size(rect)).context("open PTY")?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut cmd = CommandBuilder::new(&shell);
        // The engine speaks xterm's protocol regardless of the host terminal.
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    // EOF or EIO: child side of the PTY closed.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(ServerEvent::PtyOutput(id, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(ServerEvent::PtyExited(id));
        });

        // The engine writes encoded key input back through `writer` into
        // the PTY (REQ-PANE-011).
        let engine = Engine::new(
            term_size(rect),
            Arc::new(LuxConfig),
            "lux",
            env!("CARGO_PKG_VERSION"),
            Box::new(writer),
        );

        Ok(Self {
            id,
            engine,
            rect,
            drawn_seqno: 0,
            scroll_top: None,
            agent: None,
            master: pair.master,
            child,
        })
    }

    /// Re-identify the tab and re-evaluate detection after new PTY output
    /// (REQ-AGENT-002/010). Returns whether the displayed agent state
    /// (including the symbol appearing or disappearing) changed.
    pub fn refresh_agent(&mut self) -> bool {
        if !self.foreground_is_claude() {
            // REQ-AGENT-016/017: not Claude Code — no symbol, drop any
            // stale state.
            return self.agent.take().is_some();
        }
        let snapshot = agent::Snapshot::capture(&self.engine);
        let raw = agent::evaluate(&snapshot);
        let appeared = self.agent.is_none();
        let tracker = self.agent.get_or_insert_default();
        let changed = tracker.observe(raw, std::time::Instant::now());
        appeared || changed
    }

    /// REQ-AGENT-002: the PTY's foreground process command name must be
    /// `claude`. `/proc/<pid>/comm` is the kernel's command name; argv[0]'s
    /// basename covers the same name when comm is truncated or wrapped.
    fn foreground_is_claude(&self) -> bool {
        let Some(pid) = self.master.process_group_leader() else {
            return false;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() == "claude" {
            return true;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let arg0 = cmdline.split(|b| *b == 0).next().unwrap_or(b"");
        std::path::Path::new(&String::from_utf8_lossy(arg0).into_owned())
            .file_name()
            .is_some_and(|name| name == "claude")
    }

    /// Commit a pending idle debounce whose window has elapsed with no
    /// further output (REQ-AGENT-011); returns whether display changed.
    pub fn tick_agent(&mut self, now: std::time::Instant) -> bool {
        self.agent.as_mut().is_some_and(|t| t.tick(now))
    }

    pub fn agent_pending_idle(&self) -> bool {
        self.agent.as_ref().is_some_and(|t| t.pending())
    }

    pub fn scroll_mode(&self) -> bool {
        self.scroll_top.is_some()
    }

    /// Enter scroll mode anchored at the current live view (REQ-SCROLL-003).
    pub fn enter_scroll_mode(&mut self) {
        if self.scroll_top.is_none() {
            self.scroll_top = Some(self.engine.screen().visible_row_to_stable_row(0));
        }
    }

    /// Exit scroll mode and resume following live output (REQ-SCROLL-011).
    pub fn exit_scroll_mode(&mut self) {
        self.scroll_top = None;
    }

    /// Scroll the view by `delta` lines (negative = up into history),
    /// clamped to the oldest scrollback line and the live view
    /// (REQ-SCROLL-005..008). Returns true if the view is at the live
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
    /// (REQ-SCROLL-010) or the live tail (REQ-SCROLL-012).
    pub fn view_range(&self) -> std::ops::Range<usize> {
        let screen = self.engine.screen();
        let rows = screen.physical_rows as isize;
        match self.scroll_top {
            Some(top) => screen.stable_range(&(top..top + rows)),
            None => screen.phys_range(&(0..rows as i64)),
        }
    }

    /// Resize the PTY and engine to a new content rectangle
    /// (REQ-PANE-012/013, REQ-WINDOW-018/019).
    pub fn resize(&mut self, rect: Rect) {
        self.rect = rect;
        let _ = self.master.resize(pty_size(rect));
        self.engine.resize(term_size(rect));
    }

    /// Reap the exited child and return its exit status (REQ-WINDOW-022).
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
