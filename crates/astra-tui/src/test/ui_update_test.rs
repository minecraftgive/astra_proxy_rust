use super::*;
use crate::i18n::{with_language, Language};
use crate::ui::{shell::Palette, Session};
use std::path::PathBuf;

#[test]
fn update_palette_actions_work_without_an_instance() {
    with_language(Language::English, || {
        let mut ui = Ui::new(Session::default());
        ui.update_status = Some(UpdateStatus::default());
        for (query, expected) in [("Update status", 0), ("Import local update package", 2)] {
            let palette = Palette {
                query: query.into(),
                ..Default::default()
            };
            ui.palette = Some(palette);
            match (expected, ui.palette_run(0)) {
                (0, Input::Run(Action::UpdateStatus, None)) => {}
                (2, Input::Idle) => assert!(matches!(ui.prompt, Some(Prompt::UpdatePackage))),
                _ => panic!("Unexpected update command"),
            }
        }
    });
}

#[test]
fn package_prompt_keeps_quoted_paths_and_rejects_multiple_lines() {
    let directory = std::env::current_dir().unwrap();
    let action = package_action("& 'release files/local update.zip'", &directory).unwrap();
    assert!(
        matches!(action, Action::UpdateTransaction {request:UpdateRequest {intent:UpdateIntent::Stage {package}, ..}} if package == directory.join("release files/local update.zip"))
    );
    for input in ["", "  ", "''", "one.zip\ntwo.zip", "one.zip\0"] {
        assert!(package_action(input, &directory).is_err());
    }
    let mut ui = Ui::new(Session::default());
    ui.working_dir = PathBuf::from(&directory);
    assert!(
        matches!(ui.submit(Prompt::UpdatePackage, "release.tar.gz".into()).unwrap(), Some(Action::UpdateTransaction {request:UpdateRequest {intent:UpdateIntent::Stage {package}, ..}}) if package == directory.join("release.tar.gz"))
    );
}

#[test]
fn transactions_require_capabilities_and_freeze_confirmed_target() {
    assert!(Command::Import.unavailable(None).is_some());
    let mut status = UpdateStatus::default();
    assert!(Command::Activate.request(&status).is_err());
    status.capabilities.activation_allowed = true;
    status.staged_release_id = Some("first".into());
    status.staged_compatible = Some(true);
    let request = Command::Activate.request(&status).unwrap();
    status.staged_release_id = Some("second".into());
    assert_eq!(
        request.intent,
        UpdateIntent::Activate {
            release_id: "first".into()
        }
    );
    assert!(Command::Rollback.request(&status).is_err());
    assert!(View::decode("{\"host_version\":\"legacy\"}").is_err());
    assert!(View::decode("{\"schema_version\":2}").is_err());
    assert!(View::decode(&serde_json::to_string(&status).unwrap()).is_ok());
}

#[test]
fn update_status_uses_the_selected_ui_language() {
    let status = UpdateStatus {
        manager_binary: None,
        runtime_directory: PathBuf::from("bundle"),
        ..Default::default()
    };
    for (language, expected) in [
        (Language::English, "Manager binary:"),
        (Language::Simplified, "管理器程序："),
        (Language::Traditional, "管理器程式："),
    ] {
        with_language(language, || {
            let rendered = View::new(status.clone())
                .lines()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rendered.contains(expected), "{rendered}");
            if language == Language::English {
                assert!(!rendered
                    .chars()
                    .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)));
            }
        });
    }
}
