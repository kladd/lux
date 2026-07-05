//! The lux server: owns every session's layout tree, windows, tabs, PTYs,
//! and terminal engines, independent of attached clients
//! (REQ-SESSION-005), decodes all terminal input, and renders directly to
//! each attached client's passed descriptors (REQ-SESSION-010/030). The
//! client side lives in `crate::client`; the two communicate only through
//! `crate::protocol` (REQ-SESSION-001).

pub mod agent;
pub mod anim;
pub mod config;
pub mod ex;
pub mod input;
pub mod keys;
pub mod layout;
pub mod session;
pub mod term;
pub mod window;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

use ratatui::Terminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode as CtKeyCode;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::protocol::{self, Request};
use input::{DecodedInput, InputDecoder};
use keys::KeyTable;
use session::{Effect, Session};
use term::FdBackend;
use window::TabId;

type ConnId = u64;
type SessionId = usize;

pub enum ServerEvent {
    /// Output bytes read from a tab's PTY (REQ-PANE-005).
    PtyOutput(TabId, Vec<u8>),
    /// A tab's PTY reached EOF: the child's side is closed.
    PtyExited(TabId),
    /// A client finished its attach handshake (REQ-SESSION-029).
    Attach {
        conn: ConnId,
        stream: UnixStream,
        request: Request,
        stdin: OwnedFd,
        stdout: OwnedFd,
    },
    /// `ls` over a fresh connection (REQ-SESSION-020).
    Ls(UnixStream),
    /// `kill-server` over a fresh connection (REQ-SESSION-021).
    Kill(UnixStream),
    /// The client relayed a SIGWINCH (REQ-SESSION-031).
    Resized(ConnId),
    /// The control connection ended, for any reason (REQ-SESSION-014).
    ConnGone(ConnId),
    /// Raw bytes read from an attached client's stdin descriptor.
    Input(ConnId, Vec<u8>),
}

/// One attached client (REQ-SESSION-025: at most one per session).
struct Client {
    control: UnixStream,
    terminal: Terminal<FdBackend>,
    /// A second handle on the client's stdout for raw escape writes
    /// (OSC 52 clipboard mirroring).
    raw_out: File,
    decoder: InputDecoder,
    /// Stops the stdin reader thread so a detached client's keystrokes
    /// are never consumed by a stale read (REQ-SESSION-012).
    stdin_stop: Arc<AtomicBool>,
    attached: SessionId,
    /// `Some(highlighted index)` while in switcher mode (REQ-SESSION-015).
    switcher: Option<usize>,
}

pub fn run() -> i32 {
    // REQ-SESSION-004: detach from the controlling terminal so the server
    // outlives it. Fails harmlessly if already a session leader.
    let _ = rustix::process::setsid();

    let dir = protocol::socket_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("lux server: cannot create {}", dir.display());
        return 1;
    }
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let path = protocol::socket_path();
    let _ = std::fs::remove_file(&path);
    // REQ-SESSION-003: the well-known per-user socket.
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("lux server: bind {}: {err}", path.display());
            return 1;
        }
    };

    // REQ-SESSION-009: the keybinding table lives server-side; config is
    // loaded here (REQ-CONFIG-002).
    let keys = Arc::new(config::load());
    let (tx, rx) = mpsc::channel::<ServerEvent>();

    let accept_tx = tx.clone();
    thread::spawn(move || {
        static NEXT_CONN: AtomicU64 = AtomicU64::new(0);
        for stream in listener.incoming().flatten() {
            let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
            let tx = accept_tx.clone();
            thread::spawn(move || connection_thread(conn, stream, tx));
        }
    });

    let mut server = Server {
        sessions: BTreeMap::new(),
        clients: HashMap::new(),
        keys,
        clipboard: arboard::Clipboard::new().ok(),
        next_session_id: 0,
        tx,
    };
    loop {
        // While an idle debounce is pending (REQ-AGENT-011) or an
        // attached client is showing an animated status text
        // (REQ-UI-005/006), wake on a short timer so the debounce can
        // commit and the animation advances; otherwise block until
        // something happens.
        let event = if server.needs_timed_tick() {
            match rx.recv_timeout(std::time::Duration::from_millis(60)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
            }
        } else {
            match rx.recv() {
                Ok(event) => Some(event),
                Err(_) => return 0,
            }
        };
        if let Some(event) = event {
            server.handle(event);
            // Coalesce whatever else is already pending into this frame.
            while let Ok(event) = rx.try_recv() {
                server.handle(event);
            }
        }
        server.tick_agents();
        server.render_all();
    }
}

