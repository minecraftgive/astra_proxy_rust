//! Terminal presentation of the shared update model.
#[cfg(test)]
use super::*;
#[cfg(test)]
use astra_client::update::package_action;
pub use astra_client::update::Restart;
pub use astra_client::update::View;
use ratatui::text::Line;
pub trait ViewRender {
    fn lines(&self) -> Vec<Line<'static>>;
}
impl ViewRender for View {
    fn lines(&self) -> Vec<Line<'static>> {
        self.summary()
            .into_iter()
            .map(|tokens| {
                Line::from(
                    tokens
                        .into_iter()
                        .map(|token| ratatui::text::Span::raw(token.content))
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "../test/ui_update_test.rs"]
mod tests;
