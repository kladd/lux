//! The attach protocol shared by client and server — the only thing the
//! two sides communicate through.
//!
//! One Unix stream per client. The client sends a single request line; for
//! attach requests the same sendmsg carries its stdin and stdout file
//! descriptors as SCM_RIGHTS ancillary data. After a
//! successful attach the stream stays open as a control channel: the
//! client sends `resize` lines, and the server ending
//! the stream is the detach/end signal. All terminal
//! input and rendered output flow over the passed descriptors directly,
//! never through protocol messages.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use sendfd::{RecvWithFd, SendWithFd};

/// Client request line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Create a new session (optionally named) and attach.
    /// Carries fds.
    New(Option<String>),
    /// Attach to an existing named session. Carries fds.
    Attach(String),
    /// List session names.
    Ls,
    /// Terminate the server.
    Kill,
    /// The attached client's terminal was resized.
    Resize,
}

impl Request {
    pub fn encode(&self) -> String {
        match self {
            Request::New(None) => "new\n".into(),
            Request::New(Some(name)) => format!("new {name}\n"),
            Request::Attach(name) => format!("attach {name}\n"),
            Request::Ls => "ls\n".into(),
            Request::Kill => "kill\n".into(),
            Request::Resize => "resize\n".into(),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        Some(match line.split_once(' ') {
            Some(("new", name)) if !name.is_empty() => Request::New(Some(name.into())),
            Some(("attach", name)) if !name.is_empty() => Request::Attach(name.into()),
            None => match line {
                "new" => Request::New(None),
                "ls" => Request::Ls,
                "kill" => Request::Kill,
                "resize" => Request::Resize,
                _ => return None,
            },
            _ => return None,
        })
    }
}

/// `$XDG_RUNTIME_DIR/lux/server.sock`, falling back to
/// `/tmp/lux-$UID/server.sock`.
pub fn socket_path() -> PathBuf {
    socket_dir().join("server.sock")
}

pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("lux"),
        _ => PathBuf::from(format!("/tmp/lux-{}", rustix::process::getuid().as_raw())),
    }
}

/// Send a request line together with the fds to pass.
pub fn send_request_with_fds(
    stream: &UnixStream,
    request: &Request,
    fds: &[RawFd],
) -> std::io::Result<()> {
    let bytes = request.encode();
    let sent = stream.send_with_fd(bytes.as_bytes(), fds)?;
    if sent != bytes.len() {
        return Err(std::io::Error::other("short protocol write"));
    }
    Ok(())
}

/// Receive the initial request line, capturing any passed fds.
pub fn recv_request_with_fds(stream: &UnixStream) -> std::io::Result<(String, Vec<OwnedFd>)> {
    let mut buf = [0u8; 256];
    let mut fd_buf = [-1 as RawFd; 4];
    let (n, nfds) = stream.recv_with_fd(&mut buf, &mut fd_buf)?;
    let fds = fd_buf[..nfds]
        .iter()
        // Safety: recv_with_fd returns fds freshly installed in this
        // process by the kernel; we are their sole owner.
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    Ok((String::from_utf8_lossy(&buf[..n]).into_owned(), fds))
}

pub fn write_line(stream: &mut UnixStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Read one newline-terminated line; `None` on EOF.
pub fn read_line(stream: &mut UnixStream) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte)? {
            0 => return Ok(if line.is_empty() { None } else { Some(String::from_utf8_lossy(&line).into_owned()) }),
            _ => {
                if byte[0] == b'\n' {
                    return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
                }
                line.push(byte[0]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for req in [
            Request::New(None),
            Request::New(Some("work".into())),
            Request::Attach("work".into()),
            Request::Ls,
            Request::Kill,
            Request::Resize,
        ] {
            assert_eq!(Request::decode(&req.encode()), Some(req));
        }
        assert_eq!(Request::decode("bogus\n"), None);
        assert_eq!(Request::decode("new \n"), None);
    }
}
