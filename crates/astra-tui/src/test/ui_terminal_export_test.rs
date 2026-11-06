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
