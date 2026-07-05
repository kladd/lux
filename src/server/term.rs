//! A ratatui backend over a client's passed stdout descriptor
//! (REQ-SESSION-010/030). CrosstermBackend writes escape sequences to any
//! writer, but its `size()` queries the *server process's* stdout — which
//! is /dev/null once daemonized — so this wrapper tracks the client
//! terminal's size explicitly (updated via REQ-SESSION-032 resize
//! handling).
//!
//! Frames are bracketed in DEC 2026 synchronized updates with the cursor
//! hidden while cells are written: ratatui only repositions the cursor
//! *after* the diff, so without this the client terminal renders the
//! cursor hopping across changing cells, and a resize's clear + full
//! redraw shows up as a blank-screen flash.

use std::fs::File;
use std::io::{self, BufWriter};

use ratatui::backend::{Backend, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::crossterm::cursor::Hide;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::{Position, Size};

pub struct FdBackend {
    inner: CrosstermBackend<BufWriter<File>>,
    size: Size,
    /// Whether a synchronized update is open, i.e. we're mid-frame.
    synced: bool,
}

impl FdBackend {
    pub fn new(out: File, size: Size) -> Self {
        Self {
            // Large enough to hold a typical full-screen redraw, so a
            // frame reaches the terminal in one write even where DEC 2026
            // isn't supported.
            inner: CrosstermBackend::new(BufWriter::with_capacity(1 << 16, out)),
            size,
            synced: false,
        }
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    /// Open the frame's synchronized update before its first byte of
    /// output. The cursor stays hidden until ratatui re-shows it at its
    /// final position after the diff.
    fn begin_sync(&mut self) -> io::Result<()> {
        if !self.synced {
            self.synced = true;
            queue!(self.inner, BeginSynchronizedUpdate, Hide)?;
        }
        Ok(())
    }
}

impl Backend for FdBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.begin_sync()?;
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        // Querying would require reading the client's tty; nothing calls
        // this in lux's render path.
        Ok(Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.begin_sync()?;
        self.inner.clear()
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size,
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.synced {
            self.synced = false;
            queue!(self.inner, EndSynchronizedUpdate)?;
        }
        self.inner.flush()
    }
}

/// The client terminal's current dimensions, read from the passed
/// descriptor itself (REQ-SESSION-032).
pub fn fd_size(fd: &impl std::os::fd::AsFd) -> Size {
    match rustix::termios::tcgetwinsize(fd) {
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 0 => Size::new(ws.ws_col, ws.ws_row),
        _ => Size::new(80, 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn frame_bytes(name: &str, ops: impl FnOnce(&mut FdBackend)) -> Vec<u8> {
        let dir = std::env::temp_dir().join(format!("lux-term-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let out = File::create(&path).unwrap();
        let mut backend = FdBackend::new(out, Size::new(80, 24));
        ops(&mut backend);
        let mut bytes = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut bytes).unwrap();
        std::fs::remove_file(&path).ok();
        bytes
    }

    #[test]
    fn frames_are_synchronized_with_cursor_hidden_during_the_diff() {
        let cell = Cell::new("x");
        let bytes = frame_bytes("draw", |b| {
            b.draw([(0u16, 0u16, &cell)].into_iter()).unwrap();
            b.show_cursor().unwrap();
            b.flush().unwrap();
        });
        // BSU then Hide open the frame, ESU closes it after the cursor is
        // re-shown at its final position.
        assert!(bytes.starts_with(b"\x1b[?2026h\x1b[?25l"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
        let show = bytes.windows(6).position(|w| w == b"\x1b[?25h");
        let esu = bytes.windows(8).position(|w| w == b"\x1b[?2026l");
        assert!(show.unwrap() < esu.unwrap());
    }

    #[test]
    fn resize_clear_opens_the_synchronized_update() {
        let bytes = frame_bytes("clear", |b| {
            b.clear().unwrap();
            b.flush().unwrap();
        });
        assert!(bytes.starts_with(b"\x1b[?2026h\x1b[?25l"));
        assert!(bytes.ends_with(b"\x1b[?2026l"));
    }
}
