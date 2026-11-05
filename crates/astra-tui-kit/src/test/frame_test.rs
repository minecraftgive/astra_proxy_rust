use super::*;
use ratatui::widgets::Paragraph;
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<Vec<u8>>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().push(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn take(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}
fn terminal(sink: &Sink) -> Screen<Sink> {
    let backend = CrosstermBackend::new(FrameOutput::new(sink.clone(), true));
    Terminal::with_options(
        backend,
        ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 20, 4)),
        },
    )
    .unwrap()
}
fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn each_frame_is_one_synchronized_write() {
    let sink = Sink::default();
    let mut terminal = terminal(&sink);
    draw(&mut terminal, |f| {
        f.render_widget(Paragraph::new("hello\nworld"), f.area())
    })
    .unwrap();
    let writes = sink.take();
    assert_eq!(writes.len(), 1);
    let frame = &writes[0];
    assert!(frame.starts_with(b"\x1b[?2026h"));
    assert!(frame.ends_with(b"\x1b[?2026l"));
    assert!(contains(frame, "hello") && contains(frame, "world"));
}

#[test]
fn unchanged_cells_are_not_resent() {
    let sink = Sink::default();
    let mut terminal = terminal(&sink);
    draw(&mut terminal, |f| {
        f.render_widget(Paragraph::new("hello\nworld"), f.area())
    })
    .unwrap();
    sink.take();
    draw(&mut terminal, |f| {
        f.render_widget(Paragraph::new("hello\nwOrld"), f.area())
    })
    .unwrap();
    let writes = sink.take();
    assert_eq!(writes.len(), 1);
    assert!(contains(&writes[0], "O"));
    assert!(!contains(&writes[0], "hello"));
    assert!(!contains(&writes[0], "rld"));
}

#[test]
fn invalidation_clears_inside_the_same_frame() {
    let sink = Sink::default();
    let mut terminal = terminal(&sink);
    draw(&mut terminal, |f| {
        f.render_widget(Paragraph::new("hi"), f.area())
    })
    .unwrap();
    sink.take();
    terminal.backend_mut().writer_mut().invalidate();
    draw(&mut terminal, |f| {
        f.render_widget(Paragraph::new("hi"), f.area())
    })
    .unwrap();
    let writes = sink.take();
    assert_eq!(writes.len(), 1, "clear and repaint arrive together");
    assert!(contains(&writes[0], "hi"));
}

#[test]
fn legacy_consoles_are_not_deferred() {
    let sink = Sink::default();
    let mut output = FrameOutput::new(sink.clone(), false);
    output.hold = true;
    output.write_all(b"x").unwrap();
    output.flush().unwrap();
    assert_eq!(sink.take(), vec![b"x".to_vec()]);
}
