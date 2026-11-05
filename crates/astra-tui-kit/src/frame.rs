//! Frame-atomic terminal output.
//!
//! Ratatui already diffs each frame against the previous one and emits only the
//! changed cells. What still flickers is *how* those bytes arrive: `Stdout` is
//! line-buffered with a small buffer, so a large diff (page switch, resize,
//! popup) reaches the terminal in several chunks and the terminal paints the
//! half-written frame in between. On Windows every chunk is its own console
//! write, which makes the tearing very visible.
//!
//! [`FrameOutput`] holds everything a frame produces — cursor changes, the cell
//! diff, a pending full clear — and hands it to the terminal in one write,
//! bracketed by synchronized-update markers (DEC mode 2026) so terminals that
//! support them present the frame atomically. Terminals without support ignore
//! the markers and still receive a single contiguous write.
use crossterm::{
    queue,
    terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate},
};
use ratatui::{backend::CrosstermBackend, Frame, Terminal};
use std::io::{self, Stdout, Write};

pub struct FrameOutput<W: Write = Stdout> {
    inner: W,
    pending: Vec<u8>,
    hold: bool,
    /// Legacy Windows consoles execute commands through WinAPI the moment they
    /// are queued; holding bytes back there would reorder output, so only
    /// escape-sequence terminals defer.
    deferrable: bool,
    invalidated: bool,
}

impl FrameOutput<Stdout> {
    pub fn stdout() -> Self {
        Self::new(io::stdout(), ansi())
    }
}

impl<W: Write> FrameOutput<W> {
    fn new(inner: W, deferrable: bool) -> Self {
        Self {
            inner,
            pending: Vec::with_capacity(64 * 1024),
            hold: false,
            deferrable,
            invalidated: false,
        }
    }
    /// The next frame repaints every cell. The clear travels inside that frame's
    /// write instead of blanking the screen on its own.
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }
    fn commit(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return self.inner.flush();
        }
        let result = self
            .inner
            .write_all(&self.pending)
            .and_then(|()| self.inner.flush());
        // A failed write must not grow the buffer frame after frame.
        self.pending.clear();
        if self.pending.capacity() > 1 << 20 {
            self.pending.shrink_to(64 * 1024);
        }
        result
    }
}

impl<W: Write> Write for FrameOutput<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.hold && self.deferrable {
            return Ok(());
        }
        self.commit()
    }
}

#[cfg(windows)]
fn ansi() -> bool {
    crossterm::ansi_support::supports_ansi()
}
#[cfg(not(windows))]
fn ansi() -> bool {
    true
}

pub type ScreenBackend<W = Stdout> = CrosstermBackend<FrameOutput<W>>;
pub type Screen<W = Stdout> = Terminal<ScreenBackend<W>>;

pub fn screen() -> io::Result<Screen> {
    Terminal::new(CrosstermBackend::new(FrameOutput::stdout()))
}

/// Draw one frame and deliver it as a single synchronized write.
pub fn draw<W: Write>(
    terminal: &mut Screen<W>,
    render: impl FnOnce(&mut Frame<'_>),
) -> io::Result<()> {
    let out = terminal.backend_mut().writer_mut();
    out.hold = true;
    let sync = out.deferrable;
    let invalidated = std::mem::take(&mut out.invalidated);
    let drawn = (|| {
        if sync {
            queue!(terminal.backend_mut().writer_mut(), BeginSynchronizedUpdate)?;
        }
        if invalidated {
            terminal.clear()?;
        }
        terminal.draw(render).map(drop)
    })();
    let out = terminal.backend_mut().writer_mut();
    // Always close the update, or a supporting terminal stops presenting frames.
    let closed = if sync {
        queue!(out, EndSynchronizedUpdate).map(drop)
    } else {
        Ok(())
    };
    out.hold = false;
    let flushed = out.flush();
    if drawn.is_err() {
        out.invalidated = true;
    }
    drawn.and(closed).and(flushed)
}

#[cfg(test)]
#[path = "test/frame_test.rs"]
mod tests;
