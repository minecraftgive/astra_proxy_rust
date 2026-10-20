use crate::{
    i18n::t,
    protocol::{self, Action},
    tr,
};
use astra_ipc::diagnostics::Report;
use ratatui::{
    layout::{Margin, Rect},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use std::path::Path;
use tokio::{sync::oneshot, task::JoinHandle};

pub struct View {
    pub name: String,
    task: JoinHandle<()>,
    receiver: oneshot::Receiver<anyhow::Result<Report>>,
    report: Option<Report>,
    error: Option<String>,
    pub scroll: u16,
}
impl Drop for View {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl View {
    pub fn start(socket: &Path, name: String) -> Self {
        let socket = socket.to_path_buf();
        let target = name.clone();
        let (sender, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = async {
                let reply = protocol::call(&socket, Action::Diagnostics(target)).await?;
                Ok(serde_json::from_str(&reply.text)?)
            }
            .await;
            let _ = sender.send(result);
        });
        Self {
            name,
            task,
            receiver,
            report: None,
            error: None,
            scroll: 0,
        }
    }
    pub fn poll(&mut self) {
        if self.report.is_some() || self.error.is_some() {
            return;
        }
        match self.receiver.try_recv() {
            Ok(Ok(report)) => self.report = Some(report),
            Ok(Err(error)) => self.error = Some(format!("{error:#}")),
            Err(oneshot::error::TryRecvError::Closed) => {
                self.error = Some(t("Diagnostics interrupted").into())
            }
            _ => {}
        }
    }
    fn content(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        let Some(report) = &self.report else {
            return t("Checking DIRECT and proxy exits… · Esc cancel").into();
        };
        let mut lines = vec![
            report.url.clone(),
            t(if report.running_config {
                "Using running configuration"
            } else {
                "Using saved configuration · instance stopped"
            })
            .into(),
            String::new(),
        ];
        for probe in &report.probes {
            let tag: String = probe.outbound.chars().filter(|c| !c.is_control()).collect();
            lines.push(if probe.direct {
                "DIRECT".into()
            } else {
                tr!("Proxy · {}", tag)
            });
            if let Some(ip) = probe.ip {
                lines.push(tr!("Connected · {} ms · Exit IP {}", probe.elapsed_ms, ip));
            } else {
                lines.push(match probe.connect_ms {
                    Some(ms) => tr!("Connected · {} ms · IP check failed", ms),
                    None => t("Connection failed").into(),
                });
                if let Some(error) = &probe.error {
                    lines.push(error.clone());
                }
            }
            lines.push(String::new());
        }
        lines.join("\n")
    }
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let popup = area.inner(Margin::new(2, 2));
        frame.render_widget(Clear, popup);
        frame.render_widget(
            Paragraph::new(self.content())
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(tr!("Connectivity / Exit IP · {}", self.name))
                        .title_bottom(t("↑↓ scroll · t retry · Esc close")),
                ),
            popup,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn report_renders_direct_proxy_ip_latency_and_errors_in_all_locales() {
        use astra_ipc::diagnostics::Probe;
        use ratatui::{backend::TestBackend, Terminal};
        let mut view = View::start(Path::new("unused-diagnostic-manager.sock"), "proxy".into());
        view.report = Some(Report {
            url: "https://api.ip.sb/ip".into(),
            running_config: true,
            probes: vec![
                Probe {
                    outbound: "DIRECT".into(),
                    direct: true,
                    connect_ms: Some(1),
                    elapsed_ms: 15,
                    ip: Some("192.0.2.1".parse().unwrap()),
                    error: None,
                },
                Probe {
                    outbound: "edge".into(),
                    direct: false,
                    connect_ms: Some(2),
                    elapsed_ms: 20,
                    ip: Some("2001:db8::1".parse().unwrap()),
                    error: None,
                },
                Probe {
                    outbound: "failed".into(),
                    direct: false,
                    connect_ms: None,
                    elapsed_ms: 8,
                    ip: None,
                    error: Some("refused".into()),
                },
            ],
        });
        let content = view.content();
        for text in ["DIRECT", "192.0.2.1", "edge", "2001:db8::1", "refused"] {
            assert!(content.contains(text));
        }
        for lang in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(lang, || {
                for (width, height) in [(100, 30), (40, 12), (10, 5)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal
                        .draw(|frame| view.render(frame, frame.area()))
                        .unwrap();
                }
            });
        }
    }
}
