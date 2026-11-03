//! Application state mapping; visual styles live in astra-tui-kit.
pub use astra_tui_kit::theme::{
    key_hints, key_hints_fit, mark_matches, matched, selected, selected_inactive,
    selected_keep_colors, BLUE, MUTED,
};
use ratatui::style::{Color, Modifier, Style};

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
