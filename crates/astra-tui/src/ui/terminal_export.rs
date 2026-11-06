//! Terminal presentation for the shared clipboard operation.
use super::*;
impl Ui {
    pub(super) fn copy_terminal_commands(&mut self) -> Input {
        self.copy_terminal_commands_with(crate::clipboard::copy)
    }
    fn copy_terminal_commands_with(&mut self, copy: impl FnOnce(&str) -> Result<()>) -> Input {
        let input = self.client.copy_terminal_commands_with(copy);
        self.operation_steps.clear();
        self.operation_result = None;
        input
    }
}
#[cfg(test)]
#[path = "../test/ui_terminal_export_test.rs"]
mod tests;
