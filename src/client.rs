//! The client process: it hands its terminal descriptors to the server and
//! stays out of the data path.

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

/// Hand the terminal to the server and block until it ends the connection.
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

    // The client owns terminal modes so it can restore them even if the
    // server dies. Bracketed paste keeps pasted newlines from arriving as
    // Enter keypresses.
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

    // The server reads the new size from the descriptor, so only the signal
    // is relayed.
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

    // Block until the server closes the stream, whether by detach or by
    // dying.
    while let Ok(Some(_)) = protocol::read_line(&mut stream) {}

    restore_terminal();
    0
}

fn restore_terminal() {
    // Reset the pointer shape the server may have set for boundary hover.
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

pub fn kill_session(name: &str) -> i32 {
    let Some(mut stream) = connect_existing() else {
        return 1;
    };
    if protocol::write_line(
        &mut stream,
        Request::KillSession(name.into()).encode().trim_end(),
    )
    .is_err()
    {
        eprintln!("lux: server connection failed");
        return 1;
    }
    match protocol::read_line(&mut stream) {
        Ok(Some(line)) if line == "ok" => 0,
        Ok(Some(line)) => {
            eprintln!("lux: {}", line.strip_prefix("err ").unwrap_or(&line));
            1
        }
        _ => {
            eprintln!("lux: server closed the connection");
            1
        }
    }
}

pub fn kill_server() -> i32 {
    let Some(mut stream) = connect_existing() else {
        return 1;
    };
    if protocol::write_line(&mut stream, Request::Kill.encode().trim_end()).is_err() {
        eprintln!("lux: server connection failed");
        return 1;
    }
    // Wait for the ack, or for the server's exit to close the stream.
    let _ = protocol::read_line(&mut stream);
    0
}

fn connect_existing() -> Option<UnixStream> {
    match UnixStream::connect(protocol::socket_path()) {
        Ok(stream) => Some(stream),
        Err(_) => {
            eprintln!("lux: no server running");
            None
        }
    }
}

fn connect_or_spawn() -> std::io::Result<UnixStream> {
    let path = protocol::socket_path();
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    // The socket may be stale from a dead server.
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
    for _ in 0..150 {
        std::thread::sleep(Duration::from_millis(20));
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
    }
    Err(std::io::Error::other("server did not start"))
}
