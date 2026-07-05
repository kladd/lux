//! A ratatui backend over a client's passed stdout descriptor
//! (REQ-SESSION-010/030). CrosstermBackend writes escape sequences to any
//! writer, but its `size()` queries the *server process's* stdout — which
//! is /dev/null once daemonized — so this wrapper tracks the client
//! terminal's size explicitly (updated via REQ-SESSION-032 resize
//! handling).

use std::fs::File;
use std::io;

use ratatui::backend::{Backend, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

pub struct FdBackend {
    inner: CrosstermBackend<File>,
    size: Size,
}

impl FdBackend {
    pub fn new(out: File, size: Size) -> Self {
        Self {
            inner: CrosstermBackend::new(out),
            size,
        }
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }
}

impl Backend for FdBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
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
