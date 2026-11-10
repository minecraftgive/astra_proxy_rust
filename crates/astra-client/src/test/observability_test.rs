use super::*;
use crate::{
    commands::Id,
    input::{Key, KeyCode, KeyModifiers},
    protocol::Kind,
    state::{Client, Input},
};

#[test]
fn page_commands_do_not_edit_hidden_documents_or_logs() {
    let mut client = Client::new(Kind::Proxy);
    client.editor = Some(crate::editor::Editor::new(
        "profile".into(),
        "standalone=true\n".into(),
    ));
    client.viewer = Some(Viewer::new("profile".into(), Settings::default()));
    client.run_page_command(crate::navigation::CONNECTIONS, Id::Search);
    assert!(client.connection_filtering);
    assert!(client.editor.as_ref().unwrap().browsing());
    assert!(client.viewer.as_ref().unwrap().browsing());
    client.run_page_command(crate::navigation::LOGS, Id::Pause);
    assert!(client.viewer.as_ref().unwrap().paused());
    assert!(client.editor.as_ref().unwrap().browsing());
}
#[test]
fn log_controls_validate_limits_and_keep_frontend_source_independent() {
    let mut client = Client::new(Kind::Proxy);
    client.viewer = Some(Viewer::new("profile".into(), Settings::default()));
    let mut frontend = Viewer::new("@gui".into(), Settings::default());
    for text in ["INFO one", "ERROR needle", "INFO three"] {
        frontend.records.push_back(Record {
            text: text.into(),
            level: detect_level(text),
        });
    }
    frontend.bytes = frontend.records.iter().map(|r| r.text.len()).sum();
    assert!(frontend.set_memory_limit("0").is_err());
    assert_eq!(frontend.records.len(), 3);
    frontend.set_display_level(Some(LogLevel::Error));
    assert_eq!(frontend.visible().len(), 1);
    frontend.set_search("needle".into());
    assert!(!frontend.settings.follow);
    frontend.set_memory_limit("1").unwrap();
    assert_eq!(frontend.records.len(), 1);
    client.tui_viewer = Some(frontend);
    assert!(
        matches!(client.frontend_log_command(Command::Reconnect),Input::Run(Action::Logs(name),None)if name=="@gui")
    );
    client.frontend_log_command(Command::Close);
    assert!(client.tui_viewer.is_none());
    assert_eq!(client.viewer.as_ref().unwrap().name, "profile");
    assert_eq!(crate::tui_log::source_name("@gui"), Some("gui"));
    assert_eq!(crate::tui_log::source_name("@tui"), Some("tui"));
    assert_eq!(crate::tui_log::source_name("../gui"), None);
}
#[tokio::test]
async fn diagnostics_mouse_intents_share_validation_and_abort_stream() {
    let mut view =
        crate::diagnostics::View::start(std::path::Path::new("/missing-astra-socket"), "p".into());
    if let Some(task) = view.task.take() {
        task.abort();
    }
    view.receiver = None;
    view.loading = false;
    view.targets = vec!["first".into(), "second".into()];
    view.toggle_target(1);
    assert!(view.checked.contains("second"));
    view.toggle_target(9);
    assert_eq!(view.checked.len(), 1);
    view.select_all(false);
    view.run_selected();
    assert!(view.receiver.is_none());
    assert!(view.error.is_some());
    view.select_all(true);
    view.run_selected();
    assert!(view.receiver.is_some());
    view.stop();
    assert!(view.receiver.is_none());
    assert!(view.task.is_none());
    assert!(view.finished.is_some());
    view.key(Key::new(KeyCode::Char(' '), KeyModifiers::NONE));
    assert_eq!(view.checked.len(), 1);
}
#[test]
fn nettools_form_changes_keep_requests_valid_and_preserve_host() {
    use crate::nettools::{Field, Kind, View};
    let mut view = View::new(
        std::path::Path::new("/missing-astra-socket"),
        "p".into(),
        &["proxy-one".into()],
    );
    view.set_text(Field::Target, "example.com:8443".into())
        .unwrap();
    view.set_kind(Kind::Dns);
    assert_eq!(view.target, "example.com");
    assert!(view.fields().contains(&Field::Record));
    view.set_text(Field::Record, "AAAA".into()).unwrap();
    assert!(view.set_text(Field::Count, "0".into()).is_err());
    let choices = view.egress_choices("PROXY-ONE");
    assert_eq!(choices.len(), 1);
    view.select_egress(choices[0].0);
    assert!(
        matches!(view.request().unwrap().egress,astra_model::nettool::Egress::Outbound(name)if name=="proxy-one")
    );
    view.set_kind(Kind::Tcp);
    assert_eq!(view.target, "example.com:443");
    assert!(!view.fields().contains(&Field::Record));
}
#[test]
fn monitor_series_preserves_missing_samples_and_bounds_history() {
    let mut view = crate::monitor::View::new("p".into());
    for n in 0..125 {
        let snapshot=serde_json::from_value(serde_json::json!({"sampled_at_ms":(n*1000).to_string(),"instance":"p","pid":42,"uptime_secs":n.to_string(),"cpu_percent":if n==124{None}else{Some(25.0)},"memory_bytes":"2048","virtual_memory_bytes":"4096","upload_bytes":(n*100).to_string(),"download_bytes":(n*200).to_string()})).unwrap();
        view.accept(snapshot);
    }
    let series = view.series();
    assert_eq!(series.cpu.len(), crate::monitor::LIMIT);
    assert_eq!(series.cpu.last(), Some(&None));
    assert_eq!(series.upload.last(), Some(&Some(100.)));
    view.set_paused(true);
    assert!(view.paused);
    assert!(view.baseline.is_none());
    view.clear();
    assert!(view.series().cpu.is_empty());
    assert!(view.paused);
}
