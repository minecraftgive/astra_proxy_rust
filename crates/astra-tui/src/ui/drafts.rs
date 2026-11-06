//! Per-configuration drafts. Navigation never writes the source configuration.
use super::*;

impl Ui {
    #[cfg(test)]
    pub(super) fn open_draft(&mut self, name: String, disk: String) {
        self.suspend_editor();
        let prepared = operations::OperationInput::capture(self).prepare(name.clone(), disk);
        self.apply_operation(
            operations::JobResult::Document {
                prepared,
                binding: None,
            },
            &Action::Read(name),
            operations::ApplyContext {
                same: true,
                log_target_current: true,
                background: false,
            },
        );
    }
}

#[cfg(test)]
#[path = "../test/ui_drafts_test.rs"]
mod tests;
