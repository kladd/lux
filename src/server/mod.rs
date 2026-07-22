//! The lux server: owns every session's layout tree, windows, tabs, PTYs,
//! and terminal engines, independent of attached clients, decodes all
//! terminal input, and renders directly to
//! each attached client's passed descriptors. The
//! client side lives in `crate::client`; the two communicate only through
//! `crate::protocol`.

pub mod agent;
pub mod anim;
pub mod config;
pub mod ex;
pub mod find;
pub mod grid;
pub mod input;
pub mod keys;
pub mod layout;
pub mod persist;
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
use ratatui::crossterm::event::{
    KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers as CtMods,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};

use crate::protocol::{self, Request};
use grid::GridState;
use input::{DecodedInput, InputDecoder};
use keys::{KeyMatch, KeyTable};
use layout::Dir;
use session::{Effect, Session};
use term::FdBackend;
use window::TabId;

type ConnId = u64;
type SessionId = usize;

pub enum ServerEvent {
    /// Output bytes read from a tab's PTY.
    PtyOutput(TabId, Vec<u8>),
    /// A tab's PTY reached EOF: the child's side is closed.
    PtyExited(TabId),
    /// A client finished its attach handshake.
    Attach {
        conn: ConnId,
        stream: UnixStream,
        request: Request,
        stdin: OwnedFd,
        stdout: OwnedFd,
    },
    /// `ls` over a fresh connection.
    Ls(UnixStream),
    /// `kill-server` over a fresh connection.
    Kill(UnixStream),
    /// `kill-session` over a fresh connection.
    KillSession(UnixStream, String),
    /// The client relayed a SIGWINCH.
    Resized(ConnId),
    /// The control connection ended, for any reason.
    ConnGone(ConnId),
    /// Raw bytes read from an attached client's stdin descriptor.
    Input(ConnId, Vec<u8>),
    /// The client's stdin went quiet after input: resolve any bytes the
    /// decoder held back waiting for more (a partial paste marker).
    InputIdle(ConnId),
    /// A tab's program set the clipboard via OSC 52.
    ProgramCopy(TabId, String),
}

/// Where a captured grid tab's prefix command sent the connection, when
/// it left the grid.
enum GridExit {
    Switcher,
    Finder,
}

/// One attached client (at most one per session).
struct Client {
    control: UnixStream,
    terminal: Terminal<FdBackend>,
    /// A second handle on the client's stdout for raw escape writes
    /// (OSC 52 clipboard mirroring).
    raw_out: File,
    decoder: InputDecoder,
    /// Stops the stdin reader thread so a detached client's keystrokes
    /// are never consumed by a stale read.
    stdin_stop: Arc<AtomicBool>,
    attached: SessionId,
    /// `Some(highlighted index)` while in switcher mode.
    switcher: Option<usize>,
    /// `Some` while viewing the CLAUDECOM grid.
    grid: Option<GridState>,
    /// `Some` while in fuzzy tab-find mode.
    finder: Option<find::FinderState>,
    /// The pointer shape last written to the client's terminal (an OSC
    /// 22 name), so hover updates only write changes.
    pointer: &'static str,
}

pub fn run() -> i32 {
    // Detach from the controlling terminal so the server
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
    // The well-known per-user socket.
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("lux server: bind {}: {err}", path.display());
            return 1;
        }
    };

    // The keybinding table lives server-side; config is
    // loaded here.
    let config = config::load();
    let keys = Arc::new(config.keys);
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
        attach_order: Vec::new(),
        keys,
        clipboard: arboard::Clipboard::new().ok(),
        notify: config.notify,
        next_session_id: 0,
        save_deadline: None,
        last_saved: None,
        tx,
    };
    // Bring back every persisted session before any client
    // attaches; disabled restore starts empty, as if no state existed.
    if config.restore
        && let Some(snapshot) = persist::load()
    {
        server.restore_sessions(&snapshot);
    }
    loop {
        // While an idle debounce is pending or an
        // attached client is showing an animated status text, wake on a
        // short timer so the debounce can
        // commit and the animation advances; otherwise block until
        // something happens.
        let event = if server.needs_timed_tick() {
            match rx.recv_timeout(std::time::Duration::from_millis(60)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
            }
        } else {
            // Even fully quiet, wake at the next wall-clock
            // minute so the session status line's clock advances.
            match rx.recv_timeout(until_next_minute()) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
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
        server.tick_save();
        server.render_all();
    }
}

