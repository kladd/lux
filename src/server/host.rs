//! Adopted hosts: the hub's ssh connection to each, reconnects, and
//! mirroring what each host sends.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::protocol;
use crate::server::palette::TermColors;
use crate::server::session::{Session, SessionHost};
use crate::server::window::{Notice, TabId};
use crate::server::wire::{
    self, HubMsg, RemoteMsg, RemoteSessionId, RemoteTabId, SessionSnap, TabRef,
};
use crate::server::{ConnId, Server, ServerEvent, SessionId, term};

pub type HostId = usize;

/// Reconnect attempts before giving up on a lost host.
const MAX_ATTEMPTS: u32 = 6;

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

/// Carries messages to a host while it is connected and drops them
/// otherwise.
#[derive(Clone, Default)]
pub struct HostLink(Arc<Mutex<Option<Sender<HubMsg>>>>);

impl HostLink {
    pub fn send(&self, msg: HubMsg) {
        if let Some(tx) = self.0.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }

    pub fn connected(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    fn set(&self, tx: Option<Sender<HubMsg>>) {
        *self.0.lock().unwrap() = tx;
    }

    #[cfg(test)]
    pub fn connected_to(tx: Sender<HubMsg>) -> Self {
        let link = Self::default();
        link.set(Some(tx));
        link
    }
}

pub enum HostEvent {
    Msg(RemoteMsg),
    /// The ssh process ended, with the last thing it printed.
    Closed(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostState {
    /// Handshake or snapshot in flight.
    Connecting,
    Connected,
    /// Waiting to retry a lost connection.
    Backoff(Instant),
    /// Gave up; the host stays listed until `:disconnect`.
    Exhausted,
}

pub struct Host {
    pub alias: String,
    pub state: HostState,
    /// A first connect that fails drops the host instead of retrying.
    established: bool,
    attempts: u32,
    /// Tags events so a replaced connection's stragglers are ignored.
    generation: u64,
    pub link: HostLink,
    /// The live connection, handed to `link` once the host's sessions
    /// arrive.
    pending_tx: Option<Sender<HubMsg>>,
    child: Option<Child>,
    /// The hub tab mirroring each of the host's tabs.
    tabs: HashMap<RemoteTabId, TabId>,
    /// Clients waiting on a session they asked the host to create.
    waiting: HashMap<u64, ConnId>,
    colors: Option<TermColors>,
    /// Why the host turned the hub away, which ends retries.
    refusal: Option<String>,
}

impl Host {
    fn new(alias: String) -> Self {
        Self {
            alias,
            state: HostState::Connecting,
            established: false,
            attempts: 0,
            generation: 0,
            link: HostLink::default(),
            pending_tx: None,
            child: None,
            tabs: HashMap::new(),
            waiting: HashMap::new(),
            colors: None,
            refusal: None,
        }
    }

    pub fn online(&self) -> bool {
        self.state == HostState::Connected
    }

    fn start(&mut self, id: HostId, tx: &Sender<ServerEvent>, instance: u64) -> Result<(), String> {
        self.stop();
        self.generation += 1;
        self.refusal = None;
        self.state = HostState::Connecting;
        let generation = self.generation;
        let mut child = Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "Compression=yes",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "--",
                &self.alias,
                "lux",
                "proxy",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| format!("cannot run ssh: {err}"))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            return Err("cannot run ssh".into());
        };
        let (msg_tx, msg_rx) = mpsc::channel::<HubMsg>();
        thread::spawn(move || {
            for msg in msg_rx {
                if wire::write_frame(&mut stdin, &msg).is_err() {
                    return;
                }
            }
        });
        let (err_tx, err_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            let _ = err_tx.send(text);
        });
        let tx = tx.clone();
        thread::spawn(move || {
            let mut stdout = BufReader::with_capacity(1 << 16, stdout);
            while let Ok(Some(msg)) = wire::read_frame::<RemoteMsg>(&mut stdout) {
                let event = ServerEvent::Host {
                    host: id,
                    generation,
                    event: HostEvent::Msg(msg),
                };
                if tx.send(event).is_err() {
                    return;
                }
            }
            // ssh prints why it quit just before closing both pipes.
            let text = err_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_default();
            let reason = text
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("connection closed")
                .to_string();
            let _ = tx.send(ServerEvent::Host {
                host: id,
                generation,
                event: HostEvent::Closed(reason),
            });
        });
        let _ = msg_tx.send(HubMsg::Hello {
            version: wire::version(),
            instance,
        });
        self.pending_tx = Some(msg_tx);
        self.child = Some(child);
        Ok(())
    }

