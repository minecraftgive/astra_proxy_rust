use crate::{i18n::t, tr};
use anyhow::{ensure, Context, Result};
use astra_ipc::API_VERSION;
use astra_model::control::RouteExplanation;
use astra_model::Inbound;
use astra_model::Network;
use astra_model::ProcessInfo;
use astra_model::RouteAction;
use astra_model::RouteRequest;
use astra_model::Target;
use ratatui::{
    layout::Rect,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

pub fn request(input: &str) -> Result<RouteRequest> {
    let input = input.trim();
    let (endpoint, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let rest = rest.trim();
    let (network, process) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    ensure!(
        !endpoint.is_empty(),
        t("Enter host[:port] [tcp|udp] [process name or path]")
    );
    let (host, port) = if let Ok(address) = endpoint.parse::<std::net::SocketAddr>() {
        (address.ip().to_string(), address.port())
    } else if let Ok(address) = endpoint.parse::<std::net::IpAddr>() {
        (address.to_string(), 443)
    } else if let Some((host, port)) = endpoint.rsplit_once(':') {
        ensure!(!host.contains(':'), t("Use [IPv6]:port for an IPv6 target"));
        (
            host.to_owned(),
            port.parse::<u16>().context(t("Invalid target port"))?,
        )
    } else {
        (endpoint.to_owned(), 443)
    };
    ensure!(
        !host.is_empty() && host.len() <= 253 && port != 0 && !host.contains(['/', '\\', '[', ']']),
        t("Invalid route query target")
    );
    let network = match network.to_ascii_lowercase().as_str() {
        "" | "tcp" => Network::Tcp,
        "udp" => Network::Udp,
        _ => anyhow::bail!(t("Enter host[:port] [tcp|udp] [process name or path]")),
    };
    let process = process.trim();
    let process = if process.is_empty() {
        None
    } else {
        let quoted = process.starts_with(['\'', '"']);
        let identity = if quoted {
            ensure!(
                process.len() >= 2 && process.ends_with(process.chars().next().unwrap()),
                t("Invalid process name or absolute path")
            );
            &process[1..process.len() - 1]
        } else {
            process
        };
        let absolute = identity.starts_with('/')
            || identity.starts_with("\\\\")
            || (identity.as_bytes().get(1) == Some(&b':')
                && matches!(identity.as_bytes().get(2), Some(b'/' | b'\\')));
        ensure!(
            !identity.trim().is_empty()
                && identity.len() <= 4096
                && !identity.contains('\0')
                && (absolute
                    || (!identity.contains(['/', '\\'])
                        && (quoted || !identity.chars().any(char::is_whitespace)))),
            t("Invalid process name or absolute path")
        );
        Some(ProcessInfo {
            pid: 0,
            name: identity.rsplit(['/', '\\']).next().unwrap().into(),
            path: if absolute {
                identity.into()
            } else {
                String::new()
            },
        })
    };
    Ok(RouteRequest {
        destination_ip: None,
        api_version: API_VERSION,
        id: 0,
        target: Target { host, port },
        network,
        inbound: Inbound::Socks5,
        process,
    })
}
/// A name without a dot (other than localhost) is a LAN/NetBIOS-style host,
/// not the site the user most likely meant to query.
fn single_label(host: &str) -> Option<&str> {
    let host = host.trim_end_matches('.');
    (!host.contains('.')
        && host.parse::<std::net::IpAddr>().is_err()
        && !host.eq_ignore_ascii_case("localhost"))
    .then_some(host)
}
fn action(value: &RouteAction) -> String {
    match value {
        RouteAction::Direct => "DIRECT".into(),
        RouteAction::Block => "REJECT".into(),
        RouteAction::Proxy(tag) => format!("proxy:{tag}"),
    }
}
pub struct View {
    pub report: RouteExplanation,
    pub scroll: u16,
}
impl View {
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
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
