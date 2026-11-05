use super::*;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, StatefulWidget, Widget},
};

#[test]
fn colorful_selection_keeps_proxy_status_and_muted_text_visible() {
    let colors = [BLUE, Color::Green, Color::Red, MUTED];
    let line = Line::from(
        colors
            .iter()
            .map(|color| Span::styled("x", Style::default().fg(*color)))
            .collect::<Vec<_>>(),
    );
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    // Overview connection rows apply the selection before the span styles.
    Paragraph::new(line.clone().style(selected_keep_colors()))
        .render(Rect::new(0, 0, 4, 1), &mut buffer);
    // Editor lists apply the same selection after the span styles.
    StatefulWidget::render(
        List::new([ListItem::new(line)]).highlight_style(selected_keep_colors()),
        Rect::new(0, 1, 4, 1),
        &mut buffer,
        &mut ListState::default().with_selected(Some(0)),
    );
    for y in 0..2 {
        for (x, color) in colors.iter().enumerate() {
            let cell = &buffer[(x as u16, y)];
            assert_eq!(cell.fg, *color, "semantic foreground must be preserved");
            assert_eq!(Some(cell.bg), selected_keep_colors().bg);
            assert_ne!(cell.fg, cell.bg, "selection must not hide its text");
        }
    }
}