    /// Ends the ssh process. Its stragglers carry a stale generation.
    fn stop(&mut self) {
        self.link.set(None);
        self.pending_tx = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A CLI attach to `name@alias`, which hears `ok` or `err` only once the
/// host has connected and the session exists.
pub struct PendingAttach {
    stream: UnixStream,
    stdin: OwnedFd,
    stdout: File,
    host: HostId,
    name: Option<String>,
}

fn usable_alias(alias: &str) -> bool {
    !alias.is_empty() && !alias.contains(['.', '@']) && !alias.contains(char::is_whitespace)
}

/// The delay before reconnect `attempt`, counting from 1.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.saturating_sub(1).min(5))
}

/// Splits `name@alias`, the address of a session on a connected host.
pub fn split_address(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('@') {
        Some((name, alias)) => (name, Some(alias)),
        None => (name, None),
    }
}

impl Server {
    pub(super) fn host_by_alias(&self, alias: &str) -> Option<HostId> {
        self.hosts
            .iter()
            .find(|(_, h)| h.alias == alias)
            .map(|(&id, _)| id)
    }

    pub(super) fn remote_session_by_name(&self, host: HostId, name: &str) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|(_, s)| s.host.as_ref().is_some_and(|h| h.id == host) && s.name == name)
            .map(|(&sid, _)| sid)
    }

    pub(super) fn connect(&mut self, conn: ConnId, alias: String) {
        if !usable_alias(&alias) {
            self.tell(
                conn,
                format!("connect: '{alias}' is not a usable ssh alias"),
            );
            return;
        }
        if let Some(id) = self.host_by_alias(&alias) {
            let host = &self.hosts[&id];
            match host.state {
                HostState::Connected => {
                    self.tell(conn, format!("already connected to {alias}"));
                }
                HostState::Connecting if !host.established => {
                    self.tell(conn, format!("already connecting to {alias}"));
                }
                // A retry by hand starts the count over.
                HostState::Connecting | HostState::Backoff(_) | HostState::Exhausted => {
                    self.hosts.get_mut(&id).expect("host exists").attempts = 0;
                    self.restart_host(id);
                }
            }
            return;
        }
        if let Err(err) = self.add_host(&alias) {
            self.tell(conn, format!("connect {alias}: {err}"));
        }
    }

    pub(super) fn add_host(&mut self, alias: &str) -> Result<HostId, String> {
        let id = self.next_host_id;
        let mut host = Host::new(alias.to_string());
        host.start(id, &self.tx, self.instance)?;
        self.next_host_id += 1;
        self.hosts.insert(id, host);
        Ok(id)
    }

    /// Attaches a CLI client to `address`'s host, connecting to it first
    /// when it isn't connected.
    pub(super) fn attach_remote(
        &mut self,
        conn: ConnId,
        mut stream: UnixStream,
        address: &str,
        stdin: OwnedFd,
        stdout: File,
    ) {
        let (name, alias) = split_address(address);
        let alias = alias.unwrap_or_default();
        let id = match self.host_for_attach(alias) {
            Ok(id) => id,
            Err(err) => {
                let _ = protocol::write_line(&mut stream, &format!("err connect {alias}: {err}"));
                return;
            }
        };
        let name = (!name.is_empty()).then(|| name.to_string());
        self.pending_attaches.insert(
            conn,
            PendingAttach {
                stream,
                stdin,
                stdout,
                host: id,
                name,
            },
        );
        if self.hosts[&id].online() {
            self.resolve_attach(conn);
        }
    }

    /// The host named `alias`, with a connection under way unless it is
    /// already connected or connecting.
    fn host_for_attach(&mut self, alias: &str) -> Result<HostId, String> {
        if !usable_alias(alias) {
            return Err("not a usable ssh alias".into());
        }
        let Some(id) = self.host_by_alias(alias) else {
            return self.add_host(alias);
        };
        let host = self.hosts.get_mut(&id).expect("host exists");
        if matches!(host.state, HostState::Backoff(_) | HostState::Exhausted) {
            host.attempts = 0;
            if let Err(err) = host.start(id, &self.tx, self.instance) {
                self.host_closed(id, err.clone());
                return Err(err);
            }
        }
        Ok(id)
    }

    /// Attaches a pending client to its session, or asks the connected
    /// host to create it.
    fn resolve_attach(&mut self, conn: ConnId) {
        let Some(pending) = self.pending_attaches.get(&conn) else {
            return;
        };
        let (host, name) = (pending.host, pending.name.clone());
        let size = term::fd_size(&pending.stdout);
        if let Some(sid) = name
            .as_deref()
            .and_then(|n| self.remote_session_by_name(host, n))
        {
            self.finish_attach(conn, sid);
            return;
        }
        let area = super::session_area(&self.config, size);
        self.request_session(conn, host, name, area);
    }

    fn finish_attach(&mut self, conn: ConnId, sid: SessionId) {
        if let Some(p) = self.pending_attaches.remove(&conn) {
            self.install_client(conn, p.stream, sid, p.stdin, p.stdout);
        }
    }

    fn fail_attach(&mut self, conn: ConnId, reason: &str) {
        if let Some(mut p) = self.pending_attaches.remove(&conn) {
            let _ = protocol::write_line(&mut p.stream, &format!("err {reason}"));
        }
    }

    fn fail_attaches(&mut self, host: HostId, reason: &str) {
        let conns: Vec<ConnId> = self
            .pending_attaches
            .iter()
            .filter(|(_, p)| p.host == host)
            .map(|(&conn, _)| conn)
            .collect();
        for conn in conns {
            self.fail_attach(conn, reason);
        }
    }

    pub(super) fn disconnect(&mut self, conn: ConnId, alias: String) {
        let Some(id) = self.host_by_alias(&alias) else {
            self.tell(conn, format!("not connected to {alias}"));
            return;
        };
        if let Some(mut host) = self.hosts.remove(&id) {
            host.stop();
        }
        self.fail_attaches(id, &format!("disconnected from {alias}"));
        let gone: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.host.as_ref().is_some_and(|h| h.id == id))
            .map(|(&sid, _)| sid)
            .collect();
        for sid in gone {
            self.relocate_clients(sid);
            self.remove_session(sid);
        }
        self.tell(conn, format!("disconnected from {alias}"));
    }

    /// Moves clients off a session that is going away, to the most recent
    /// local session, or detaches them when there is none.
    fn relocate_clients(&mut self, sid: SessionId) {
        let conns: Vec<ConnId> = self
            .clients
            .iter()
            .filter(|(_, c)| c.attached == sid)
            .map(|(&conn, _)| conn)
            .collect();
        if conns.is_empty() {
            return;
        }
        let target = self
            .attach_order
            .iter()
            .rev()
            .copied()
            .find(|s| self.sessions.get(s).is_some_and(|s| !s.is_remote()));
        for conn in conns {
            match target {
                Some(target) => self.switch_client(conn, target),
                None => self.detach(conn),
            }
        }
    }

    pub(super) fn restart_host(&mut self, id: HostId) {
        let Some(host) = self.hosts.get_mut(&id) else {
            return;
        };
        if let Err(err) = host.start(id, &self.tx, self.instance) {
            self.host_closed(id, err);
        }
    }

    /// Starts each reconnect whose delay is up.
    pub(super) fn tick_hosts(&mut self) {
        let now = Instant::now();
        let due: Vec<HostId> = self
            .hosts
            .iter()
            .filter(|(_, h)| matches!(h.state, HostState::Backoff(at) if now >= at))
            .map(|(&id, _)| id)
            .collect();
        for id in due {
            self.restart_host(id);
        }
    }

    pub(super) fn hosts_pending(&self) -> bool {
        self.hosts
            .values()
            .any(|h| matches!(h.state, HostState::Connecting | HostState::Backoff(_)))
    }

    /// Stops every ssh process, as the server exits.
    pub(super) fn drop_hosts(&mut self) {
        for host in self.hosts.values_mut() {
            host.stop();
        }
    }

    pub(super) fn host_event(&mut self, id: HostId, generation: u64, event: HostEvent) {
        if self
            .hosts
            .get(&id)
            .is_none_or(|h| h.generation != generation)
        {
            return;
        }
        match event {
            HostEvent::Msg(msg) => self.host_msg(id, msg),
            HostEvent::Closed(reason) => self.host_closed(id, reason),
        }
    }

    fn host_closed(&mut self, id: HostId, reason: String) {
        let Some(host) = self.hosts.get_mut(&id) else {
            return;
        };
        host.stop();
        let alias = host.alias.clone();
        let why = host.refusal.clone().unwrap_or_else(|| reason.clone());
        self.fail_attaches(id, &format!("connect {alias}: {why}"));
        let host = self.hosts.get_mut(&id).expect("host exists");
        if !host.established {
            let reason = host.refusal.take().unwrap_or(reason);
            self.hosts.remove(&id);
            self.tell_all(format!("connect {alias}: {reason}"));
            return;
        }
        if let Some(refusal) = host.refusal.take() {
            host.state = HostState::Exhausted;
            self.tell_all(format!("{refusal}; :disconnect {alias} to remove it"));
            return;
        }
        host.attempts += 1;
        if host.attempts > MAX_ATTEMPTS {
            host.state = HostState::Exhausted;
            self.tell_all(format!(
                "lost {alias}: {reason}; gave up reconnecting, :connect {alias} to retry or :disconnect {alias} to remove it"
            ));
            return;
        }
        host.state = HostState::Backoff(Instant::now() + backoff(host.attempts));
        if host.attempts == 1 {
            self.tell_all(format!("lost {alias}: {reason}; reconnecting"));
        }
    }

    fn host_msg(&mut self, id: HostId, msg: RemoteMsg) {
        let now = Instant::now();
        match msg {
            RemoteMsg::Hello { version, .. } => {
                if version != wire::version() {
                    let host = self.hosts.get_mut(&id).expect("host exists");
                    host.refusal = Some(format!(
                        "{} runs lux {version} but this host runs {}",
                        host.alias,
                        wire::version()
                    ));
                    self.host_closed(id, String::new());
                }
            }
            RemoteMsg::Refused(reason) => {
                let host = self.hosts.get_mut(&id).expect("host exists");
                host.refusal = Some(format!("{} refused: {reason}", host.alias));
                self.host_closed(id, String::new());
            }
            RemoteMsg::Evicted => {
                let host = self.hosts.get_mut(&id).expect("host exists");
                host.refusal = Some(format!("{} was adopted by another hub", host.alias));
            }
            RemoteMsg::Sessions(snaps) => self.host_sessions(id, snaps, now),
            RemoteMsg::SessionAdded { request, session } => {
                let Some(sid) = self.add_remote_session(id, session, None, now) else {
                    return;
                };
                let conn = request
                    .and_then(|r| self.hosts.get_mut(&id).and_then(|h| h.waiting.remove(&r)));
                match conn {
                    Some(conn) if self.pending_attaches.contains_key(&conn) => {
                        self.finish_attach(conn, sid);
                    }
                    Some(conn) => self.switch_client(conn, sid),
                    None => {}
                }
            }
            RemoteMsg::NewSessionFailed { request, reason } => {
                let conn = self
                    .hosts
                    .get_mut(&id)
                    .and_then(|h| h.waiting.remove(&request));
                if let Some(conn) = conn {
                    if self.pending_attaches.contains_key(&conn) {
                        self.fail_attach(conn, &format!("new session: {reason}"));
                    } else {
                        self.tell(conn, format!("new session: {reason}"));
                    }
                }
            }
            RemoteMsg::SessionEnded(remote) => {
                if let Some(sid) = self.remote_sid(id, remote) {
                    self.remove_session(sid);
                }
            }
            RemoteMsg::Spawned { token, tab } => {
                let remote = tab.id;
                let Some(hub_tab) = self.bind_token(token) else {
                    return;
                };
                if let Some(session) = self.session_with_tab(hub_tab)
                    && let Some(t) = session.find_tab_mut(hub_tab)
                {
                    t.load_remote(tab, now);
                    session.request_redraw();
                }
                if let Some(host) = self.hosts.get_mut(&id) {
                    host.tabs.insert(remote, hub_tab);
                }
            }
            RemoteMsg::SpawnFailed(token) => {
                if let Some(hub_tab) = self.bind_token(token) {
                    self.tab_exited(hub_tab);
                }
            }
            RemoteMsg::Update { tab, update } => {
                let Some(hub_tab) = self.hub_tab(id, tab) else {
                    return;
                };
                if let Some(session) = self.session_with_tab(hub_tab) {
                    session.remote_update(hub_tab, update, now);
                }
            }
            RemoteMsg::Full { tab, snap } => {
                let Some(hub_tab) = self.hub_tab(id, tab) else {
                    return;
                };
                if let Some(session) = self.session_with_tab(hub_tab)
                    && let Some(t) = session.find_tab_mut(hub_tab)
                {
                    t.load_remote(snap, now);
                    session.request_redraw();
                }
            }
            RemoteMsg::Exited(tab) => {
                let hub_tab = self.hosts.get_mut(&id).and_then(|h| h.tabs.remove(&tab));
                if let Some(hub_tab) = hub_tab {
                    self.tab_exited(hub_tab);
                }
            }
            RemoteMsg::Notice {
                tab,
                blocked,
                summary,
            } => {
                let Some(hub_tab) = self.hub_tab(id, tab) else {
                    return;
                };
                let Some((session, name)) = self.sessions.values().find_map(|s| {
                    let name = s.tabs().find(|t| t.id == hub_tab)?.name.clone();
                    Some((s.name.clone(), name))
                }) else {
                    return;
                };
                let notice = Notice {
                    id: hub_tab,
                    tab: name,
                    blocked,
                    summary,
                };
                self.raise_notification(&session, &notice);
            }
            RemoteMsg::Clipboard { tab, text } => {
                if let Some(hub_tab) = self.hub_tab(id, tab) {
                    self.program_copy(hub_tab, text);
                }
            }
        }
    }

    /// Mirrors every session a host offered, reusing the hub's slot for
    /// one it already listed so the switcher keeps its order.
    fn host_sessions(&mut self, id: HostId, snaps: Vec<SessionSnap>, now: Instant) {
        let Some(host) = self.hosts.get_mut(&id) else {
            return;
        };
        let reconnect = host.established;
        host.state = HostState::Connected;
        host.established = true;
        host.attempts = 0;
        host.tabs.clear();
        host.colors = None;
        host.link.set(host.pending_tx.take());
        let alias = host.alias.clone();
        let mut stale: Vec<(SessionId, RemoteSessionId, String)> = self
            .sessions
            .iter()
            .filter_map(|(&sid, s)| {
                let h = s.host.as_ref().filter(|h| h.id == id)?;
                Some((sid, h.remote, s.name.clone()))
            })
            .collect();
        for snap in snaps {
            let slot = stale
                .iter()
                .position(|(_, remote, _)| *remote == snap.id)
                .or_else(|| stale.iter().position(|(_, _, name)| *name == snap.name))
                .map(|i| stale.remove(i).0);
            self.add_remote_session(id, snap, slot, now);
        }
        for (sid, ..) in stale {
            self.remove_session(sid);
        }
        let verb = if reconnect {
            "reconnected to"
        } else {
            "connected to"
        };
        self.tell_all(format!("{verb} {alias}"));
        let conns: Vec<ConnId> = self
            .pending_attaches
            .iter()
            .filter(|(_, p)| p.host == id)
            .map(|(&conn, _)| conn)
            .collect();
        for conn in conns {
            self.resolve_attach(conn);
        }
    }

    /// Mirrors a host's session into `slot`, or a new one. Returns its id.
    fn add_remote_session(
        &mut self,
        id: HostId,
        snap: SessionSnap,
        slot: Option<SessionId>,
        now: Instant,
    ) -> Option<SessionId> {
        let host = self.hosts.get(&id)?;
        let session_host = SessionHost {
            id,
            link: host.link.clone(),
            remote: snap.id,
            synced: None,
        };
        let area = self.client_area().unwrap_or(Rect::new(0, 0, 80, 24));
        let mut session = Session::from_remote(
            snap,
            session_host,
            area,
            self.config.clone(),
            self.tx.clone(),
            now,
        )?;
        let synced = session.layout_snap();
        if let Some(h) = session.host.as_mut() {
            h.synced = synced;
        }
        let host = self.hosts.get_mut(&id)?;
        for tab in session.tabs() {
            if let Some(remote) = tab.remote_id() {
                host.tabs.insert(remote, tab.id);
            }
        }
        let sid = slot.unwrap_or_else(|| {
            let sid = self.next_session_id;
            self.next_session_id += 1;
            sid
        });
        self.sessions.insert(sid, session);
        for client in self.clients.values().filter(|c| c.attached == sid) {
            let size = term::fd_size(&client.raw_out);
            if let Some(session) = self.sessions.get_mut(&sid) {
                session.set_area(super::session_area(&self.config, size));
            }
        }
        Some(sid)
    }

    fn client_area(&self) -> Option<Rect> {
        let client = self.clients.values().next()?;
        Some(super::session_area(
            &self.config,
            term::fd_size(&client.raw_out),
        ))
    }

    fn remote_sid(&self, host: HostId, remote: RemoteSessionId) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|(_, s)| {
                s.host
                    .as_ref()
                    .is_some_and(|h| h.id == host && h.remote == remote)
            })
            .map(|(&sid, _)| sid)
    }

    fn hub_tab(&self, host: HostId, tab: RemoteTabId) -> Option<TabId> {
        self.hosts.get(&host)?.tabs.get(&tab).copied()
    }

    /// The hub tab that was spawned under `token`.
    fn bind_token(&self, token: u64) -> Option<TabId> {
        self.sessions.values().find_map(|s| {
            s.tabs()
                .find(|t| t.wire() == Some(TabRef::Token(token)))
                .map(|t| t.id)
        })
    }

    fn session_with_tab(&mut self, tab: TabId) -> Option<&mut Session> {
        self.sessions.values_mut().find(|s| s.has_tab(tab))
    }

    /// Asks the host for a session, which the client switches to once it
    /// arrives.
    pub(super) fn new_remote_session(&mut self, conn: ConnId, host: HostId, name: Option<String>) {
        let Some(h) = self.hosts.get(&host) else {
            return;
        };
        if !h.online() {
            let alias = h.alias.clone();
            self.tell(conn, format!("{alias} is unreachable"));
            return;
        }
        if let Some(name) = &name
            && self.remote_session_by_name(host, name).is_some()
        {
            return;
        }
        let area = self
            .clients
            .get(&conn)
            .map(|c| term::fd_size(&c.raw_out))
            .map_or(Rect::new(0, 0, 80, 24), |s| {
                super::session_area(&self.config, s)
            });
        self.request_session(conn, host, name, area);
    }

    /// Asks a connected host for a session sized to `area`, answered to
    /// `conn` once it arrives.
    fn request_session(&mut self, conn: ConnId, host: HostId, name: Option<String>, area: Rect) {
        let request = next_token();
        let h = self.hosts.get_mut(&host).expect("host exists");
        h.waiting.insert(request, conn);
        h.link.send(HubMsg::NewSession {
            request,
            name,
            cols: area.width,
            rows: area.height,
        });
    }

    /// Sends each remote session's layout when it changed since the host
    /// last heard.
    pub(super) fn sync_layouts(&mut self) {
        for session in self.sessions.values_mut() {
            let Some(layout) = session.layout_snap() else {
                continue;
            };
            let Some(host) = session.host.as_mut() else {
                continue;
            };
            if host.synced.as_ref() == Some(&layout) || !host.link.connected() {
                continue;
            }
            host.link.send(HubMsg::Layout {
                session: host.remote,
                layout: layout.clone(),
            });
            host.synced = Some(layout);
        }
    }

    /// Hands each host the colors of a client looking at one of its
    /// sessions, or of any client, for answering programs' color queries.
    pub(super) fn sync_host_colors(&mut self) {
        let ids: Vec<HostId> = self.hosts.keys().copied().collect();
        for id in ids {
            let viewing = self.clients.values().find(|c| {
                self.sessions
                    .get(&c.attached)
                    .and_then(|s| s.host.as_ref())
                    .is_some_and(|h| h.id == id)
            });
            let Some(client) = viewing.or_else(|| self.clients.values().next()) else {
                continue;
            };
            let colors = client.colors;
            let Some(host) = self.hosts.get_mut(&id) else {
                continue;
            };
            if host.online() && host.colors != Some(colors) {
                host.colors = Some(colors);
                host.link.send(HubMsg::Colors(colors));
            }
        }
    }

    /// Per render pass: which sessions are unreachable, and which host is
    /// connecting.
    pub(super) fn mark_host_state(&mut self) {
        let connecting = self
            .hosts
            .values()
            .find(|h| h.state == HostState::Connecting)
            .map(|h| h.alias.clone());
        for session in self.sessions.values_mut() {
            let offline = session
                .host
                .as_ref()
                .is_some_and(|h| self.hosts.get(&h.id).is_none_or(|h| !h.online()));
            session.set_offline(offline);
            session.set_connecting(connecting.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc::Receiver;

    use super::*;
    use crate::server::config::Config;
    use crate::server::keys::Command;
    use crate::server::window::Tab;
    use termwiz::input::{KeyCode, Modifiers};

    const HOST: HostId = 0;
    const HUB_CONN: ConnId = 1;

    fn server() -> (Server, Receiver<ServerEvent>) {
        let (tx, rx) = mpsc::channel();
        let server = Server {
            sessions: BTreeMap::new(),
            clients: HashMap::new(),
            attach_order: Vec::new(),
            config: Arc::new(Config::default()),
            clipboard: None,
            next_session_id: 0,
            save_deadline: None,
            last_saved: None,
            tx,
            hosts: BTreeMap::new(),
            next_host_id: 0,
            pending_attaches: HashMap::new(),
            hub: None,
            pending_hubs: Vec::new(),
            instance: next_token(),
            exe: std::path::PathBuf::new(),
            listener: -1,
            pending_upgrade: None,
        };
        (server, rx)
    }

    /// A hub and a remote joined in-process in place of ssh: the hub's
    /// messages go straight to the remote, and the remote's frames come
    /// back over a socket pair.
    struct Pair {
        hub: Server,
        remote: Server,
        remote_events: Receiver<ServerEvent>,
        to_remote: Receiver<HubMsg>,
        from_remote: Receiver<RemoteMsg>,
        _hub_events: Receiver<ServerEvent>,
    }

    impl Pair {
        fn new() -> Self {
            Self::with_version(wire::version())
        }

        fn with_version(version: String) -> Self {
            let (mut hub, hub_events) = server();
            let (mut remote, remote_events) = server();
            remote
                .create_session(Some("work".into()), Rect::new(0, 0, 80, 24))
                .unwrap();
            let (near, far) = UnixStream::pair().unwrap();
            remote.hub_attach(HUB_CONN, near);
            let (frames_tx, from_remote) = mpsc::channel();
            thread::spawn(move || {
                let mut far = BufReader::new(far);
                while let Ok(Some(msg)) = wire::read_frame::<RemoteMsg>(&mut far) {
                    if frames_tx.send(msg).is_err() {
                        return;
                    }
                }
            });
            let (tx, to_remote) = mpsc::channel();
            let mut host = Host::new("devbox".into());
            host.pending_tx = Some(tx.clone());
            hub.hosts.insert(HOST, host);
            hub.next_host_id = HOST + 1;
            tx.send(HubMsg::Hello {
                version,
                instance: hub.instance,
            })
            .unwrap();
            Self {
                hub,
                remote,
                remote_events,
                to_remote,
                from_remote,
                _hub_events: hub_events,
            }
        }

        /// One pass of both sides. Returns whether anything happened.
        fn step(&mut self) -> bool {
            let mut busy = false;
            while let Ok(msg) = self.to_remote.try_recv() {
                self.remote.hub_msg(HUB_CONN, msg);
                busy = true;
            }
            while let Ok(event) = self.remote_events.try_recv() {
                self.remote.handle(event);
                busy = true;
            }
            self.remote.pump_hub();
            while let Ok(msg) = self.from_remote.recv_timeout(Duration::from_millis(5)) {
                if self.hub.hosts.contains_key(&HOST) {
                    self.hub.host_msg(HOST, msg);
                }
                busy = true;
            }
            self.hub.sync_layouts();
            busy
        }

        /// Steps until `done` holds, or fails after a few seconds.
        fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                if !self.step() {
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        fn hub_session(&self) -> &Session {
            self.hub
                .sessions
                .values()
                .find(|s| s.name == "work")
                .expect("hub mirrors the session")
        }

        fn hub_session_mut(&mut self) -> &mut Session {
            self.hub
                .sessions
                .values_mut()
                .find(|s| s.name == "work")
                .expect("hub mirrors the session")
        }

        fn remote_session(&self) -> &Session {
            self.remote
                .sessions
                .values()
                .next()
                .expect("remote session")
        }

        /// The hub's text for each tab, alongside the remote's.
        fn texts(&self) -> (Vec<String>, Vec<String>) {
            let text = |s: &Session| {
                let mut tabs: Vec<(usize, String)> = s
                    .tabs()
                    .map(|t| {
                        let key = t.remote_id().unwrap_or(t.id);
                        let screen = t.engine.screen();
                        let text = screen
                            .lines_in_phys_range(0..screen.scrollback_rows())
                            .iter()
                            .map(|l| l.as_str().trim_end().to_string())
                            .collect::<Vec<_>>()
                            .join("\n");
                        (key, text)
                    })
                    .collect();
                tabs.sort();
                tabs.into_iter().map(|(_, text)| text).collect()
            };
            (text(self.hub_session()), text(self.remote_session()))
        }

        fn type_text(&mut self, text: &str) {
            let session = self.hub_session_mut();
            let (window, index) = session.focused_active();
            let tab: &mut Tab = session.tab_at_mut(window, index).unwrap();
            for ch in text.chars() {
                let code = if ch == '\r' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(ch)
                };
                tab.key_down(code, Modifiers::NONE);
            }
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            for session in self.remote.sessions.values_mut() {
                for tab in session.tabs_mut() {
                    tab.kill();
                }
            }
        }
    }

    #[test]
    fn a_hub_mirrors_a_hosts_sessions_and_types_into_them() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| {
            p.hub.hosts[&HOST].online() && p.hub.sessions.values().any(|s| s.name == "work")
        });
        assert!(pair.remote.adopted());
        assert!(pair.hub_session().is_remote());

        pair.type_text("echo lux-$((40+2))-ok\r");
        pair.until("the command's output", |p| {
            p.texts().0.iter().any(|t| t.contains("lux-42-ok"))
        });
        pair.until("the mirror to settle", |p| {
            let (hub, remote) = p.texts();
            hub == remote
        });
    }

    #[test]
    fn a_split_on_the_hub_spawns_on_the_host_and_an_exit_collapses_it() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| p.hub.hosts[&HOST].online());
        pair.hub_session_mut().run(Command::SplitSideBySide);
        pair.until("the host to lay out the split", |p| {
            p.remote_session().window_count() == 2
                && p.hub_session().tabs().all(|t| t.remote_id().is_some())
        });
        assert_eq!(pair.hub_session().window_count(), 2);

        pair.type_text("exit\r");
        pair.until("the split to close", |p| {
            p.hub_session().window_count() == 1 && p.remote_session().window_count() == 1
        });
    }

    #[test]
    fn a_hub_creates_renames_and_kills_sessions_on_its_host() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| p.hub.hosts[&HOST].online());
        pair.hub.new_remote_session(99, HOST, Some("second".into()));
        pair.until("the new session", |p| {
            p.hub.sessions.values().any(|s| s.name == "second")
                && p.remote.sessions.values().any(|s| s.name == "second")
        });
        let sid = pair.hub.remote_session_by_name(HOST, "second").unwrap();
        pair.hub.end_session(sid);
        pair.until("the kill", |p| {
            p.remote.sessions.values().all(|s| s.name != "second")
        });
        assert!(pair.hub.remote_session_by_name(HOST, "second").is_none());
    }

    #[test]
    fn a_lost_host_goes_gray_and_drops_input_until_it_returns() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| p.hub.hosts[&HOST].online());
        pair.hub.host_closed(HOST, "network down".into());
        assert!(matches!(pair.hub.hosts[&HOST].state, HostState::Backoff(_)));
        pair.hub.mark_host_state();
        assert!(pair.hub_session().is_offline());
        pair.type_text("echo dropped\r");
        assert!(pair.to_remote.try_recv().is_err());
    }

    #[test]
    fn a_host_on_another_version_stays_unadopted() {
        let mut pair = Pair::with_version("0.0.0 (protocol 0)".into());
        pair.until("the host's answer", |p| p.to_remote.try_recv().is_err());
        pair.step();
        assert!(!pair.remote.adopted());
    }

    #[test]
    fn a_hub_refuses_a_host_on_another_version() {
        let mut pair = Pair::new();
        pair.hub.host_msg(
            HOST,
            RemoteMsg::Hello {
                version: "0.0.0 (protocol 0)".into(),
                instance: 7,
            },
        );
        assert!(!pair.hub.hosts.contains_key(&HOST));
        assert!(pair.hub.sessions.is_empty());
    }

    #[test]
    fn the_host_lifts_its_lock_when_the_hub_leaves() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| p.hub.hosts[&HOST].online());
        assert!(pair.remote.adopted());
        pair.remote.hub_gone(HUB_CONN);
        assert!(!pair.remote.adopted());
    }

    const CLI_CONN: ConnId = 50;

    /// Starts a CLI attach to `address`, returning the client's end of
    /// its control stream.
    fn cli_attach(hub: &mut Server, address: &str) -> UnixStream {
        let (near, far) = UnixStream::pair().unwrap();
        let null = || File::options().read(true).write(true).open("/dev/null");
        let (stdin, stdout) = (null().unwrap(), null().unwrap());
        hub.attach_remote(CLI_CONN, near, address, stdin.into(), stdout);
        far.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        far
    }

    fn answer(stream: &mut UnixStream) -> Option<String> {
        protocol::read_line(stream).ok().flatten()
    }

    #[test]
    fn a_cli_attach_waits_for_the_host_then_creates_the_session() {
        let mut pair = Pair::new();
        let mut cli = cli_attach(&mut pair.hub, "fresh@devbox");
        assert_eq!(answer(&mut cli), None);
        pair.until("the attach", |p| p.hub.clients.contains_key(&CLI_CONN));
        assert_eq!(answer(&mut cli).as_deref(), Some("ok"));
        let sid = pair.hub.remote_session_by_name(HOST, "fresh").unwrap();
        assert_eq!(pair.hub.clients[&CLI_CONN].attached, sid);
        assert!(pair.remote.sessions.values().any(|s| s.name == "fresh"));
    }

    #[test]
    fn a_cli_attach_joins_an_existing_session_on_a_connected_host() {
        let mut pair = Pair::new();
        pair.until("the snapshot", |p| p.hub.hosts[&HOST].online());
        let mut cli = cli_attach(&mut pair.hub, "work@devbox");
        assert_eq!(answer(&mut cli).as_deref(), Some("ok"));
        let sid = pair.hub.remote_session_by_name(HOST, "work").unwrap();
        assert_eq!(pair.hub.clients[&CLI_CONN].attached, sid);
        assert_eq!(pair.remote.sessions.len(), 1);
    }

    #[test]
    fn a_cli_attach_fails_when_the_connect_does() {
        let mut pair = Pair::new();
        let mut cli = cli_attach(&mut pair.hub, "work@devbox");
        pair.hub
            .host_closed(HOST, "Permission denied (publickey).".into());
        assert_eq!(
            answer(&mut cli).as_deref(),
            Some("err connect devbox: Permission denied (publickey).")
        );
        assert!(pair.hub.pending_attaches.is_empty());
        assert!(!pair.hub.clients.contains_key(&CLI_CONN));
    }

    #[test]
    fn a_cli_attach_refuses_an_unusable_alias() {
        let (mut hub, _events) = server();
        let mut cli = cli_attach(&mut hub, "work@dev.box");
        assert_eq!(
            answer(&mut cli).as_deref(),
            Some("err connect dev.box: not a usable ssh alias")
        );
        assert!(hub.hosts.is_empty());
    }

    #[test]
    fn addresses_split_at_the_last_at_sign() {
        assert_eq!(split_address("work"), ("work", None));
        assert_eq!(split_address("work@dev"), ("work", Some("dev")));
        assert_eq!(split_address("@dev"), ("", Some("dev")));
    }
}
