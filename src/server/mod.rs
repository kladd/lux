//! The lux server: owns every session, decodes client input, and renders
//! to each attached client's descriptors.

pub mod agent;
pub mod anim;
pub mod auto;
pub mod config;
pub mod ex;
pub mod find;
pub mod grid;
pub mod host;
pub mod input;
pub mod keys;
pub mod layout;
pub mod palette;
pub mod persist;
pub mod search;
pub mod serve;
pub mod session;
pub mod term;
pub mod transition;
pub mod window;
pub mod wire;

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
    MouseButton as CtMouseButton, MouseEvent as CtMouseEvent, MouseEventKind as CtMouseKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use ratatui_textarea::TextArea;

use crate::protocol::{self, Request};
use anim::Anim;
use auto::AutoState;
use config::Config;
use grid::GridState;
use host::{Host, HostId};
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
    /// A hub reaching this host through `lux proxy`.
    HubAttach {
        conn: ConnId,
        stream: UnixStream,
    },
    HubMsg(ConnId, wire::HubMsg),
    HubGone(ConnId),
    /// From the ssh connection to an adopted host.
    Host {
        host: HostId,
        generation: u64,
        event: host::HostEvent,
    },
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
    /// The switcher's new-session name prompt while it is open.
    new_session: Option<TextArea<'static>>,
    /// Where a session named at that prompt should run, while the choice
    /// is open.
    host_choice: Option<HostChoice>,
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

struct HostChoice {
    name: Option<String>,
    /// `None` is this host.
    hosts: Vec<Option<HostId>>,
    highlight: usize,
    /// Escape returns to the session rather than the switcher.
    from_command_line: bool,
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
        hosts: BTreeMap::new(),
        next_host_id: 0,
        pending_attaches: HashMap::new(),
        hub: None,
        pending_hubs: Vec::new(),
        instance: instance_id(),
    };
    if config.restore
        && let Some(snapshot) = persist::load()
    {
        server.restore_sessions(&snapshot);
    }
    loop {
        let timeout = if server.needs_frame_tick() {
            FRAME
        } else if server.needs_timed_tick() {
            TICK
        } else {
            // Wake at the next minute so the status line clock advances.
            until_next_minute()
        };
        let event = match rx.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return 0,
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
        server.tick_hosts();
        server.sync_layouts();
        server.sync_host_colors();
        server.pump_hub();
        server.tick_save();
        server.render_all();
    }
}

/// Tells this server apart from others, so a hub never adopts itself.
fn instance_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(std::process::id()) << 32)
}

const SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);
const TICK: std::time::Duration = std::time::Duration::from_millis(60);
const FRAME: std::time::Duration = std::time::Duration::from_millis(16);

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
        Request::Proxy => {
            let Ok(reader) = stream.try_clone() else {
                return;
            };
            let mut ack = &stream;
            if ack.write_all(b"ok\n").is_err() {
                return;
            }
            if tx.send(ServerEvent::HubAttach { conn, stream }).is_err() {
                return;
            }
            let mut reader = std::io::BufReader::with_capacity(1 << 16, reader);
            while let Ok(Some(msg)) = wire::read_frame(&mut reader) {
                if tx.send(ServerEvent::HubMsg(conn, msg)).is_err() {
                    return;
                }
            }
            let _ = tx.send(ServerEvent::HubGone(conn));
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
    /// Hosts whose sessions this server adopted.
    hosts: BTreeMap<HostId, Host>,
    next_host_id: HostId,
    /// CLI attaches to a remote session, held until its host answers.
    pending_attaches: HashMap<ConnId, host::PendingAttach>,
    /// The hub that adopted this host's sessions, or one mid-handshake.
    hub: Option<serve::Hub>,
    /// Hubs mid-handshake while another holds this host.
    pending_hubs: Vec<serve::Hub>,
    instance: u64,
}

impl Server {
    fn has_pending_idle(&self) -> bool {
        self.sessions.values().any(|s| s.has_pending_idle())
    }

    fn any_shown(&self, pred: impl Fn(&Session) -> bool) -> bool {
        self.clients.values().any(|c| {
            // The switcher, sidebar, grid, and blank auto screen show every
            // session.
            if c.switcher.is_some()
                || self.config.sidebar
                || c.grid.is_some()
                || c.auto.is_some_and(|a| a.presented.is_none())
            {
                self.sessions.values().any(&pred)
            } else {
                self.sessions.get(&c.attached).is_some_and(&pred)
            }
        })
    }

    fn needs_timed_tick(&self) -> bool {
        self.has_pending_idle()
            || self.save_deadline.is_some()
            || self.hosts_pending()
            || self.adopted()
            || self.sessions.values().any(|s| s.has_pending_repeat())
            || self.any_shown(Session::has_animation)
    }

