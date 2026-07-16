//! The client process: connect (spawning the server if needed), perform
//! the attach handshake, then get out of the data path entirely — the
//! server reads input from and renders to the descriptors the client
//! passed. The client only relays resize signals and waits for the
//! connection to end.

use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};

use crate::protocol::{self, Request};

/// Create or attach to a session and hand the terminal to the server.
/// Returns the process exit code.
pub fn attach(request: Request) -> i32 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("lux: not a terminal");
        return 1;
    }
    let mut stream = match connect_or_spawn() {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!("lux: cannot reach server: {err}");
            return 1;
        }
    };

    // Save the terminal's mode and go raw before passing
    // descriptors (enable_raw_mode saves the mode disable_raw_mode
    // restores). The alt screen, mouse capture, and bracketed paste are
    // also the client's to set up and tear down, so restore works even if
    // the server died. Bracketed paste makes the terminal wrap pastes in
    // markers instead of sending newlines as Enter keypresses.
    if enable_raw_mode().is_err() {
        eprintln!("lux: cannot enter raw mode");
        return 1;
    }
    let _ = execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    );

    // Pass stdin and stdout to the server.
    let fds = [std::io::stdin().as_raw_fd(), std::io::stdout().as_raw_fd()];
    if protocol::send_request_with_fds(&stream, &request, &fds).is_err() {
        restore_terminal();
        eprintln!("lux: server handshake failed");
        return 1;
    }
    match protocol::read_line(&mut stream) {
        Ok(Some(line)) if line == "ok" => {}
        Ok(Some(line)) => {
            restore_terminal();
            eprintln!("lux: {}", line.strip_prefix("err ").unwrap_or(&line));
            return 1;
        }
        _ => {
            restore_terminal();
            eprintln!("lux: server closed the connection during attach");
            return 1;
        }
    }

    // Relay resize signals; the server reads the actual
    // dimensions from the descriptor itself.
    let winch_stream = stream.try_clone().ok();
    std::thread::spawn(move || {
        let Some(mut stream) = winch_stream else {
            return;
        };
        let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
        else {
            return;
        };
        for _ in signals.forever() {
            if protocol::write_line(&mut stream, "resize").is_err() {
                return;
            }
        }
    });

    // No reads from or writes to the host terminal from
    // here on. Block until the server ends the connection — deliberate
    // detach and lost connection are handled identically.
    while let Ok(Some(_)) = protocol::read_line(&mut stream) {}

    // Restore the terminal's original mode and exit.
    restore_terminal();
    0
}

fn restore_terminal() {
    // The server may have set a resize pointer shape (the boundary hover
    // cue); put the default back before leaving.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b]22;default\x1b\\");
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
    let _ = disable_raw_mode();
}

/// List sessions; errors if no server runs.
pub fn ls() -> i32 {
    let Some(mut stream) = connect_existing() else {
        return 1;
    };
    if protocol::write_line(&mut stream, Request::Ls.encode().trim_end()).is_err() {
        eprintln!("lux: server connection failed");
        return 1;
    }
    while let Ok(Some(line)) = protocol::read_line(&mut stream) {
        println!("{line}");
    }
    0
}

/// Terminate the server; errors if none runs.
pub fn kill_server() -> i32 {
    let Some(mut stream) = connect_existing() else {
        return 1;
    };
    if protocol::write_line(&mut stream, Request::Kill.encode().trim_end()).is_err() {
        eprintln!("lux: server connection failed");
        return 1;
    }
    // Wait for the ack (or the server's exit closing the stream).
    let _ = protocol::read_line(&mut stream);
    0
}

/// `ls`/`kill-server` never start a server.
fn connect_existing() -> Option<UnixStream> {
    match UnixStream::connect(protocol::socket_path()) {
        Ok(stream) => Some(stream),
        Err(_) => {
            eprintln!("lux: no server running");
            None
        }
    }
}

/// Connect to the server, spawning and daemonizing one first if none is
/// running.
fn connect_or_spawn() -> std::io::Result<UnixStream> {
    let path = protocol::socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    // No server (or a stale socket from a dead one).
    let _ = std::fs::remove_file(&path);
    let dir = protocol::socket_dir();
    std::fs::create_dir_all(&dir)?;
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    std::process::Command::new(std::env::current_exe()?)
        .arg("__server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // The server binds the socket as it comes up.
    for _ in 0..150 {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
    }
    Err(std::io::Error::other("server did not start"))
}
