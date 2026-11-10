//! Window navigation stays independent of the originating editor and session.
use super::*;
use crate::windows::View;
impl Ui {
    pub(super) fn window_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if key.modifiers == KeyModifiers::ALT {
            if let KeyCode::Char(c @ '1'..='8') = key.code {
                return Some(Input::OpenWindow(self.client.detached_launch(
                    View::from_page(shell::page_at(c as usize - '1' as usize)),
                )));
            }
        }
        (key.code == KeyCode::F(8) && key.modifiers.is_empty())
            .then(|| self.client.detach_current())
    }
}
#[cfg(test)]
#[path = "../test/ui_detached_test.rs"]
mod tests;
