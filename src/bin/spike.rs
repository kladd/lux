//! Phase 0 engine spike (throwaway; superseded by Phase 1).
//!
//! Validates that `wezterm-term` can be embedded as lux's terminal engine:
//! spawn a PTY running $SHELL, feed its output into a `Terminal`, and dump
//! the visible cell grid to stdout whenever it changes. No rendering, no
//! input passthrough, no pane management (REQ-SPIKE-008).

use std::io::Read;
use std::sync::Arc;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use wezterm_term::color::ColorPalette;
use wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

/// `wezterm_surface::SequenceNo`; the alias isn't re-exported by wezterm-term.
type SequenceNo = usize;

const ROWS: u16 = 24;
const COLS: u16 = 80;

#[derive(Debug)]
struct SpikeConfig;

impl TerminalConfiguration for SpikeConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // REQ-SPIKE-007
            eprintln!("lux spike: {err:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> anyhow::Result<i32> {
    // REQ-SPIKE-002: PTY running $SHELL.
    let pty = native_pty_system();
    let pair = pty.openpty(PtySize {
        rows: ROWS,
        cols: COLS,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut child = pair.slave.spawn_command(CommandBuilder::new(&shell))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    // REQ-SPIKE-003: Terminal sized to match the PTY.
    let mut terminal = Terminal::new(
        TerminalSize {
            rows: ROWS as usize,
            cols: COLS as usize,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        },
        Arc::new(SpikeConfig),
        "lux-spike",
        env!("CARGO_PKG_VERSION"),
        Box::new(writer),
    );

    let mut last_seqno = terminal.current_seqno();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            // EOF or EIO: child side of the PTY closed.
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // REQ-SPIKE-004
                terminal.advance_bytes(&buf[..n]);
                // REQ-SPIKE-005: dump only when visible rows actually changed
                // (advance_bytes bumps the seqno even for no-op input).
                if grid_changed(&terminal, last_seqno) {
                    dump_grid(&terminal);
                }
                last_seqno = terminal.current_seqno();
            }
        }
    }
    drop(reader);
    drop(pair.master);

    // REQ-SPIKE-006
    let status = child.wait()?;
    Ok(status.exit_code() as i32)
}

fn grid_changed(terminal: &Terminal, since: SequenceNo) -> bool {
    let screen = terminal.screen();
    let first = screen.visible_row_to_stable_row(0);
    let range = first..first + screen.physical_rows as isize;
    !screen.get_changed_stable_rows(range, since).is_empty()
}

fn dump_grid(terminal: &Terminal) {
    let screen = terminal.screen();
    println!("---- grid @ seqno {} ----", terminal.current_seqno());
    let visible = screen.phys_range(&(0..screen.physical_rows as i64));
    screen.with_phys_lines(visible, |lines| {
        for line in lines {
            println!("{}", line.as_str().trim_end());
        }
    });
}
