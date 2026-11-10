//! "完整检查": a fixed set of probes run as one backend batch, so the shared
//! analysis rules (DNS/certificate differences, HTTPS vs STUN exit, IPv4 vs
//! IPv6 exit) have the evidence they compare. No rules run here.
use astra_management::v2::{NetworkBatchProject, NetworkBatchRequest, NetworkDiagnosticsRequest};
use astra_model::nettool::{AddressFamily, Egress, Probe, Request};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Run DNS/TLS a second time; required for the difference rules.
    pub compare_rounds: bool,
    /// Add an IPv6 HTTPS exit observation beside the IPv4 one.
    pub dual_stack: bool,
}

pub const DNS_NAME: &str = "example.com";
pub const TLS_HOST: &str = "example.com";
pub const HTTP_URL: &str = "https://example.com/";
pub const PUBLIC_IP_URL: &str = "https://api.ipify.org?format=json";
pub const PUBLIC_IP6_URL: &str = "https://api64.ipify.org?format=json";
pub const STUN_SERVER: &str = "stun.cloudflare.com:3478";

fn request(egress: &Egress, probe: Probe, resolver: &Option<String>) -> NetworkDiagnosticsRequest {
    let mut request: NetworkDiagnosticsRequest = Request {
        config: String::new(),
        egress: egress.clone(),
        probe,
        count: 1,
        interval_ms: 200,
        timeout_ms: 10_000,
    }
    .into();
    request.resolver = resolver.clone();
    request
}

/// Fixed probes for one path. Proxy paths only carry TCP-based DNS, so DNS
/// uses TCP and DoH endpoints that every supported path accepts.
fn path_probes(
    egress: &Egress,
    resolver: &Option<String>,
    options: &Options,
) -> Vec<(&'static str, NetworkDiagnosticsRequest)> {
    let mut projects = vec![
        (
            "dns-tcp",
            request(
                egress,
                Probe::Dns {
                    name: DNS_NAME.into(),
                    record: "A".into(),
                    server: "tcp://1.1.1.1:53".into(),
                },
                resolver,
            ),
        ),
        (
            "dns-doh",
            request(
                egress,
                Probe::Dns {
                    name: DNS_NAME.into(),
                    record: "A".into(),
                    server: "https://1.1.1.1/dns-query".into(),
                },
                resolver,
            ),
        ),
        (
            "tls",
            request(
                egress,
                Probe::Tls {
                    host: TLS_HOST.into(),
                    port: 443,
                    server_name: None,
                    compare_bundled: true,
                },
                resolver,
            ),
        ),
        (
            "http",
            request(
                egress,
                Probe::Http {
                    url: HTTP_URL.into(),
                    method: "HEAD".into(),
                    compare_bundled: false,
                },
                resolver,
            ),
        ),
    ];
    let mut ip4 = request(
        egress,
        Probe::HttpsIp {
            url: PUBLIC_IP_URL.into(),
            compare_bundled: false,
        },
        resolver,
    );
    ip4.family = Some(AddressFamily::Ipv4);
    projects.push(("ip4", ip4));
    if options.dual_stack {
        let mut ip6 = request(
            egress,
            Probe::HttpsIp {
                url: PUBLIC_IP6_URL.into(),
                compare_bundled: false,
            },
            resolver,
        );
        ip6.family = Some(AddressFamily::Ipv6);
        projects.push(("ip6", ip6));
    }
    let mut stun = request(
        egress,
        Probe::Stun {
            server: STUN_SERVER.into(),
        },
        resolver,
    );
    stun.family = Some(AddressFamily::Ipv4);
    projects.push(("stun", stun));
    projects
}

/// `comparison` adds a second path (at most two user paths; NAT is its own
/// physical path, so the backend limit of three paths still holds).
pub fn build(
    primary: &Egress,
    comparison: Option<&Egress>,
    resolver: Option<String>,
    options: &Options,
    nat: Option<astra_model::network_nat::Request>,
) -> Result<NetworkBatchRequest, String> {
    if comparison == Some(primary) {
        return Err("对照出口与主出口相同；请选择另一个出口或关闭对照".into());
    }
    let mut projects = vec![];
    for (path, egress) in std::iter::once(("a", primary)).chain(comparison.map(|e| ("b", e))) {
        for (name, request) in path_probes(egress, &resolver, options) {
            projects.push(NetworkBatchProject {
                id: format!("{path}-{name}"),
                path_id: format!("path-{path}"),
                request,
            });
        }
    }
    if let Some(nat) = nat {
        nat.validate()?;
        let interface = nat.interface_id.clone();
        projects.push(NetworkBatchProject {
            id: "nat".into(),
            path_id: "path-nat".into(),
            request: Request {
                config: String::new(),
                egress: Egress::Interface(interface),
                probe: Probe::Nat { request: nat },
                count: 1,
                interval_ms: 200,
                timeout_ms: 30_000,
            }
            .into(),
        });
    }
    Ok(NetworkBatchRequest {
        projects,
        comparison: options.compare_rounds,
    })
}
