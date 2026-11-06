//! Shared operation execution and application; TUI runtime presentation stays local.
use super::*;
pub use astra_client::operations::*;
impl astra_client::operations::OperationHost for Ui {
    fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }
    fn accept_runtime(&mut self, name: String, snapshot: astra_model::runtime::Snapshot) {
        Ui::accept_runtime(self, name, snapshot);
    }
}
impl Ui {
    pub(super) fn apply_operation(
        &mut self,
        result: JobResult,
        action: &Action,
        context: ApplyContext,
    ) -> bool {
        astra_client::operations::apply_operation(self, result, action, context)
    }
}
#[cfg(test)]
#[path = "../test/ui_operations_test.rs"]
mod tests;
