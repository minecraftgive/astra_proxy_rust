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
mod tests {
    use super::*;

    struct Config(PathBuf);
    impl Config {
        fn new(name: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "astra-terminal-export-{name}-{}.toml",
                std::process::id()
            ));
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }
        fn ui(&self) -> Ui {
            let mut ui = Ui::new(Session::default());
            ui.page = PROFILES;
            ui.rows.push(Row {
                instance: Instance {
                    name: "proxy".into(),
                    kind: Kind::Proxy,
                    config: self.0.clone(),
                },
                pid: None,
                state: "Stopped".into(),
                uptime: 0,
                changed: false,
            });
            ui
        }
    }
    impl Drop for Config {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn terminal_export_copies_shell_commands_without_a_path_prompt() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let config = Config::new("success", "mixed_listen = '127.0.0.1:7890'\n");
            let mut ui = config.ui();
            let mut copied = None;
            ui.copy_terminal_commands_with(|text| {
                copied = Some(text.to_owned());
                Ok(())
            });
            assert_eq!(
                copied.unwrap(),
                crate::export::current_shell_commands("mixed_listen = '127.0.0.1:7890'\n").unwrap()
            );
            assert!(ui.prompt.is_none());
            assert!(ui.recovery.is_none());
            assert!(!ui.operation_failed);
            assert_eq!(ui.message, "Terminal proxy commands copied to clipboard");
            assert!(!ui.tasks.notice_is_old());
        });
    }

    #[test]
    fn terminal_export_reports_clipboard_errors_without_claiming_success() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let config = Config::new("failure", "mixed_listen = '127.0.0.1:7890'\n");
            let mut ui = config.ui();
            ui.copy_terminal_commands_with(|_| anyhow::bail!("clipboard busy"));
            assert!(ui.operation_failed);
            assert_eq!(
                ui.message,
                "Cannot copy terminal proxy commands: clipboard busy"
            );
            assert!(ui.prompt.is_none());
            assert!(ui.recovery.is_none());
        });
    }

    #[test]
    fn terminal_export_rejects_invalid_config_before_touching_clipboard() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let config = Config::new("invalid", "standalone = true\n");
            let mut ui = config.ui();
            ui.copy_terminal_commands_with(|_| panic!("must not replace clipboard"));
            assert!(ui.operation_failed);
            assert!(ui.message.contains("Configuration has no proxy listener"));
            assert!(ui.prompt.is_none());
        });
    }

    #[test]
    fn terminal_export_shortcut_and_palette_use_direct_copy_action() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let config = Config::new("shortcut", "standalone = true\n");
            let mut ui = config.ui();
            ui.page = shell::OVERVIEW;
            ui.key(event::KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE));
            assert!(ui.message.contains("Configuration has no proxy listener"));
            assert!(ui.prompt.is_none());
            assert!(ui
                .command_help()
                .contains("F4  Copy terminal proxy commands to clipboard"));
            ui.message.clear();
            ui.palette = Some(shell::Palette {
                query: "Copy terminal proxy commands to clipboard".into(),
                ..Default::default()
            });
            ui.palette_run(0);
            assert!(ui.palette.is_none());
            assert!(ui.message.contains("Configuration has no proxy listener"));
            assert!(ui.prompt.is_none());
            ui.page = PROFILES;
            ui.message.clear();
            ui.key(event::KeyEvent::new(
                KeyCode::Char('P'),
                KeyModifiers::SHIFT,
            ));
            assert!(ui.message.contains("Configuration has no proxy listener"));
            assert!(ui.prompt.is_none());
        });
    }

    #[test]
    fn terminal_export_uses_selected_profile_on_profiles_page() {
        let selected = Config::new("selected", "mixed_listen = '127.0.0.1:7890'\n");
        let active = Config::new("active", "mixed_listen = '127.0.0.1:1080'\n");
        let mut ui = selected.ui();
        let mut other = ui.rows[0].clone();
        other.instance.name = "active".into();
        other.instance.config = active.0.clone();
        ui.rows.push(other);
        ui.active_profile = Some("active".into());
        assert_eq!(
            ui.terminal_export_profile().unwrap().instance.config,
            selected.0
        );
        ui.page = shell::OVERVIEW;
        assert_eq!(
            ui.terminal_export_profile().unwrap().instance.config,
            active.0
        );
    }
}
