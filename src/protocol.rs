//! The client-server protocol: one request line per Unix stream, with the
//! client's terminal descriptors passed as SCM_RIGHTS on attach.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use sendfd::{RecvWithFd, SendWithFd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Attach to a new auto-named session. Carries fds.
    New,
    /// Attach to the named session, creating it if needed. Carries fds.
    Session(String),
    /// Attach to the last attached session, or a new one if none. Carries fds.
    Recent,
    Ls,
    /// Stop the server.
    Kill,
    KillSession(String),
    Resize,
}

impl Request {
    pub fn encode(&self) -> String {
        match self {
            Request::New => "new\n".into(),
            Request::Session(name) => format!("session {name}\n"),
            Request::Recent => "recent\n".into(),
            Request::Ls => "ls\n".into(),
            Request::Kill => "kill\n".into(),
            Request::KillSession(name) => format!("kill-session {name}\n"),
            Request::Resize => "resize\n".into(),
        }
    }

    pub fn decode(line: &str) -> Option<Self> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        Some(match line.split_once(' ') {
            Some(("session", name)) if !name.is_empty() => Request::Session(name.into()),
            Some(("kill-session", name)) if !name.is_empty() => Request::KillSession(name.into()),
            None => match line {
                "new" => Request::New,
                "recent" => Request::Recent,
                "ls" => Request::Ls,
                "kill" => Request::Kill,
                "resize" => Request::Resize,
                _ => return None,
            },
            _ => return None,
        })
    }
}

pub fn socket_path() -> PathBuf {
    socket_dir().join("server.sock")
}

pub fn socket_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("lux"),
        _ => PathBuf::from(format!("/tmp/lux-{}", rustix::process::getuid().as_raw())),
    }
}

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

pub fn recv_request_with_fds(stream: &UnixStream) -> std::io::Result<(String, Vec<OwnedFd>)> {
    let mut buf = [0u8; 256];
    let mut fd_buf = [-1 as RawFd; 4];
    let (n, nfds) = stream.recv_with_fd(&mut buf, &mut fd_buf)?;
    let fds = fd_buf[..nfds]
        .iter()
        // SAFETY: the kernel just installed these fds in this process and
        // nothing else owns them.
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    Ok((String::from_utf8_lossy(&buf[..n]).into_owned(), fds))
}

pub fn write_line(stream: &mut UnixStream, line: &str) -> std::io::Result<()> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

pub fn read_line(stream: &mut UnixStream) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte)? {
            0 => {
                return Ok(if line.is_empty() {
                    None
                } else {
                    Some(String::from_utf8_lossy(&line).into_owned())
                });
            }
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
            Request::New,
            Request::Session("work".into()),
            Request::Recent,
            Request::Ls,
            Request::Kill,
            Request::KillSession("work".into()),
            Request::Resize,
        ] {
            assert_eq!(Request::decode(&req.encode()), Some(req));
        }
        assert_eq!(Request::decode("bogus\n"), None);
        assert_eq!(Request::decode("session \n"), None);
    }
}
