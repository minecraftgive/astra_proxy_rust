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
/// Newly observed connections use teal while retaining route and status colors.
pub fn connection_row_style(selected: bool, fresh: bool) -> Style {
    match (selected, fresh) {
        (false, false) => Style::default(),
        (true, false) => selected_keep_colors(),
        (false, true) => Style::default().bg(Color::Rgb(10, 36, 32)),
        (true, true) => Style::default()
            .bg(Color::Rgb(16, 48, 42))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
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
#[path = "test/theme_test.rs"]
mod tests;