/// How long after a state-changing event the automatic save runs,
/// coalescing bursts (keystrokes, streaming output) into one write.
const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Time until the next wall-clock minute boundary. Sub-second
/// truncation lands the wake just past the boundary, never before it.
fn until_next_minute() -> std::time::Duration {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    std::time::Duration::from_secs(60 - secs % 60)
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
        Request::KillSession(name) => {
            let _ = tx.send(ServerEvent::KillSession(stream, name));
        }
        Request::New | Request::Session(_) | Request::Recent => {
            let mut fds = fds.into_iter();
            let (Some(stdin), Some(stdout)) = (fds.next(), fds.next()) else {
                return;
            };
            let Ok(mut control) = stream.try_clone() else {
                return;
            };
            if tx
                .send(ServerEvent::Attach {
                    conn,
                    stream,
                    request,
                    stdin,
                    stdout,
                })
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
    /// Sessions in attachment order, most recent last — the fallback
    /// targets for an attach with no name. Ended sessions are skipped on
    /// lookup rather than eagerly pruned.
    attach_order: Vec<SessionId>,
    keys: Arc<KeyTable>,
    clipboard: Option<arboard::Clipboard>,
    /// Whether desktop notifications are enabled (config `notify`).
    notify: bool,
    next_session_id: SessionId,
    /// When the pending automatic save runs; armed by any event that can
    /// change persisted state.
    save_deadline: Option<std::time::Instant>,
    /// The last snapshot written, to skip writes when nothing changed.
    last_saved: Option<String>,
    tx: Sender<ServerEvent>,
}

impl Server {
    fn has_pending_idle(&self) -> bool {
        self.sessions.values().any(|s| s.has_pending_idle())
    }

    /// Whether the event loop should wake on a timer rather than block:
    /// an idle debounce is waiting to commit, a repeat deadline (resize
    /// submap or move-tab) is armed, an automatic save is pending, or a
    /// session some client is viewing — attached or as a live switcher
    /// preview — has an animated status text to advance.
    fn needs_timed_tick(&self) -> bool {
        self.has_pending_idle()
            || self.save_deadline.is_some()
            || self.sessions.values().any(|s| s.has_pending_repeat())
            || self.clients.values().any(|c| {
                if c.switcher.is_some() || c.grid.is_some() {
                    self.sessions.values().any(|s| s.has_animation())
                } else {
                    self.sessions
                        .get(&c.attached)
                        .is_some_and(|s| s.has_animation())
                }
            })
    }

    /// Commit any idle debounces whose window elapsed, and close any
    /// repeat window whose deadline did. Agents landing in done raise
    /// desktop notifications.
    fn tick_agents(&mut self) {
        let now = std::time::Instant::now();
        let mut notices = Vec::new();
        for session in self.sessions.values_mut() {
            for notice in session.tick_agents(now) {
                notices.push((session.name.clone(), notice));
            }
            session.tick_repeats(now);
        }
        for (session, notice) in notices {
            self.raise_notification(&session, &notice);
        }
    }

    /// Raise a desktop notification for a Claude Code tab that reached
    /// done or blocked: forward it as a plain OSC 9 escape to every
    /// attached client's terminal — whichever terminal the user is
    /// looking at displays it, even for a background session. With no
    /// client attached the notification is discarded; there is no
    /// history to hold it.
    fn raise_notification(&mut self, session: &str, notice: &window::Notice) {
        if !self.notify {
            return;
        }
        let what = if notice.blocked {
            "needs your input"
        } else {
            "is done"
        };
        let mut text = format!("{session}:{} {what}", notice.tab);
        if let Some(summary) = &notice.summary {
            text.push_str(": ");
            text.push_str(summary);
        }
        // OSC string content must stay free of control bytes; a stray
        // ESC or BEL in a name would cut the sequence short.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        for client in self.clients.values_mut() {
            let _ = write!(client.raw_out, "\x1b]9;{text}\x1b\\");
            let _ = client.raw_out.flush();
        }
    }

    /// Arm the automatic save; the debounce coalesces event bursts into
    /// one write.
    fn mark_dirty(&mut self) {
        if self.save_deadline.is_none() {
            self.save_deadline = Some(std::time::Instant::now() + SAVE_DEBOUNCE);
        }
    }

    /// Run a pending automatic save whose debounce elapsed.
    fn tick_save(&mut self) {
        if self
            .save_deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            self.save_deadline = None;
            self.save_sessions();
        }
    }

    /// Persist every session's state now, skipping the
    /// write when nothing changed since the last save.
    fn save_sessions(&mut self) {
        let snapshot = persist::StateSnapshot {
            sessions: self.sessions.values_mut().map(Session::snapshot).collect(),
        };
        let Ok(json) = serde_json::to_string_pretty(&snapshot) else {
            return;
        };
        if self.last_saved.as_deref() == Some(&json) {
            return;
        }
        persist::save(&json);
        self.last_saved = Some(json);
    }

    /// Recreate every persisted session at startup; clients then attach
    /// to them by name as usual.
    fn restore_sessions(&mut self, snapshot: &persist::StateSnapshot) {
        // No client is attached yet; a plausible size until one is.
        let area = Rect::new(0, 0, 80, 24);
        for snap in &snapshot.sessions {
            if self.session_by_name(&snap.name).is_some() {
                continue;
            }
            let Some(session) = Session::restore(snap, area, self.keys.clone(), self.tx.clone())
            else {
                continue;
            };
            let sid = self.next_session_id;
            self.next_session_id += 1;
            self.sessions.insert(sid, session);
        }
    }

    fn handle(&mut self, event: ServerEvent) {
        // Anything a client does, and anything a tab's process does, can
        // change persisted state; connection lifecycle and reads can't.
        match event {
            ServerEvent::PtyOutput(..)
            | ServerEvent::PtyExited(_)
            | ServerEvent::Attach { .. }
            | ServerEvent::Input(..)
            | ServerEvent::InputIdle(_) => self.mark_dirty(),
            ServerEvent::Ls(_)
            | ServerEvent::Kill(_)
            | ServerEvent::KillSession(..)
            | ServerEvent::Resized(_)
            | ServerEvent::ConnGone(_)
            | ServerEvent::ProgramCopy(..) => {}
        }
        match event {
            ServerEvent::PtyOutput(tab, bytes) => {
                if let Some(session) = self.sessions.values_mut().find(|s| s.has_tab(tab)) {
                    let notice = session.pty_output(tab, &bytes);
                    let name = session.name.clone();
                    if let Some(notice) = notice {
                        self.raise_notification(&name, &notice);
                    }
                }
            }
            ServerEvent::PtyExited(tab) => {
                let Some((&sid, session)) = self.sessions.iter_mut().find(|(_, s)| s.has_tab(tab))
                else {
                    return;
                };
                if let Some(Effect::Ended) = session.pty_exited(tab) {
                    self.end_session(sid);
                }
            }
            ServerEvent::Attach {
                conn,
                stream,
                request,
                stdin,
                stdout,
            } => {
                self.attach(conn, stream, request, stdin, stdout);
            }
            ServerEvent::Ls(mut stream) => {
                // One name per line.
                for session in self.sessions.values() {
                    let _ = protocol::write_line(&mut stream, &session.name);
                }
            }
            ServerEvent::Kill(mut stream) => {
                // End every session, disconnect every
                // client, terminate. A final save first, so the killed
                // sessions restore when the server next starts.
                self.save_sessions();
                let _ = protocol::write_line(&mut stream, "ok");
                let conns: Vec<ConnId> = self.clients.keys().copied().collect();
                for conn in conns {
                    self.detach(conn);
                }
                let _ = std::fs::remove_file(protocol::socket_path());
                std::process::exit(0);
            }
            ServerEvent::KillSession(mut stream, name) => match self.session_by_name(&name) {
                Some(sid) => {
                    self.end_session(sid);
                    let _ = protocol::write_line(&mut stream, "ok");
                }
                None => {
                    let _ = protocol::write_line(
                        &mut stream,
                        &format!("err no session named '{name}'"),
                    );
                }
            },
            ServerEvent::Resized(conn) => {
                // Read the real dimensions from the
                // client's descriptor and resize the attached session.
                let Some(client) = self.clients.get_mut(&conn) else {
                    return;
                };
                let size = term::fd_size(&client.raw_out);
                client.terminal.backend_mut().set_size(size);
                if let Some(session) = self.sessions.get_mut(&client.attached) {
                    session.set_area(Rect::new(0, 0, size.width, size.height));
                }
            }
            ServerEvent::ConnGone(conn) => {
                // The session lives on regardless of why
                // the connection ended.
                self.detach(conn);
            }
            ServerEvent::Input(conn, bytes) => self.client_input(conn, bytes),
            ServerEvent::InputIdle(conn) => self.client_input_idle(conn),
            // A program inside a tab copied: put the text on the system
            // clipboard and mirror it via OSC 52 to the terminal of the
            // client attached to that tab's session, matching the yank
            // path.
            ServerEvent::ProgramCopy(tab, text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    let _ = clipboard.set_text(text.clone());
                }
                let Some(sid) = self
                    .sessions
                    .iter()
                    .find(|(_, s)| s.has_tab(tab))
                    .map(|(&sid, _)| sid)
                else {
                    return;
                };
                for client in self.clients.values_mut().filter(|c| c.attached == sid) {
                    osc52_copy(&mut client.raw_out, &text);
                }
            }
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
            // Bare connect creates an auto-named session.
            Request::New => self.create_session(None, area),
            // A name attaches to its session, or creates it.
            Request::Session(name) => match self.session_by_name(&name) {
                Some(sid) => Ok(sid),
                None => self.create_session(Some(name), area),
            },
            // No target falls back to the most recently attached
            // session, or starts fresh when there is none.
            Request::Recent => match self.recent_session() {
                Some(sid) => Ok(sid),
                None => self.create_session(None, area),
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

        // Attachment is exclusive; the old client is
        // disconnected first.
        if let Some(&old) = self
            .clients
            .iter()
            .find(|(_, c)| c.attached == sid)
            .map(|(conn, _)| conn)
        {
            self.detach(old);
        }

        let Ok(raw_out) = stdout_file.try_clone() else {
            return;
        };
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
                grid: None,
                finder: None,
                pointer: "default",
            },
        );
        self.note_attached(sid);
    }

    fn create_session(&mut self, name: Option<String>, area: Rect) -> Result<SessionId, String> {
        // Generate a name when none was requested — the
        // smallest unused non-negative integer.
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

    /// Record `sid` as the most recently attached session.
    fn note_attached(&mut self, sid: SessionId) {
        self.attach_order.retain(|&id| id != sid);
        self.attach_order.push(sid);
    }

    /// The most recently attached session still running.
    fn recent_session(&self) -> Option<SessionId> {
        self.attach_order
            .iter()
            .rev()
            .copied()
            .find(|sid| self.sessions.contains_key(sid))
    }

    fn session_by_name(&self, name: &str) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|(_, s)| s.name == name)
            .map(|(&sid, _)| sid)
    }

    /// End a client's connection, keeping its session running.
    /// The client restores its own terminal on
    /// seeing the stream close.
    fn detach(&mut self, conn: ConnId) {
        let Some(client) = self.clients.remove(&conn) else {
            return;
        };
        // Stop the stdin reader before dropping our fds so a lingering
        // read can't swallow keystrokes meant for the user's shell.
        client.stdin_stop.store(true, Ordering::Relaxed);
        let _ = client.control.shutdown(Shutdown::Both);
    }

    /// A session that ended takes its attached
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
        let remaining = self.pinned_entries() + self.sessions.len();
        for client in self.clients.values_mut() {
            if let Some(highlight) = client.switcher.as_mut() {
                *highlight = (*highlight).min(remaining.saturating_sub(1));
            }
        }
    }

    /// How many pinned entries precede the sessions in the switcher's
    /// list: the CLAUDECOM entry while any tab anywhere is
    /// identified as running Claude Code.
    fn pinned_entries(&self) -> usize {
        self.sessions.values().any(Session::has_claude_tab) as usize
    }

    fn client_input(&mut self, conn: ConnId, bytes: Vec<u8>) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let events = client.decoder.decode(&bytes);
        self.route_input(conn, events);
    }

    /// The stdin stream went idle: input the decoder held back as a
    /// possible paste marker turned out to be ordinary keys.
    fn client_input_idle(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let events = client.decoder.flush();
        self.route_input(conn, events);
    }

    fn route_input(&mut self, conn: ConnId, events: Vec<DecodedInput>) {
        for event in events {
            let Some(client) = self.clients.get(&conn) else {
                return;
            };
            if client.finder.is_some() {
                self.finder_input(conn, &event);
                continue;
            }
            if client.grid.is_some() {
                self.grid_input(conn, &event);
                continue;
            }
            if client.switcher.is_some() {
                self.switcher_input(conn, &event);
                continue;
            }
            let sid = client.attached;
            let Some(session) = self.sessions.get_mut(&sid) else {
                continue;
            };
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
            Effect::Detach => self.detach(conn),
            // Switcher mode is per-connection.
            Effect::OpenSwitcher => self.open_switcher(conn),
            // The grid opens directly, without passing through the
            // switcher.
            Effect::OpenGrid => {
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.grid = Some(GridState::default());
                }
            }
            // So does the fuzzy tab finder.
            Effect::OpenFinder => self.open_finder(conn),
            Effect::NewSession(name) => self.new_session_for(conn, name),
            Effect::RenameSession(name) => {
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.name = name;
                    session.request_redraw();
                }
            }
            Effect::KillSession(name) => {
                let target = match name {
                    Some(n) => self.session_by_name(&n),
                    None => Some(sid),
                };
                if let Some(target_sid) = target {
                    self.end_session(target_sid);
                }
            }
            // Native clipboard plus OSC 52 so the client's
            // terminal (or an outer multiplexer/SSH hop) mirrors it.
            Effect::Copy(text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    let _ = clipboard.set_text(text.clone());
                }
                if let Some(client) = self.clients.get_mut(&conn) {
                    osc52_copy(&mut client.raw_out, &text);
                }
            }
            // Paste the system clipboard's current text.
            Effect::Paste => {
                let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) else {
                    return;
                };
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.paste_text(&text);
                }
            }
            // Written only on change, so plain mouse motion doesn't spam
            // escape sequences at the client's terminal.
            Effect::Pointer(shape) => {
                if let Some(client) = self.clients.get_mut(&conn)
                    && client.pointer != shape
                {
                    client.pointer = shape;
                    let _ = write!(client.raw_out, "\x1b]22;{shape}\x1b\\");
                }
            }
            Effect::Ended => self.end_session(sid),
        }
    }

    fn switcher_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let DecodedInput::Key(key) = event else {
            return;
        };
        let pinned = self.pinned_entries();
        let count = pinned + self.sessions.len();
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(highlight) = client.switcher else {
            return;
        };
        // The pinned entry can disappear under an open switcher; a stale
        // highlight clamps rather than pointing past the list.
        let highlight = highlight.min(count.saturating_sub(1));
        let ctrl = key
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            // `k`, Up, or Ctrl-p moves the highlight up, wrapping to the last.
            CtKeyCode::Up | CtKeyCode::Char('k') if !ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
            CtKeyCode::Char('p') if ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
            // `j`, Down, or Ctrl-n moves the highlight down, wrapping to the first.
            CtKeyCode::Down | CtKeyCode::Char('j') if !ctrl => {
                client.switcher = Some(if count == 0 {
                    0
                } else {
                    (highlight + 1) % count
                });
            }
            CtKeyCode::Char('n') if ctrl => {
                client.switcher = Some(if count == 0 {
                    0
                } else {
                    (highlight + 1) % count
                });
            }
            // Back out without changing attachment.
            CtKeyCode::Esc => {
                client.switcher = None;
                let sid = client.attached;
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.request_redraw();
                }
            }
            // Re-attach the connection to the selection; the pinned
            // CLAUDECOM entry opens its grid instead of any
            // session.
            CtKeyCode::Enter => {
                client.switcher = None;
                if highlight < pinned {
                    client.grid = Some(GridState::default());
                    return;
                }
                let Some(&target) = self.sessions.keys().nth(highlight - pinned) else {
                    return;
                };
                let current = client.attached;
                let size = term::fd_size(&client.raw_out);
                if target != current {
                    // Exclusive attachment.
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
                self.note_attached(target);
                if let Some(session) = self.sessions.get_mut(&target) {
                    session.set_area(Rect::new(0, 0, size.width, size.height));
                    session.request_redraw();
                }
            }
            _ => {}
        }
    }

    /// Input while the fuzzy tab finder is open: Ctrl-p/Up and Ctrl-n/Down
    /// move the highlighted match with wrap, Enter attaches to the
    /// highlighted match's tab, Escape closes without changing which
    /// session the connection is attached to, and every other key edits
    /// the query, re-narrowing the matched list.
    fn finder_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let DecodedInput::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        let items = find::items(&self.sessions);
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(state) = client.finder.as_mut() else {
            return;
        };
        let matched = find::matches(&items, &state.query());
        let count = matched.len();
        let highlight = state.highlight.min(count.saturating_sub(1));
        let ctrl = key.modifiers.contains(CtMods::CONTROL);
        let up = key.code == CtKeyCode::Up || (ctrl && key.code == CtKeyCode::Char('p'));
        let down = key.code == CtKeyCode::Down || (ctrl && key.code == CtKeyCode::Char('n'));
        if up && count > 0 {
            state.highlight = highlight.checked_sub(1).unwrap_or(count - 1);
            return;
        }
        if down && count > 0 {
            state.highlight = (highlight + 1) % count;
            return;
        }
        match key.code {
            // Back out without changing attachment.
            CtKeyCode::Esc => {
                client.finder = None;
                let sid = client.attached;
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.request_redraw();
                }
            }
            // Attach to the highlighted match's home session, focused on
            // its tab. With nothing matched there is nothing to select.
            CtKeyCode::Enter => {
                let Some(&idx) = matched.get(highlight) else {
                    return;
                };
                let item = &items[idx];
                let (sid, window, tab) = (item.session, item.window, item.tab);
                client.finder = None;
                self.attach_to_tab(conn, sid, window, tab);
            }
            // Everything else edits the query. The highlight follows the
            // match it was on through the re-narrowed list; a match that
            // narrowed away resets it to the top one.
            _ => {
                let followed = matched.get(highlight).map(|&i| items[i].id);
                state.textarea.input(tui_textarea::Input::from(*key));
                let matched = find::matches(&items, &state.query());
                state.highlight = followed
                    .and_then(|id| matched.iter().position(|&i| items[i].id == id))
                    .unwrap_or(0);
            }
        }
    }

    /// Re-attach `conn` to `sid`, focused on `window`'s tab at `index`:
    /// attachment stays exclusive, is recorded as most recent, and the
    /// session takes the client's terminal size.
    fn attach_to_tab(
        &mut self,
        conn: ConnId,
        sid: SessionId,
        window: layout::WindowId,
        index: usize,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let size = term::fd_size(&client.raw_out);
        if client.attached != sid {
            if let Some(other) = self
                .clients
                .iter()
                .find(|(c, cl)| **c != conn && cl.attached == sid)
                .map(|(conn, _)| *conn)
            {
                self.detach(other);
            }
            if let Some(client) = self.clients.get_mut(&conn) {
                client.attached = sid;
            }
        }
        self.note_attached(sid);
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.focus_tab(window, index);
            session.set_area(Rect::new(0, 0, size.width, size.height));
            session.request_redraw();
        }
    }

    /// Enter switcher mode for `conn`, leaving the grid if it was open;
    /// the highlight starts on the connection's attached session.
    fn open_switcher(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let sid = client.attached;
        let highlight =
            self.pinned_entries() + self.sessions.keys().position(|&id| id == sid).unwrap_or(0);
        if let Some(client) = self.clients.get_mut(&conn) {
            client.grid = None;
            client.switcher = Some(highlight);
        }
    }

    /// Enter fuzzy tab-find mode for `conn`, leaving the grid if it was
    /// open. The attached session's content is snapshotted here as the
    /// finder's backdrop — the content behind the floating window stays
    /// as it was at entry, so the finder's preview is the only view
    /// resizing tabs while it is open.
    fn open_finder(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let size = term::fd_size(&client.raw_out);
        let area = Rect::new(0, 0, size.width, size.height);
        let attached = client.attached;
        let mut backdrop = Buffer::empty(area);
        if let Some(session) = self.sessions.get_mut(&attached) {
            session.render_preview(&mut backdrop, area);
        }
        if let Some(client) = self.clients.get_mut(&conn) {
            client.grid = None;
            client.finder = Some(find::FinderState::new(backdrop));
        }
    }

    /// Create a session — named, or auto-named when `name` is `None` —
    /// and attach `conn` to it. A name already in use closes with no
    /// other action, mirroring the ex command line's silent-discard
    /// handling.
    fn new_session_for(&mut self, conn: ConnId, name: Option<String>) {
        if let Some(name) = &name
            && self.session_by_name(name).is_some()
        {
            return;
        }
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let size = term::fd_size(&client.raw_out);
        let area = Rect::new(0, 0, size.width, size.height);
        let Ok(sid) = self.create_session(name, area) else {
            return;
        };
        if let Some(client) = self.clients.get_mut(&conn) {
            client.attached = sid;
        }
        self.note_attached(sid);
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.request_redraw();
        }
    }

    /// Input while viewing the CLAUDECOM grid. Outside capture mode grid
    /// items are non-interactive: directional keys move the highlight,
    /// Enter enters capture mode for the highlighted tile's tab, `g`
    /// attaches to the highlighted tile's tab in its home session,
    /// Escape or `q` leaves the grid and resumes the session this
    /// connection stayed attached to underneath it, the prefix key leads
    /// `s` (switcher) or `f` (finder), and everything else — mouse
    /// included — is discarded; nothing reaches any tab's PTY.
    fn grid_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let Some(mut state) = self.clients.get(&conn).and_then(|c| c.grid) else {
            return;
        };
        let items = grid::items(&self.sessions);
        // A captured tab owns the input. The tab can leave the grid (its
        // process exited or stopped being Claude Code); capture ends
        // with it and the event falls through to grid navigation.
        if let Some(id) = state.capture {
            let target = items.iter().copied().find(|item| {
                self.sessions
                    .get(&item.session)
                    .and_then(|s| s.tab_at(item.window, item.tab))
                    .is_some_and(|t| t.id == id)
            });
            if let Some(item) = target {
                match self.capture_input(&mut state, item, event) {
                    None => self.store_grid_state(conn, state),
                    Some(GridExit::Switcher) => self.open_switcher(conn),
                    Some(GridExit::Finder) => self.open_finder(conn),
                }
                return;
            }
            state.capture = None;
            state.pending_prefix = false;
        }
        let DecodedInput::Key(key) = event else {
            self.store_grid_state(conn, state);
            return;
        };
        if key.kind == KeyEventKind::Release {
            self.store_grid_state(conn, state);
            return;
        }
        // A pending prefix resolves to the switcher or finder shortcut;
        // any other follow-up discards both keys, mirroring the
        // unrecognized-sequence handling everywhere else.
        if state.pending_prefix {
            state.pending_prefix = false;
            if plain_char(key, 's') {
                self.open_switcher(conn);
            } else if plain_char(key, 'f') {
                self.open_finder(conn);
            } else {
                self.store_grid_state(conn, state);
            }
            return;
        }
        if self.keys.is_prefix(*key) {
            state.pending_prefix = true;
            self.store_grid_state(conn, state);
            return;
        }
        let dir = match key.code {
            CtKeyCode::Char('h') | CtKeyCode::Left => Some(Dir::Left),
            CtKeyCode::Char('j') | CtKeyCode::Down => Some(Dir::Down),
            CtKeyCode::Char('k') | CtKeyCode::Up => Some(Dir::Up),
            CtKeyCode::Char('l') | CtKeyCode::Right => Some(Dir::Right),
            _ => None,
        };
        if let Some(dir) = dir {
            if let Some(client) = self.clients.get(&conn) {
                let size = term::fd_size(&client.raw_out);
                let area = Rect::new(0, 0, size.width, size.height);
                grid::navigate(&mut state, area, items.len(), dir);
            }
            self.store_grid_state(conn, state);
            return;
        }
        match key.code {
            // Leave the grid, resuming the session the connection was
            // attached to before opening it — attachment never changed
            // while the grid rendered over it.
            CtKeyCode::Esc | CtKeyCode::Char('q') => {
                let Some(client) = self.clients.get_mut(&conn) else {
                    return;
                };
                client.grid = None;
                let sid = client.attached;
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.request_redraw();
                }
            }
            // Capture the highlighted tile's tab for direct interaction
            // in place. An empty grid has nothing to capture.
            CtKeyCode::Enter => {
                let highlight = state.highlight.min(items.len().saturating_sub(1));
                if let Some(item) = items.get(highlight)
                    && let Some(tab) = self
                        .sessions
                        .get(&item.session)
                        .and_then(|s| s.tab_at(item.window, item.tab))
                {
                    state.capture = Some(tab.id);
                    state.pending_prefix = false;
                }
                self.store_grid_state(conn, state);
            }
            // Attach to the highlighted tile's tab in its home session —
            // `g` inverts prefix+g's "go to the grid" sense. An empty
            // grid has nowhere to go.
            CtKeyCode::Char('g') => {
                let highlight = state.highlight.min(items.len().saturating_sub(1));
                let Some(item) = items.get(highlight).copied() else {
                    self.store_grid_state(conn, state);
                    return;
                };
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.grid = None;
                }
                self.attach_to_tab(conn, item.session, item.window, item.tab);
            }
            _ => self.store_grid_state(conn, state),
        }
    }

    /// One input event for a captured grid tab: key presses are routed
    /// to its PTY, except that the prefix key always leads a command
    /// rather than ever reaching the tab raw — `g` or Escape exits
    /// capture back to grid navigation, `s` and `f` leave the grid for
    /// the switcher or finder (the returned `GridExit`), and any other
    /// follow-up discards both keys. Pastes reach the tab; mouse input
    /// is discarded.
    fn capture_input(
        &mut self,
        state: &mut GridState,
        item: grid::GridItem,
        event: &DecodedInput,
    ) -> Option<GridExit> {
        let session = self.sessions.get_mut(&item.session)?;
        match event {
            DecodedInput::Key(key) => {
                if key.kind == KeyEventKind::Release {
                    return None;
                }
                if state.pending_prefix {
                    state.pending_prefix = false;
                    if key.code == CtKeyCode::Esc || plain_char(key, 'g') {
                        state.capture = None;
                    } else if plain_char(key, 's') {
                        return Some(GridExit::Switcher);
                    } else if plain_char(key, 'f') {
                        return Some(GridExit::Finder);
                    }
                    return None;
                }
                if self.keys.is_prefix(*key) {
                    state.pending_prefix = true;
                    return None;
                }
                session.key_to_tab(item.window, item.tab, *key);
            }
            DecodedInput::Paste(text) => session.paste_to_tab(item.window, item.tab, text),
            DecodedInput::Mouse(_) => {}
        }
        None
    }

    /// Write a grid state copy back to the connection's open grid, if it
    /// is still open.
    fn store_grid_state(&mut self, conn: ConnId, state: GridState) {
        if let Some(grid) = self.clients.get_mut(&conn).and_then(|c| c.grid.as_mut()) {
            *grid = state;
        }
    }

    /// Draw every attached client that needs it: switcher and grid frames
    /// render each pass (their content is live); attached
    /// sessions render when their state advanced.
    fn render_all(&mut self) {
        let Server {
            sessions, clients, ..
        } = self;
        for client in clients.values_mut() {
            if client.finder.is_some() {
                render_finder(client, sessions);
            } else if client.grid.is_some() {
                render_grid(client, sessions);
            } else if let Some(highlight) = client.switcher {
                render_switcher(client, sessions, highlight);
            } else if let Some(session) = sessions.get_mut(&client.attached)
                && session.needs_redraw()
            {
                let _ = session.draw_frame(&mut client.terminal);
            }
        }
    }
}

