//! The lux server: owns every session, decodes client input, and renders
//! to each attached client's descriptors.

pub mod agent;
pub mod anim;
pub mod auto;
pub mod config;
pub mod ex;
pub mod find;
pub mod grid;
pub mod input;
pub mod keys;
pub mod layout;
pub mod palette;
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
    MouseButton as CtMouseButton, MouseEventKind as CtMouseKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};

use crate::protocol::{self, Request};
use anim::Anim;
use auto::AutoState;
use config::Config;
use grid::GridState;
use input::{DecodedInput, InputDecoder};
use keys::KeyMatch;
use layout::Dir;
use session::{Effect, Session};
use term::FdBackend;
use window::TabId;

type ConnId = u64;
type SessionId = usize;

pub enum ServerEvent {
    PtyOutput(TabId, Vec<u8>),
    PtyExited(TabId),
    Attach {
        conn: ConnId,
        stream: UnixStream,
        request: Request,
        stdin: OwnedFd,
        stdout: OwnedFd,
    },
    Ls(UnixStream),
    Kill(UnixStream),
    KillSession(UnixStream, String),
    Resized(ConnId),
    ConnGone(ConnId),
    Input(ConnId, Vec<u8>),
    /// Stdin went quiet, so flush the bytes the decoder held back as a
    /// possible paste marker.
    InputIdle(ConnId),
    /// A tab's program set the clipboard via OSC 52.
    ProgramCopy(TabId, String),
    /// SIGTERM or SIGHUP.
    Shutdown,
}

enum GridExit {
    Switcher,
    Finder,
}

/// An attached client. Each session has at most one.
struct Client {
    control: UnixStream,
    terminal: Terminal<FdBackend>,
    /// A second handle on stdout for raw escape writes.
    raw_out: File,
    decoder: InputDecoder,
    stdin_stop: Arc<AtomicBool>,
    attached: SessionId,
    /// The highlighted index while in switcher mode.
    switcher: Option<usize>,
    grid: Option<GridState>,
    auto: Option<AutoState>,
    finder: Option<find::FinderState>,
    /// The pending yank, which stays in place until paste moves it.
    yank: Option<TabId>,
    /// The OSC 22 pointer shape last written, so hover only writes changes.
    pointer: &'static str,
    /// What the terminal has answered to the color queries sent at attach.
    colors: palette::TermColors,
}

pub fn run() -> i32 {
    // Detach from the controlling terminal so the server outlives it.
    // Fails harmlessly if already a session leader.
    let _ = rustix::process::setsid();

    let dir = protocol::socket_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!("lux server: cannot create {}", dir.display());
        return 1;
    }
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let path = protocol::socket_path();
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("lux server: bind {}: {err}", path.display());
            return 1;
        }
    };

    let config = Arc::new(config::load());
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

    // Logout and reboot end the server by signal. Save first so the last
    // debounce window's changes aren't lost.
    let signal_tx = tx.clone();
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ]) {
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                let _ = signal_tx.send(ServerEvent::Shutdown);
            }
        });
    }

    let mut server = Server {
        sessions: BTreeMap::new(),
        clients: HashMap::new(),
        attach_order: Vec::new(),
        config: config.clone(),
        clipboard: arboard::Clipboard::new().ok(),
        next_session_id: 0,
        save_deadline: None,
        last_saved: None,
        tx,
    };
    if config.restore
        && let Some(snapshot) = persist::load()
    {
        server.restore_sessions(&snapshot);
    }
    loop {
        let event = if server.needs_timed_tick() {
            match rx.recv_timeout(std::time::Duration::from_millis(60)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
            }
        } else {
            // Wake at the next minute so the status line clock advances.
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
        server.tick_auto();
        server.tick_save();
        server.render_all();
    }
}

const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Truncating to whole seconds lands the wake just past the minute
/// boundary, never before it.
fn until_next_minute() -> std::time::Duration {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    std::time::Duration::from_secs(60 - secs % 60)
}

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
    /// Keyed in creation order, which `ls` and the switcher present.
    sessions: BTreeMap<SessionId, Session>,
    clients: HashMap<ConnId, Client>,
    /// Most recent last. Ended sessions are skipped on lookup, not pruned.
    attach_order: Vec<SessionId>,
    config: Arc<Config>,
    clipboard: Option<arboard::Clipboard>,
    next_session_id: SessionId,
    save_deadline: Option<std::time::Instant>,
    last_saved: Option<String>,
    tx: Sender<ServerEvent>,
}

