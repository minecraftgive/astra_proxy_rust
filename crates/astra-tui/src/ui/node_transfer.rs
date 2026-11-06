//! Terminal key mapping and presentation for shared node transfer.
use super::*;
#[cfg(test)]
use astra_client::node_transfer::export_path;
use astra_client::node_transfer::TransferAction;
impl Ui {
    pub(super) fn key_node_transfer(&mut self, key: event::KeyEvent) -> Option<Input> {
        self.key_node_transfer_with(key, crate::clipboard::copy)
    }
    fn key_node_transfer_with(
        &mut self,
        key: event::KeyEvent,
        copy: impl FnOnce(&str) -> Result<()>,
    ) -> Option<Input> {
        let action = match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => TransferAction::SwitchDestination,
            KeyCode::Up => TransferAction::Previous,
            KeyCode::Char('k')
                if matches!(self.prompt, Some(Prompt::ImportNodeSelection { .. })) =>
            {
                TransferAction::Previous
            }
            KeyCode::Down => TransferAction::Next,
            KeyCode::Char('j')
                if matches!(self.prompt, Some(Prompt::ImportNodeSelection { .. })) =>
            {
                TransferAction::Next
            }
            KeyCode::Home => TransferAction::First,
            KeyCode::End => TransferAction::Last,
            KeyCode::PageUp => TransferAction::PagePrevious,
            KeyCode::PageDown => TransferAction::PageNext,
            KeyCode::Char(' ') | KeyCode::Char('v') => TransferAction::Toggle,
            KeyCode::Char('a') | KeyCode::Char('V') => TransferAction::ToggleAll,
            KeyCode::Enter => TransferAction::Confirm,
            KeyCode::Esc => TransferAction::Cancel,
            _ => {
                return self
                    .prompt
                    .as_ref()
                    .filter(|p| p.picker())
                    .map(|_| Input::Idle)
            }
        };
        self.client.node_transfer_with(action, copy)
    }
    pub(super) fn render_node_transfer(&self, frame: &mut Frame<'_>) {
        use astra_client::node_transfer::{picker, Picker, PickerKind};
        let Some(Picker {
            items,
            cursor,
            title,
            warnings,
            kind,
        }) = self.prompt.as_ref().and_then(picker)
        else {
            return;
        };
        let footer = t(match kind {
            PickerKind::Export => "Tab / arrows choose · Enter export · Esc cancel",
            PickerKind::Import => "Space select · a all/clear · Enter import · Esc cancel",
        });
        let mut measure = format!("{title}\n{footer}\n{}", items.join("\n"));
        if !warnings.is_empty() {
            measure.push_str(&format!("\n{warnings}"));
        }
        let popup = crate::help::popup(frame.area(), &measure);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_bottom(footer);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let chunks = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(if warnings.is_empty() { 0 } else { 3 }),
        ])
        .split(inner);
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(Color::Cyan));
        frame.render_stateful_widget(
            list,
            chunks[0],
            &mut ListState::default().with_selected(Some(cursor)),
        );
        if !warnings.is_empty() {
            frame.render_widget(
                Paragraph::new(warnings)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow)),
                chunks[1],
            );
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_node_transfer_test.rs"]
mod tests;
