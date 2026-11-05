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
#[path = "test/widgets_test.rs"]
mod tests;
