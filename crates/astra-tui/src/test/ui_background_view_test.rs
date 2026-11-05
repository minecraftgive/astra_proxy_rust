use super::*;
#[tokio::test]
async fn late_tui_log_failure_does_not_reopen_a_replaced_overlay_and_current_failure_is_visible() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for switch_to_palette in [true, false] {
                let fixture = crate::import::tests::Fixture::new();
                let socket = fixture.0.join("overlay-log.sock");
                let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
                let started = std::sync::Arc::new(tokio::sync::Notify::new());
                let release = std::sync::Arc::new(tokio::sync::Notify::new());
                let (entered, finish) = (started.clone(), release.clone());
                let server = tokio::spawn(async move {
                    use astra_management::v2::*;
                    let (stream, request) = test_rpc::next(&listener).await;
                    assert!(matches!(&request.body,RequestBody::Query(Query::LogLevel{instance}) if instance=="@tui"));
                    entered.notify_one();finish.notified().await;
                    test_rpc::error(stream,request,"TUI log unavailable in test").await;
                });
                let mut ui = Ui::new(Session::default());
                ui.editor = Some(Editor::new("draft".into(), "standalone=true\n".into()));
                let draft = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
                let mut jobs = Jobs::new(&socket);
                let Input::Run(action, origin) =
                    ui.key(event::KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
                else {
                    panic!("log request");
                };
                jobs.start(&mut ui, action, origin.map(|origin| *origin));
                assert!(ui.tui_viewer.is_some());
                tokio::time::timeout(Duration::from_secs(3), started.notified())
                    .await
                    .unwrap();
                if switch_to_palette {
                    ui.key(event::KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                }
                release.notify_one();
                server.await.unwrap();
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !jobs.jobs.is_empty() {
                        jobs.poll(&mut ui).await;
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                assert!(ui.tui_viewer.is_none());
                assert_eq!(
                    serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
                    draft
                );
                if switch_to_palette {
                    assert!(ui.palette.is_some());
                    assert!(ui.recovery.is_none());
                } else {
                    assert!(ui
                        .recovery
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("TUI log unavailable in test"));
                }
                crate::i18n::with_language(crate::i18n::Language::English, || {
                    let mut terminal =
                        Terminal::new(ratatui::backend::TestBackend::new(110, 30)).unwrap();
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                    let text: String = terminal
                        .backend()
                        .buffer()
                        .content
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect();
                    assert!(
                        text.contains(if switch_to_palette {
                            "Commands"
                        } else {
                            "TUI log unavailable in test"
                        }),
                        "{text}"
                    );
                });
                drop(jobs);
                astra_platform::platform::remove_socket(&socket);
            }
        })
        .await;
}
#[test]
fn details_and_tui_log_capture_input_without_touching_editor() {
    let mut ui = Ui::new(Session::default());
    ui.editor =
        Some(Editor::new("draft".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
    let draft = ui.editor.as_ref().unwrap().text();
    ui.tasks.records.push_back(Record {
        id: 1,
        title: "Save draft".into(),
        started: Instant::now(),
        finished: None,
        events: Default::default(),
    });
    ui.tasks
        .event(1, Progress::new("Validate configuration", "draft.toml"));
    for language in [
        crate::i18n::Language::English,
        crate::i18n::Language::Simplified,
        crate::i18n::Language::Traditional,
    ] {
        crate::i18n::with_language(language, || {
            for (width, height) in [(100, 30), (24, 8)] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                ui.tasks.details = None;
                terminal.draw(|frame| ui.render(frame)).unwrap();
                assert!(ui
                    .hits
                    .borrow()
                    .iter()
                    .any(|(_, target)| matches!(target, dashboard::Target::TaskDetails)));
                ui.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
                ui.paste("must not reach draft");
                terminal.draw(|frame| ui.render(frame)).unwrap();
                assert!(ui
                    .hits
                    .borrow()
                    .iter()
                    .all(|(_, target)| matches!(target, dashboard::Target::Back)));
                ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
                ui.paste("must not reach draft");
                terminal.draw(|frame| ui.render(frame)).unwrap();
                ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert!(ui.tui_viewer.is_none());
            }
        });
    }
    assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
    assert!(matches!(
        ui.key(event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )),
        Input::Quit
    ));
}
