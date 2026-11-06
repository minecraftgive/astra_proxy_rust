use super::*;
use std::path::PathBuf;

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
    fn ui(&self) -> Client {
        let mut ui = Client::from_session(Session::default());
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
    ui.page = OVERVIEW;
    assert_eq!(
        ui.terminal_export_profile().unwrap().instance.config,
        active.0
    );
}
