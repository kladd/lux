//! `lux upgrade`: the server execs the executable it started from, handing
//! its tabs' PTYs, its clients' terminals, and everything shown on them to
//! the new image through an unlinked state file.

use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use ratatui::layout::Rect;
use rustix::io::FdFlags;
use rustix::process::{Pid, Signal, WaitOptions};
use serde::{Deserialize, Serialize};
use wezterm_term::Line;

use crate::protocol;
use crate::server::layout::WindowId;
use crate::server::persist::NodeSnapshot;
use crate::server::session::Session;
use crate::server::{ConnId, HostId, Server, ServerEvent, SessionId, park, switcher_order};

/// Bumped whenever `State` changes shape, including through the wezterm
/// types it carries.
pub const FORMAT: u32 = 1;

pub fn version() -> String {
    format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("LUX_BUILD"))
}

/// Answers whether this executable reads state in `format`, for the
/// server about to exec it.
pub fn check_format(format: &str) -> i32 {
    if format.parse() == Ok(FORMAT) {
        return 0;
    }
    eprintln!("it reads upgrade state format {FORMAT}, not {format}");
    1
}

#[derive(Serialize, Deserialize)]
pub struct State {
    format: u32,
    pub listener: RawFd,
    pub sessions: Vec<SessionState>,
    pub clients: Vec<ClientState>,
    /// Indexes into `sessions`, most recent last.
    pub attach_order: Vec<usize>,
    /// Aliases of the hosts that were connected.
    pub hosts: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SessionState {
    pub name: String,
    pub area: [u16; 4],
    pub tree: NodeSnapshot,
    pub windows: Vec<WindowState>,
    pub minimized: Vec<WindowId>,
    pub focus: WindowId,
}

#[derive(Serialize, Deserialize)]
pub struct WindowState {
    pub id: WindowId,
    pub rect: [u16; 4],
    pub active: usize,
    pub tabs: Vec<TabState>,
}

#[derive(Serialize, Deserialize)]
pub struct TabState {
    pub pty: RawFd,
    pub pid: Option<u32>,
    pub rect: [u16; 4],
    pub name: String,
    pub manual_name: bool,
    pub claude_session: Option<String>,
    pub running_claude: bool,
    pub osc_title: Option<String>,
    pub modes: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
    pub cursor_visible: bool,
    pub primary: ScreenState,
    /// Set while the program is on the alternate screen.
    pub alt: Option<ScreenState>,
}

#[derive(Serialize, Deserialize)]
pub struct ScreenState {
    /// History first, then the visible rows.
    pub lines: Vec<Line>,
    pub cursor_x: usize,
    pub cursor_y: i64,
}

#[derive(Serialize, Deserialize)]
pub struct ClientState {
    pub control: RawFd,
    pub stdin: RawFd,
    pub stdout: RawFd,
    pub attached: Attached,
}

#[derive(Serialize, Deserialize)]
pub enum Attached {
    /// An index into `State::sessions`.
    Local(usize),
    /// `name@alias`, attached again once the host reconnects.
    Remote(String),
}

impl State {
    fn fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        let tabs = self
            .sessions
            .iter()
            .flat_map(|s| &s.windows)
            .flat_map(|w| &w.tabs)
            .map(|t| t.pty);
        let clients = self
            .clients
            .iter()
            .flat_map(|c| [c.control, c.stdin, c.stdout]);
        std::iter::once(self.listener).chain(tabs).chain(clients)
    }
}

pub fn rect_state(rect: Rect) -> [u16; 4] {
    [rect.x, rect.y, rect.width, rect.height]
}

pub fn state_rect([x, y, width, height]: [u16; 4]) -> Rect {
    Rect::new(x, y, width, height)
}

