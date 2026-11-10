//! Detailed diagnostics use an explicit resolver boundary and literal socket endpoints.
use super::*;
pub(super) mod path;
use astra_model::nettool::DetailedRequest;
use astra_model::nettool::{DnsDetail, DnsEdns, DnsFlags, DnsQuestion, DnsRecord};
use hickory_proto::{op::ResponseCode, rr::RData};
use http_body_util::{BodyExt, Full};
use tokio::net::{TcpSocket, TcpStream};

const RECORD_LIMIT: usize = 256;
const TEXT_LIMIT: usize = 8 * 1024;
const CHUNK_LIMIT: usize = 48 * 1024;
#[cfg(test)]
tokio::task_local! { static SYSTEM_LOOKUPS: std::cell::Cell<usize>; }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Protocol {
    Udp,
    Tcp,
    Dot,
    Doh,
}
impl Protocol {
    fn label(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Dot => "dot",
            Self::Doh => "doh",
        }
    }
}
struct Endpoint {
    protocol: Protocol,
    host: String,
    port: u16,
    path: String,
    bootstrap: Option<IpAddr>,
}
impl Endpoint {
    /// Reports identify the service, never URL userinfo/path/query credentials.
    fn report_label(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let scheme = if self.protocol == Protocol::Doh {
            "https"
        } else {
            self.protocol.label()
        };
        let mut label = format!("{scheme}://{host}:{}", self.port);
        if self.protocol == Protocol::Doh {
            label.push_str("/[path and query omitted]");
        }
        if let Some(ip) = self.bootstrap {
            label.push_str(&format!("#bootstrap={ip}"));
        }
        label
    }
    fn parse(text: &str) -> Result<Self> {
        let (text, bootstrap) = match text.split_once('#') {
            Some((text, fragment)) => (
                text,
                Some(
                    fragment
                        .strip_prefix("bootstrap=")
                        .context("Use #bootstrap=IP for an explicit DNS service bootstrap")?
                        .parse()
                        .context("Invalid bootstrap IP")?,
                ),
            ),
            None => (text, None),
        };
        if text.starts_with("https://") {
            let url = url::Url::parse(text).map_err(|_| anyhow::anyhow!("Invalid DoH endpoint"))?;
            ensure!(
                url.username().is_empty() && url.password().is_none(),
                "DNS URLs cannot contain credentials"
            );
            let host = url
                .host_str()
                .context("Missing DoH hostname")?
                .trim_matches(['[', ']'])
                .to_owned();
            let mut path = url.path().to_owned();
            if path.is_empty() || path == "/" {
                path = "/dns-query".into();
            }
            if let Some(query) = url.query() {
                path.push('?');
                path.push_str(query);
            }
            ensure!(path.len() <= 4096, "DoH path exceeds limit");
            Ok(Self {
                protocol: Protocol::Doh,
                host,
                port: url.port().unwrap_or(443),
                path,
                bootstrap,
            })
        } else {
            let authority = text
                .split_once("://")
                .map_or(text, |(_, authority)| authority);
            ensure!(
                !authority.contains(['@', '?', '/']),
                "DNS endpoints cannot contain credentials, paths or query parameters except in DoH"
            );
            // Legacy parser errors may quote the supplied text. Keep arbitrary
            // endpoint input out of errors sent to persisted diagnostic reports.
            let (transport, _, host, port) = parse_server(text)
                .map_err(|_| anyhow::anyhow!("Invalid diagnostic DNS endpoint"))?;
            Ok(Self {
                protocol: match transport {
                    Transport::Udp => Protocol::Udp,
                    Transport::Tcp => Protocol::Tcp,
                    Transport::Tls => Protocol::Dot,
                },
                host,
                port,
                path: String::new(),
                bootstrap,
            })
        }
    }
}

