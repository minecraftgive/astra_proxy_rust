//! Admission checks only: these parsers never resolve names or open sockets.
use anyhow::{ensure, Result};
use astra_management::v2::NetworkDiagnosticsRequest;
use astra_model::nettool::{Egress, Probe};
use std::net::IpAddr;

pub(crate) const LEGACY_NOTICE: &str = "旧 Agent：仅系统路由 TCP/UDP-TCP DNS 或网卡 IP 端点；系统域名解析路径不可确认，不支持配置出口、bootstrap、后台报告和确认取消";

struct Endpoint<'a> {
    scheme: &'a str,
    literal: bool,
    bootstrap: bool,
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok_and(|value| value != 0)
}

/// Validate the whole authority before classifying its host. A valid-looking
/// prefix or bootstrap must not hide credentials, a bad port, or trailing text.
fn authority_host(authority: &str) -> Option<&str> {
    if authority.is_empty()
        || authority.chars().any(char::is_whitespace)
        || authority.contains(['@', '/', '?', '#', '\\'])
    {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        if !matches!(host.parse::<IpAddr>().ok()?, IpAddr::V6(_))
            || !(suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port))
        {
            return None;
        }
        return Some(host);
    }
    if authority.contains(['[', ']']) {
        return None;
    }
    if authority.parse::<IpAddr>().is_ok() {
        return Some(authority);
    }
    let host = match authority.split_once(':') {
        Some((host, port)) if valid_port(port) => host,
        Some(_) => return None,
        None => authority,
    };
    (!host.is_empty() && !host.contains(':')).then_some(host)
}

fn endpoint(text: &str) -> Option<Endpoint<'_>> {
    let text = text.trim();
    let text = if text.is_empty() { "1.1.1.1" } else { text };
    let (endpoint, bootstrap) = match text.split_once('#') {
        Some((endpoint, fragment)) => {
            fragment
                .strip_prefix("bootstrap=")?
                .parse::<IpAddr>()
                .ok()?;
            (endpoint, true)
        }
        None => (text, false),
    };
    let (scheme, authority) = endpoint.split_once("://").unwrap_or(("udp", endpoint));
    if !["udp", "tcp", "tls", "dot", "https", "stun"]
        .iter()
        .any(|known| scheme.eq_ignore_ascii_case(known))
    {
        return None;
    }
    let authority = if scheme.eq_ignore_ascii_case("https") {
        authority.split(['/', '?']).next()?
    } else {
        // Legacy parse_server accepts terminal slashes. Preserve those safe
        // aliases without accepting an actual path or truncating credentials.
        authority.trim_end_matches('/')
    };
    let host = authority_host(authority)?;
    Some(Endpoint {
        scheme,
        literal: host.parse::<IpAddr>().is_ok(),
        bootstrap,
    })
}

pub(super) fn encrypted_endpoint(server: &str) -> bool {
    ["https://", "tls://", "dot://"]
        .iter()
        .any(|scheme| server.trim().to_ascii_lowercase().starts_with(scheme))
}

pub(crate) fn encrypted_dns(probe: &Probe) -> bool {
    matches!(probe, Probe::Dns { server, .. } if encrypted_endpoint(server))
}

fn endpoint_needs_resolution(server: &str, bound: bool) -> bool {
    if encrypted_endpoint(server) {
        return true;
    }
    endpoint(server).is_none_or(|endpoint| endpoint.bootstrap || (bound && !endpoint.literal))
}

pub(super) fn needs_resolution(request: &NetworkDiagnosticsRequest) -> bool {
    if request.resolver.is_some() {
        return true;
    }
    let bound = matches!(request.egress, Egress::Interface(_));
    match &request.probe {
        Probe::Dns { server, .. } => endpoint_needs_resolution(server, bound),
        Probe::Tcp { host, .. }
        | Probe::TcpPorts { host, .. }
        | Probe::Tls { host, .. }
        | Probe::Ipv6 { host, .. }
        | Probe::Icmp { host }
        | Probe::Trace { host, .. } => bound && host.parse::<IpAddr>().is_err(),
        Probe::Http { url, .. } | Probe::HttpsIp { url, .. } => {
            bound
                && url
                    .split_once("://")
                    .and_then(|(_, rest)| authority_host(rest.split(['/', '?', '#']).next()?))
                    .is_none_or(|host| host.parse::<IpAddr>().is_err())
        }
        Probe::Stun { server } => {
            let server = server
                .strip_prefix("stun:")
                .filter(|rest| !rest.starts_with("//"))
                .unwrap_or(server);
            endpoint_needs_resolution(server, bound)
        }
        Probe::Nat { request } => request
            .servers
            .iter()
            .any(|server| endpoint_needs_resolution(server, true)),
    }
}

pub(crate) fn validate_legacy(request: &NetworkDiagnosticsRequest) -> Result<()> {
    ensure!(!matches!(request.probe, Probe::Icmp { .. }), "旧 Agent 无法确认 ICMP 是否由 TUN 本地处理，不能证明目标可达；请升级后使用 TUN 感知 ICMP。未启动探测");
    ensure!(
        request.family.is_none() && matches!(request.probe, Probe::Dns { .. } | Probe::Tcp { .. }),
        "旧 Agent 不支持此工具或地址族约束；未启动探测"
    );
    ensure!(
        !matches!(request.egress, Egress::Outbound(_)),
        "旧 Agent 无法保证配置出口的解析路径，包括 DIRECT；请升级后使用。未启动探测"
    );
    ensure!(
        request.resolver.is_none(),
        "旧 Agent 不支持约束解析器；未启动探测"
    );
    if let Probe::Dns { server, .. } = &request.probe {
        let endpoint = endpoint(server).filter(|endpoint| {
            ["udp", "tcp"]
                .iter()
                .any(|scheme| endpoint.scheme.eq_ignore_ascii_case(scheme))
                && !endpoint.bootstrap
        });
        ensure!(
            endpoint.is_some(),
            "旧 Agent 只支持无 bootstrap 的有效 UDP/TCP DNS 端点；未启动探测"
        );
    }
    ensure!(
        !needs_resolution(request),
        "旧 Agent 无法约束网卡目标或 DNS 服务名的系统解析；请填写 IP 或升级后使用。未启动探测"
    );
    Ok(())
}