    fn needs_frame_tick(&self) -> bool {
        self.any_shown(Session::has_transition)
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
            self.notify(&session, &notice);
        }
    }

    /// An adopting hub raises a served tab's notification in place of
    /// this host.
    fn notify(&mut self, session: &str, notice: &window::Notice) {
        if !self.hub_notice(notice) {
            self.raise_notification(session, notice);
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
        // Adopted sessions belong to their hosts.
        let snapshot = persist::StateSnapshot {
            sessions: self
                .sessions
                .values_mut()
                .filter(|s| !s.is_remote())
                .map(Session::snapshot)
                .collect(),
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
            | ServerEvent::InputIdle(_)
            | ServerEvent::HubMsg(..) => self.mark_dirty(),
            ServerEvent::Ls(_)
            | ServerEvent::Kill(_)
            | ServerEvent::KillSession(..)
            | ServerEvent::Resized(_)
            | ServerEvent::ConnGone(_)
            | ServerEvent::ProgramCopy(..)
            | ServerEvent::Shutdown
            | ServerEvent::HubAttach { .. }
            | ServerEvent::HubGone(_)
            | ServerEvent::Host { .. } => {}
        }
        match event {
            ServerEvent::PtyOutput(tab, bytes) => {
                if let Some(session) = self.sessions.values_mut().find(|s| s.has_tab(tab)) {
                    let notice = session.pty_output(tab, &bytes);
                    let name = session.name.clone();
                    if let Some(notice) = notice {
                        self.notify(&name, &notice);
                    }
                }
            }
            ServerEvent::PtyExited(tab) => {
                self.tab_exited(tab);
                self.hub_tab_exited(tab);
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
                    let line = match &session.host {
                        None => session.name.clone(),
                        Some(h) => match self.hosts.get(&h.id).filter(|h| h.online()) {
                            Some(host) => format!("{}@{}", session.name, host.alias),
                            None => continue,
                        },
                    };
                    let _ = protocol::write_line(&mut stream, &line);
                }
            }
            ServerEvent::Kill(mut stream) => {
                // Save first so the killed sessions restore on the next start.
                self.save_sessions();
                self.drop_hosts();
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
                    session.set_area(session_area(&self.config, size));
                }
            }
            ServerEvent::ConnGone(conn) => {
                self.pending_attaches.remove(&conn);
                self.detach(conn);
            }
            ServerEvent::Input(conn, bytes) => self.client_input(conn, bytes),
            ServerEvent::InputIdle(conn) => self.client_input_idle(conn),
            ServerEvent::ProgramCopy(tab, text) => {
                if !self.hub_clipboard(tab, &text) {
                    self.program_copy(tab, text);
                }
            }
            ServerEvent::HubAttach { conn, stream } => self.hub_attach(conn, stream),
            ServerEvent::HubMsg(conn, msg) => self.hub_msg(conn, msg),
            ServerEvent::HubGone(conn) => self.hub_gone(conn),
            ServerEvent::Host {
                host,
                generation,
                event,
            } => self.host_event(host, generation, event),
            ServerEvent::Shutdown => {
                self.save_sessions();
                self.drop_hosts();
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
        let area = session_area(&self.config, term::fd_size(&stdout_file));

        if self.adopted() {
            let _ =
                protocol::write_line(&mut stream, "err this host's sessions are adopted by a hub");
            return;
        }
        let sid = match request {
            Request::New => self.create_session(None, area),
            Request::Session(name) if name.contains('@') => {
                self.attach_remote(conn, stream, &name, stdin, stdout_file);
                return;
            }
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
        self.install_client(conn, stream, sid, stdin, stdout_file);
    }

    /// Acknowledges the attach and hands the client's terminal to `sid`.
    fn install_client(
        &mut self,
        conn: ConnId,
        mut stream: UnixStream,
        sid: SessionId,
        stdin: OwnedFd,
        stdout_file: File,
    ) {
        if protocol::write_line(&mut stream, "ok").is_err() {
            return;
        }
        let size = term::fd_size(&stdout_file);
        let area = session_area(&self.config, size);

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
            session.materialize();
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
                new_session: None,
                host_choice: None,
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

    /// A remote session counts only while its host is connected; past
    /// that, the most recent local one.
    fn recent_session(&self) -> Option<SessionId> {
        let mut order = self
            .attach_order
            .iter()
            .rev()
            .copied()
            .filter(|sid| self.sessions.contains_key(sid));
        let latest = order.next()?;
        if self.reachable(&self.sessions[&latest]) {
            return Some(latest);
        }
        order.find(|sid| !self.sessions[sid].is_remote())
    }

    fn reachable(&self, session: &Session) -> bool {
        session
            .host
            .as_ref()
            .is_none_or(|h| self.hosts.get(&h.id).is_some_and(Host::online))
    }

    /// Only this host's own sessions go by bare name.
    fn session_by_name(&self, name: &str) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|(_, s)| !s.is_remote() && s.name == name)
            .map(|(&sid, _)| sid)
    }

    /// A bare name or `name@alias`.
    fn session_by_address(&self, address: &str) -> Option<SessionId> {
        match host::split_address(address) {
            (name, None) => self.session_by_name(name),
            (name, Some(alias)) => self.remote_session_by_name(self.host_by_alias(alias)?, name),
        }
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

    /// Ends a session, on its host too when it runs on one.
    fn end_session(&mut self, sid: SessionId) {
        let Some(session) = self.sessions.get(&sid) else {
            return;
        };
        match &session.host {
            Some(host) => {
                let (link, remote) = (host.link.clone(), host.remote);
                // A tab pasted out of the session must reach its new
                // layout before the host drops the session.
                self.sync_layouts();
                link.send(wire::HubMsg::KillSession(remote));
                self.remove_session(sid);
            }
            None => {
                self.remove_session(sid);
                self.hub_session_ended(sid);
            }
        }
    }

    /// Forgets a session and detaches its client, leaving any host alone.
    fn remove_session(&mut self, sid: SessionId) {
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
        let remaining = self.sessions.len();
        for client in self.clients.values_mut() {
            if let Some(highlight) = client.switcher.as_mut() {
                *highlight = (*highlight).min(remaining.saturating_sub(1));
            }
        }
    }

    fn tab_exited(&mut self, tab: TabId) {
        let Some((&sid, session)) = self.sessions.iter_mut().find(|(_, s)| s.has_tab(tab)) else {
            return;
        };
        if let Some(Effect::Ended) = session.pty_exited(tab) {
            self.end_session(sid);
        }
    }

    fn program_copy(&mut self, tab: TabId, text: String) {
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

    /// Shows a status message to one client.
    fn tell(&mut self, conn: ConnId, text: String) {
        let Some(sid) = self.clients.get(&conn).map(|c| c.attached) else {
            return;
        };
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.show_message(text);
        }
    }

    /// Shows a status message to every client.
    fn tell_all(&mut self, text: String) {
        let sids: Vec<SessionId> = self.clients.values().map(|c| c.attached).collect();
        for sid in sids {
            if let Some(session) = self.sessions.get_mut(&sid) {
                session.show_message(text.clone());
            }
        }
    }

    /// Attaches a client to another session, taking it from any client
    /// already there.
    fn switch_client(&mut self, conn: ConnId, target: SessionId) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let size = term::fd_size(&client.raw_out);
        if client.attached != target
            && let Some(other) = self
                .clients
                .iter()
                .find(|(c, cl)| **c != conn && cl.attached == target)
                .map(|(conn, _)| *conn)
        {
            self.detach(other);
        }
        if let Some(client) = self.clients.get_mut(&conn) {
            client.attached = target;
            client.switcher = None;
            client.new_session = None;
            client.host_choice = None;
            client.auto = None;
        }
        self.note_attached(target);
        if let Some(session) = self.sessions.get_mut(&target) {
            session.set_area(session_area(&self.config, size));
            session.request_redraw();
        }
    }

    /// Sizes each client's session to what its terminal leaves it.
    fn fit_sessions(&mut self) {
        for client in self.clients.values() {
            let size = term::fd_size(&client.raw_out);
            if let Some(session) = self.sessions.get_mut(&client.attached) {
                session.set_area(session_area(&self.config, size));
            }
        }
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
            if let DecodedInput::Mouse(mouse) = &event
                && self.sidebar_mouse(conn, mouse)
            {
                continue;
            }
            let Some(client) = self.clients.get(&conn) else {
                return;
            };
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
            Effect::NewSession(name) => self.submit_new_session(conn, name, true),
            Effect::RenameSession(name) => {
                if name.contains('@') {
                    self.tell(conn, AT_IN_NAME.into());
                    return;
                }
                if let Some(session) = self.sessions.get_mut(&sid) {
                    if let Some(host) = &session.host {
                        host.link.send(wire::HubMsg::RenameSession {
                            session: host.remote,
                            name: name.clone(),
                        });
                    }
                    session.name = name;
                    session.request_redraw();
                }
            }
            Effect::KillSession(name) => {
                let target = match name {
                    Some(n) => self.session_by_address(&n),
                    None => Some(sid),
                };
                if let Some(target_sid) = target {
                    self.end_session(target_sid);
                }
            }
            Effect::Connect(alias) => self.connect(conn, alias),
            Effect::Disconnect(alias) => self.disconnect(conn, alias),
            Effect::ReloadConfig => self.apply_config(sid, config::reload()),
            Effect::SetConfig(key, value) => self.apply_config(sid, config::set(&key, &value)),
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

    /// Presses and hovers over an unfocused sidebar. Drags and releases
    /// pass through, so one begun in the layout can finish there.
    fn sidebar_mouse(
        &mut self,
        conn: ConnId,
        mouse: &ratatui::crossterm::event::MouseEvent,
    ) -> bool {
        let Some(client) = self.clients.get(&conn) else {
            return false;
        };
        if mouse.column >= sidebar_width(&self.config, term::fd_size(&client.raw_out)) {
            return false;
        }
        let entry = self.switcher_entry_at(conn, mouse.column, mouse.row);
        match mouse.kind {
            CtMouseKind::Drag(_) | CtMouseKind::Up(_) => return false,
            CtMouseKind::Moved => {
                self.set_pointer(
                    conn,
                    if entry.is_some() {
                        "pointer"
                    } else {
                        "default"
                    },
                );
            }
            CtMouseKind::Down(CtMouseButton::Left) => {
                if let Some(index) = entry {
                    self.switcher_select(conn, index);
                }
            }
            _ => {}
        }
        true
    }

    /// On error every session keeps the config it has.
    fn apply_config(&mut self, sid: SessionId, config: Result<Config, String>) {
        let config = match config {
            Ok(config) => config,
            Err(err) => {
                eprintln!("lux: {err}");
                return;
            }
        };
        self.config = Arc::new(config);
        for session in self.sessions.values_mut() {
            session.set_config(self.config.clone());
        }
        // The sidebar may have come or gone.
        self.fit_sessions();
        if let Some(session) = self.sessions.get_mut(&sid) {
            session.show_message("config reloaded".into());
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
            DecodedInput::Paste(text) => {
                if let Some(prompt) = self
                    .clients
                    .get_mut(&conn)
                    .and_then(|c| c.new_session.as_mut())
                {
                    prompt.insert_str(input::prompt_paste(text));
                }
                return;
            }
            DecodedInput::Color(..) => return,
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self
            .clients
            .get(&conn)
            .is_some_and(|c| c.host_choice.is_some())
        {
            self.host_choice_input(conn, key);
            return;
        }
        if self
            .clients
            .get(&conn)
            .is_some_and(|c| c.new_session.is_some())
        {
            self.new_session_prompt_input(conn, key);
            return;
        }
        let count = self.sessions.len();
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(highlight) = client.switcher else {
            return;
        };
        // Sessions can end while the switcher is open.
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
            CtKeyCode::Char('n') if !ctrl => {
                let mut prompt = TextArea::default();
                // The default cursor-line underline looks like stray chrome
                // in a one-line input.
                prompt.set_cursor_line_style(Style::default());
                client.new_session = Some(prompt);
            }
            CtKeyCode::Esc => self.switcher_cancel(conn),
            CtKeyCode::Enter => self.switcher_select(conn, highlight),
            _ => {}
        }
    }

    /// Enter creates and attaches unless the name is taken; Escape and a
    /// taken name both return to the switcher.
    fn new_session_prompt_input(&mut self, conn: ConnId, key: &KeyEvent) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let Some(prompt) = client.new_session.as_mut() else {
            return;
        };
        match key.code {
            CtKeyCode::Esc => client.new_session = None,
            CtKeyCode::Enter => {
                let text = prompt.lines().first().cloned().unwrap_or_default();
                client.new_session = None;
                let name = (!text.is_empty()).then_some(text);
                self.submit_new_session(conn, name, false);
            }
            _ => {
                prompt.input(ratatui_textarea::Input::from(*key));
            }
        }
    }

    /// Asks which host to run on while any is online, unless the name is
    /// already an address; otherwise creates here unless the name is taken.
    fn submit_new_session(&mut self, conn: ConnId, name: Option<String>, from_command_line: bool) {
        let hosts: Vec<Option<HostId>> = std::iter::once(None)
            .chain(
                self.hosts
                    .iter()
                    .filter(|(_, h)| h.online())
                    .map(|(&id, _)| Some(id)),
            )
            .collect();
        if hosts.len() > 1 && !name.as_deref().is_some_and(|n| n.contains('@')) {
            // The choice renders over the switcher.
            if from_command_line {
                self.open_switcher(conn);
            }
            if let Some(client) = self.clients.get_mut(&conn) {
                client.host_choice = Some(HostChoice {
                    name,
                    hosts,
                    highlight: 0,
                    from_command_line,
                });
            }
            return;
        }
        if name
            .as_deref()
            .is_some_and(|n| self.session_by_address(n).is_some())
        {
            return;
        }
        self.new_session_for(conn, name);
    }

    /// Enter creates the session on the highlighted host; Escape returns
    /// to where the choice was opened from.
    fn host_choice_input(&mut self, conn: ConnId, key: &KeyEvent) {
        let Some(choice) = self
            .clients
            .get_mut(&conn)
            .and_then(|c| c.host_choice.as_mut())
        else {
            return;
        };
        let count = choice.hosts.len();
        match key.code {
            CtKeyCode::Esc if choice.from_command_line => self.switcher_cancel(conn),
            CtKeyCode::Esc => {
                if let Some(client) = self.clients.get_mut(&conn) {
                    client.host_choice = None;
                }
            }
            CtKeyCode::Left | CtKeyCode::Up | CtKeyCode::BackTab | CtKeyCode::Char('h' | 'k') => {
                choice.highlight = choice.highlight.checked_sub(1).unwrap_or(count - 1);
            }
            CtKeyCode::Right | CtKeyCode::Down | CtKeyCode::Tab | CtKeyCode::Char('l' | 'j') => {
                choice.highlight = (choice.highlight + 1) % count;
            }
            CtKeyCode::Enter => {
                let Some(choice) = self
                    .clients
                    .get_mut(&conn)
                    .and_then(|c| c.host_choice.take())
                else {
                    return;
                };
                if choice.from_command_line {
                    self.switcher_cancel(conn);
                }
                match choice.hosts[choice.highlight] {
                    None => {
                        if choice
                            .name
                            .as_deref()
                            .is_some_and(|n| self.session_by_name(n).is_some())
                        {
                            return;
                        }
                        self.new_session_for(conn, choice.name);
                    }
                    Some(host) => self.new_remote_session(conn, host, choice.name),
                }
            }
            _ => {}
        }
    }

    fn switcher_cancel(&mut self, conn: ConnId) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        client.switcher = None;
        client.new_session = None;
        client.host_choice = None;
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
        // Beside the sidebar, the session's own status line icon.
        let x = sidebar_width(&self.config, size);
        size.height > 0 && column == x && row == size.height - 1
    }

    fn switcher_select(&mut self, conn: ConnId, highlight: usize) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        client.switcher = None;
        client.new_session = None;
        client.auto = None;
        let Some(&target) = switcher_order(&self.sessions).get(highlight) else {
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
            session.set_area(session_area(&self.config, size));
            session.request_redraw();
        }
    }

    fn switcher_entry_at(&self, conn: ConnId, column: u16, row: u16) -> Option<usize> {
        let client = self.clients.get(&conn)?;
        let size = term::fd_size(&client.raw_out);
        if column >= SWITCHER_LIST_WIDTH.min(size.width) || row < 1 {
            return None;
        }
        let rows = switcher_rows(&self.sessions, &self.hosts);
        session_at_row(&rows, (row - 1) as usize)
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
        let items = find::items(&self.sessions, &self.hosts);
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
                state.textarea.input(ratatui_textarea::Input::from(*key));
                let matched = find::matches(&items, &state.query());
                state.highlight = followed
                    .and_then(|id| matched.iter().position(|&i| items[i].id == id))
                    .unwrap_or(0);
            }
        }
    }

    fn finder_paste(&mut self, conn: ConnId, text: String) {
        let items = find::items(&self.sessions, &self.hosts);
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
    /// Auto mode leaves out remote sessions.
    fn next_attention(
        &self,
        cursor: Option<(usize, usize, usize)>,
        remote: bool,
    ) -> Option<(SessionId, layout::WindowId, usize)> {
        let mut queue = Vec::new();
        for (spos, &sid) in self.sessions_by_name().iter().enumerate() {
            let session = &self.sessions[&sid];
            if session.is_remote() && !remote {
                continue;
            }
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
        if let Some((sid, window, index)) = self.next_attention(cursor, true) {
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
        let host_of = |sid: SessionId| {
            self.sessions
                .get(&sid)
                .and_then(|s| s.host.as_ref())
                .map(|h| h.id)
        };
        // A running PTY can't move to another machine.
        if let Some((src_sid, ..)) = self.locate(id)
            && host_of(src_sid) != host_of(dest_sid)
        {
            self.tell(
                conn,
                "a tab can only move between sessions on the same host".into(),
            );
            return;
        }
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
            client.new_session = None;
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
                        .is_none_or(|t| !t.busy())
                });
                if keep {
                    continue;
                }
            }
            let cursor = state.presented.and_then(|id| self.order_key(id));
            match self.next_attention(cursor, false) {
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
            session.set_area(session_area(&self.config, size));
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
        let highlight = switcher_order(&self.sessions)
            .iter()
            .position(|&id| id == sid)
            .unwrap_or(0);
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

    /// `name@alias` creates the session on that host.
    fn new_session_for(&mut self, conn: ConnId, name: Option<String>) {
        if let Some((local, Some(alias))) = name.as_deref().map(host::split_address) {
            let local = (!local.is_empty()).then(|| local.to_string());
            match self.host_by_alias(alias) {
                Some(host) => self.new_remote_session(conn, host, local),
                None => self.tell(conn, format!("not connected to {alias}")),
            }
            return;
        }
        if let Some(name) = &name
            && self.session_by_name(name).is_some()
        {
            return;
        }
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let area = session_area(&self.config, term::fd_size(&client.raw_out));
        let Ok(sid) = self.create_session(name, area) else {
            return;
        };
        if let Some(client) = self.clients.get_mut(&conn) {
            client.attached = sid;
            client.auto = None;
            client.switcher = None;
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
        if let DecodedInput::Mouse(mouse) = event {
            if self.config.grid_mouse {
                self.grid_mouse(conn, state, &items, mouse);
            }
            return;
        }
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

    /// A click captures the tile under it or releases it if it already
    /// is, a double click goes to its tab, and a click on no tile ends
    /// capture. The wheel scrolls the captured tab, or else the grid, and
    /// the highlight follows the pointer.
    fn grid_mouse(
        &mut self,
        conn: ConnId,
        mut state: GridState,
        items: &[grid::GridItem],
        mouse: &CtMouseEvent,
    ) {
        let Some(client) = self.clients.get_mut(&conn) else {
            return;
        };
        let size = term::fd_size(&client.raw_out);
        let area = Rect::new(0, 0, size.width, size.height);
        let pos = Position::new(mouse.column, mouse.row);
        let under = grid::tile_at(area, items.len(), state.scroll, pos)
            .and_then(|(i, rect)| Some((i, *items.get(i)?, rect)));
        let tab_id = |sessions: &BTreeMap<SessionId, Session>, item: grid::GridItem| {
            sessions
                .get(&item.session)
                .and_then(|s| s.tab_at(item.window, item.tab))
                .map(|tab| tab.id)
        };
        match mouse.kind {
            CtMouseKind::Down(CtMouseButton::Left) => {
                let double = grid::is_double_click(&mut state, pos);
                state.pending_prefix = false;
                match under {
                    Some((_, item, _)) if double => {
                        client.grid = None;
                        self.attach_to_tab(conn, item.session, item.window, item.tab);
                        return;
                    }
                    Some((i, item, _)) => {
                        let id = tab_id(&self.sessions, item);
                        state.highlight = i;
                        state.capture = if state.capture == id { None } else { id };
                    }
                    None => state.capture = None,
                }
            }
            CtMouseKind::ScrollUp | CtMouseKind::ScrollDown => match (state.capture, under) {
                (Some(captured), Some((_, item, rect))) => {
                    if tab_id(&self.sessions, item) == Some(captured)
                        && let Some(session) = self.sessions.get_mut(&item.session)
                    {
                        let content = grid::tile_content(rect);
                        session.wheel_to_tab(item.window, item.tab, mouse, content);
                    }
                }
                (Some(_), None) => {}
                (None, _) => {
                    let dir = if mouse.kind == CtMouseKind::ScrollUp {
                        Dir::Up
                    } else {
                        Dir::Down
                    };
                    grid::navigate(&mut state, area, items.len(), dir);
                }
            },
            // A captured tile keeps the highlight, as it does with keys.
            CtMouseKind::Moved => {
                if state.capture.is_none()
                    && let Some((i, _, _)) = under
                {
                    state.highlight = i;
                }
            }
            _ => {}
        }
        self.store_grid_state(conn, state);
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
        self.mark_host_state();
        // The indicator spans sessions, so compute it here and hand it to
        // the session to render.
        let indicators: Vec<(SessionId, session::Indicator)> = self
            .clients
            .values()
            .filter(|c| {
                // The sidebar holding focus leaves its session in view.
                let beside = sidebar_width(&self.config, term::fd_size(&c.raw_out)) > 0;
                c.finder.is_none()
                    && c.grid.is_none()
                    && (c.switcher.is_none() || beside)
                    && !(c.switcher.is_none() && c.auto.is_some_and(|a| a.presented.is_none()))
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
            hosts,
            ..
        } = self;
        for client in clients.values_mut() {
            let sidebar = sidebar_width(config, term::fd_size(&client.raw_out));
            if client.finder.is_some() {
                render_finder(client, sessions, hosts, config);
            } else if client.grid.is_some() {
                render_grid(client, sessions, &config.palette);
            } else if let Some(highlight) = client.switcher
                && sidebar == 0
            {
                render_switcher(client, sessions, hosts, highlight, config);
            } else if client.switcher.is_none()
                && client.auto.is_some_and(|a| a.presented.is_none())
            {
                render_auto_blank(client, sessions, &config.palette);
            } else if sidebar > 0 {
                // Any session's change can show in the sidebar.
                render_with_sidebar(client, sessions, hosts, config, sidebar);
            } else if let Some(session) = sessions.get_mut(&client.attached)
                && session.needs_redraw()
            {
                let _ = session.draw_frame(&mut client.terminal, true, |_| {});
            }
        }
    }
}

fn render_finder(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    hosts: &BTreeMap<HostId, Host>,
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
        find::render(buf, area, sessions, hosts, state, config, colors);
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

/// A switcher row: a connected host's heading, or one of its sessions.
enum SwitcherRow {
    Heading { alias: String, offline: bool },
    Session(SwitcherEntry),
}

/// A session's text, its urgency, and whether its host is unreachable.
struct SwitcherEntry {
    text: String,
    urgency: Option<agent::Urgency>,
    offline: bool,
}

/// The list plus its divider.
const SIDEBAR_WIDTH: u16 = SWITCHER_LIST_WIDTH + 1;

/// The narrowest layout the sidebar still makes room for.
const SIDEBAR_MIN_LAYOUT: u16 = 20;

/// Zero while the sidebar is off or the terminal is too narrow for it.
fn sidebar_width(config: &Config, size: ratatui::layout::Size) -> u16 {
    if config.sidebar && size.width >= SIDEBAR_WIDTH + SIDEBAR_MIN_LAYOUT {
        SIDEBAR_WIDTH
    } else {
        0
    }
}

/// Where a client's attached session draws: right of any sidebar.
fn session_area(config: &Config, size: ratatui::layout::Size) -> Rect {
    let x = sidebar_width(config, size);
    Rect::new(x, 0, size.width - x, size.height)
}

/// The switcher's sessions: this host's, then each connected host's,
/// grouped. The switcher's highlight indexes this order.
fn switcher_order(sessions: &BTreeMap<SessionId, Session>) -> Vec<SessionId> {
    let mut order: Vec<(Option<HostId>, SessionId)> = sessions
        .iter()
        .map(|(&sid, s)| (s.host.as_ref().map(|h| h.id), sid))
        .collect();
    order.sort();
    order.into_iter().map(|(_, sid)| sid).collect()
}

/// `switcher_order` with a heading ahead of each host's group.
fn switcher_rows(
    sessions: &BTreeMap<SessionId, Session>,
    hosts: &BTreeMap<HostId, Host>,
) -> Vec<SwitcherRow> {
    let mut rows = Vec::with_capacity(sessions.len());
    let mut group = None;
    for sid in switcher_order(sessions) {
        let s = &sessions[&sid];
        let host = s.host.as_ref().map(|h| h.id);
        if let Some(id) = host
            && group != host
        {
            rows.push(SwitcherRow::Heading {
                alias: hosts.get(&id).map_or("?".into(), |h| h.alias.clone()),
                offline: s.is_offline(),
            });
        }
        group = host;
        rows.push(SwitcherRow::Session(SwitcherEntry {
            text: format!("{} ({} windows)", s.name, s.window_count()),
            urgency: s.urgency().filter(|_| !s.is_offline()),
            offline: s.is_offline(),
        }));
    }
    rows
}

/// The session at a row, by its place in `switcher_order`.
fn session_at_row(rows: &[SwitcherRow], row: usize) -> Option<usize> {
    match rows.get(row)? {
        SwitcherRow::Heading { .. } => None,
        SwitcherRow::Session(_) => Some(
            rows[..row]
                .iter()
                .filter(|r| matches!(r, SwitcherRow::Session(_)))
                .count(),
        ),
    }
}

/// One row each below a blank top row, with a divider on the right when
/// `area` has room. An unfocused list marks its highlight in bold rather
/// than reversed.
fn render_session_list(
    buf: &mut Buffer,
    area: Rect,
    rows: &[SwitcherRow],
    highlight: usize,
    focused: bool,
    palette: &palette::Palette,
    colors: &palette::TermColors,
) {
    let elapsed = anim::elapsed();
    let default_bg = colors
        .bg
        .map_or(palette.bg, |(r, g, b)| Color::Rgb(r, g, b));
    let mark = if focused {
        Modifier::REVERSED
    } else {
        Modifier::BOLD
    };
    let list_w = SWITCHER_LIST_WIDTH.min(area.width);
    let mut next = 0;
    for (row, entry) in rows.iter().enumerate() {
        let y = area.y + 1 + row as u16;
        if y >= area.bottom() {
            break;
        }
        let entry = match entry {
            SwitcherRow::Heading { alias, offline } => {
                let color = if *offline { palette.dim } else { palette.muted };
                let style = Style::default().fg(color).add_modifier(Modifier::BOLD);
                for (j, ch) in format!(" {alias}").chars().enumerate() {
                    let x = area.x + j as u16;
                    if x >= area.x + list_w {
                        break;
                    }
                    if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                        dst.set_char(ch);
                        dst.set_style(style);
                    }
                }
                continue;
            }
            SwitcherRow::Session(entry) => entry,
        };
        let i = next;
        next += 1;
        let urgency = entry.urgency;
        let (base, modifier) = match (i == highlight, entry.offline) {
            (true, false) => (palette.accent, mark),
            (true, true) => (palette.dim, mark),
            (false, false) => (palette.text, Modifier::empty()),
            (false, true) => (palette.dim, Modifier::empty()),
        };
        let (color, anim) = urgency.map_or((base, Anim::None), |urgency| {
            let (status, anim) = urgency.visual();
            (palette.status(status), anim)
        });
        let text = format!(" {} ", entry.text);
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
            // Explicit colors, since some terminals reverse a default
            // background into the default foreground.
            let style = if focused && i == highlight && urgency.is_some() {
                Style::default().fg(default_bg).bg(fg)
            } else {
                Style::default().fg(fg).add_modifier(modifier)
            };
            if let Some(dst) = buf.cell_mut(Position::new(x, y)) {
                dst.set_char(ch);
                dst.set_style(style);
            }
        }
    }
    if area.width > list_w {
        for y in area.top()..area.bottom() {
            if let Some(dst) = buf.cell_mut(Position::new(area.x + list_w, y)) {
                dst.set_symbol("│");
                dst.set_style(Style::default().fg(palette.dim));
            }
        }
    }
}

/// The session list beside the attached session, focused while the client
/// is in switcher mode.
fn render_with_sidebar(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    hosts: &BTreeMap<HostId, Host>,
    config: &Config,
    width: u16,
) {
    let rows = switcher_rows(sessions, hosts);
    let focused = client.switcher.is_some();
    let highlight = client
        .switcher
        .or_else(|| {
            switcher_order(sessions)
                .iter()
                .position(|&sid| sid == client.attached)
        })
        .unwrap_or(0)
        .min(sessions.len().saturating_sub(1));
    let Client {
        terminal,
        new_session,
        host_choice,
        colors,
        attached,
        ..
    } = client;
    let Some(session) = sessions.get_mut(attached) else {
        return;
    };
    let palette = &config.palette;
    let _ = session.draw_frame(terminal, !focused, |buf| {
        let area = *buf.area();
        let sidebar = Rect {
            width: width.min(area.width),
            ..area
        };
        clear_region(buf, sidebar);
        render_session_list(buf, sidebar, &rows, highlight, focused, palette, colors);
        render_switcher_prompts(buf, area, new_session, host_choice, hosts, palette);
    });
}

fn render_switcher(
    client: &mut Client,
    sessions: &mut BTreeMap<SessionId, Session>,
    hosts: &BTreeMap<HostId, Host>,
    highlight: usize,
    config: &Config,
) {
    let palette = &config.palette;
    let rows = switcher_rows(sessions, hosts);
    let highlight = highlight.min(sessions.len().saturating_sub(1));
    let highlighted_sid = switcher_order(sessions).get(highlight).copied();
    let Client {
        terminal,
        new_session,
        host_choice,
        colors,
        ..
    } = client;
    let colors = *colors;
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let buf = frame.buffer_mut();
        clear_region(buf, area);
        let list_w = SWITCHER_LIST_WIDTH.min(area.width);
        render_session_list(buf, area, &rows, highlight, true, palette, &colors);
        // The menu icon, which exits on click.
        if area.height > 0
            && let Some(dst) = buf.cell_mut(Position::new(area.x, area.bottom() - 1))
        {
            dst.set_char('○');
            dst.set_style(Style::default().fg(palette.accent));
        }
        if area.width > list_w {
            let preview = Rect {
                x: area.x + list_w + 1,
                width: area.width - list_w - 1,
                ..area
            };
            if let Some(session) = highlighted_sid.and_then(|sid| sessions.get_mut(&sid)) {
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
        render_switcher_prompts(buf, area, new_session, host_choice, hosts, palette);
    });
}

/// The new-session prompt or host choice, on the bottom row where the
/// command line lives.
fn render_switcher_prompts(
    buf: &mut Buffer,
    area: Rect,
    new_session: &Option<TextArea<'static>>,
    host_choice: &Option<HostChoice>,
    hosts: &BTreeMap<HostId, Host>,
    palette: &palette::Palette,
) {
    if area.height == 0 {
        return;
    }
    let line = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    if let Some(prompt) = new_session.as_ref() {
        let label = NEW_SESSION_LABEL;
        let label_len = label.chars().count() as u16;
        if area.width <= label_len {
            return;
        }
        clear_region(buf, line);
        for (i, ch) in label.chars().enumerate() {
            if let Some(dst) = buf.cell_mut(Position::new(line.x + i as u16, line.y)) {
                dst.set_char(ch);
            }
        }
        let input = Rect {
            x: line.x + label_len,
            width: line.width - label_len,
            ..line
        };
        prompt.render(input, buf);
    }
    if let Some(choice) = host_choice.as_ref() {
        clear_region(buf, line);
        let mut x = line.x;
        let mut put = |text: &str, style: Style| {
            for ch in text.chars() {
                if x >= line.right() {
                    return;
                }
                if let Some(dst) = buf.cell_mut(Position::new(x, line.y)) {
                    dst.set_char(ch);
                    dst.set_style(style);
                }
                x += 1;
            }
        };
        put(HOST_CHOICE_LABEL, Style::default());
        for (i, host) in choice.hosts.iter().enumerate() {
            let name = match host {
                Some(id) => hosts.get(id).map_or("?", |h| h.alias.as_str()),
                None => session::hostname(),
            };
            let style = if i == choice.highlight {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(palette.text)
            };
            put(&format!(" {name} "), style);
            put(" ", Style::default());
        }
    }
}

const NEW_SESSION_LABEL: &str = "new session: ";

const HOST_CHOICE_LABEL: &str = "run on: ";

const AT_IN_NAME: &str = "session names cannot contain '@'";

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
        let idle = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: 25_000_000,
        };
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
            match rustix::event::poll(&mut fds, Some(&idle)) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SwitcherRow {
        SwitcherRow::Session(SwitcherEntry {
            text: String::new(),
            urgency: None,
            offline: false,
        })
    }

    fn heading() -> SwitcherRow {
        SwitcherRow::Heading {
            alias: "dev".into(),
            offline: false,
        }
    }

    #[test]
    fn heading_rows_map_to_no_session() {
        let rows = [session(), heading(), session(), session()];
        let mapped: Vec<Option<usize>> = (0..5).map(|r| session_at_row(&rows, r)).collect();
        assert_eq!(mapped, [Some(0), None, Some(1), Some(2), None]);
    }
}