/// The sole system resolver boundary in the new diagnostic implementation.
#[cfg(test)]
async fn resolve_for(
    host: &str,
    port: u16,
    interface: Option<&str>,
    bootstrap: Option<IpAddr>,
) -> Result<SocketAddr> {
    resolve_for_family(host, port, interface, bootstrap, None).await
}
async fn resolve_for_family(
    host: &str,
    port: u16,
    interface: Option<&str>,
    bootstrap: Option<IpAddr>,
    family: Option<astra_model::nettool::AddressFamily>,
) -> Result<SocketAddr> {
    if let Some(ip) = bootstrap.or_else(|| host.parse().ok()) {
        ensure!(
            family.is_none_or(|family| family.matches(ip)),
            "Literal endpoint does not match requested address family"
        );
        return Ok(SocketAddr::new(ip, port));
    }
    ensure!(interface.is_none(),"A bound diagnostic cannot use the system resolver: supply a target IP or DNS service #bootstrap=IP");
    #[cfg(test)]
    let _ = SYSTEM_LOOKUPS.try_with(|n| n.set(n.get() + 1));
    tokio::net::lookup_host((host, port))
        .await?
        .find(|address| family.is_none_or(|family| family.matches(address.ip())))
        .context("System resolver has no address in the requested family")
}
#[expect(
    clippy::too_many_arguments,
    reason = "Physical resolver calls explicitly carry target, guard, sequence, and evidence sink"
)]
pub(super) async fn resolve_physical_bound(
    request: &DetailedRequest,
    interface: &str,
    host: &str,
    port: u16,
    family: Option<bool>,
    seq: u32,
    guard: &astra_platform::diagnostic_path::PhysicalGuard,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<SocketAddr> {
    ensure!(
        matches!(&request.egress,Egress::Interface(name) if name==interface),
        "Physical resolver requires the explicitly selected interface"
    );
    let mut request = request.clone();
    request.family = family.map(|v| {
        if v {
            astra_model::nettool::AddressFamily::Ipv6
        } else {
            astra_model::nettool::AddressFamily::Ipv4
        }
    });
    let route = path::Route::new(&request)?.with_physical_guard(guard);
    let address = route
        .address(seq, "business_target", host, port, None, true, emit)
        .await?;
    guard.recheck(address).await?;
    Ok(address)
}
async fn dial(address: SocketAddr, interface: Option<&str>) -> Result<TcpStream> {
    let address = egress::scoped_destination(address, interface)?;
    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    egress::bind_interface(&socket, interface, address.is_ipv4())?;
    Ok(socket.connect(address).await?)
}
async fn udp(address: SocketAddr, interface: Option<&str>, query: &[u8]) -> Result<Vec<u8>> {
    let address = egress::scoped_destination(address, interface)?;
    let socket = tokio::net::UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    egress::bind_interface(&socket, interface, address.is_ipv4())?;
    socket.connect(address).await?;
    socket.send(query).await?;
    let mut bytes = vec![0; 65535];
    loop {
        let len = socket.recv(&mut bytes).await?;
        if len >= 2 && bytes[..2] == query[..2] {
            bytes.truncate(len);
            return Ok(bytes);
        }
    }
}
#[expect(
    clippy::too_many_arguments,
    reason = "Record the DNS exchange without hiding transport and binding constraints"
)]
async fn exchange_recorded(
    tls: &super::tls::TlsState,
    seq: u32,
    compare: bool,
    emit: &impl Fn(Event) -> Result<()>,
    protocol: Protocol,
    endpoint: &Endpoint,
    address: SocketAddr,
    interface: Option<&str>,
    query: &[u8],
) -> Result<Vec<u8>> {
    if protocol == Protocol::Udp {
        return udp(address, interface, query).await;
    }
    let stream = dial(address, interface).await?;
    let peer = stream.peer_addr()?;
    emit(Event::Connection {
        seq,
        purpose: "dns_service".into(),
        host: endpoint.host.clone(),
        address: Some(peer.to_string()),
        interface: interface.map(str::to_owned),
        address_source: "socket_peer".into(),
        family: Some(if peer.is_ipv4() {
            astra_model::nettool::AddressFamily::Ipv4
        } else {
            astra_model::nettool::AddressFamily::Ipv6
        }),
    })?;
    if protocol == Protocol::Tcp {
        return crate::dns::framed(stream, query).await;
    }
    let stream = if protocol == Protocol::Doh {
        tls.connect_http(seq, &endpoint.host, stream, compare, emit)
            .await?
    } else {
        tls.connect(seq, &endpoint.host, stream, compare, emit)
            .await?
    };
    encrypted_dns(protocol, endpoint, stream, query).await
}
async fn encrypted_dns(
    stream_protocol: Protocol,
    endpoint: &Endpoint,
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    query: &[u8],
) -> Result<Vec<u8>> {
    if stream_protocol == Protocol::Dot {
        crate::dns::framed(stream, query).await
    } else {
        doh_http(stream, endpoint, query).await
    }
}
#[cfg(test)]
async fn encrypted_exchange(
    protocol: Protocol,
    endpoint: &Endpoint,
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    query: &[u8],
    connector: &tokio_rustls::TlsConnector,
) -> Result<Vec<u8>> {
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(endpoint.host.clone())?;
    // This capability explicitly labels the existing bundled roots. System root
    // snapshot validation and certificate failure evidence arrive separately.
    let tls = connector
        .connect(name, stream)
        .await
        .context("DNS TLS validation/handshake (bundled webpki roots)")?;
    if protocol == Protocol::Dot {
        return crate::dns::framed(tls, query).await;
    }
    doh_http(tls, endpoint, query).await
}
async fn doh_http(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    endpoint: &Endpoint,
    query: &[u8],
) -> Result<Vec<u8>> {
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(32 * 1024)
        .handshake(hyper_util::rt::TokioIo::new(stream))
        .await?;
    let authority = if endpoint.host.contains(':') {
        format!("[{}]:{}", endpoint.host, endpoint.port)
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    let request = http::Request::post(&endpoint.path)
        .header("Host", authority)
        .header("Content-Type", "application/dns-message")
        .header("Accept", "application/dns-message")
        .body(Full::new(bytes::Bytes::copy_from_slice(query)))?;
    let response = async move {
        let reply = sender.send_request(request).await?;
        ensure!(
            reply.status() == http::StatusCode::OK,
            "DoH HTTP status {} (redirects are not followed)",
            reply.status()
        );
        ensure!(
            reply
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v
                    .split(';')
                    .next()
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("application/dns-message"))),
            "Invalid DoH content type"
        );
        let mut body = reply.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame?.into_data() {
                ensure!(
                    bytes.len() + data.len() <= 65535,
                    "DoH response exceeds DNS wire limit"
                );
                bytes.extend_from_slice(&data);
            }
        }
        Ok(bytes)
    };
    tokio::pin!(response);
    // Both futures are owned here. Timeout/cancellation drops the entire socket
    // and connection driver; no HTTP worker is detached.
    tokio::select! { result = &mut response => result, result = connection => { result?; response.await } }
}
fn text_limit(text: String, limit: usize) -> (String, bool) {
    let mut length = text.len().min(limit);
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    (text[..length].to_owned(), text.len() > length)
}
fn emit_detail(seq: u32, detail: DnsDetail, emit: &impl Fn(Event) -> Result<()>) -> Result<()> {
    let event = Event::Dns {
        seq,
        detail: Box::new(detail),
    };
    ensure!(
        serde_json::to_vec(&event)?.len() < 56 * 1024,
        "DNS detail event exceeds budget"
    );
    emit(event)
}
fn outcome(reply: &Message, record: RecordType) -> &'static str {
    match reply.metadata.response_code {
        ResponseCode::NXDomain => return "nxdomain",
        ResponseCode::ServFail => return "server_failure",
        ResponseCode::NoError => {}
        _ => return "dns_error",
    }
    let Some(question) = reply.queries.first() else {
        return "no_records";
    };
    let mut name = question.name().clone();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..32 {
        if !seen.insert(name.clone()) {
            break;
        }
        if reply.answers.iter().any(|r| {
            r.name == name && (r.data.record_type() == record || record == RecordType::ANY)
        }) {
            return "resolved";
        }
        match reply
            .answers
            .iter()
            .find(|r| r.name == name && matches!(r.data, RData::CNAME(_)))
            .map(|r| &r.data)
        {
            Some(RData::CNAME(target)) => name = target.0.clone(),
            _ => break,
        }
    }
    "no_records"
}
fn details(
    seq: u32,
    reply: &Message,
    endpoint: &Endpoint,
    protocol: Protocol,
    address: impl std::fmt::Display,
    record: RecordType,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<()> {
    let count = reply.answers.len() + reply.authorities.len() + reply.additionals.len();
    let mut cname_chain = Vec::new();
    let mut cname_name = reply.queries.first().map(|q| q.name().clone());
    for _ in 0..32 {
        let next = cname_name.as_ref().and_then(|name| {
            reply
                .answers
                .iter()
                .find(|r| &r.name == name && matches!(r.data, RData::CNAME(_)))
        });
        let Some(next) = next else {
            break;
        };
        let RData::CNAME(target) = &next.data else {
            unreachable!()
        };
        if cname_chain.contains(&target.to_string()) {
            break;
        }
        cname_chain.push(target.to_string());
        cname_name = Some(target.0.clone());
    }
    let flags = &reply.metadata;
    let mut edns_truncated = false;
    let edns = reply.edns.as_ref().map(|edns| DnsEdns {
        version: edns.version(),
        udp_payload: edns.max_payload(),
        dnssec_ok: edns.flags().dnssec_ok,
        options: edns
            .options()
            .as_ref()
            .iter()
            .take(32)
            .map(|(code, value)| {
                let (text, truncated) = text_limit(format!("{code:?}: {value:?}"), 512);
                edns_truncated |= truncated;
                text
            })
            .collect(),
    });
    let truncated = edns_truncated
        || (reply.metadata.truncation && protocol != Protocol::Udp)
        || count > RECORD_LIMIT
        || cname_chain.len() == 32
        || reply.queries.len() > 16
        || reply
            .edns
            .as_ref()
            .is_some_and(|e| e.options().as_ref().len() > 32)
        || reply
            .answers
            .iter()
            .chain(&reply.authorities)
            .chain(&reply.additionals)
            .take(RECORD_LIMIT)
            .any(|r| r.data.to_string().len() > TEXT_LIMIT);
    let mut header = DnsDetail::Response {
        transport: protocol.label().into(),
        server: format!("{} via {address}", endpoint.host),
        outcome: outcome(reply, record).into(),
        response_received: true,
        parsed: true,
        rcode: reply.metadata.response_code.to_string(),
        flags: DnsFlags {
            authoritative: flags.authoritative,
            truncated: flags.truncation,
            recursion_desired: flags.recursion_desired,
            recursion_available: flags.recursion_available,
            authenticated_data: flags.authentic_data,
            checking_disabled: flags.checking_disabled,
        },
        edns,
        questions: reply
            .queries
            .iter()
            .take(16)
            .map(|q| DnsQuestion {
                name: q.name().to_string(),
                record_type: q.query_type().to_string(),
                class: q.query_class().to_string(),
            })
            .collect(),
        answer_count: reply.answers.len() as u32,
        authority_count: reply.authorities.len() as u32,
        additional_count: reply.additionals.len() as u32,
        cname_chain,
        truncated,
        dnssec_locally_validated: false,
    };
    // Keep metadata independently readable even for long CNAME chains or EDNS data.
    while serde_json::to_vec(&header)?.len() > CHUNK_LIMIT - 256 {
        let DnsDetail::Response {
            cname_chain,
            edns,
            truncated,
            ..
        } = &mut header
        else {
            unreachable!()
        };
        *truncated = true;
        if cname_chain.pop().is_none()
            && edns
                .as_mut()
                .is_none_or(|edns| edns.options.pop().is_none())
        {
            bail!("DNS metadata exceeds budget");
        }
    }
    emit_detail(seq, header, emit)?;
    let mut remaining = RECORD_LIMIT;
    for (section, records) in [
        ("answer", &reply.answers),
        ("authority", &reply.authorities),
        ("additional", &reply.additionals),
    ] {
        let mut chunk = Vec::new();
        let mut size = 256;
        for r in records.iter().take(remaining) {
            let (text, text_truncated) = text_limit(r.data.to_string(), TEXT_LIMIT);
            let record = DnsRecord {
                name: r.name.to_string(),
                record_type: r.data.record_type().to_string(),
                class: r.dns_class.to_string(),
                ttl: r.ttl,
                text,
                text_truncated,
            };
            let bytes = serde_json::to_vec(&record)?.len() + 1;
            if size + bytes > CHUNK_LIMIT && !chunk.is_empty() {
                emit_detail(
                    seq,
                    DnsDetail::Records {
                        section: section.into(),
                        records: std::mem::take(&mut chunk),
                    },
                    emit,
                )?;
                size = 256;
            }
            chunk.push(record);
            size += bytes;
        }
        remaining = remaining.saturating_sub(records.len());
        if !chunk.is_empty() {
            emit_detail(
                seq,
                DnsDetail::Records {
                    section: section.into(),
                    records: chunk,
                },
                emit,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
async fn run(request: &Request, emit: impl Fn(Event) -> Result<()>) -> Result<Summary> {
    run_context(&request.clone().into(), emit).await
}
pub async fn run_context(
    request: &DetailedRequest,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    validate(request)?;
    ensure!(
        request.config.is_empty(),
        "Detailed diagnostics cannot initialize a profile"
    );
    match &request.probe {
        Probe::Tcp { .. } | Probe::TcpPorts { .. } => {
            return super::basic::run(request, emit).await
        }
        Probe::Nat { request: options } => {
            emit(Event::Started { egress: format!("iface:{}", options.interface_id), probe: request.probe.to_string(), note: "Manually selected physical interface; direct STUN services observe this network's exit IP".into(), running_config: false })?;
            let summary = super::nat::run(request, options, &emit).await?;
            emit(Event::Summary(summary.clone()))?;
            emit(Event::Done)?;
            return Ok(summary);
        }
        Probe::Stun { server } => {
            let route = path::Route::new(request)?;
            emit(Event::Started {
                egress: request.egress.to_string(),
                probe: request.probe.to_string(),
                note: format!(
                    "{}; independent UDP session, no NAT classification",
                    route.note()
                ),
                running_config: false,
            })?;
            let summary = super::stun::run(request, server, &emit).await?;
            emit(Event::Summary(summary.clone()))?;
            emit(Event::Done)?;
            return Ok(summary);
        }
        Probe::Trace { .. } => return super::path_probe::run_trace(request, emit).await,
        Probe::Icmp { .. } => return super::path_probe::run_icmp(request, emit).await,
        _ => {}
    }
    let text_ok =
        |text: &str| !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control);
    match &request.probe {
        Probe::Tcp { host, .. } => {
            ensure!(text_ok(host), "Invalid diagnostic hostname")
        }
        Probe::Dns {
            name,
            record,
            server,
        } => ensure!(
            text_ok(name) && text_ok(record) && text_ok(server),
            "Invalid diagnostic DNS fields"
        ),
        Probe::Tls {
            host,
            port,
            server_name,
            ..
        } => ensure!(
            text_ok(host) && *port > 0 && server_name.as_deref().is_none_or(text_ok),
            "Invalid TLS target"
        ),
        Probe::Http { url, method, .. } => {
            super::http_probe::Endpoint::parse(url)?;
            ensure!(
                method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD"),
                "HTTP diagnostic method must be GET or HEAD"
            );
        }
        Probe::HttpsIp { url, .. } => {
            ensure!(
                super::http_probe::Endpoint::parse(url)?.url.scheme() == "https",
                "Public IP observation requires HTTPS"
            );
        }
        _ => unreachable!(),
    }
    ensure!(
        request.resolver.as_ref().is_none_or(|r| text_ok(r)),
        "Invalid diagnostic resolver"
    );
    let route = path::Route::new(request)?;
    let timeout = Duration::from_millis(request.timeout_ms);
    let mut note = route.note();
    let endpoint = match &request.probe {
        Probe::Dns { server, .. } => Some(Endpoint::parse(server)?),
        _ => None,
    };
    if matches!(request.probe, Probe::Tls { .. } | Probe::HttpsIp { .. })
        || matches!(&request.probe,Probe::Http {url,..} if url.starts_with("https://"))
        || endpoint
            .as_ref()
            .is_some_and(|e| matches!(e.protocol, Protocol::Dot | Protocol::Doh))
        || request
            .resolver
            .as_deref()
            .and_then(|r| Endpoint::parse(r).ok())
            .is_some_and(|e| matches!(e.protocol, Protocol::Dot | Protocol::Doh))
    {
        note.push_str("; TLS trust: account-visible system root snapshot via rustls; not the complete OS certificate policy; no bundled fallback");
    }
    emit(Event::Started {
        egress: request.egress.to_string(),
        probe: match (&request.probe, &endpoint) {
            (Probe::Dns { name, record, .. }, Some(endpoint)) => {
                format!("DNS {record} {name} @{}", endpoint.report_label())
            }
            _ => request.probe.to_string(),
        },
        note,
        running_config: false,
    })?;
    let mut attempts = Vec::new();
    for seq in 1..=request.count {
        if seq > 1 {
            tokio::time::sleep(Duration::from_millis(request.interval_ms)).await;
        }
        let start = Instant::now();
        let mut received = false;
        let mut failure = "network_error";
        let result = tokio::time::timeout(timeout, async {
            match &request.probe {
                Probe::Tcp { host, port } => {
                    let _stream = route.connect(seq,"business_target",host.trim_matches(['[',']']),*port,None,&emit).await?;
                    Ok::<_, anyhow::Error>(format!("connected to {host}:{port}"))
                }
                Probe::Dns { name, record, .. } => {
                    let endpoint = endpoint.as_ref().unwrap();
                    // Resolve once: UDP truncation must retry the exact same
                    // resolver endpoint even when OS DNS rotates addresses.
                    let direct = if route.is_proxy() { None } else {
                        Some(route.address(seq,"dns_service",&endpoint.host,endpoint.port,endpoint.bootstrap,false,&emit).await?)
                    };
                    let kind = record_type(record)?;
                    let query = astra_dns::wire::query(name, kind)?;
                    let bytes = query.to_vec()?;
                    let mut protocol = endpoint.protocol;
                    loop {
                        let (raw,address) = if let Some(address) = direct {
                            (exchange_recorded(&route.tls,seq,false,&emit,protocol,endpoint,address,route.interface.as_deref(),&bytes).await?, address.to_string())
                        } else { route.dns_exchange(seq,protocol,endpoint,&bytes,&emit).await? };
                        received = true;
                        failure = "malformed";
                        let reply = Message::from_vec(&raw).context("Malformed DNS response")?;
                        failure = "response_mismatch";
                        astra_dns::wire::validate_response(&query, &reply)?;
                        details(seq, &reply, endpoint, protocol, address, kind, &emit)?;
                        if reply.metadata.truncation && protocol == Protocol::Udp {
                            emit_detail(seq, DnsDetail::Retry { from:"udp".into(), to:"tcp".into(), reason:"Truncated UDP reply; retry same resolver endpoint and egress over TCP".into() }, &emit)?;
                            protocol = Protocol::Tcp;
                            failure = "network_error";
                            continue;
                        }
                        if reply.metadata.truncation {
                            failure = "truncated_response";
                            bail!("DNS response remains truncated over {}",protocol.label());
                        }
                        return Ok(format!("{} · {}", outcome(&reply, kind), answers(&reply)));
                    }
                }
                Probe::Tls {host,port,server_name,compare_bundled} => {
                    let stream=route.connect(seq,"business_target",host.trim_matches(['[',']']),*port,None,&emit).await?;
                    let name=server_name.as_deref().unwrap_or(host).trim_matches(['[',']']);
                    let _stream=route.tls.connect(seq,name,stream,*compare_bundled,&emit).await?;
                    Ok(format!("TLS system-root snapshot validation succeeded for {name}"))
                },
                Probe::Http {url,method,compare_bundled}=>super::http_probe::run(&route,seq,url,method,*compare_bundled,false,&emit).await,
                Probe::HttpsIp {url,compare_bundled}=>super::http_probe::run(&route,seq,url,"GET",*compare_bundled,true,&emit).await,
                _ => unreachable!(),
            }
        }).await;
        route
            .tls
            .flush(seq, "TLS handshake timed out or was interrupted", &emit)?;
        let attempt = match result {
            Ok(Ok(detail)) => Attempt {
                seq,
                rtt_ms: Some(millis(start.elapsed())),
                detail: clean(&detail, 400),
                error: None,
            },
            error => {
                let message = match error {
                    Err(_) => {
                        failure = "timeout";
                        format!("Timed out after {} ms", request.timeout_ms)
                    }
                    Ok(Err(error)) => format!("{error:#}"),
                    _ => unreachable!(),
                };
                if failure == "timeout"
                    && matches!(request.probe, Probe::Http { .. } | Probe::HttpsIp { .. })
                {
                    emit(Event::Http {
                        seq,
                        detail: Box::new(astra_model::network_tls::HttpDetail::Failure {
                            hop: None,
                            phase: "attempt deadline".into(),
                            message: clean(&message, 2048),
                        }),
                    })?;
                }
                if endpoint.is_some() {
                    emit_detail(
                        seq,
                        DnsDetail::Failure {
                            outcome: failure.into(),
                            response_received: received,
                            message: clean(&message, 2048),
                        },
                        &emit,
                    )?;
                }
                Attempt {
                    seq,
                    rtt_ms: None,
                    detail: String::new(),
                    error: Some(clean(&message, 1024)),
                }
            }
        };
        emit(Event::Attempt(attempt.clone()))?;
        attempts.push(attempt);
    }
    let summary = Summary::from_attempts(&attempts);
    emit(Event::Summary(summary.clone()))?;
    emit(Event::Done)?;
    Ok(summary)
}

#[cfg(test)]
#[path = "../test/nettool_detailed_test.rs"]
mod tests;

#[cfg(test)]
#[path = "../test/nettool_routes_test.rs"]
mod route_tests;
