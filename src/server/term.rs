//! A ratatui backend over a client's passed stdout descriptor.

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
    /// Tracked here because `CrosstermBackend::size()` queries the server's
    /// own stdout, which is /dev/null once daemonized.
    size: Size,
    synced: bool,
}

impl FdBackend {
    pub fn new(out: File, size: Size) -> Self {
        Self {
            // Big enough that a full-screen redraw reaches the terminal in
            // one write, even without DEC 2026.
            inner: CrosstermBackend::new(BufWriter::with_capacity(1 << 16, out)),
            size,
            synced: false,
        }
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    /// Ratatui repositions the cursor only after the diff, so without a
    /// synchronized update and a hidden cursor the cursor hops across
    /// changing cells and a resize flashes blank.
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
        // The render path never calls this, and a real answer would need
        // to read the client's tty.
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
        // BSU and Hide open the frame. ESU closes it after the cursor is
        // re-shown.
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