impl Server {
    fn has_pending_idle(&self) -> bool {
        self.sessions.values().any(|s| s.has_pending_idle())
    }

    /// The switcher, grid, and blank auto screen show every session, so
    /// any session's animation counts there.
    fn needs_timed_tick(&self) -> bool {
        self.has_pending_idle()
            || self.save_deadline.is_some()
            || self.sessions.values().any(|s| s.has_pending_repeat())
            || self.clients.values().any(|c| {
                if c.switcher.is_some()
                    || c.grid.is_some()
                    || c.auto.is_some_and(|a| a.presented.is_none())
                {
                    self.sessions.values().any(|s| s.has_animation())
                } else {
                    self.sessions
                        .get(&c.attached)
                        .is_some_and(|s| s.has_animation())
                }
            })
    }

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

    /// Writes an OSC 9 notification to every attached client, so whichever
    /// terminal the user is watching shows it.
    fn raise_notification(&mut self, session: &str, notice: &window::Notice) {
        if !self.config.notify {
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
        // A stray ESC or BEL in a name would cut the OSC sequence short.
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        for client in self.clients.values_mut() {
            let _ = write!(client.raw_out, "\x1b]9;{text}\x1b\\");
            let _ = client.raw_out.flush();
        }
    }

    /// Retries the clipboard connection. The daemon outlives the
    /// environment it started in, so a later attempt can succeed where
    /// startup failed.
    fn clipboard_text(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut().and_then(|c| c.get_text().ok())
    }

    fn mark_dirty(&mut self) {
        if self.save_deadline.is_none() {
            self.save_deadline = Some(std::time::Instant::now() + SAVE_DEBOUNCE);
        }
    }

    fn tick_save(&mut self) {
        if self
            .save_deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
        {
            self.save_deadline = None;
            self.save_sessions();
        }
    }

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

    fn restore_sessions(&mut self, snapshot: &persist::StateSnapshot) {
        // A placeholder size until a client attaches.
        let area = Rect::new(0, 0, 80, 24);
        for snap in &snapshot.sessions {
            if self.session_by_name(&snap.name).is_some() {
                continue;
            }
            let Some(session) = Session::restore(snap, area, self.config.clone(), self.tx.clone())
            else {
                continue;
            };
            let sid = self.next_session_id;
            self.next_session_id += 1;
            self.sessions.insert(sid, session);
        }
    }

    fn handle(&mut self, event: ServerEvent) {
        // Only client input and tab activity can change persisted state.
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
            | ServerEvent::ProgramCopy(..)
            | ServerEvent::Shutdown => {}
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
                for session in self.sessions.values() {
                    let _ = protocol::write_line(&mut stream, &session.name);
                }
            }
            ServerEvent::Kill(mut stream) => {
                // Save first so the killed sessions restore on the next start.
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
                self.detach(conn);
            }
            ServerEvent::Input(conn, bytes) => self.client_input(conn, bytes),
            ServerEvent::InputIdle(conn) => self.client_input_idle(conn),
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
            ServerEvent::Shutdown => {
                self.save_sessions();
                let conns: Vec<ConnId> = self.clients.keys().copied().collect();
                for conn in conns {
                    self.detach(conn);
                }
                let _ = std::fs::remove_file(protocol::socket_path());
                std::process::exit(0);
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
            Request::New => self.create_session(None, area),
            Request::Session(name) => match self.session_by_name(&name) {
                Some(sid) => Ok(sid),
                None => self.create_session(Some(name), area),
            },
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

        if let Some(&old) = self
            .clients
            .iter()
            .find(|(_, c)| c.attached == sid)
            .map(|(conn, _)| conn)
        {
            self.detach(old);
        }

        let Ok(mut raw_out) = stdout_file.try_clone() else {
            return;
        };
        let mut terminal = match Terminal::new(FdBackend::new(stdout_file, size)) {
            Ok(terminal) => terminal,
            Err(_) => return,
        };
        let _ = terminal.clear();
        query_colors(&mut raw_out);

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
                auto: None,
                finder: None,
                yank: None,
                pointer: "default",
                colors: palette::TermColors::default(),
            },
        );
        self.note_attached(sid);
    }

    fn create_session(&mut self, name: Option<String>, area: Rect) -> Result<SessionId, String> {
        let name = match name {
            Some(name) => name,
            None => (0..)
                .map(|n| n.to_string())
                .find(|candidate| self.session_by_name(candidate).is_none())
                .expect("some integer name is free"),
        };
        let session = Session::new(name, area, self.config.clone(), self.tx.clone())
            .map_err(|err| format!("cannot start session: {err:#}"))?;
        let sid = self.next_session_id;
        self.next_session_id += 1;
        self.sessions.insert(sid, session);
        Ok(sid)
    }

    fn note_attached(&mut self, sid: SessionId) {
        self.attach_order.retain(|&id| id != sid);
        self.attach_order.push(sid);
    }

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

    /// Drops the connection but not the session. The client restores its
    /// own terminal when the stream closes.
    fn detach(&mut self, conn: ConnId) {
        let Some(client) = self.clients.remove(&conn) else {
            return;
        };
        // Stop the reader before dropping fds, or a lingering read swallows
        // keystrokes meant for the user's shell.
        client.stdin_stop.store(true, Ordering::Relaxed);
        let _ = client.control.shutdown(Shutdown::Both);
    }

    fn end_session(&mut self, sid: SessionId) {
        self.sessions.remove(&sid);
        // Without this a CLI kill-session only persists if some other event
        // saves before the server exits.
        self.mark_dirty();
        if let Some(&conn) = self
            .clients
            .iter()
            .find(|(_, c)| c.attached == sid)
            .map(|(conn, _)| conn)
        {
            self.detach(conn);
        }
        let remaining = self.pinned_entries() + self.sessions.len();
        for client in self.clients.values_mut() {
            if let Some(highlight) = client.switcher.as_mut() {
                *highlight = (*highlight).min(remaining.saturating_sub(1));
            }
        }
    }

    /// The CLAUDECOM entry leads the switcher list while any agent tab
    /// exists.
    fn pinned_entries(&self) -> usize {
        self.sessions.values().any(Session::has_agent_tab) as usize
    }

    fn client_input(&mut self, conn: ConnId, bytes: Vec<u8>) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let events = client.decoder.decode(&bytes);
        self.route_input(conn, events);
    }

    fn client_input_idle(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let events = client.decoder.flush();
        self.route_input(conn, events);
    }

    fn route_input(&mut self, conn: ConnId, events: Vec<DecodedInput>) {
        for event in events {
            let Some(client) = self.clients.get_mut(&conn) else {
                return;
            };
            // The terminal's color answers apply whatever mode the client
            // is in.
            if let DecodedInput::Color(slot, rgb) = event {
                client.colors.set(slot, rgb);
                continue;
            }
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
            // Auto mode only intercepts input on its blank screen.
            if client.auto.is_some_and(|a| a.presented.is_none()) {
                self.auto_input(conn, &event);
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
                DecodedInput::Color(..) => None,
            };
            if let Some(effect) = effect {
                self.apply_effect(conn, sid, effect);
            }
        }
    }

    fn apply_effect(&mut self, conn: ConnId, sid: SessionId, effect: Effect) {
        match effect {
            Effect::Detach => self.detach(conn),
            Effect::OpenSwitcher => self.open_switcher(conn),
            Effect::OpenGrid => {
                if self.config.automode {
                    self.begin_auto(conn);
                } else if let Some(client) = self.clients.get_mut(&conn) {
                    client.grid = Some(GridState::default());
                }
            }
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
            // OSC 52 too, so an outer terminal or SSH hop sees it.
            Effect::Copy(text) => {
                if let Some(clipboard) = &mut self.clipboard {
                    let _ = clipboard.set_text(text.clone());
                }
                if let Some(client) = self.clients.get_mut(&conn) {
                    osc52_copy(&mut client.raw_out, &text);
                }
            }
            Effect::Paste => {
                let Some(text) = self.clipboard_text() else {
                    return;
                };
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.paste_text(&text);
                }
            }
            Effect::Pointer(shape) => self.set_pointer(conn, shape),
            Effect::GotoIndicator(ind) => {
                self.attach_to_tab(conn, ind.session, ind.window, ind.tab);
            }
            Effect::YankTab(id) => {
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.yank = Some(id);
                }
            }
            Effect::PasteTab => self.paste_yank(conn),
            Effect::ClearYank => {
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.yank = None;
                }
            }
            Effect::CycleAgent => self.cycle_agent(conn),
            Effect::Ended => self.end_session(sid),
        }
    }

    fn switcher_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let key = match event {
            DecodedInput::Key(key) => key,
            DecodedInput::Mouse(mouse) => {
                if mouse.kind == CtMouseKind::Moved {
                    let clickable = self.switcher_icon_at(conn, mouse.column, mouse.row)
                        || self
                            .switcher_entry_at(conn, mouse.column, mouse.row)
                            .is_some();
                    self.set_pointer(conn, if clickable { "pointer" } else { "default" });
                } else if matches!(mouse.kind, CtMouseKind::Down(CtMouseButton::Left)) {
                    if self.switcher_icon_at(conn, mouse.column, mouse.row) {
                        self.switcher_cancel(conn);
                    } else if let Some(index) =
                        self.switcher_entry_at(conn, mouse.column, mouse.row)
                    {
                        if let Some(client) = self.clients.get_mut(&conn) {
                            client.switcher = Some(index);
                        }
                        self.switcher_select(conn, index);
                    }
                }
                return;
            }
            DecodedInput::Paste(_) | DecodedInput::Color(..) => return,
        };
        let pinned = self.pinned_entries();
        let count = pinned + self.sessions.len();
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(highlight) = client.switcher else {
            return;
        };
        // The pinned entry can vanish while the switcher is open.
        let highlight = highlight.min(count.saturating_sub(1));
        let ctrl = key
            .modifiers
            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            CtKeyCode::Up | CtKeyCode::Char('k') if !ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
            CtKeyCode::Char('p') if ctrl => {
                client.switcher = Some(highlight.checked_sub(1).unwrap_or(count.saturating_sub(1)));
            }
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
            CtKeyCode::Esc => self.switcher_cancel(conn),
            CtKeyCode::Enter => self.switcher_select(conn, highlight),
            _ => {}
        }
    }

    fn switcher_cancel(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        client.switcher = None;
        let sid = client.attached;
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.request_redraw();
        }
    }

    /// Sets the pointer shape via OSC 22.
    fn set_pointer(&mut self, conn: ConnId, shape: &'static str) {
        if let Some(client) = self.clients.get_mut(&conn)
            && client.pointer != shape
        {
            client.pointer = shape;
            let _ = write!(client.raw_out, "\x1b]22;{shape}\x1b\\");
        }
    }

    fn switcher_icon_at(&self, conn: ConnId, column: u16, row: u16) -> bool {
        let Some(client) = self.clients.get(&conn) else {
            return false;
        };
        let size = term::fd_size(&client.raw_out);
        size.height > 0 && column == 0 && row == size.height - 1
    }

    fn switcher_select(&mut self, conn: ConnId, highlight: usize) {
        let pinned = self.pinned_entries();
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        client.switcher = None;
        if highlight < pinned {
            if self.config.automode {
                self.begin_auto(conn);
            } else {
                client.grid = Some(GridState::default());
            }
            return;
        }
        client.auto = None;
        let Some(&target) = self.sessions.keys().nth(highlight - pinned) else {
            return;
        };
        let current = client.attached;
        let size = term::fd_size(&client.raw_out);
        if target != current {
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

    fn switcher_entry_at(&self, conn: ConnId, column: u16, row: u16) -> Option<usize> {
        let count = self.pinned_entries() + self.sessions.len();
        let client = self.clients.get(&conn)?;
        let size = term::fd_size(&client.raw_out);
        if column >= SWITCHER_LIST_WIDTH.min(size.width) || row < 1 {
            return None;
        }
        let index = (row - 1) as usize;
        (index < count).then_some(index)
    }

    fn finder_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let key = match event {
            DecodedInput::Key(key) => key,
            DecodedInput::Paste(text) => {
                self.finder_paste(conn, text.clone());
                return;
            }
            DecodedInput::Mouse(mouse) => {
                if matches!(mouse.kind, CtMouseKind::Down(CtMouseButton::Right))
                    && let Some(text) = self.clipboard_text()
                {
                    self.finder_paste(conn, text);
                }
                return;
            }
            DecodedInput::Color(..) => return,
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
            CtKeyCode::Esc => {
                client.finder = None;
                let sid = client.attached;
                if let Some(session) = self.sessions.get_mut(&sid) {
                    session.request_redraw();
                }
            }
            // Leaves auto mode too: the user navigated away from what it
            // presented.
            CtKeyCode::Enter => {
                let Some(&idx) = matched.get(highlight) else {
                    return;
                };
                let item = &items[idx];
                let (sid, window, tab) = (item.session, item.window, item.tab);
                client.finder = None;
                client.auto = None;
                self.attach_to_tab(conn, sid, window, tab);
            }
            // The highlight follows its match through the re-narrowed list,
            // or resets to the top.
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

    fn finder_paste(&mut self, conn: ConnId, text: String) {
        let items = find::items(&self.sessions);
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(state) = client.finder.as_mut() else {
            return;
        };
        let matched = find::matches(&items, &state.query());
        let highlight = state.highlight.min(matched.len().saturating_sub(1));
        let followed = matched.get(highlight).map(|&i| items[i].id);
        state.textarea.insert_str(input::prompt_paste(&text));
        let matched = find::matches(&items, &state.query());
        state.highlight = followed
            .and_then(|id| matched.iter().position(|&i| items[i].id == id))
            .unwrap_or(0);
    }

    /// The CLAUDECOM grid's session order.
    fn sessions_by_name(&self) -> Vec<SessionId> {
        let mut by_name: Vec<(&str, SessionId)> = self
            .sessions
            .iter()
            .map(|(&sid, s)| (s.name.as_str(), sid))
            .collect();
        by_name.sort();
        by_name.into_iter().map(|(_, sid)| sid).collect()
    }

    fn locate(&self, id: TabId) -> Option<(SessionId, layout::WindowId, usize)> {
        self.sessions
            .iter()
            .find_map(|(&sid, s)| s.locate_tab(id).map(|(window, index)| (sid, window, index)))
    }

    /// A tab's position in the CLAUDECOM grid's session/window/tab order.
    fn order_key(&self, id: TabId) -> Option<(usize, usize, usize)> {
        let (sid, window, index) = self.locate(id)?;
        let spos = self.sessions_by_name().iter().position(|&s| s == sid)?;
        let order = self.sessions[&sid].window_order();
        let wpos = order.iter().position(|&w| w == window).unwrap_or(0);
        Some((spos, wpos, index))
    }

    /// The first attention tab after `cursor` in grid order, wrapping.
    fn next_attention(
        &self,
        cursor: Option<(usize, usize, usize)>,
    ) -> Option<(SessionId, layout::WindowId, usize)> {
        let mut queue = Vec::new();
        for (spos, &sid) in self.sessions_by_name().iter().enumerate() {
            let session = &self.sessions[&sid];
            let order = session.window_order();
            for (window, index) in session.attention_tabs() {
                let wpos = order.iter().position(|&w| w == window).unwrap_or(0);
                queue.push(((spos, wpos, index), sid, window, index));
            }
        }
        queue
            .iter()
            .find(|&&(key, ..)| Some(key) > cursor)
            .or_else(|| queue.first())
            .map(|&(_, sid, window, index)| (sid, window, index))
    }

    fn cycle_agent(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let attached = client.attached;
        let cursor = self.sessions.get(&attached).and_then(|session| {
            let spos = self
                .sessions_by_name()
                .iter()
                .position(|&sid| sid == attached)?;
            let (window, index) = session.focused_active();
            let order = session.window_order();
            let wpos = order.iter().position(|&w| w == window).unwrap_or(0);
            Some((spos, wpos, index))
        });
        if let Some((sid, window, index)) = self.next_attention(cursor) {
            self.attach_to_tab(conn, sid, window, index);
        }
    }

    fn paste_yank(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let Some(id) = client.yank else {
            return;
        };
        let dest_sid = client.attached;
        let Some(dest_focus) = self.sessions.get(&dest_sid).map(|s| s.focused_active().0) else {
            return;
        };
        if let Some(client) = self.clients.get_mut(&conn) {
            client.yank = None;
        }
        let Some((src_sid, src_window, _)) = self.locate(id) else {
            return;
        };
        if src_sid == dest_sid && src_window == dest_focus {
            return;
        }
        let Some((tab, ended)) = self
            .sessions
            .get_mut(&src_sid)
            .and_then(|s| s.extract_tab(id))
        else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&dest_sid) {
            session.insert_tab(tab);
        }
        if ended {
            self.end_session(src_sid);
        }
    }

    fn begin_auto(&mut self, conn: ConnId) {
        if let Some(client) = self.clients.get_mut(&conn) {
            client.grid = None;
            client.switcher = None;
            client.finder = None;
            client.auto = Some(AutoState::default());
        }
    }

    /// Moves each auto-mode client on once its presented tab is gone or
    /// its agent is working again.
    fn tick_auto(&mut self) {
        let conns: Vec<ConnId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.auto.is_some() && c.switcher.is_none() && c.finder.is_none())
            .map(|(&conn, _)| conn)
            .collect();
        for conn in conns {
            let Some(state) = self.clients.get(&conn).and_then(|c| c.auto) else {
                continue;
            };
            if let Some(id) = state.presented {
                let keep = self.locate(id).is_some_and(|(sid, window, index)| {
                    self.sessions[&sid]
                        .tab_at(window, index)
                        .and_then(|t| t.agent.as_ref())
                        .is_none_or(|t| !t.working())
                });
                if keep {
                    continue;
                }
            }
            let cursor = state.presented.and_then(|id| self.order_key(id));
            match self.next_attention(cursor) {
                Some((sid, window, index)) => self.attach_to_tab(conn, sid, window, index),
                None => {
                    if let Some(state) = self.clients.get_mut(&conn).and_then(|c| c.auto.as_mut()) {
                        state.presented = None;
                    }
                }
            }
        }
    }

    fn auto_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let Some(mut state) = self.clients.get(&conn).and_then(|c| c.auto) else {
            return;
        };
        let DecodedInput::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if state.pending_prefix {
            state.pending_prefix = false;
            self.store_auto_state(conn, state);
            if plain_char(key, 's') {
                self.open_switcher(conn);
            } else if plain_char(key, 'f') {
                self.open_finder(conn);
            }
            return;
        }
        if self.config.keys.is_prefix(*key) {
            state.pending_prefix = true;
            self.store_auto_state(conn, state);
            return;
        }
        if let CtKeyCode::Esc | CtKeyCode::Char('q') = key.code {
            let Some(client) = self.clients.get_mut(&conn) else {
                return;
            };
            client.auto = None;
            let sid = client.attached;
            if let Some(session) = self.sessions.get_mut(&sid) {
                session.request_redraw();
            }
        }
    }

    fn store_auto_state(&mut self, conn: ConnId, state: AutoState) {
        if let Some(auto) = self.clients.get_mut(&conn).and_then(|c| c.auto.as_mut()) {
            *auto = state;
        }
    }

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
            // Set the area first: restoring a minimized window checks
            // minimum sizes against it.
            session.set_area(Rect::new(0, 0, size.width, size.height));
            session.goto_tab(window, index);
            session.request_redraw();
        }
        // In auto mode the landing tab becomes the presented one, so the
        // next hand-off advances from it.
        let id = self
            .sessions
            .get(&sid)
            .and_then(|s| s.tab_at(window, index))
            .map(|t| t.id);
        if let Some(state) = self.clients.get_mut(&conn).and_then(|c| c.auto.as_mut()) {
            state.presented = id;
        }
    }

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

    /// Snapshots the attached session as a backdrop, so the finder's
    /// preview is the only view resizing tabs while it is open.
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
            client.auto = None;
        }
        self.note_attached(sid);
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.request_redraw();
        }
    }

    fn grid_input(&mut self, conn: ConnId, event: &DecodedInput) {
        let Some(mut state) = self.clients.get(&conn).and_then(|c| c.grid) else {
            return;
        };
        let items = grid::items(&self.sessions);
        // A captured tab that left the grid ends capture, and the event
        // falls through to navigation.
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
        if self.config.keys.is_prefix(*key) {
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
                if self.config.keys.is_prefix(*key) {
                    state.pending_prefix = true;
                    return None;
                }
                session.key_to_tab(item.window, item.tab, *key);
            }
            DecodedInput::Paste(text) => session.paste_to_tab(item.window, item.tab, text),
            DecodedInput::Mouse(_) | DecodedInput::Color(..) => {}
        }
        None
    }

    fn store_grid_state(&mut self, conn: ConnId, state: GridState) {
        if let Some(grid) = self.clients.get_mut(&conn).and_then(|c| c.grid.as_mut()) {
            *grid = state;
        }
    }

    /// The first attention tab in grid order, skipping the one in view
    /// (its own tab bar already shows its status).
    fn pending_indicator(&self, attached: SessionId) -> Option<session::Indicator> {
        let looking_at = self.sessions.get(&attached).map(Session::focused_active);
        let mut by_name: Vec<(&str, SessionId)> = self
            .sessions
            .iter()
            .map(|(&sid, s)| (s.name.as_str(), sid))
            .collect();
        by_name.sort();
        let now = std::time::Instant::now();
        for (_, sid) in by_name {
            let session = &self.sessions[&sid];
            for (window, index) in session.attention_tabs() {
                if sid == attached && looking_at == Some((window, index)) {
                    continue;
                }
                let Some(tab) = session.tab_at(window, index) else {
                    continue;
                };
                let Some(visual) = tab.agent.as_ref().map(|t| t.visual(now)) else {
                    continue;
                };
                return Some(session::Indicator {
                    session: sid,
                    window,
                    tab: index,
                    text: format!("{} {}", tab.name, visual.text),
                });
            }
        }
        None
    }

    /// Full-screen modes redraw every pass. Attached sessions redraw only
    /// when they changed.
    fn render_all(&mut self) {
        // The indicator spans sessions, so compute it here and hand it to
        // the session to render.
        let indicators: Vec<(SessionId, session::Indicator)> = self
            .clients
            .values()
            .filter(|c| {
                c.finder.is_none()
                    && c.grid.is_none()
                    && c.switcher.is_none()
                    && !c.auto.is_some_and(|a| a.presented.is_none())
            })
            .filter_map(|c| Some((c.attached, self.pending_indicator(c.attached)?)))
            .collect();
        for (&sid, session) in self.sessions.iter_mut() {
            let indicator = indicators
                .iter()
                .find(|(s, _)| *s == sid)
                .map(|(_, ind)| ind.clone());
            session.set_indicator(indicator);
        }
        // Yanks are client state, so hand each session the ones pointing
        // at its tabs.
        let yanks: Vec<TabId> = self.clients.values().filter_map(|c| c.yank).collect();
        for session in self.sessions.values_mut() {
            let held = yanks
                .iter()
                .copied()
                .filter(|&id| session.has_tab(id))
                .collect();
            session.set_yanked(held);
        }
        // A session darkens against what its client's terminal answered.
        for client in self.clients.values() {
            if let Some(session) = self.sessions.get_mut(&client.attached) {
                session.set_terminal_colors(client.colors);
            }
        }
        let Server {
            sessions,
            clients,
            config,
            ..
        } = self;
        for client in clients.values_mut() {
            if client.finder.is_some() {
                render_finder(client, sessions, config);
            } else if client.grid.is_some() {
                render_grid(client, sessions, &config.palette);
            } else if let Some(highlight) = client.switcher {
                render_switcher(client, sessions, highlight, config);
            } else if client.auto.is_some_and(|a| a.presented.is_none()) {
                render_auto_blank(client, sessions, &config.palette);
            } else if let Some(session) = sessions.get_mut(&client.attached)
                && session.needs_redraw()
            {
                let _ = session.draw_frame(&mut client.terminal);
            }
        }
    }
}

