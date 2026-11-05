use super::*;
#[test]
fn fresh_connection_background_preserves_colored_text_and_selection() {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        text::{Line, Span},
        widgets::{Paragraph, Widget},
    };

    assert_eq!(connection_row_style(false, false), Style::default());
    assert_eq!(connection_row_style(true, false), selected_keep_colors());
    let fresh = connection_row_style(false, true);
    let selected_fresh = connection_row_style(true, true);
    assert_ne!(fresh.bg, selected_keep_colors().bg);
    assert_ne!(fresh.bg, selected_fresh.bg);
    assert!(selected_fresh.add_modifier.contains(Modifier::UNDERLINED));

    let colors = [BLUE, MUTED, Color::Red, Color::Green];
    for style in [fresh, selected_fresh] {
        let line = Line::from(
            colors
                .iter()
                .map(|color| Span::styled("x", Style::default().fg(*color)))
                .collect::<Vec<_>>(),
        );
        let area = Rect::new(0, 0, colors.len() as u16, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(line).style(style).render(area, &mut buffer);
        for (x, color) in colors.iter().enumerate() {
            let cell = &buffer[(x as u16, 0)];
            assert_eq!(cell.fg, *color);
            assert_eq!(Some(cell.bg), style.bg);
            assert_ne!(cell.fg, cell.bg);
        }
    }
}
#[test]
fn key_hints_separate_keys_from_descriptions() {
    let parts = |text: &str| -> Vec<(String, bool)> {
        key_hints(text)
            .into_iter()
            .filter(|s| s.content.trim() != "·" && !s.content.trim().is_empty())
            .map(|s| (s.content.trim().to_owned(), s.style.fg == Some(Color::Cyan)))
            .collect()
    };
    assert_eq!(
        parts(" c connect/disconnect · Ctrl-K commands · T TUN · Space/c connect · type to filter"),
        [
            ("c".into(), true),
            ("connect/disconnect".into(), false),
            ("Ctrl-K".into(), true),
            ("commands".into(), false),
            ("T".into(), true),
            ("TUN".into(), false),
            ("Space/c".into(), true),
            ("connect".into(), false),
            ("type to filter".into(), false),
        ]
    );
    assert_eq!(parts("c 连接/断开")[1], ("连接/断开".into(), false));
    assert_eq!(
        parts(": / Ctrl-K 命令 · ↑↓ 代理组 · F7 TUI 日志"),
        [
            (": / Ctrl-K".into(), true),
            ("命令".into(), false),
            ("↑↓".into(), true),
            ("代理组".into(), false),
            ("F7".into(), true),
            ("TUI 日志".into(), false),
        ]
    );
    let text: String = key_hints(" a b · c")
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(text, " a b · c", "text is kept verbatim");
}
#[test]
fn key_hints_drop_whole_entries_and_say_there_are_more() {
    let text = |spans: Vec<ratatui::text::Span>| -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    };
    let hints = " c connect · m mode · S system proxy · T TUN";
    assert_eq!(text(key_hints_fit(hints, 80, "? more")), hints);
    assert_eq!(
        text(key_hints_fit(hints, 32, "? more")),
        " c connect · m mode · ? more"
    );
    assert_eq!(text(key_hints_fit(hints, 12, "? more")), " ? more");
    assert!(text(key_hints_fit(hints, 3, "? more")).chars().count() <= 3);
}
#[test]
fn matches_keep_the_base_style_around_them() {
    let base = Style::default().fg(Color::Red);
    let spans = mark_matches("Error: Proxy proxy", "PROXY", base);
    let parts: Vec<_> = spans
        .iter()
        .map(|s| (s.content.as_ref(), s.style == matched()))
        .collect();
    assert_eq!(
        parts,
        [
            ("Error: ", false),
            ("Proxy", true),
            (" ", false),
            ("proxy", true)
        ]
    );
    assert_eq!(mark_matches("节点 abc", "ABC", base).len(), 2);
    assert_eq!(mark_matches("none", "", base)[0].style, base);
}
