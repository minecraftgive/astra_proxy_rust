use crate::{layout::RenderedRows, theme::MUTED};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, List, ListState},
    Frame,
};

pub struct Panel<'a> {
    title: Line<'a>,
    focused: bool,
}

impl<'a> Panel<'a> {
    pub fn new(title: impl Into<Line<'a>>) -> Self {
        Self {
            title: title.into(),
            focused: false,
        }
    }
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
    pub fn block(self) -> Block<'a> {
        Block::default()
            .title(self.title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title_style(Style::default().fg(MUTED))
            .border_style(Style::default().fg(if self.focused {
                Color::Cyan
            } else {
                Color::DarkGray
            }))
    }
    pub fn render(self, frame: &mut Frame<'_>, area: Rect) -> Rect {
        let block = self.block();
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    }
}

pub struct SelectableList;
impl SelectableList {
    /// The caller supplies the same content rectangle used by the List block.
    pub fn render(
        frame: &mut Frame<'_>,
        list: List<'_>,
        area: Rect,
        content: Rect,
        state: &mut ListState,
        count: usize,
        row_height: u16,
    ) -> RenderedRows {
        frame.render_stateful_widget(list, area, state);
        RenderedRows::fixed(content, state.offset(), count, row_height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, text::Text, widgets::ListItem, Terminal};

    #[test]
    fn list_geometry_uses_renderer_scroll_after_two_line_selection_and_resize() {
        let mut state = ListState::default().with_selected(Some(8));
        for (width, height) in [(24, 8), (16, 6), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut rendered = RenderedRows::default();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    let block = Panel::new("Profiles").block();
                    let content = block.inner(area);
                    let items = (0..10)
                        .map(|index| ListItem::new(Text::from(format!("{index}\nstate"))))
                        .collect::<Vec<_>>();
                    rendered = SelectableList::render(
                        frame,
                        List::new(items).block(block),
                        area,
                        content,
                        &mut state,
                        10,
                        2,
                    );
                })
                .unwrap();
            assert_eq!(rendered.offset, state.offset());
            if width > 1 {
                assert!(rendered.offset > 0);
                assert!(rendered.rows.iter().any(|row| row.index == 8));
                assert_eq!(rendered.visible_capacity, (height as usize - 2) / 2);
            } else {
                assert!(rendered.rows.is_empty());
            }
        }
    }
}
