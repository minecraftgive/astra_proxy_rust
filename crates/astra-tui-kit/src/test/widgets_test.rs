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
