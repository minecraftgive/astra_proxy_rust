//! Server presentation inside the same terminal shell, document editor and task system.
use super::*;
use ratatui::layout::Rect;
impl Ui {
    pub(super) fn server_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        Some(match key.code {
            KeyCode::Char('q') => Input::Quit,
            KeyCode::Char('c') => {
                self.operation(if self.selected().is_some_and(|r| r.pid.is_some()) {
                    Operation::Stop
                } else {
                    Operation::Start
                })
            }
            KeyCode::Char('r') => self.operation(Operation::Reload),
            KeyCode::Char('e') => self.operation(Operation::Edit),
            KeyCode::Char('L') | KeyCode::Enter if self.page == LOGS => {
                self.operation(Operation::Logs)
            }
            KeyCode::Char('L') => self.operation(Operation::Logs),
            KeyCode::Char('v') => match self.selected() {
                Some(row) => Input::Run(Action::Validate(row.instance.name.clone()), None),
                None => Input::Idle,
            },
            _ => return None,
        })
    }
    pub(super) fn render_server_header(&self, f: &mut Frame<'_>, area: Rect) {
        let selected = self.selected();
        let summary = selected
            .map(|r| {
                format!(
                    "Astra · Server / {} · {} · PID {} · {}",
                    r.instance.name,
                    r.state,
                    r.pid.map_or_else(|| "—".into(), |p| p.to_string()),
                    shell::duration(r.uptime)
                )
            })
            .unwrap_or_else(|| "Astra · Server · 6 Profiles → n Create".into());
        let mut lines = vec![Line::styled(summary, Style::default().fg(Color::Cyan))];
        if area.height > 1 {
            lines.push(Line::raw(
                "1 Overview · 2 Proxies · 3 Connections · 4 Rules · 5 Logs · 6 Profiles",
            ));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }
    pub(super) fn render_server(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(row) = self.selected() else {
            f.render_widget(Paragraph::new("No server selected. Open Profiles (6), select Servers and press n to initialize a configuration.").wrap(Wrap{trim:false}).block(self.focused_card("Server",focus::Region::Main)),area);
            return;
        };
        let mut lines = vec![
            Line::from(format!("{} · {}", row.instance.name, row.state)),
            Line::from(format!("Configuration: {}", row.instance.config.display())),
            Line::from(format!(
                "PID: {}    Uptime: {}",
                row.pid.map_or_else(|| "—".into(), |p| p.to_string()),
                shell::duration(row.uptime)
            )),
            Line::from(""),
        ];
        if self.page != shell::OVERVIEW {
            lines.push(Line::from(
                "This server exposes listener statistics and configuration management.",
            ));
            lines.push(Line::from("Client outbound groups and per-connection control are unavailable for this instance."));
        }
        if let Some(details) = self.details.get(&row.instance.name) {
            if let Some(status) = &details.server_status {
                lines.push(Line::from(format!(
                    "Connections: {} active / {} accepted / {} rejected",
                    status.active, status.accepted, status.rejected
                )));
                lines.push(Line::from(format!(
                    "Traffic: ↑ {} B / ↓ {} B",
                    status.upload, status.download
                )));
            }
            lines.push(Line::from("Listeners"));
            lines.extend(
                details
                    .listeners
                    .iter()
                    .map(|s| Line::from(format!("  {s}"))),
            );
        }
        lines.push(Line::from(""));
        lines.push(Line::from(
            "c Start / stop · r Reload · e Edit listeners and users · v Validate · L Logs",
        ));
        lines.push(Line::from("Lifecycle, document conflicts and background tasks use the same Agent service as the web."));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(self.focused_card("Server", focus::Region::Main)),
            area,
        );
    }
}
#[cfg(test)]
#[path = "../test/ui_server_test.rs"]
mod tests;