/// Per-connection thread: read the handshake (with any passed fds), then
/// keep relaying control lines until the peer goes away.
fn connection_thread(conn: ConnId, stream: UnixStream, tx: Sender<ServerEvent>) {
    let Ok((line, fds)) = protocol::recv_request_with_fds(&stream) else {
        return;
    };
    let Some(request) = Request::decode(&line) else {
        return;
    };
    match request {
        Request::Ls => {
            let _ = tx.send(ServerEvent::Ls(stream));
        }
        Request::Kill => {
            let _ = tx.send(ServerEvent::Kill(stream));
        }
        Request::New(_) | Request::Attach(_) => {
            let mut fds = fds.into_iter();
            let (Some(stdin), Some(stdout)) = (fds.next(), fds.next()) else {
                return;
            };
            let Ok(mut control) = stream.try_clone() else {
                return;
            };
            if tx
                .send(ServerEvent::Attach { conn, stream, request, stdin, stdout })
                .is_err()
            {
                return;
            }
            loop {
                match protocol::read_line(&mut control) {
                    Ok(Some(line)) if Request::decode(&line) == Some(Request::Resize) => {
                        let _ = tx.send(ServerEvent::Resized(conn));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        let _ = tx.send(ServerEvent::ConnGone(conn));
                        return;
                    }
                }
            }
        }
        Request::Resize => {}
    }
}

struct Server {
    /// Creation-ordered, the order `ls` and the switcher present.
    sessions: BTreeMap<SessionId, Session>,
    clients: HashMap<ConnId, Client>,
    keys: Arc<KeyTable>,
    clipboard: Option<arboard::Clipboard>,
    next_session_id: SessionId,
    tx: Sender<ServerEvent>,
}

impl Server {
    fn has_pending_idle(&self) -> bool {
        self.sessions.values().any(|s| s.has_pending_idle())
    }

    /// Whether the event loop should wake on a timer rather than block:
    /// an idle debounce is waiting to commit (REQ-AGENT-011), or a
    /// session some client is viewing — attached or as a live switcher
    /// preview (REQ-SESSION-016) — has an animated status text to advance
    /// (REQ-UI-005/006).
    fn needs_timed_tick(&self) -> bool {
        self.has_pending_idle()
            || self.clients.values().any(|c| {
                if c.switcher.is_some() {
                    self.sessions.values().any(|s| s.has_animation())
                } else {
                    self.sessions.get(&c.attached).is_some_and(|s| s.has_animation())
                }
            })
    }

    /// Commit any idle debounces whose window elapsed (REQ-AGENT-011).
    fn tick_agents(&mut self) {
        let now = std::time::Instant::now();
        for session in self.sessions.values_mut() {
            session.tick_agents(now);
        }
    }

