use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
    Frame,
};

pub fn popup(area: ratatui::layout::Rect, text: &str) -> ratatui::layout::Rect {
    astra_tui_kit::layout::content_popup(area, text)
}

pub fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    scroll: u16,
    block: Block<'_>,
    operations: bool,
) {
    frame.render_widget(Clear, area);
    if !operations {
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0))
                .block(block),
            area,
        );
        return;
    }

    let body = block.inner(area);
    let base = Style::default().fg(Color::Gray).bg(Color::Black);
    frame.render_widget(block.style(base), area);
    if body.is_empty() {
        return;
    }

    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut action_index = 0;
    let mut skip = usize::from(scroll);
    let mut y = body.y;
    for line in text.lines() {
        // Help entries separate their key and description with two spaces.
        // Empty keys still identify actions available through the palette.
        let (line, style) = if let Some((key, description)) = line.split_once("  ") {
            let background = if action_index % 2 == 0 {
                Color::Black
            } else {
                Color::Rgb(32, 32, 32)
            };
            action_index += 1;
            (
                Line::from(vec![
                    Span::styled(key, key_style),
                    Span::raw("  "),
                    Span::raw(description),
                ]),
                base.bg(background),
            )
        } else {
            (Line::from(line), base)
        };
        let paragraph = Paragraph::new(line).style(style).wrap(Wrap { trim: false });
        let height = paragraph.line_count(body.width);
        if skip >= height {
            skip -= height;
            continue;
        }
        let visible = (height - skip).min(usize::from(body.bottom() - y)) as u16;
        // Paint the full entry width, including wrapped lines and trailing space.
        frame.render_widget(
            paragraph.scroll((skip as u16, 0)),
            Rect::new(body.x, y, body.width, visible),
        );
        skip = 0;
        y += visible;
        if y == body.bottom() {
            break;
        }
    }
}