fn render_finder(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    config: &Config,
) {
    let Client {
        finder,
        terminal,
        colors,
        ..
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
        find::render(buf, area, sessions, state, config, colors);
    });
}

fn render_auto_blank(
    client: &mut Client,
    sessions: &BTreeMap<SessionId, Session>,
    palette: &palette::Palette,
) {
    let _ = client.terminal.draw(|frame| {
        let area = frame.area();
        auto::render_blank(frame.buffer_mut(), area, sessions, palette);
    });
}

fn render_grid(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    palette: &palette::Palette,
) {
    let Client { grid, terminal, .. } = client;
    let Some(state) = grid.as_mut() else {
        return;
    };
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        grid::render(frame.buffer_mut(), area, sessions, state, palette);
    });
}

const SWITCHER_LIST_WIDTH: u16 = 28;

fn render_switcher(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    highlight: usize,
    config: &Config,
) {
    let palette = &config.palette;
    let pinned = sessions.values().any(Session::has_agent_tab) as usize;
    let mut entries: Vec<(String, Option<agent::Urgency>)> =
        Vec::with_capacity(pinned + sessions.len());
    if pinned > 0 {
        entries.push((grid::ENTRY_NAME.to_string(), None));
    }
    entries.extend(sessions.values().map(|s| {
        let name = format!("{} ({} windows)", s.name, s.window_count());
        (name, s.urgency())
    }));
    let highlight = highlight.min(entries.len().saturating_sub(1));
    let highlighted_sid = highlight
        .checked_sub(pinned)
        .and_then(|i| sessions.keys().nth(i).copied());
    let elapsed = anim::elapsed();
    let colors = client.colors;
    let _ = client.terminal.draw(|frame| {
        let area = frame.area();
        let buf = frame.buffer_mut();
        clear_region(buf, area);
        let list_w = SWITCHER_LIST_WIDTH.min(area.width);
        for (i, (name, urgency)) in entries.iter().enumerate() {
            let y = area.y + 1 + i as u16;
            if y >= area.bottom() {
                break;
            }
            let (base, modifier) = if i == highlight {
                (palette.accent, Modifier::REVERSED)
            } else {
                (palette.text, Modifier::empty())
            };
            let (color, anim) = urgency.map_or((base, Anim::None), |urgency| {
                let (status, anim) = urgency.visual();
                (palette.status(status), anim)
            });
            let text = format!(" {name} ");
            let len = text.chars().count();
            for (j, ch) in text.chars().enumerate() {
                let x = area.x + j as u16;
                if x >= area.x + list_w {
                    break;
                }
                let fg = match anim {
                    Anim::None => color,
                    Anim::Shimmer => anim::shimmer(color, j, len, elapsed),
                    Anim::Breathe => anim::breathe(color, elapsed),
                };
                if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                    dst.set_char(ch);
                    dst.set_style(Style::default().fg(fg).add_modifier(modifier));
                }
            }
        }
        // The menu icon, which exits on click.
        if area.height > 0
            && let Some(dst) = buf.cell_mut(Position::new(area.x, area.bottom() - 1))
        {
            dst.set_char('○');
            dst.set_style(Style::default().fg(palette.accent));
        }
        if area.width > list_w {
            for y in area.top()..area.bottom() {
                if let Some(dst) = buf.cell_mut(Position::new(area.x + list_w, y)) {
                    dst.set_symbol("│");
                    dst.set_style(Style::default().fg(palette.dim));
                }
            }
            let preview = Rect {
                x: area.x + list_w + 1,
                width: area.width - list_w - 1,
                ..area
            };
            if pinned > 0 && highlight == 0 {
                grid::render_preview(buf, preview, sessions, palette);
            } else if let Some(session) = highlighted_sid.and_then(|sid| sessions.get_mut(&sid)) {
                session.render_preview(buf, preview);
            }
        }
        // The list and its divider are the panel; the shadow falls on the
        // preview.
        if config.shadows {
            let panel = Rect {
                width: (list_w + 1).min(area.width),
                ..area
            };
            palette::shadow(buf, panel, area, palette, &colors);
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

/// Polls with a short timeout so `stop` ends the thread promptly. A
/// blocked read would race the user's shell for keystrokes typed after
/// detach.
fn spawn_stdin_reader(
    conn: ConnId,
    stdin: OwnedFd,
    stop: Arc<AtomicBool>,
    tx: Sender<ServerEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // The first timeout after a burst signals idle exactly once.
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

fn plain_char(key: &KeyEvent, ch: char) -> bool {
    KeyMatch::from_event(*key)
        == KeyMatch {
            code: CtKeyCode::Char(ch),
            ctrl: false,
            shift: false,
        }
}

/// Asks the terminal for its default and ANSI colors. The answers arrive
/// as input.
fn query_colors(out: &mut File) {
    let mut seq = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for i in 0..16 {
        seq.push_str(&format!("\x1b]4;{i};?\x1b\\"));
    }
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

fn osc52_copy(out: &mut File, text: &str) {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let _ = write!(out, "\x1b]52;c;{encoded}\x07");
    let _ = out.flush();
}