    fn handle(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::PtyOutput(tab, bytes) => {
                if let Some(session) = self.sessions.values_mut().find(|s| s.has_tab(tab)) {
                    session.pty_output(tab, &bytes);
                }
            }
            ServerEvent::PtyExited(tab) => {
                let Some((&sid, session)) =
                    self.sessions.iter_mut().find(|(_, s)| s.has_tab(tab))
                else {
                    return;
                };
                if let Some(Effect::Ended) = session.pty_exited(tab) {
                    self.end_session(sid);
                }
            }
            ServerEvent::Attach { conn, stream, request, stdin, stdout } => {
                self.attach(conn, stream, request, stdin, stdout);
            }
            ServerEvent::Ls(mut stream) => {
                // REQ-SESSION-020: one name per line.
                for session in self.sessions.values() {
                    let _ = protocol::write_line(&mut stream, &session.name);
                }
            }
            ServerEvent::Kill(mut stream) => {
                // REQ-SESSION-021: end every session, disconnect every
                // client, terminate.
                let _ = protocol::write_line(&mut stream, "ok");
                let conns: Vec<ConnId> = self.clients.keys().copied().collect();
                for conn in conns {
                    self.detach(conn);
                }
                let _ = std::fs::remove_file(protocol::socket_path());
                std::process::exit(0);
            }
            ServerEvent::Resized(conn) => {
                // REQ-SESSION-032: read the real dimensions from the
                // client's descriptor and resize the attached session.
                let Some(client) = self.clients.get_mut(&conn) else { return };
                let size = term::fd_size(&client.raw_out);
                client.terminal.backend_mut().set_size(size);
                if let Some(session) = self.sessions.get_mut(&client.attached) {
                    session.set_area(Rect::new(0, 0, size.width, size.height));
                }
            }
            ServerEvent::ConnGone(conn) => {
                // REQ-SESSION-014: the session lives on regardless of why
                // the connection ended.
                self.detach(conn);
            }
            ServerEvent::Input(conn, bytes) => self.client_input(conn, bytes),
        }
    }

    fn attach(
        &mut self,
        conn: ConnId,
        mut stream: UnixStream,
        request: Request,
        stdin: OwnedFd,
        stdout: OwnedFd,
    ) {
        let stdout_file = File::from(stdout);
        let size = term::fd_size(&stdout_file);
        let area = Rect::new(0, 0, size.width, size.height);

        let sid = match request {
            // REQ-SESSION-006/007: bare connect creates an auto-named
            // session.
            Request::New(None) => self.create_session(None, area),
            // REQ-SESSION-019: named create fails on collision.
            Request::New(Some(name)) => {
                if self.session_by_name(&name).is_some() {
                    let _ = protocol::write_line(
                        &mut stream,
                        &format!("err session '{name}' already exists"),
                    );
                    return;
                }
                self.create_session(Some(name), area)
            }
            // REQ-SESSION-008/026: attach to the named session or fail.
            Request::Attach(name) => match self.session_by_name(&name) {
                Some(sid) => Ok(sid),
                None => {
                    let _ = protocol::write_line(
                        &mut stream,
                        &format!("err no session named '{name}'"),
                    );
                    return;
                }
            },
            _ => return,
        };
        let sid = match sid {
            Ok(sid) => sid,
            Err(msg) => {
                let _ = protocol::write_line(&mut stream, &format!("err {msg}"));
                return;
            }
        };

        if protocol::write_line(&mut stream, "ok").is_err() {
            return;
        }

        // REQ-SESSION-027: attachment is exclusive; the old client is
        // disconnected first.
        if let Some(&old) = self
            .clients
            .iter()
            .find(|(_, c)| c.attached == sid)
            .map(|(conn, _)| conn)
        {
            self.detach(old);
        }

        let Ok(raw_out) = stdout_file.try_clone() else { return };
        let mut terminal = match Terminal::new(FdBackend::new(stdout_file, size)) {
            Ok(terminal) => terminal,
            Err(_) => return,
        };
        // Start from a clean screen (the client just entered the alternate
        // screen).
        let _ = terminal.clear();

        let stdin_stop = Arc::new(AtomicBool::new(false));
        spawn_stdin_reader(conn, stdin, stdin_stop.clone(), self.tx.clone());

        if let Some(session) = self.sessions.get_mut(&sid) {
            session.set_area(area);
        }

        self.clients.insert(
            conn,
            Client {
                control: stream,
                terminal,
                raw_out,
                decoder: InputDecoder::default(),
                stdin_stop,
                attached: sid,
                switcher: None,
            },
        );
    }

    fn create_session(&mut self, name: Option<String>, area: Rect) -> Result<SessionId, String> {
        // REQ-SESSION-007: generate a name when none was requested — the
        // smallest unused non-negative integer, tmux-style.
        let name = match name {
            Some(name) => name,
            None => (0..)
                .map(|n| n.to_string())
                .find(|candidate| self.session_by_name(candidate).is_none())
                .expect("some integer name is free"),
        };
        let session = Session::new(name, area, self.keys.clone(), self.tx.clone())
            .map_err(|err| format!("cannot start session: {err:#}"))?;
        let sid = self.next_session_id;
        self.next_session_id += 1;
        self.sessions.insert(sid, session);
        Ok(sid)
    }

    fn session_by_name(&self, name: &str) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|(_, s)| s.name == name)
            .map(|(&sid, _)| sid)
    }

    /// End a client's connection, keeping its session running
    /// (REQ-SESSION-012/014). The client restores its own terminal on
    /// seeing the stream close (REQ-SESSION-013).
    fn detach(&mut self, conn: ConnId) {
        let Some(client) = self.clients.remove(&conn) else { return };
        // Stop the stdin reader before dropping our fds so a lingering
        // read can't swallow keystrokes meant for the user's shell.
        client.stdin_stop.store(true, Ordering::Relaxed);
        let _ = client.control.shutdown(Shutdown::Both);
    }

    /// REQ-SESSION-023/024: a session that ended takes its attached
    /// client's connection with it.
    fn end_session(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
        if let Some(&conn) = self
            .clients
            .iter()
            .find(|(_, c)| c.attached == sid)
            .map(|(conn, _)| conn)
        {
            self.detach(conn);
        }
        // Clamp switcher highlights that pointed past the removed session.
        let remaining = self.sessions.len();
        for client in self.clients.values_mut() {
            if let Some(highlight) = client.switcher.as_mut() {
                *highlight = (*highlight).min(remaining.saturating_sub(1));
            }
        }
    }

    fn client_input(&mut self, conn: ConnId, bytes: Vec<u8>) {
        let Some(client) = self.clients.get_mut(&conn) else { return };
        let events = client.decoder.decode(&bytes);
        for event in events {
            let Some(client) = self.clients.get(&conn) else { return };
            if client.switcher.is_some() {
                self.switcher_input(conn, &event);
                continue;
            }
            let sid = client.attached;
            let Some(session) = self.sessions.get_mut(&sid) else { continue };
            let effect = match event {
                DecodedInput::Key(key) => session.handle_key(key),
                DecodedInput::Mouse(mouse) => session.handle_mouse(mouse),
                DecodedInput::Paste(text) => {
                    session.paste_text(&text);
                    None
                }
            };
            if let Some(effect) = effect {
                self.apply_effect(conn, sid, effect);
            }
        }
    }

    fn apply_effect(&mut self, conn: ConnId, sid: SessionId, effect: Effect) {
        match effect {
            // REQ-SESSION-012.
            Effect::Detach => self.detach(conn),
            // REQ-SESSION-015: switcher mode is per-connection.
            Effect::OpenSwitcher => {
                let highlight = self
                    .sessions
                    .keys()
                    .position(|&id| id == sid)
                    .unwrap_or(0);
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.switcher = Some(highlight);
                }
            }
            // REQ-SCROLL-016: native clipboard plus OSC 52 so the client's
            // terminal (or an outer multiplexer/SSH hop) mirrors it.
            Effect::Copy(text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    let _ = clipboard.set_text(text.clone());
                }
                if let Some(client) = self.clients.get_mut(&conn) {
                    osc52_copy(&mut client.raw_out, &text);
                }
            }
            // REQ-SCROLL-023: paste the system clipboard's current text.
            Effect::Paste => {
                let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else {
                    return;
                };
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.paste_text(&text);
                }
            }
            Effect::Ended => self.end_session(sid),
        }
    }

    fn switcher_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let DecodedInput::Key(key) = event else { return };
        let count = self.sessions.len();
        let Some(client) = self.clients.get_mut(&conn) else { return };
        let Some(highlight) = client.switcher else { return };
        let ctrl = key.modifiers.contains(ratatui::crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            // REQ-SESSION-034: `k`, Up, or Ctrl-p (tmux's `choose-tree`
            // binding) moves the highlight up, wrapping to the last.
            CtKeyCode::Up | CtKeyCode::Char('k') if !ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
            CtKeyCode::Char('p') if ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
            // REQ-SESSION-035: `j`, Down, or Ctrl-n (tmux's `choose-tree`
            // binding) moves the highlight down, wrapping to the first.
            CtKeyCode::Down | CtKeyCode::Char('j') if !ctrl => {
                client.switcher = Some(if count == 0 { 0 } else { (highlight + 1) % count });
            }
            CtKeyCode::Char('n') if ctrl => {
                client.switcher = Some(if count == 0 { 0 } else { (highlight + 1) % count });
            }
            // REQ-SESSION-018: back out without changing attachment.
            CtKeyCode::Esc => {
                client.switcher = None;
                let sid = client.attached;
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.request_redraw();
                }
            }
            // REQ-SESSION-017: re-attach the connection to the selection.
            CtKeyCode::Enter => {
                client.switcher = None;
                let Some(&target) = self.sessions.keys().nth(highlight) else { return };
                let current = client.attached;
                let size = term::fd_size(&client.raw_out);
                if target != current {
                    // REQ-SESSION-027: exclusive attachment.
                    if let Some(other) = self
                        .clients
                        .iter()
                        .find(|(c, cl)| **c != conn && cl.attached == target)
                        .map(|(conn, _)| *conn)
                    {
                        self.detach(other);
                    }
                    if let Some(client) = self.clients.get_mut(&conn) {
                        client.attached = target;
                    }
                }
                if let Some(session) = self.sessions.get_mut(&target) {
                    session.set_area(Rect::new(0, 0, size.width, size.height));
                    session.request_redraw();
                }
            }
            _ => {}
        }
    }

    /// Draw every attached client that needs it: switcher frames render
    /// each pass (their preview is live, REQ-SESSION-016); attached
    /// sessions render when their state advanced.
    fn render_all(&mut self) {
        let Server { sessions, clients, .. } = self;
        for client in clients.values_mut() {
            if let Some(highlight) = client.switcher {
                render_switcher(client, sessions, highlight);
            } else if let Some(session) = sessions.get_mut(&client.attached)
                && session.needs_redraw()
            {
                let _ = session.draw_frame(&mut client.terminal);
            }
        }
    }
}

