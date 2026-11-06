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

    pub(super) fn activate_profile(&mut self, name: String) {
        self.suspend_editor();
        self.active_profile = Some(name);
        self.runtime_picker = None;
        self.focus = Default::default();
        if self.page == shell::LOGS {
            self.close_viewer();
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_drafts_test.rs"]
mod tests;
