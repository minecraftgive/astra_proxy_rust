use super::*;
#[test]
fn pause_freezes_display_while_collection_and_rotation_continue() {
    let mut view = Viewer::new("test".into(), Settings::default());
    view.settings.memory_lines = 1;
    view.push("INFO first".into());
    view.toggle_pause();
    view.push("INFO second".into());
    assert!(view.paused());
    assert_eq!(view.visible()[0].text, "INFO first");
    assert_eq!(view.records[0].text, "INFO second");
    view.apply(LogEvent::Data {
        bytes: b"WARN rotated\n".to_vec(),
        cursor: LogCursor {
            device: 1,
            inode: 2,
            offset: 13,
        },
        reset: true,
        skip_prefix: false,
    });
    assert_eq!(view.visible()[0].text, "INFO first");
    view.toggle_pause();
    assert!(!view.paused());
    assert_eq!(view.visible()[0].text, "WARN rotated");
    assert!(view.settings.follow);
}
#[test]
fn search_escape_restores_query_and_control_chords_do_not_pause() {
    let mut view = Viewer::new("test".into(), Settings::default());
    view.settings.search = "original".into();
    view.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    view.paste(" changed");
    view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(view.settings.search, "original");
    view.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(!view.paused());
}
#[test]
fn wrapping_preserves_unicode_tail_navigation_search_and_settings() {
    let mut view = Viewer::new("test".into(), Settings::default());
    view.records.push_back(Record {
        text: format!("INFO {}TAIL", "中文長字串".repeat(12)),
        level: Some(LogLevel::Info),
    });
    view.records.push_back(Record {
        text: "INFO END".into(),
        level: Some(LogLevel::Info),
    });
    view.key(key('w'));
    let screen = view.test_screen(30, 24);
    assert!(
        screen.contains("TAIL") && screen.contains("INFO END"),
        "{screen}"
    );
    assert!(view.display_len() > view.visible().len());
    let total = view.display_len();
    view.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        view.top,
        total
            .saturating_sub(view.viewport_height.get())
            .saturating_sub(1)
    );
    view.settings.search = "END".into();
    view.jump(false);
    assert_eq!(view.top, view.row_count(&view.records[0]));
    let restored: Settings =
        serde_json::from_str(&serde_json::to_string(&view.settings).unwrap()).unwrap();
    assert!(restored.wrap);
    view.key(key('w'));
    assert!(!view.settings.wrap);
    assert_eq!(view.display_len(), 2);
    for width in [10, 30, 80] {
        view.test_screen(width, 12);
    }
}
#[test]
fn vim_log_navigation_preserves_search_input_and_level_selection() {
    let mut view = Viewer::new("proxy".into(), Settings::default());
    view.settings.vim_navigation = true;
    for index in 0..30 {
        view.push(format!("INFO {index}"));
    }
    view.key(key('g'));
    view.key(key('g'));
    assert_eq!(view.top, 0);
    view.key(key('j'));
    assert_eq!(view.top, 1);
    view.key(key('k'));
    assert_eq!(view.top, 0);
    view.key(key('l'));
    assert_eq!(view.horizontal, 8);
    view.key(key('h'));
    assert_eq!(view.horizontal, 0);
    view.key(key('G'));
    assert!(view.settings.follow);
    view.key(key('/'));
    for ch in "hjklgg".chars() {
        view.key(key(ch));
    }
    assert!(matches!(&view.settings.mode,Mode::Search(text) if text=="hjklgg"));
    view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    view.key(key('v'));
    assert!(matches!(view.settings.mode, Mode::DisplayLevel(_)));
}
#[test]
fn memory_limit_is_editable_persistent_and_releases_old_rows_immediately() {
    let mut view = Viewer::new("proxy".into(), Settings::default());
    for i in 0..100 {
        view.push(format!("INFO {i}"));
    }
    view.key(key('m'));
    view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    view.paste("0");
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(view.records.len(), 100);
    assert!(matches!(view.settings.mode, Mode::MemoryLimit(_)));
    view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    view.paste("3");
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(view.records.len(), 3);
    assert_eq!(view.dropped, 97);
    assert_eq!(view.records.front().unwrap().text, "INFO 97");
    let settings = serde_json::from_slice(&serde_json::to_vec(&view.settings).unwrap()).unwrap();
    let restored = Viewer::new("proxy".into(), settings);
    assert_eq!(restored.settings.memory_lines, 3);
    view.settings.memory_lines = 100_000;
    for _ in 0..100 {
        view.push("x".repeat(64 * 1024));
    }
    assert!(view.bytes <= 2 * 1024 * 1024);
    assert!(view.records.len() <= 32);
}
#[tokio::test]
async fn real_log_stream_releases_rows_while_retaining_the_complete_local_file() {
    let fixture = crate::import::tests::Fixture::new();
    let text = (0..50)
        .map(|i| format!("INFO line {i}\n"))
        .collect::<String>();
    let path = fixture.write("proxy.log", &text);
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        use astra_management::{client, v2::*};
        let request: Request = client::receive(&mut stream).await.unwrap();
        assert!(matches!(
            request.body,
            RequestBody::Query(Query::LogChunk { .. })
        ));
        let bytes = std::fs::read(path).unwrap();
        let offset = bytes.len().to_string();
        client::send(
            &mut stream,
            &Response::success(
                request.id,
                ResponseBody::LogChunk(LogChunk::Data {
                    bytes,
                    cursor: LogCursor {
                        device: "1".into(),
                        inode: "1".into(),
                        offset,
                    },
                    reset: true,
                    skip_prefix: false,
                }),
            ),
        )
        .await
        .unwrap();
    });
    let mut view = Viewer::new(
        "proxy".into(),
        Settings {
            memory_lines: 3,
            ..Default::default()
        },
    );
    view.connect(&socket).await.unwrap();
    for _ in 0..100 {
        view.poll();
        if view.dropped == 47 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(view.records.len(), 3);
    assert_eq!(view.dropped, 47);
    assert_eq!(std::fs::read_to_string(&view.source).unwrap(), text);
    view.stop();
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    astra_platform::platform::remove_socket(&socket);
}
#[test]
fn route_is_between_debug_and_info_in_filter_and_capture_picker() {
    let mut view = Viewer::new("proxy".into(), Settings::default());
    view.push("DEBUG debug-detail".into());
    view.push("2026-10-19T00:00:00Z ROUTE astra_route: forwarding started".into());
    view.push("INFO ready".into());
    assert_eq!(view.records[1].level, Some(LogLevel::Route));
    view.settings.min_level = Some(LogLevel::Route);
    assert_eq!(view.visible().len(), 2);
    view.settings.min_level = Some(LogLevel::Info);
    assert_eq!(view.visible().len(), 1);
    view.key(key('L'));
    view.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert!(matches!(
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Command::Capture(LogLevel::Route)
    ));
    view.cancel_input();
    view.key(key('v'));
    for _ in 0..20 {
        view.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(view.settings.min_level, Some(LogLevel::Error));
}
#[test]
fn pending_level_picker_migrates_without_changing_its_selection() {
    let old =
        r#"{"filter":"","search":"","min_level":"info","follow":true,"mode":{"CaptureLevel":2}}"#;
    let settings: Settings = serde_json::from_str(old).unwrap();
    let mut view = Viewer::new("proxy".into(), settings);
    assert!(matches!(
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Command::Capture(LogLevel::Info)
    ));
}
#[test]
fn fixed_capture_logs_discard_pending_capture_and_keep_display_filtering() {
    let settings = Settings {
        mode: Mode::CaptureLevel(LogLevel::Debug.severity() as usize),
        ..Settings::default()
    };
    let mut view = Viewer::new("server".into(), settings);
    view.set_capture_supported(false);
    assert!(view.browsing());
    assert!(matches!(view.key(key('L')), Command::None));
    assert!(view.browsing());
    assert!(matches!(
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Command::None
    ));
    let screen = view.test_screen(120, 24);
    assert!(!screen.contains("info"), "A fixed capture level is unknown");

    view.push("DEBUG diagnostic".into());
    view.push("ERROR failed".into());
    view.key(key('v'));
    assert!(matches!(view.settings.mode, Mode::DisplayLevel(_)));
    for _ in 0..LogLevel::ALL.len() {
        view.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert!(matches!(
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Command::None
    ));
    assert_eq!(view.settings.min_level, Some(LogLevel::Error));
    assert_eq!(view.visible().len(), 1);
    assert_eq!(view.visible()[0].text, "ERROR failed");
}
fn event(bytes: &[u8], offset: u64, reset: bool) -> LogEvent {
    LogEvent::Data {
        bytes: bytes.to_vec(),
        cursor: LogCursor {
            device: 1,
            inode: 2,
            offset,
        },
        reset,
        skip_prefix: false,
    }
}
fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
#[test]
fn fragmented_utf8_ansi_and_rotation_keep_valid_complete_lines() {
    let mut view = Viewer::new("test".into(), Settings::default());
    let text = "2026-10-18 INFO target: 中文\n".as_bytes();
    let split = text.len() - 3;
    view.apply(event(&text[..split], split as u64, true));
    assert!(view.records.is_empty());
    view.apply(event(&text[split..], text.len() as u64, false));
    assert_eq!(view.records[0].text, "2026-10-18 INFO target: 中文");
    view.apply(event(b"\x1b[31mERROR bad\x1b[0m\n", 100, false));
    assert_eq!(view.records[1].text, "ERROR bad");
    assert_eq!(view.records[1].level, Some(LogLevel::Error));
    view.apply(event(b"WARN rotated\n", 13, true));
    assert_eq!(view.records.back().unwrap().text, "WARN rotated");
    assert!(view.records.iter().any(|r| r.text.contains("轮换")));
}
#[test]
fn search_filter_and_level_are_independent_and_follow_does_not_stop_ingestion() {
    let mut view = Viewer::new("test".into(), Settings::default());
    view.apply(event(
        "INFO alpha\nDEBUG beta ALPHA\nERROR 中文 Alpha\nplain stderr\n".as_bytes(),
        70,
        true,
    ));
    view.settings.filter = "aLpHa".into();
    assert_eq!(view.visible().len(), 3);
    view.settings.min_level = Some(LogLevel::Warn);
    assert_eq!(view.visible().len(), 1);
    assert!(view.visible()[0].text.contains("中文"));
    view.settings.filter.clear();
    assert_eq!(view.visible().len(), 2); // Unlevelled stderr remains visible.
    view.settings.min_level = None;
    view.settings.search = "beta".into();
    view.jump(false);
    assert_eq!(view.top, 1);
    assert!(!view.settings.follow);
    view.apply(event(b"INFO new\n", 80, false));
    assert_eq!(view.records.len(), 5);
    assert_eq!(view.top, 1);
    view.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert!(view.settings.follow);
    view.key(key('L'));
    let Command::Capture(level) = view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    else {
        panic!("capture choice")
    };
    assert_eq!(level, LogLevel::Info);
    assert!(view.settings.min_level.is_none());
}
#[test]
fn bounded_memory_and_pending_search_survive_settings_restore() {
    let mut view = Viewer::new("test".into(), Settings::default());
    for i in 0..6000 {
        view.push(format!("INFO line {i}"));
    }
    assert_eq!(view.records.len(), 5000);
    assert_eq!(view.dropped, 1000);
    view.key(key('/'));
    view.paste("pending 中文 search");
    let settings: Settings =
        serde_json::from_slice(&serde_json::to_vec(&view.settings).unwrap()).unwrap();
    assert!(matches!(settings.mode,Mode::Search(input) if input=="pending 中文 search"));
    view.apply(event(&vec![b'x'; 80 * 1024], 100000, false));
    assert!(view.partial.len() <= 64 * 1024);
    assert!(view.records.back().unwrap().text.contains("已截断"));
}
#[test]
fn log_view_renders_filter_status_and_level_picker() {
    use ratatui::{backend::TestBackend, Terminal};
    let mut view = Viewer::new("rules".into(), Settings::default());
    view.apply(event(b"INFO ready\nERROR failed\n", 30, true));
    view.settings.filter = "ready".into();
    view.settings.search = "INFO".into();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("INFO ready"));
    assert!(!text.contains("ERROR failed"));
    // Wide CJK cells include a trailing blank cell in TestBackend buffers.
    assert!(text.replace(' ', "").contains("过滤:ready"));
    assert!(text.replace(' ', "").contains("搜索:INFO"));
    view.key(key('L'));
    terminal
        .draw(|frame| view.render(frame, frame.area()))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.replace(' ', "").contains("记录级别"));
    assert!(text.contains("TRACE"));
    assert!(text.contains("ERROR"));
}
