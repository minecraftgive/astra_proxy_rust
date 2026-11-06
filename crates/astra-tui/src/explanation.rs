use crate::{i18n::t, tr};
pub use astra_client::explanation::*;
use ratatui::{
    layout::Rect,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
pub trait Render {
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
impl Render for View {
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let report = &self.report;
        let mut lines = vec![
            report.config.display().to_string(),
            String::new(),
            tr!(
                "Target: {}:{} · {}",
                report.request.target.host,
                report.request.target.port,
                format!("{:?}", report.request.network)
            ),
            tr!(
                "Process: {}",
                report
                    .request
                    .process
                    .as_ref()
                    .map(|p| if p.path.is_empty() {
                        p.name.as_str()
                    } else {
                        p.path.as_str()
                    })
                    .unwrap_or("—")
            ),
            tr!("Actual action: {}", action(&report.action)),
            tr!("Matched path: {}", report.path),
            tr!("Default action: {}", report.default_action),
            String::new(),
            tr!(
                "Running core: PID {} · revision {}",
                report.pid,
                report.revision
            ),
        ];
        if let Some((kind, value)) = &report.condition {
            lines.push(tr!("Matched condition: {} {}", kind, value));
        }
        if report.path.starts_with("rules[") {
            lines.push(t("Explicit rules take precedence over the default action").into());
        }
        if let Some(host) = single_label(&report.request.target.host) {
            lines.push(String::new());
            lines.push(tr!(
                "\"{}\" has no domain suffix, so it is routed as a local hostname (geosite private matches every name without a dot); query a full domain such as {}.com",
                host,
                host
            ));
        }
        lines.push(t("Query uses active rules; no traffic is forwarded").into());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines.join("\n"))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t("Route explanation"))
                        .title_bottom(t("↑↓ scroll · Esc close")),
                ),
            area,
        );
    }
}

#[cfg(test)]
#[path = "test/explanation_test.rs"]
mod tests;
