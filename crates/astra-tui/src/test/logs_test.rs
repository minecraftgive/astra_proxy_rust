use super::*;
use astra_client::input::{KeyCode, KeyEvent, KeyModifiers};
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

#[test]
fn shared_wrap_measurement_matches_terminal_layout() {
    for text in [
        "",
        "hello world",
        "   leading and trailing   ",
        "中文長字串中文",
        "e\u{301}👩‍💻 test",
        "a\u{a0}b c",
        "a\u{200b}b",
        "foo\nbar\n",
        "\t test",
        "  ",
    ] {
        for width in 1..=40 {
            assert_eq!(
                wrapped_rows(text, width),
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .max(1),
                "text={text:?} width={width}"
            );
        }
    }
}