/// Takes ownership of a descriptor the previous image left open.
///
/// # Safety
/// `fd` must be open and owned by nothing else in this process.
pub unsafe fn adopt_fd(fd: RawFd) -> std::io::Result<OwnedFd> {
    // SAFETY: borrowed only to check that it is open.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    rustix::io::fcntl_getfd(borrowed)?;
    let _ = rustix::io::fcntl_setfd(borrowed, FdFlags::CLOEXEC);
    // SAFETY: open, per the check above, and unowned, per the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Reads the state the previous image wrote to `fd`.
pub fn load(fd: RawFd) -> Result<State, String> {
    // SAFETY: the previous image passed this descriptor for the state
    // alone.
    let file = File::from(unsafe { adopt_fd(fd) }.map_err(|err| err.to_string())?);
    let state: State =
        bincode::deserialize_from(BufReader::new(file)).map_err(|err| err.to_string())?;
    if state.format != FORMAT {
        return Err(format!("state format {} is not {FORMAT}", state.format));
    }
    Ok(state)
}

/// Runs `exe` to ask whether it reads this build's state.
fn check(exe: &Path) -> Result<(), String> {
    let out = Command::new(exe)
        .args(["__upgrade-check", &FORMAT.to_string()])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("cannot run {}: {err}", exe.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let mut why = format!("{} cannot read this server's upgrade state", exe.display());
    // Only a build that knows the check explains itself.
    if out.status.code() == Some(1)
        && let Some(line) = String::from_utf8_lossy(&out.stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
    {
        why.push_str(": ");
        why.push_str(line);
    }
    Err(why)
}

fn set_inherited(fds: impl Iterator<Item = RawFd>, inherited: bool) {
    let flags = if inherited {
        FdFlags::empty()
    } else {
        FdFlags::CLOEXEC
    };
    for fd in fds {
        // SAFETY: every descriptor in the state is open for as long as
        // the server holds it, which outlasts this call.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = rustix::io::fcntl_setfd(fd, flags);
    }
}

/// Writes the state to a file nothing else can open.
fn state_file(state: &State) -> std::io::Result<File> {
    let path = protocol::socket_dir().join(format!("upgrade-{}", std::process::id()));
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    let mut out = BufWriter::new(file);
    bincode::serialize_into(&mut out, state).map_err(std::io::Error::other)?;
    out.flush()?;
    let mut file = out.into_inner().map_err(|err| err.into_error())?;
    file.rewind()?;
    Ok(file)
}

/// Replaces the process image. Returns only on failure.
fn exec(exe: &Path, state: &State, reply: &UnixStream) -> std::io::Error {
    let file = match state_file(state) {
        Ok(file) => file,
        Err(err) => return err,
    };
    let passed = || state.fds().chain([file.as_raw_fd(), reply.as_raw_fd()]);
    set_inherited(passed(), true);
    let err = Command::new(exe)
        .args([
            "__server",
            "--resume",
            &file.as_raw_fd().to_string(),
            &reply.as_raw_fd().to_string(),
        ])
        .exec();
    // Tabs spawned from here on would otherwise inherit them.
    set_inherited(passed(), false);
    err
}

/// Written ahead of an upgrade `:upgrade` started, for the new image to
/// find on its reply.
const EX_MARKER: &str = "ex";

impl Server {
    /// Queues an upgrade whose outcome shows as a status message, since
    /// no upgrade command is waiting on a reply.
    pub(super) fn start_ex_upgrade(&mut self, conn: ConnId) {
        if self.pending_upgrade.is_some() {
            self.tell(conn, "an upgrade is under way".into());
            return;
        }
        let pair = UnixStream::pair().and_then(|(mut ours, theirs)| {
            protocol::write_line(&mut ours, EX_MARKER)?;
            Ok((ours, theirs))
        });
        match pair {
            Ok((ours, theirs)) => {
                self.pending_upgrade = Some(theirs);
                self.ex_upgrade = Some((conn, ours));
            }
            Err(err) => self.tell(conn, format!("upgrade: {err}")),
        }
    }

    /// Shows the error an `:upgrade` that returned wrote to its reply.
    pub(super) fn report_ex_upgrade_failure(&mut self) {
        let Some((conn, mut ours)) = self.ex_upgrade.take() else {
            return;
        };
        let _ = ours.set_nonblocking(true);
        if let Ok(Some(line)) = protocol::read_line(&mut ours) {
            let err = line.strip_prefix("err ").unwrap_or(&line);
            self.tell(conn, format!("upgrade: {err}"));
        }
    }

    pub(super) fn upgrade(&mut self, mut reply: UnixStream, rx: &Receiver<ServerEvent>) {
        if let Err(err) = check(&self.exe) {
            let _ = protocol::write_line(&mut reply, &format!("err {err}"));
            return;
        }
        let parked = park::all();
        // What the readers took before parking still lands.
        while let Ok(event) = rx.try_recv() {
            self.handle(event);
        }
        let conns: Vec<ConnId> = self.clients.keys().copied().collect();
        for conn in conns {
            self.client_input_idle(conn);
        }
        // Each engine hands keystrokes to a writer thread of its own,
        // which exec would end mid-queue.
        std::thread::sleep(Duration::from_millis(50));
        self.save_sessions();
        let state = self.capture();
        self.drop_hosts();
        let err = exec(&self.exe, &state, &reply);
        let _ = protocol::write_line(
            &mut reply,
            &format!("err cannot run {}: {err}", self.exe.display()),
        );
        drop(parked);
        for alias in &state.hosts {
            if let Some(id) = self.host_by_alias(alias) {
                self.restart_host(id);
            }
        }
    }

    fn capture(&mut self) -> State {
        // In list order, which resuming keeps.
        let order = switcher_order(&self.sessions, &self.list_order);
        let local: Vec<SessionId> = order
            .iter()
            .copied()
            .filter(|sid| self.sessions.get(sid).is_some_and(|s| !s.is_remote()))
            .collect();
        let position = |sid: &SessionId| local.iter().position(|s| s == sid);
        let attach_order = self.attach_order.iter().filter_map(position).collect();
        let clients = self
            .clients
            .values()
            .filter_map(|client| {
                let attached = match position(&client.attached) {
                    Some(i) => Attached::Local(i),
                    None => Attached::Remote(self.session_address(client.attached)?),
                };
                Some(ClientState {
                    control: client.control.as_raw_fd(),
                    stdin: client.stdin.as_raw_fd(),
                    stdout: client.raw_out.as_raw_fd(),
                    attached,
                })
            })
            .collect();
        // Reconnected in this order, so their groups keep their places.
        let mut host_ids: Vec<HostId> = Vec::new();
        for id in order
            .iter()
            .filter_map(|sid| self.sessions.get(sid)?.host.as_ref().map(|h| h.id))
            .chain(self.hosts.keys().copied())
        {
            if !host_ids.contains(&id) {
                host_ids.push(id);
            }
        }
        let hosts = host_ids
            .iter()
            .filter_map(|id| self.hosts.get(id))
            .filter(|h| h.online())
            .map(|h| h.alias.clone())
            .collect();
        let sessions = local
            .iter()
            .filter_map(|sid| self.sessions.get_mut(sid).map(Session::handoff))
            .collect();
        State {
            format: FORMAT,
            listener: self.listener,
            sessions,
            clients,
            attach_order,
            hosts,
        }
    }

    /// `name@alias` for a session on a connected host.
    fn session_address(&self, sid: SessionId) -> Option<String> {
        let session = self.sessions.get(&sid)?;
        let host = self.hosts.get(&session.host.as_ref()?.id)?;
        Some(format!("{}@{}", session.name, host.alias))
    }

    /// Takes over what the previous image handed off, then tells the
    /// upgrade command which build now runs.
    pub(super) fn resume(&mut self, state: State, mut reply: Option<UnixStream>) {
        let mut sids: Vec<Option<SessionId>> = Vec::new();
        for snap in state.sessions {
            let sid = Session::resume(snap, self.config.clone(), self.tx.clone()).map(|session| {
                let sid = self.next_session_id;
                self.next_session_id += 1;
                self.sessions.insert(sid, session);
                sid
            });
            sids.push(sid);
        }
        self.attach_order = state
            .attach_order
            .iter()
            .filter_map(|&i| sids.get(i).copied().flatten())
            .collect();
        for alias in &state.hosts {
            if let Err(err) = self.add_host(alias) {
                self.tell_all(format!("connect {alias}: {err}"));
            }
        }
        for client in state.clients {
            self.resume_client(client, &sids);
        }
        for session in self.sessions.values_mut() {
            for tab in session.tabs_mut() {
                tab.prompt_redraw();
            }
        }
        let Some(reply) = reply.as_mut() else {
            return;
        };
        // The upgrade command sends nothing after its request, so only
        // `:upgrade` leaves a line to read.
        let _ = reply.set_nonblocking(true);
        if matches!(protocol::read_line(reply), Ok(Some(line)) if line == EX_MARKER) {
            self.tell_all(format!("upgraded to {}", version()));
        } else {
            let _ = reply.set_nonblocking(false);
            let _ = protocol::write_line(reply, &format!("ok {}", version()));
        }
    }

    fn resume_client(&mut self, client: ClientState, sids: &[Option<SessionId>]) {
        // SAFETY: the previous image passed these for this client alone.
        let fds = unsafe {
            (
                adopt_fd(client.control),
                adopt_fd(client.stdin),
                adopt_fd(client.stdout),
            )
        };
        let (Ok(control), Ok(stdin), Ok(stdout)) = fds else {
            return;
        };
        let control = UnixStream::from(control);
        let stdout = File::from(stdout);
        let conn = super::next_conn();
        let Ok(watched) = control.try_clone() else {
            return;
        };
        let tx = self.tx.clone();
        std::thread::spawn(move || super::watch_control(conn, watched, tx));
        match client.attached {
            Attached::Local(i) => {
                if let Some(sid) = sids.get(i).copied().flatten() {
                    self.seat_client(conn, control, sid, stdin, stdout);
                }
            }
            Attached::Remote(address) => self.attach_remote(conn, control, &address, stdin, stdout),
        }
    }
}

/// A PTY handed over by the previous image.
pub struct AdoptedPty(pub OwnedFd);

impl MasterPty for AdoptedPty {
    fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        rustix::termios::tcsetwinsize(
            &self.0,
            rustix::termios::Winsize {
                ws_row: size.rows,
                ws_col: size.cols,
                ws_xpixel: size.pixel_width,
                ws_ypixel: size.pixel_height,
            },
        )?;
        Ok(())
    }

    fn get_size(&self) -> anyhow::Result<PtySize> {
        let ws = rustix::termios::tcgetwinsize(&self.0)?;
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> anyhow::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(File::from(rustix::io::fcntl_dupfd_cloexec(
            &self.0, 0,
        )?)))
    }

    fn take_writer(&self) -> anyhow::Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(File::from(rustix::io::fcntl_dupfd_cloexec(
            &self.0, 0,
        )?)))
    }

    fn process_group_leader(&self) -> Option<i32> {
        rustix::termios::tcgetpgrp(&self.0)
            .ok()
            .map(Pid::as_raw_nonzero)
            .map(|pid| pid.get())
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.0.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// A tab's program, still this process's child across the exec.
#[derive(Debug, Clone, Copy)]
pub struct AdoptedChild(pub Option<Pid>);

impl AdoptedChild {
    fn reap(&self, options: WaitOptions) -> std::io::Result<Option<ExitStatus>> {
        let Some(pid) = self.0 else {
            return Ok(Some(ExitStatus::with_exit_code(0)));
        };
        Ok(
            rustix::process::waitpid(Some(pid), options)?.map(|(_, status)| {
                match status.terminating_signal() {
                    Some(signal) => ExitStatus::with_signal(&signal.to_string()),
                    None => ExitStatus::with_exit_code(status.exit_status().unwrap_or(0) as u32),
                }
            }),
        )
    }
}

impl ChildKiller for AdoptedChild {
    /// SIGHUP first, as for a spawned tab, then SIGKILL if it lingers.
    fn kill(&mut self) -> std::io::Result<()> {
        let Some(pid) = self.0 else {
            return Ok(());
        };
        rustix::process::kill_process(pid, Signal::HUP)?;
        for _ in 0..5 {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        rustix::process::kill_process(pid, Signal::KILL)?;
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(*self)
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.reap(WaitOptions::NOHANG)
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Ok(self
            .reap(WaitOptions::empty())?
            .unwrap_or_else(|| ExitStatus::with_exit_code(0)))
    }

    fn process_id(&self) -> Option<u32> {
        self.0.map(|pid| pid.as_raw_nonzero().get() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::window::Tab;

    #[test]
    fn state_survives_the_state_file() {
        let (tx, _) = std::sync::mpsc::channel();
        let mut tab = Tab::spawn_argv(Rect::new(0, 0, 20, 4), None, &["sleep", "30"], tx).unwrap();
        tab.advance(b"hello\r\n\x1b[?1000h");
        let state = State {
            format: FORMAT,
            listener: 7,
            sessions: vec![SessionState {
                name: "work".into(),
                area: [0, 0, 20, 5],
                tree: NodeSnapshot::Leaf(0),
                windows: vec![WindowState {
                    id: 0,
                    rect: [0, 0, 20, 5],
                    active: 0,
                    tabs: vec![tab.handoff().unwrap()],
                }],
                minimized: Vec::new(),
                focus: 0,
            }],
            clients: vec![ClientState {
                control: 8,
                stdin: 9,
                stdout: 10,
                attached: Attached::Remote("box@devbox".into()),
            }],
            attach_order: vec![0],
            hosts: vec!["devbox".into()],
        };
        std::fs::create_dir_all(protocol::socket_dir()).unwrap();
        let file = state_file(&state).unwrap();
        let loaded = load(std::os::fd::IntoRawFd::into_raw_fd(file)).unwrap();
        let tab_state = &loaded.sessions[0].windows[0].tabs[0];
        assert_eq!(tab_state.primary.lines[0].as_str().trim_end(), "hello");
        assert_eq!(tab_state.modes, state.sessions[0].windows[0].tabs[0].modes);
        assert!(matches!(&loaded.clients[0].attached, Attached::Remote(a) if a == "box@devbox"));
        assert_eq!(loaded.hosts, ["devbox"]);
        tab.kill();
    }

    #[test]
    fn only_this_format_passes_the_check() {
        assert_eq!(check_format(&FORMAT.to_string()), 0);
        assert_eq!(check_format(&(FORMAT + 1).to_string()), 1);
        assert_eq!(check_format("x"), 1);
    }
}
