//! Copy the selected profile's shell-specific proxy assignments as plain text.
use super::*;

impl Ui {
    pub(super) fn terminal_export_profile(&self) -> Option<&Row> {
        if self.page == PROFILES && self.kind == Kind::Proxy {
            self.selected()
        } else {
            self.active_proxy()
        }
    }

    pub(super) fn copy_terminal_commands(&mut self) -> Input {
        self.copy_terminal_commands_with(crate::clipboard::copy)
    }

    fn copy_terminal_commands_with(&mut self, copy: impl FnOnce(&str) -> Result<()>) -> Input {
        let result = self
            .terminal_export_profile()
            .context(t("No profile yet"))
            .and_then(|row| {
                std::fs::read_to_string(&row.instance.config).with_context(|| {
                    format!("Read configuration {}", row.instance.config.display())
                })
            })
            .and_then(|text| crate::export::current_shell_commands(&text))
            .and_then(|text| copy(&text));
        self.operation_failed = result.is_err();
        self.message = match result {
            Ok(()) => t("Terminal proxy commands copied to clipboard").into(),
            Err(error) => tr!(
                "Cannot copy terminal proxy commands: {}",
                format!("{error:#}")
            ),
        };
        self.operation_steps.clear();
        self.operation_result = None;
        self.tasks
            .notice_result(&self.message, self.operation_failed);
        Input::Idle
    }
}

#[cfg(test)]
#[path = "../test/ui_terminal_export_test.rs"]
mod tests;
