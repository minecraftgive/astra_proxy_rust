use crate::i18n::t;
pub use astra_client::explanation::*;
use ratatui::{
    layout::Rect,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.lines();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t("Route explanation"))
                        .title_bottom(t("↑↓ scroll · Esc close")),
                ),
            area,
        );
    }
}

#[cfg(test)]
#[path = "test/explanation_test.rs"]
mod tests;