/// The switcher frame: session list on the left, live preview of the
/// highlighted session on the right (REQ-SESSION-015/016).
fn render_switcher(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    highlight: usize,
) {
    let names: Vec<String> = sessions
        .values()
        .map(|s| format!("{} ({} windows)", s.name, s.window_count()))
        .collect();
    let highlighted_sid = sessions.keys().nth(highlight).copied();
    let _ = client.terminal.draw(|frame| {
        let area = frame.area();
        let buf = frame.buffer_mut();
        clear_region(buf, area);
        let list_w = 28.min(area.width);
        for (i, name) in names.iter().enumerate() {
            let y = area.y + 1 + i as u16;
            if y >= area.bottom() {
                break;
            }
            let style = if i == highlight {
                Style::default().fg(Color::Green).add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let text = format!(" {name} ");
            for (j, ch) in text.chars().enumerate() {
                let x = area.x + j as u16;
                if x >= area.x + list_w {
                    break;
                }
                if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                    dst.set_char(ch);
                    dst.set_style(style);
                }
            }
        }
        // Divider and preview pane.
        if area.width > list_w {
            for y in area.top()..area.bottom() {
                if let Some(dst) = buf.cell_mut(Position::new(area.x + list_w, y)) {
                    dst.set_symbol("│");
                    dst.set_style(Style::default().fg(Color::DarkGray));
                }
            }
            let preview = Rect {
                x: area.x + list_w + 1,
                width: area.width - list_w - 1,
                ..area
            };
            if let Some(session) = highlighted_sid.and_then(|sid| sessions.get_mut(&sid)) {
                session.render_preview(buf, preview);
            }
        }
    });
}

fn clear_region(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.reset();
            }
        }
    }
}

/// Read raw input from a client's passed stdin descriptor
/// (REQ-SESSION-010/030). Poll with a short timeout so `stop` can end the
/// thread promptly on detach — a blocked read would otherwise race the
/// user's shell for the keystrokes typed after detach.
fn spawn_stdin_reader(
    conn: ConnId,
    stdin: OwnedFd,
    stop: Arc<AtomicBool>,
    tx: Sender<ServerEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let mut fds = [rustix::event::PollFd::new(&stdin, rustix::event::PollFlags::IN)];
            match rustix::event::poll(&mut fds, 25) {
                Ok(0) => continue,
                Ok(_) => match rustix::io::read(&stdin, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(ServerEvent::Input(conn, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                },
                Err(_) => return,
            }
        }
    });
}

/// OSC 52 written straight to the client's terminal (REQ-SCROLL-016).
fn osc52_copy(out: &mut File, text: &str) {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}