/// The fuzzy tab finder frame: the backdrop snapshotted at entry — the
/// content as it was before the finder opened — with the finder's
/// floating window rendered over its center.
fn render_finder(client: &mut Client, sessions: &mut BTreeMap<SessionId, Session>) {
    let Client {
        finder, terminal, ..
    } = client;
    let Some(state) = finder.as_ref() else {
        return;
    };
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let buf = frame.buffer_mut();
        let backdrop = state.backdrop.area();
        for y in 0..area.height.min(backdrop.height) {
            for x in 0..area.width.min(backdrop.width) {
                if let (Some(dst), Some(src)) = (
                    buf.cell_mut(Position::new(area.x + x, area.y + y)),
                    state.backdrop.cell(Position::new(x, y)),
                ) {
                    *dst = src.clone();
                }
            }
        }
        find::render(buf, area, sessions, state);
    });
}

/// The CLAUDECOM frame: the live grid over the whole
/// viewport.
fn render_grid(client: &mut Client, sessions: &mut BTreeMap<SessionId, Session>) {
    let Client { grid, terminal, .. } = client;
    let Some(state) = grid.as_mut() else {
        return;
    };
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        grid::render(frame.buffer_mut(), area, sessions, state);
    });
}

/// The switcher frame: session list on the left — the pinned CLAUDECOM
/// entry first while any Claude Code tab exists — and a live
/// preview of the highlighted entry on the right.
fn render_switcher(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    highlight: usize,
) {
    let pinned = sessions.values().any(Session::has_claude_tab) as usize;
    let mut names: Vec<String> = Vec::with_capacity(pinned + sessions.len());
    if pinned > 0 {
        names.push(grid::ENTRY_NAME.to_string());
    }
    names.extend(
        sessions
            .values()
            .map(|s| format!("{} ({} windows)", s.name, s.window_count())),
    );
    let highlight = highlight.min(names.len().saturating_sub(1));
    let highlighted_sid = highlight
        .checked_sub(pinned)
        .and_then(|i| sessions.keys().nth(i).copied());
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
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::REVERSED)
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
            if pinned > 0 && highlight == 0 {
                grid::render_preview(buf, preview, sessions);
            } else if let Some(session) = highlighted_sid.and_then(|sid| sessions.get_mut(&sid)) {
                session.render_preview(buf, preview);
            }
        }
    });
}

pub(crate) fn clear_region(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.reset();
            }
        }
    }
}

/// Read raw input from a client's passed stdin descriptor.
/// Poll with a short timeout so `stop` can end the
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
        // Whether input was sent since the stream last went quiet; the
        // first poll timeout after a burst signals idle exactly once, so
        // the decoder can resolve bytes it held waiting for more.
        let mut busy = false;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let mut fds = [rustix::event::PollFd::new(
                &stdin,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut fds, 25) {
                Ok(0) => {
                    if busy {
                        busy = false;
                        if tx.send(ServerEvent::InputIdle(conn)).is_err() {
                            return;
                        }
                    }
                }
                Ok(_) => match rustix::io::read(&stdin, &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        busy = true;
                        if tx
                            .send(ServerEvent::Input(conn, buf[..n].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                },
                Err(_) => return,
            }
        }
    });
}

/// Whether `key` is the unmodified character `ch`.
fn plain_char(key: &KeyEvent, ch: char) -> bool {
    KeyMatch::from_event(*key)
        == KeyMatch {
            code: CtKeyCode::Char(ch),
            ctrl: false,
            shift: false,
        }
}

/// OSC 52 written straight to the client's terminal.
fn osc52_copy(out: &mut File, text: &str) {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}
