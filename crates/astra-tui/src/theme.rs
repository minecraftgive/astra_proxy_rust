//! Shared colors so every screen marks the same things the same way.
use ratatui::style::{Color, Modifier, Style};

pub const BLUE: Color = Color::Rgb(79, 91, 213);
pub const MUTED: Color = Color::Rgb(140, 147, 161);

/// The row, tab or choice under the cursor; white on blue reads on every background.
pub fn selected() -> Style {
    Style::default().bg(BLUE).fg(Color::White)
}
/// Cursor row whose spans carry meaning in their colors (highlighted values); only the background changes.
pub fn selected_keep_colors() -> Style {
    Style::default().bg(BLUE)
}
/// A focused pane's cursor when keyboard focus has moved to a neighbouring pane.
pub fn selected_inactive() -> Style {
    Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
/// Text matching the current search or filter.
pub fn matched() -> Style {
    Style::default().bg(Color::Yellow).fg(Color::Black)
}
pub fn state_color(state: &str) -> Color {
    if state == "Running" {
        Color::Green
    } else if state == "Unresponsive" || state.starts_with("Exited") || state.starts_with("Failed")
    {
        Color::Red
    } else if state == "Starting" || state == "Stopping" {
        Color::Yellow
    } else {
        MUTED
    }
}
/// Key hints such as "c connect · Enter actions · Ctrl-K commands": keys in the accent
/// color, their descriptions muted, separators dimmer still.
pub fn key_hints(text: &str) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    // A key is a single symbol, a modifier chord (Ctrl-K), Fn, arrows, a named key,
    // or alternatives of those (Space/c, x/X); "connect/disconnect" is a description.
    fn is_key(token: &str) -> bool {
        const NAMES: &[&str] = &[
            "Enter",
            "Esc",
            "Space",
            "Tab",
            "Backspace",
            "Home",
            "End",
            "PgUp",
            "PgDn",
            "空格",
            "空白鍵",
        ];
        if token.chars().count() == 1 {
            return true;
        }
        if token.contains('/') {
            return token
                .split('/')
                .all(|part| !part.is_empty() && is_key(part));
        }
        ["Ctrl-", "Alt-", "Alt+", "Shift-"]
            .iter()
            .any(|modifier| token.starts_with(modifier))
            || token.chars().all(|c| "↑↓←→".contains(c))
            || (token.starts_with('F') && token[1..].chars().all(|c| c.is_ascii_digit()))
            || NAMES.contains(&token)
    }
    let key = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let description = Style::default().fg(MUTED);
    let mut spans = Vec::new();
    let leading = text.len() - text.trim_start().len();
    if leading > 0 {
        spans.push(Span::raw(text[..leading].to_owned()));
    }
    for (index, segment) in text.trim_start().split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
        }
        let tokens: Vec<&str> = segment.split(' ').collect();
        let keys = tokens
            .iter()
            .take_while(|t| !t.is_empty() && is_key(t))
            .count();
        if keys > 0 {
            spans.push(Span::styled(tokens[..keys].join(" "), key));
        }
        if keys < tokens.len() {
            let rest = tokens[keys..].join(" ");
            spans.push(Span::styled(
                if keys > 0 { format!(" {rest}") } else { rest },
                description,
            ));
        }
    }
    spans
}
/// Key hints that fit `width` columns: whole entries only, ending in `more`
/// ("? more") when some had to be left out, so no hint is cut mid-word.
pub fn key_hints_fit(text: &str, width: usize, more: &str) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Line;
    let columns = |s: &str| Line::from(s).width();
    if columns(text) <= width {
        return key_hints(text);
    }
    let leading = &text[..text.len() - text.trim_start().len()];
    let entries: Vec<&str> = text.trim_start().split(" · ").collect();
    let mut kept = leading.to_owned();
    for entry in entries {
        let candidate = if kept.trim().is_empty() {
            format!("{leading}{entry}")
        } else {
            format!("{kept} · {entry}")
        };
        if columns(&candidate) + columns(" · ") + columns(more) > width {
            break;
        }
        kept = candidate;
    }
    let shown = if kept.trim().is_empty() {
        format!("{leading}{more}")
    } else {
        format!("{kept} · {more}")
    };
    if columns(&shown) > width {
        return key_hints(&shown.chars().take(width).collect::<String>());
    }
    key_hints(&shown)
}
/// Tone of an operation message: failures red, finished work green, anything else neutral.
pub fn outcome_color(message: &str) -> Color {
    let lower = message.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if any(&[
        "fail",
        "error",
        "cannot",
        "unable",
        "unavailable",
        "失败",
        "错误",
        "無法",
        "无法",
        "不可用",
        "失敗",
        "錯誤",
    ]) {
        Color::Red
    } else if any(&["…", "...", "running", "waiting", "正在", "等待"]) {
        Color::Yellow
    } else if any(&[
        "completed",
        "saved",
        "applied",
        "started",
        "stopped",
        "updated",
        "imported",
        "完成",
        "成功",
        "已保存",
        "已应用",
        "已啟動",
        "已启动",
        "已停止",
        "已更新",
        "已導入",
        "已导入",
        "已儲存",
        "已套用",
    ]) {
        Color::Green
    } else {
        Color::Cyan
    }
}
/// Where a connection leaves: direct, rejected or through a proxy node/group.
pub fn exit_color(outbound: &str) -> Color {
    match outbound {
        "DIRECT" => Color::Green,
        "REJECT" => Color::Red,
        _ => BLUE,
    }
}
pub fn network_color(network: &str) -> Color {
    if network.eq_ignore_ascii_case("udp") {
        Color::Magenta
    } else {
        Color::Cyan
    }
}
/// Byte counters: idle flows fade, heavy ones stand out.
pub fn volume_style(bytes: u64) -> Style {
    if bytes < 1024 {
        Style::default().fg(MUTED)
    } else if bytes >= 1024 * 1024 {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}
/// Splits `text` into spans, marking every case-insensitive occurrence of `query`.
pub fn mark_matches(text: &str, query: &str, base: Style) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::text::Span;
    let lower = text.to_lowercase();
    let query = query.to_lowercase();
    // Lowercasing can change byte offsets outside ASCII; then mark the whole line.
    if query.is_empty() || lower.len() != text.len() {
        return if !query.is_empty() && lower.contains(&query) {
            vec![Span::styled(text.to_owned(), matched())]
        } else {
            vec![Span::styled(text.to_owned(), base)]
        };
    }
    let mut spans = Vec::new();
    let mut at = 0;
    for (start, _) in lower.match_indices(&query) {
        if start < at
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(start + query.len())
        {
            continue;
        }
        if start > at {
            spans.push(Span::styled(text[at..start].to_owned(), base));
        }
        spans.push(Span::styled(
            text[start..start + query.len()].to_owned(),
            matched(),
        ));
        at = start + query.len();
    }
    if at < text.len() || spans.is_empty() {
        spans.push(Span::styled(text[at..].to_owned(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
