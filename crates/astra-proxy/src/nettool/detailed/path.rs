//! Diagnostic connections deliberately bypass App/group construction and protocol dialers.
//! Every socket is dialled from a literal endpoint here, then handed to a handshake.
use super::*;
use astra_model::{config::protocol::OutboundConfig, nettool::DetailedRequest};
use astra_protocols::outbound::Stream;

#[derive(Default)]
pub(in crate::nettool) struct ConnectTimings {
    pub dns_ms: f64,
    pub transport_connect_ms: f64,
    pub proxy_handshake_ms: Option<f64>,
}
pub(in crate::nettool) struct Route<'a> {
    request: &'a DetailedRequest,
    pub interface: Option<String>,
    resolver_interface: Option<String>,
    proxy: Option<&'a OutboundConfig>,
    pub(crate) tls: super::super::tls::TlsState,
    physical: Option<&'a astra_platform::diagnostic_path::PhysicalGuard>,
}
impl<'a> Route<'a> {
    pub fn new(request: &'a DetailedRequest) -> Result<Self> {
        let (interface, proxy) = match &request.egress {
            Egress::System => {
                ensure!(request.pinned.is_none(), "Unexpected pin on system path");
                (None, None)
            }
            Egress::Interface(name) => {
                ensure!(request.pinned.is_none(), "Unexpected pin on interface path");
                (Some(name.clone()), None)
            }
            Egress::Outbound(tag) => {
                let pin = request
                    .pinned
                    .as_ref()
                    .context("Unsupported: outbound requires a verified pinned selection")?;
                ensure!(
                    pin.version == 1 && pin.requested_tag == *tag,
                    "Invalid pinned selection version or tag"
                );
                ensure!(
                    pin.node
                        .as_ref()
                        .map_or(pin.leaf_tag == "DIRECT", |n| n.tag() == pin.leaf_tag),
                    "Pinned leaf mismatch"
                );
                (pin.effective_interface.clone(), pin.node.as_ref())
            }
        };
        if let Some(node) = proxy {
            ensure!(matches!(node, OutboundConfig::Socks5 { .. } | OutboundConfig::Http { .. }), "Unsupported: only SOCKS5 and HTTP CONNECT have verified diagnostic bootstrap and remote DNS");
            if let Probe::Dns { server, .. } = &request.probe {
                ensure!(Endpoint::parse(server)?.protocol != Protocol::Udp, "Unsupported: proxy DNS requires TCP, DoT or DoH; UDP is not silently sent DIRECT");
            }
            let (server, username, password) = proxy_fields(node);
            let entry = astra_protocols::http::authority_target(server)?;
            ensure!(
                server.len() <= 1024
                    && !entry.host.contains(['@', '/', '#'])
                    && !entry.host.chars().any(char::is_control),
                "Invalid proxy endpoint"
            );
            ensure!(
                username.is_some() == password.is_some(),
                "Proxy credentials must include both fields"
            );
            ensure!(
                username.is_none_or(|v| v.len() <= 255) && password.is_none_or(|v| v.len() <= 255),
                "Proxy credentials exceed diagnostic limit"
            );
        }
        if let Some(resolver) = &request.resolver {
            let endpoint = Endpoint::parse(resolver)?;
            ensure!(endpoint.bootstrap.is_some() || endpoint.host.parse::<IpAddr>().is_ok(), "Diagnostic resolver requires a literal IP or #bootstrap=IP; recursive system bootstrap is forbidden");
        }
        let resolver_interface = interface.clone();
        let interface = interface
            .map(|name| icmp::interface_name(&name))
            .transpose()?;
        Ok(Self {
            request,
            interface,
            resolver_interface,
            proxy,
            tls: Default::default(),
            physical: None,
        })
    }
    pub(crate) fn with_physical_guard(
        mut self,
        guard: &'a astra_platform::diagnostic_path::PhysicalGuard,
    ) -> Self {
        self.physical = Some(guard);
        self
    }
    async fn check_physical(&self, address: SocketAddr) -> Result<()> {
        if let Some(guard) = self.physical {
            if guard.local_ip().is_ipv6() == address.is_ipv6() {
                guard.recheck(address).await?;
            } else {
                guard
                    .for_family(address.is_ipv6())
                    .await?
                    .recheck(address)
                    .await?;
            }
        }
        Ok(())
    }
    pub fn is_proxy(&self) -> bool {
        self.proxy.is_some()
    }
    pub fn note(&self) -> String {
        let bound = self.interface.as_deref().unwrap_or("system route");
        if self.is_proxy() {
            format!("Pinned proxy via {bound}; business and DNS service names use remote proxy DNS; entry bootstrap is resolved separately without system DNS fallback")
        } else if self.interface.is_some() {
            format!("Sockets bound to {bound}; explicit or applicable interface resolver only; no system DNS fallback")
        } else if matches!(self.request.egress, Egress::Outbound(_)) {
            "Pinned DIRECT via system route; hostname requires an explicit resolver; no system DNS fallback".into()
        } else {
            "System route; explicit resolver when supplied, otherwise system resolver internal path may be invisible".into()
        }
    }
    pub(super) fn selected_resolver(&self, host: &str) -> Option<(String, &'static str)> {
        if let Some(resolver) = &self.request.resolver {
            return Some((resolver.clone(), "explicit_resolver"));
        }
        let interface = self.resolver_interface.as_deref()?;
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let mut choices = Vec::new();
        for resolver in &self.request.interface_resolvers {
            if resolver.interface_id.as_deref() != Some(interface)
                || !matches!(
                    resolver.scope.as_str(),
                    "link" | "scoped" | "service" | "policy"
                )
            {
                continue;
            }
            let specificity = if resolver.domains.is_empty() {
                Some(0)
            } else {
                resolver
                    .domains
                    .iter()
                    .filter_map(|domain| {
                        let domain = domain
                            .trim_start_matches('~')
                            .trim_matches('.')
                            .to_ascii_lowercase();
                        (domain.is_empty()
                            || host == domain
                            || host.ends_with(&format!(".{domain}")))
                        .then_some(domain.len())
                    })
                    .max()
            };
            let Some(specificity) = specificity else {
                continue;
            };
            for server in &resolver.servers {
                // Snapshot names are never recursively bootstrapped. Ambiguous or
                // unparsed scoped literals are refused instead of using libc.
                if let Ok(ip) = server.parse::<IpAddr>() {
                    choices.push((
                        std::cmp::Reverse(specificity),
                        resolver.priority.unwrap_or(u32::MAX),
                        resolver.id.as_str(),
                        ip.to_string(),
                    ));
                }
            }
        }
        choices.sort();
        choices
            .first()
            .map(|choice| (choice.3.clone(), "interface_resolver"))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "Keep each resolution boundary and its evidence fields explicit"
    )]
    fn evidence(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        method: &str,
        resolver: Option<String>,
        addresses: Vec<String>,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<()> {
        emit(Event::Resolution {
            seq,
            purpose: purpose.into(),
            host: host.into(),
            method: method.into(),
            resolver: resolver.map(|value| {
                Endpoint::parse(&value)
                    .map(|endpoint| endpoint.report_label())
                    .unwrap_or_else(|_| "[resolver details omitted]".into())
            }),
            addresses,
            interface: self.interface.clone(),
        })
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "The target, bootstrap policy, and physical-path evidence are independent constraints"
    )]
    pub async fn address(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        port: u16,
        bootstrap: Option<IpAddr>,
        no_system: bool,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<SocketAddr> {
        let family = if purpose == "proxy_entry" {
            None
        } else {
            self.request.family
        };
        if let Some(ip) = bootstrap.or_else(|| host.parse().ok()) {
            ensure!(
                family.is_none_or(|family| family.matches(ip)),
                "Literal endpoint does not match requested address family"
            );
            let address = SocketAddr::new(ip, port);
            self.evidence(
                seq,
                purpose,
                host,
                if bootstrap.is_some() {
                    "explicit_bootstrap"
                } else {
                    "literal"
                },
                None,
                vec![address.to_string()],
                emit,
            )?;
            return Ok(address);
        }
        if let Some((resolver, method)) = self.selected_resolver(host) {
            // Record the boundary before awaiting, so timeout/error still shows
            // which resolver was attempted; successful addresses follow it.
            self.evidence(
                seq,
                purpose,
                host,
                method,
                Some(resolver.clone()),
                vec![],
                emit,
            )?;
            let endpoint = Endpoint::parse(&resolver)?;
            let ip = endpoint
                .bootstrap
                .or_else(|| endpoint.host.parse().ok())
                .context("Resolver bootstrap must be an IP")?;
            let ip = self
                .lookup(
                    seq,
                    purpose,
                    host,
                    &endpoint,
                    SocketAddr::new(ip, endpoint.port),
                    emit,
                )
                .await?;
            let address = SocketAddr::new(ip, port);
            self.evidence(
                seq,
                purpose,
                host,
                method,
                Some(resolver),
                vec![address.to_string()],
                emit,
            )?;
            return Ok(address);
        }
        ensure!(!no_system && self.interface.is_none() && !matches!(self.request.egress, Egress::Outbound(_)), "Unsupported: {purpose} hostname needs an explicit resolver or confirmed applicable interface resolver; system DNS fallback is forbidden");
        self.evidence(seq, purpose, host, "system", None, vec![], emit)?;
        let address = resolve_for_family(host, port, None, None, family).await?;
        self.evidence(
            seq,
            purpose,
            host,
            "system",
            None,
            vec![address.to_string()],
            emit,
        )?;
        Ok(address)
    }
    pub(super) async fn lookup(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        endpoint: &Endpoint,
        address: SocketAddr,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<IpAddr> {
        let family = if purpose == "proxy_entry" {
            None
        } else {
            self.request.family
        };
        for kind in [RecordType::A, RecordType::AAAA]
            .into_iter()
            .filter(|kind| {
                family.is_none_or(|family| match family {
                    astra_model::nettool::AddressFamily::Ipv4 => *kind == RecordType::A,
                    astra_model::nettool::AddressFamily::Ipv6 => *kind == RecordType::AAAA,
                })
            })
        {
            let mut name =
                hickory_proto::rr::Name::from_ascii(format!("{}.", host.trim_end_matches('.')))?;
            let mut visited = Vec::new();
            for _ in 0..8 {
                ensure!(
                    !visited.contains(&name),
                    "DNS CNAME loop during constrained resolution"
                );
                visited.push(name.clone());
                let query = astra_dns::wire::query(&name.to_string(), kind)?;
                let mut protocol = endpoint.protocol;
                let reply = loop {
                    self.check_physical(address).await?;
                    let raw = exchange_recorded(
                        &self.tls,
                        seq,
                        false,
                        emit,
                        protocol,
                        endpoint,
                        address,
                        self.interface.as_deref(),
                        &query.to_vec()?,
                    )
                    .await?;
                    self.check_physical(address).await?;
                    let reply =
                        Message::from_vec(&raw).context("Malformed bootstrap DNS response")?;
                    astra_dns::wire::validate_response(&query, &reply)?;
                    if reply.metadata.truncation && protocol == Protocol::Udp {
                        self.evidence(
                            seq,
                            purpose,
                            host,
                            "resolver_tcp_retry",
                            Some(address.to_string()),
                            vec![],
                            emit,
                        )?;
                        protocol = Protocol::Tcp;
                        continue;
                    }
                    ensure!(
                        !reply.metadata.truncation,
                        "Truncated bootstrap DNS response over stream"
                    );
                    break reply;
                };
                ensure!(
                    reply.metadata.response_code == ResponseCode::NoError,
                    "Constrained DNS resolution: {}",
                    reply.metadata.response_code
                );
                let initial = name.clone();
                for _ in 0..32 {
                    if let Some(ip) = reply.answers.iter().find_map(|record| {
                        if record.name != name {
                            return None;
                        }
                        match &record.data {
                            RData::A(ip) if kind == RecordType::A => Some(IpAddr::V4(ip.0)),
                            RData::AAAA(ip) if kind == RecordType::AAAA => Some(IpAddr::V6(ip.0)),
                            _ => None,
                        }
                    }) {
                        return Ok(ip);
                    }
                    match reply.answers.iter().find_map(|record| {
                        if record.name == name {
                            if let RData::CNAME(target) = &record.data {
                                return Some(target.0.clone());
                            }
                        }
                        None
                    }) {
                        Some(target) if target != name => {
                            // An automatic split-DNS resolver does not acquire
                            // authority over a different domain via CNAME.
                            if self.request.resolver.is_none() {
                                ensure!(self.selected_resolver(host) == self.selected_resolver(&target.to_string()), "Unsupported: CNAME crosses automatic resolver scope; supply an explicit resolver");
                            }
                            name = target;
                        }
                        _ => break,
                    }
                }
                if name == initial {
                    break;
                }
            }
        }
        bail!("Constrained DNS resolution: no A or AAAA records")
    }
    fn connected(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        address: Option<SocketAddr>,
        source: &str,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<()> {
        emit(Event::Connection {
            seq,
            purpose: purpose.into(),
            host: host.into(),
            address: address.map(|value| value.to_string()),
            interface: self.interface.clone(),
            address_source: source.into(),
            family: address.map(|address| {
                if address.is_ipv4() {
                    astra_model::nettool::AddressFamily::Ipv4
                } else {
                    astra_model::nettool::AddressFamily::Ipv6
                }
            }),
        })
    }
    pub async fn connect(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        port: u16,
        bootstrap: Option<IpAddr>,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<Stream> {
        Ok(self
            .connect_timed(seq, purpose, host, port, bootstrap, emit)
            .await?
            .0)
    }
    pub async fn connect_timed(
        &self,
        seq: u32,
        purpose: &str,
        host: &str,
        port: u16,
        bootstrap: Option<IpAddr>,
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<(Stream, ConnectTimings)> {
        let mut timings = ConnectTimings::default();
        if let Some(proxy) = self.proxy {
            if let Some(family) = self.request.family {
                let target=bootstrap.or_else(||host.parse().ok()).context("Unsupported: proxy remote DNS cannot confirm requested target address family; supply a target IP or explicit service bootstrap")?;
                ensure!(
                    family.matches(target),
                    "Proxy target does not match requested address family"
                );
            }
            let (server, username, password) = proxy_fields(proxy);
            let entry = astra_protocols::http::authority_target(server)?;
            let started = Instant::now();
            let address = self
                .address(
                    seq,
                    "proxy_entry",
                    &entry.host,
                    entry.port,
                    None,
                    true,
                    emit,
                )
                .await
                .context("Proxy entry resolution failed")?;
            timings.dns_ms = started.elapsed().as_secs_f64() * 1000.;
            let started = Instant::now();
            let tcp = dial(address, self.interface.as_deref())
                .await
                .context("Proxy transport connection failed")?;
            timings.transport_connect_ms = started.elapsed().as_secs_f64() * 1000.;
            self.connected(
                seq,
                "proxy_entry",
                &entry.host,
                Some(tcp.peer_addr()?),
                "socket_peer",
                emit,
            )?;
            let target = Target {
                host: bootstrap.map_or_else(|| host.to_owned(), |ip| ip.to_string()),
                port,
            };
            self.evidence(
                seq,
                purpose,
                host,
                if bootstrap.is_some() {
                    "explicit_bootstrap"
                } else if target.host.parse::<IpAddr>().is_ok() {
                    "literal"
                } else {
                    "remote_proxy"
                },
                None,
                vec![if target.host.contains(':') {
                    format!("[{}]:{port}", target.host)
                } else {
                    format!("{}:{port}", target.host)
                }],
                emit,
            )?;
            let started = Instant::now();
            let stream = match proxy {
                OutboundConfig::Socks5 { .. } => {
                    astra_protocols::socks5::socks5_on(tcp, username, password, &target).await
                }
                OutboundConfig::Http { .. } => {
                    astra_protocols::http::http_connect_on(tcp, username, password, &target).await
                }
                _ => unreachable!(),
            }
            .context("Proxy handshake failed")?;
            timings.proxy_handshake_ms = Some(started.elapsed().as_secs_f64() * 1000.);
            let address = target
                .host
                .parse::<IpAddr>()
                .ok()
                .map(|ip| SocketAddr::new(ip, port));
            self.connected(
                seq,
                purpose,
                host,
                address,
                if address.is_some() {
                    "proxy_target_literal"
                } else {
                    "remote_dns_unknown"
                },
                emit,
            )?;
            return Ok((stream, timings));
        }
        let started = Instant::now();
        let address = self
            .address(seq, purpose, host, port, bootstrap, false, emit)
            .await
            .context("Target resolution failed")?;
        timings.dns_ms = started.elapsed().as_secs_f64() * 1000.;
        let started = Instant::now();
        let stream = Box::new(
            dial(address, self.interface.as_deref())
                .await
                .context("Target transport connection failed")?,
        );
        timings.transport_connect_ms = started.elapsed().as_secs_f64() * 1000.;
        self.connected(
            seq,
            purpose,
            host,
            Some(stream.peer_addr()?),
            "socket_peer",
            emit,
        )?;
        Ok((stream, timings))
    }
    pub(super) async fn dns_exchange(
        &self,
        seq: u32,
        protocol: Protocol,
        endpoint: &Endpoint,
        query: &[u8],
        emit: &impl Fn(Event) -> Result<()>,
    ) -> Result<(Vec<u8>, String)> {
        if self.is_proxy() {
            ensure!(
                protocol != Protocol::Udp,
                "Unsupported: UDP through diagnostic proxy"
            );
            let stream = self
                .connect(
                    seq,
                    "dns_service",
                    &endpoint.host,
                    endpoint.port,
                    endpoint.bootstrap,
                    emit,
                )
                .await?;
            let raw = if protocol == Protocol::Tcp {
                crate::dns::framed(stream, query).await?
            } else {
                let stream = if protocol == Protocol::Doh {
                    self.tls
                        .connect_http(seq, &endpoint.host, stream, false, emit)
                        .await?
                } else {
                    self.tls
                        .connect(seq, &endpoint.host, stream, false, emit)
                        .await?
                };
                encrypted_dns(protocol, endpoint, stream, query).await?
            };
            Ok((
                raw,
                if endpoint.bootstrap.is_some() || endpoint.host.parse::<IpAddr>().is_ok() {
                    "pinned proxy tunnel (literal DNS service endpoint)".into()
                } else {
                    "pinned proxy tunnel (remote DNS)".into()
                },
            ))
        } else {
            let address = self
                .address(
                    seq,
                    "dns_service",
                    &endpoint.host,
                    endpoint.port,
                    endpoint.bootstrap,
                    false,
                    emit,
                )
                .await?;
            Ok((
                exchange_recorded(
                    &self.tls,
                    seq,
                    false,
                    emit,
                    protocol,
                    endpoint,
                    address,
                    self.interface.as_deref(),
                    query,
                )
                .await?,
                address.to_string(),
            ))
        }
    }
}
fn proxy_fields(node: &OutboundConfig) -> (&str, Option<&str>, Option<&str>) {
    match node {
        OutboundConfig::Socks5 {
            server,
            username,
            password,
            ..
        }
        | OutboundConfig::Http {
            server,
            username,
            password,
            ..
        } => (server, username.as_deref(), password.as_deref()),
        _ => unreachable!(),
    }
}
