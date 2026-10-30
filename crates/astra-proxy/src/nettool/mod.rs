//! Connectivity probes pinned to one egress: the OS route, a local adapter, or
//! any outbound / proxy group of the profile. ICMP and UDP cannot travel
//! through a proxy, so those paths are refused instead of silently leaking DIRECT.
mod icmp;

use crate::{egress, App};
use anyhow::{bail, ensure, Context, Result};
use astra_ipc::{
    nettool::{Attempt, Egress, Event, Probe, Request, Summary, MAX_COUNT},
    RouteAction, Target,
};
use hickory_proto::{op::Message, rr::RecordType};
use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

/// Resolved form of [`Egress`]; `Outbound(DIRECT)` collapses to the profile's
/// pinned TUN egress (or the OS route) for traffic a proxy cannot carry.
enum Path<'a> {
    System,
    Interface(String),
    Outbound(&'a App, RouteAction),
}
impl Path<'_> {
    fn interface(&self) -> Option<&str> {
        match self {
            Path::Interface(name) => Some(name),
            _ => None,
        }
    }
    /// Socket-level path for UDP / ICMP. None for proxy outbounds.
    fn local(&self) -> Option<Path<'static>> {
        match self {
            Path::System => Some(Path::System),
            Path::Interface(name) => Some(Path::Interface(name.clone())),
            Path::Outbound(app, RouteAction::Direct) => Some(
                match app.config.tun.as_ref().and_then(|t| t.interface.clone()) {
                    Some(name) => Path::Interface(name),
                    None => Path::System,
                },
            ),
            Path::Outbound(..) => None,
        }
    }
}

pub fn validate(request: &Request) -> Result<()> {
    ensure!(
        (1..=MAX_COUNT).contains(&request.count),
        "count must be 1–{MAX_COUNT}"
    );
    ensure!(
        (100..=30_000).contains(&request.timeout_ms),
        "timeout must be 100–30000 ms"
    );
    ensure!(
        request.interval_ms <= 10_000,
        "interval must be at most 10000 ms"
    );
    match &request.probe {
        Probe::Dns { name, record, .. } => {
            ensure!(!name.trim().is_empty(), "DNS name is empty");
            record_type(record)?;
        }
        Probe::Tcp { host, port } => {
            ensure!(!host.trim().is_empty(), "TCP host is empty");
            ensure!(*port != 0, "TCP port must be nonzero");
        }
        Probe::Icmp { host } => ensure!(!host.trim().is_empty(), "ICMP host is empty"),
    }
    if let Egress::Interface(name) = &request.egress {
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "invalid interface"
        );
    }
    Ok(())
}

pub async fn run(
    app: Option<&App>,
    request: &Request,
    running_config: bool,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    validate(request)?;
    let path = match &request.egress {
        Egress::System => Path::System,
        Egress::Interface(name) => {
            #[cfg(windows)]
            let name = crate::platform::windows_network::interface_alias(name)?;
            Path::Interface(name.clone())
        }
        Egress::Outbound(tag) => {
            let app = app.context("outbound egress requires a profile configuration")?;
            ensure!(
                crate::diagnostics::targets(app).contains(tag),
                "unknown outbound {tag}"
            );
            Path::Outbound(
                app,
                if tag == "DIRECT" {
                    RouteAction::Direct
                } else {
                    RouteAction::Proxy(tag.clone())
                },
            )
        }
    };
    let timeout = Duration::from_millis(request.timeout_ms);
    let mut prepared = tokio::time::timeout(timeout * 2, prepare(&path, &request.probe))
        .await
        .context("probe preparation timed out")??;
    emit(Event::Started {
        egress: request.egress.to_string(),
        probe: request.probe.to_string(),
        note: prepared.note.clone(),
        running_config,
    })?;
    let mut attempts = Vec::new();
    for seq in 1..=request.count {
        if seq > 1 && request.interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(request.interval_ms)).await;
        }
        let start = Instant::now();
        let result = tokio::time::timeout(timeout, prepared.attempt(seq, timeout))
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "timed out after {} ms",
                    timeout.as_millis()
                ))
            });
        let attempt = match result {
            Ok((rtt, detail)) => Attempt {
                seq,
                rtt_ms: Some(rtt.unwrap_or_else(|| millis(start.elapsed()))),
                detail: clean(&detail, 400),
                error: None,
            },
            Err(error) => Attempt {
                seq,
                rtt_ms: None,
                detail: String::new(),
                error: Some(clean(&format!("{error:#}"), 240)),
            },
        };
        emit(Event::Attempt(attempt.clone()))?;
        attempts.push(attempt);
    }
    let summary = Summary::from_attempts(&attempts);
    emit(Event::Summary(summary.clone()))?;
    emit(Event::Done)?;
    Ok(summary)
}

fn millis(duration: Duration) -> f64 {
    (duration.as_micros() as f64 / 1000.0 * 100.0).round() / 100.0
}
/// Peer-provided text (DNS data, proxy errors) must stay one bounded line.
fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

struct Prepared<'a> {
    note: String,
    kind: Kind<'a>,
}
enum Kind<'a> {
    Tcp {
        path: Path<'a>,
        target: Target,
    },
    Icmp {
        path: Path<'static>,
        address: IpAddr,
    },
    Dns {
        path: Path<'a>,
        transport: Transport,
        server: Target,
        tls_name: String,
        name: String,
        record: RecordType,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Udp,
    Tcp,
    Tls,
}

async fn prepare<'a>(path: &Path<'a>, probe: &Probe) -> Result<Prepared<'a>> {
    let mut notes = vec![describe(path).await?];
    let kind = match probe {
        Probe::Tcp { host, port } => {
            let host = host.trim().trim_matches(['[', ']']).to_owned();
            let target = match path {
                // Proxies resolve remotely; local paths ping one fixed address like ping(8).
                Path::Outbound(..) => Target { host, port: *port },
                _ => {
                    let address = resolve(&host, *port).await?;
                    if address.ip().to_string() != host {
                        notes.push(format!("{host} → {}", address.ip()));
                    }
                    Target {
                        host: address.ip().to_string(),
                        port: *port,
                    }
                }
            };
            if matches!(path, Path::Outbound(_, RouteAction::Proxy(_))) {
                notes.push("latency includes the proxy handshake".into());
            }
            Kind::Tcp {
                path: clone_path(path),
                target,
            }
        }
        Probe::Icmp { host } => {
            let local = path.local().context(
                "ICMP cannot be carried by a proxy outbound; use a TCP ping or pick DIRECT / an interface",
            )?;
            let host = host.trim().trim_matches(['[', ']']);
            let address = resolve(host, 0).await?.ip();
            if address.to_string() != host {
                notes.push(format!("{host} → {address}"));
            }
            if let (Path::Outbound(..), Path::Interface(name)) = (path, &local) {
                notes.push(format!("DIRECT is pinned to {name}"));
            }
            Kind::Icmp {
                path: local,
                address,
            }
        }
        Probe::Dns {
            name,
            record,
            server,
        } => {
            let (mut transport, explicit, host, port) = parse_server(server)?;
            if transport == Transport::Udp && path.local().is_none() {
                ensure!(
                    !explicit,
                    "UDP DNS cannot be carried by a proxy outbound; use tcp:// or tls://"
                );
                transport = Transport::Tcp;
                notes.push("UDP is not proxied; querying over TCP".into());
            }
            let server = match path {
                Path::Outbound(_, RouteAction::Proxy(_)) => Target {
                    host: host.clone(),
                    port,
                },
                _ => {
                    let address = resolve(&host, port).await?;
                    Target {
                        host: address.ip().to_string(),
                        port,
                    }
                }
            };
            let path = if transport == Transport::Udp {
                path.local().expect("checked above")
            } else {
                clone_path(path)
            };
            Kind::Dns {
                path,
                transport,
                server,
                tls_name: host,
                name: name.trim().to_owned(),
                record: record_type(record)?,
            }
        }
    };
    Ok(Prepared {
        note: notes.join(" · "),
        kind,
    })
}
fn clone_path<'a>(path: &Path<'a>) -> Path<'a> {
    match path {
        Path::System => Path::System,
        Path::Interface(name) => Path::Interface(name.clone()),
        Path::Outbound(app, action) => Path::Outbound(app, action.clone()),
    }
}
async fn describe(path: &Path<'_>) -> Result<String> {
    Ok(match path {
        Path::System => "OS routing table".into(),
        Path::Interface(name) => {
            #[cfg(windows)]
            {
                let index = crate::platform::windows_network::interface_index(name)?;
                match icmp::source_address(name, true) {
                    Ok(ip) => format!("adapter {name} (index {index}, {ip})"),
                    Err(_) => format!("adapter {name} (index {index})"),
                }
            }
            #[cfg(not(windows))]
            format!("adapter {name}")
        }
        Path::Outbound(_, RouteAction::Direct) => "profile DIRECT".into(),
        Path::Outbound(app, action) => {
            let resolved = app.resolve_action(action).await?;
            match (action, &resolved) {
                (RouteAction::Proxy(group), RouteAction::Proxy(member)) if group != member => {
                    format!("group {group} → {member}")
                }
                (_, RouteAction::Direct) => "outbound resolves to DIRECT".into(),
                (RouteAction::Proxy(tag), _) => format!("outbound {tag}"),
                _ => "outbound".into(),
            }
        }
    })
}
async fn resolve(host: &str, port: u16) -> Result<SocketAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolve {host}"))?
        .next()
        .with_context(|| format!("{host} has no addresses"))
}
fn record_type(text: &str) -> Result<RecordType> {
    let text = text.trim().to_ascii_uppercase();
    let text = if text.is_empty() { "A".into() } else { text };
    text.parse::<RecordType>()
        .ok()
        .filter(|kind| !matches!(kind, RecordType::Unknown(_)))
        .with_context(|| format!("unknown DNS record type {text}"))
}
/// `[udp|tcp|tls://]host[:port]`; IPv6 literals need brackets when a port is given.
fn parse_server(text: &str) -> Result<(Transport, bool, String, u16)> {
    let text = text.trim();
    let text = if text.is_empty() { "1.1.1.1" } else { text };
    let (transport, explicit, rest) = match text.split_once("://") {
        Some((scheme, rest)) => (
            match scheme.to_ascii_lowercase().as_str() {
                "udp" => Transport::Udp,
                "tcp" => Transport::Tcp,
                "tls" | "dot" => Transport::Tls,
                other => bail!("unsupported DNS scheme {other}; use udp, tcp or tls"),
            },
            true,
            rest,
        ),
        None => (Transport::Udp, false, text),
    };
    let rest = rest.trim_end_matches('/');
    let default = if transport == Transport::Tls { 853 } else { 53 };
    if let Ok(ip) = rest.parse::<IpAddr>() {
        return Ok((transport, explicit, ip.to_string(), default));
    }
    if let Ok(address) = rest.parse::<SocketAddr>() {
        return Ok((
            transport,
            explicit,
            address.ip().to_string(),
            address.port(),
        ));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            (host, port.parse().context("invalid DNS server port")?)
        }
        _ => (rest, default),
    };
    let host = host.trim_matches(['[', ']']);
    ensure!(!host.is_empty() && port != 0, "invalid DNS server {text}");
    Ok((transport, explicit, host.to_owned(), port))
}

impl Prepared<'_> {
    /// Returns a precise RTT when the transport measures one itself.
    async fn attempt(&mut self, seq: u32, timeout: Duration) -> Result<(Option<f64>, String)> {
        match &self.kind {
            Kind::Tcp { path, target } => {
                let start = Instant::now();
                let detail = match path {
                    Path::Outbound(app, action) => {
                        drop(app.connect(target, action).await?);
                        "connected".to_owned()
                    }
                    _ => {
                        let stream = egress::connect(target, path.interface()).await?;
                        let peer = stream.peer_addr()?;
                        drop(stream);
                        format!("connected to {peer}")
                    }
                };
                Ok((Some(millis(start.elapsed())), detail))
            }
            Kind::Icmp { path, address } => {
                let (address, interface) = (*address, path.interface().map(str::to_owned));
                let reply = tokio::task::spawn_blocking(move || {
                    icmp::echo(address, interface.as_deref(), seq as u16, timeout)
                })
                .await??;
                Ok((
                    Some(reply.rtt_ms),
                    match reply.ttl {
                        Some(ttl) => format!("reply from {} ttl={ttl}", reply.from),
                        None => format!("reply from {}", reply.from),
                    },
                ))
            }
            Kind::Dns {
                path,
                transport,
                server,
                tls_name,
                name,
                record,
            } => {
                let query = astra_dns::wire::query(name, *record)?;
                let bytes = query.to_vec()?;
                let start = Instant::now();
                let reply = match transport {
                    Transport::Udp => udp_exchange(path.interface(), server, &bytes).await?,
                    Transport::Tcp | Transport::Tls => {
                        let stream: crate::outbound::Stream = match path {
                            Path::Outbound(app, action) => app.connect(server, action).await?,
                            _ => Box::new(egress::connect(server, path.interface()).await?),
                        };
                        if *transport == Transport::Tls {
                            let name = tokio_rustls::rustls::pki_types::ServerName::try_from(
                                tls_name.clone(),
                            )
                            .context("invalid TLS server name")?;
                            let tls = crate::dns::tls_connector(false)?
                                .connect(name, stream)
                                .await
                                .context("DNS-over-TLS handshake")?;
                            crate::dns::framed(tls, &bytes).await?
                        } else {
                            crate::dns::framed(stream, &bytes).await?
                        }
                    }
                };
                let rtt = millis(start.elapsed());
                let reply = Message::from_vec(&reply).context("malformed DNS reply")?;
                astra_dns::wire::validate_response(&query, &reply)?;
                Ok((Some(rtt), answers(&reply)))
            }
        }
    }
}

async fn udp_exchange(interface: Option<&str>, server: &Target, query: &[u8]) -> Result<Vec<u8>> {
    let address = SocketAddr::new(server.host.parse()?, server.port);
    let address = egress::scoped_destination(address, interface)?;
    let socket = tokio::net::UdpSocket::bind(if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .await?;
    egress::bind_interface(
        &socket,
        interface.filter(|_| !address.ip().is_loopback()),
        address.is_ipv4(),
    )?;
    socket.connect(address).await?;
    socket.send(query).await?;
    let mut bytes = vec![0; 65535];
    loop {
        let size = socket.recv(&mut bytes).await?;
        // Ignore stray datagrams; the caller's timeout bounds the wait.
        if size >= 2 && bytes[..2] == query[..2] {
            bytes.truncate(size);
            return Ok(bytes);
        }
    }
}

fn answers(reply: &Message) -> String {
    let code = reply.metadata.response_code;
    let records: Vec<String> = reply
        .answers
        .iter()
        .map(|record| format!("{} {}", record.data.record_type(), record.data))
        .collect();
    if records.is_empty() {
        return format!("{code} · no answers");
    }
    let shown = records.len().min(8);
    let mut text = format!("{code} · {}", records[..shown].join(" · "));
    if records.len() > shown {
        text.push_str(&format!(" · +{} more", records.len() - shown));
    }
    text
}

/// `astra-proxy --nettool [--config PATH] [--via EGRESS] [-c N] [-i MS] [-W MS] dns|tcp|icmp ...`
pub async fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    const USAGE: &str = "usage: astra-proxy --nettool [--config PATH] [--via system|iface:NAME|outbound:TAG] [-c COUNT] [-i INTERVAL_MS] [-W TIMEOUT_MS]\n  dns NAME [TYPE] [@udp|tcp|tls://SERVER[:PORT]]\n  tcp HOST:PORT | tcp HOST PORT\n  icmp HOST";
    let (mut config, mut egress, mut count, mut interval, mut wait) =
        (None, Egress::System, None, 1000, 3000);
    let mut positional = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("{name} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--config" => config = Some(value("--config")?),
            "--via" => egress = value("--via")?.parse().map_err(anyhow::Error::msg)?,
            "-c" => count = Some(value("-c")?.parse()?),
            "-i" => interval = value("-i")?.parse()?,
            "-W" => wait = value("-W")?.parse()?,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    let probe = match positional.next().as_deref() {
        Some("dns") => {
            let name = positional.next().context(USAGE)?;
            let (mut record, mut server) = ("A".to_owned(), "1.1.1.1".to_owned());
            for arg in positional.by_ref() {
                match arg.strip_prefix('@') {
                    Some(value) => server = value.to_owned(),
                    None => record = arg,
                }
            }
            Probe::Dns {
                name,
                record,
                server,
            }
        }
        Some("tcp") => {
            let host = positional.next().context(USAGE)?;
            let (host, port) = match positional.next() {
                Some(port) => (host, port.parse()?),
                None => {
                    let address = host.rsplit_once(':').context("tcp needs HOST:PORT")?;
                    (
                        address.0.trim_matches(['[', ']']).to_owned(),
                        address.1.parse()?,
                    )
                }
            };
            Probe::Tcp { host, port }
        }
        Some("icmp" | "ping") => Probe::Icmp {
            host: positional.next().context(USAGE)?,
        },
        _ => bail!("{USAGE}"),
    };
    let request = Request {
        config: String::new(),
        egress,
        count: count.unwrap_or(if matches!(probe, Probe::Dns { .. }) {
            1
        } else {
            4
        }),
        probe,
        interval_ms: interval,
        timeout_ms: wait,
    };
    let app = match config {
        Some(path) => {
            let path = std::path::absolute(path)?;
            Some(crate::subscription::cached_app(&path, App::load(&path)?))
        }
        None => None,
    };
    let summary = run(app.as_ref(), &request, false, |event| {
        match event {
            Event::Started {
                egress,
                probe,
                note,
                ..
            } => println!("{probe} via {egress} ({note})"),
            Event::Attempt(a) => match (&a.error, a.rtt_ms) {
                (None, Some(ms)) => println!("  #{:<3} {ms:>8.2} ms  {}", a.seq, a.detail),
                (error, _) => println!(
                    "  #{:<3} {:>11}  {}",
                    a.seq,
                    "failed",
                    error.as_deref().unwrap_or("")
                ),
            },
            Event::Summary(s) => println!(
                "{} sent, {} received, {}% loss{}",
                s.sent,
                s.received,
                s.loss_percent(),
                match (s.min_ms, s.avg_ms, s.max_ms) {
                    (Some(min), Some(avg), Some(max)) =>
                        format!(" · min/avg/max {min:.2}/{avg:.2}/{max:.2} ms"),
                    _ => String::new(),
                }
            ),
            Event::Done => {}
        }
        Ok(())
    })
    .await?;
    ensure!(summary.received > 0, "no successful replies");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn request(egress: Egress, probe: Probe) -> Request {
        Request {
            config: String::new(),
            egress,
            probe,
            count: 2,
            interval_ms: 0,
            timeout_ms: 2000,
        }
    }
    async fn collect(app: Option<&App>, request: Request) -> Result<Vec<Event>> {
        let events = Mutex::new(Vec::new());
        run(app, &request, false, |event| {
            events.lock().unwrap().push(event);
            Ok(())
        })
        .await?;
        Ok(events.into_inner().unwrap())
    }

    #[test]
    fn server_forms_and_record_types_parse() {
        assert_eq!(
            parse_server("").unwrap(),
            (Transport::Udp, false, "1.1.1.1".into(), 53)
        );
        assert_eq!(
            parse_server("tls://dns.google").unwrap(),
            (Transport::Tls, true, "dns.google".into(), 853)
        );
        assert_eq!(
            parse_server("tcp://[2001:db8::1]:5353").unwrap(),
            (Transport::Tcp, true, "2001:db8::1".into(), 5353)
        );
        assert_eq!(
            parse_server("2001:db8::1").unwrap(),
            (Transport::Udp, false, "2001:db8::1".into(), 53)
        );
        assert!(parse_server("https://dns.google").is_err());
        assert_eq!(record_type("aaaa").unwrap(), RecordType::AAAA);
        assert!(record_type("NOPE").is_err());
    }

    #[tokio::test]
    async fn tcp_ping_reports_each_attempt_and_refused_ports() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let events = collect(
            None,
            request(
                Egress::System,
                Probe::Tcp {
                    host: "127.0.0.1".into(),
                    port,
                },
            ),
        )
        .await
        .unwrap();
        assert!(matches!(events[0], Event::Started { .. }));
        let Event::Summary(summary) = &events[3] else {
            panic!("{events:?}")
        };
        assert_eq!((summary.sent, summary.received), (2, 2));
        assert_eq!(events.last(), Some(&Event::Done));

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let events = collect(
            None,
            request(
                Egress::System,
                Probe::Tcp {
                    host: "127.0.0.1".into(),
                    port,
                },
            ),
        )
        .await
        .unwrap();
        let Event::Attempt(attempt) = &events[1] else {
            panic!()
        };
        assert!(attempt.error.is_some() && attempt.rtt_ms.is_none());
    }

    async fn dns_server() -> (SocketAddr, SocketAddr) {
        let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addresses = (udp.local_addr().unwrap(), tcp.local_addr().unwrap());
        fn answer(query: &[u8]) -> Vec<u8> {
            let query = Message::from_vec(query).unwrap();
            let mut reply =
                astra_dns::wire::response(&query, hickory_proto::op::ResponseCode::NoError);
            reply.add_answer(hickory_proto::rr::Record::from_rdata(
                query.queries[0].name().clone(),
                60,
                hickory_proto::rr::RData::A("192.0.2.7".parse().unwrap()),
            ));
            reply.to_vec().unwrap()
        }
        tokio::spawn(async move {
            let mut buffer = [0; 1500];
            while let Ok((size, peer)) = udp.recv_from(&mut buffer).await {
                udp.send_to(&answer(&buffer[..size]), peer).await.unwrap();
            }
        });
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = tcp.accept().await {
                let size = socket.read_u16().await.unwrap() as usize;
                let mut query = vec![0; size];
                socket.read_exact(&mut query).await.unwrap();
                let reply = answer(&query);
                socket.write_u16(reply.len() as u16).await.unwrap();
                socket.write_all(&reply).await.unwrap();
            }
        });
        addresses
    }

    #[tokio::test]
    async fn dns_queries_over_udp_tcp_and_through_a_proxy_outbound() {
        let (udp, tcp) = dns_server().await;
        for server in [udp.to_string(), format!("tcp://{tcp}")] {
            let events = collect(
                None,
                request(
                    Egress::System,
                    Probe::Dns {
                        name: "example.test".into(),
                        record: "A".into(),
                        server,
                    },
                ),
            )
            .await
            .unwrap();
            let Event::Attempt(attempt) = &events[1] else {
                panic!()
            };
            assert!(attempt.detail.contains("192.0.2.7"), "{attempt:?}");
        }
        // A SOCKS5 outbound carries the query as DNS-over-TCP; bare servers switch from UDP.
        let socks = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_address = socks.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut client, _) = socks.accept().await.unwrap();
            let mut greeting = [0; 3];
            client.read_exact(&mut greeting).await.unwrap();
            client.write_all(&[5, 0]).await.unwrap();
            let mut header = [0; 4];
            client.read_exact(&mut header).await.unwrap();
            assert_eq!(
                header[3], 1,
                "proxy must receive the literal server address"
            );
            let mut rest = [0; 6];
            client.read_exact(&mut rest).await.unwrap();
            client
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let mut upstream = tokio::net::TcpStream::connect(tcp).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
        let app = App::from_text(
            std::path::Path::new("nettool.toml"),
            format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='{socks_address}'\n"),
        )
        .unwrap();
        let mut dns = request(
            Egress::Outbound("edge".into()),
            Probe::Dns {
                name: "example.test".into(),
                record: "A".into(),
                server: tcp.to_string(),
            },
        );
        dns.count = 1;
        let events = collect(Some(&app), dns.clone()).await.unwrap();
        let Event::Started { note, .. } = &events[0] else {
            panic!()
        };
        assert!(note.contains("over TCP"), "{note}");
        let Event::Attempt(attempt) = &events[1] else {
            panic!()
        };
        assert!(attempt.detail.contains("192.0.2.7"), "{attempt:?}");

        if let Probe::Dns { server, .. } = &mut dns.probe {
            *server = format!("udp://{udp}");
        }
        assert!(collect(Some(&app), dns).await.is_err());
        let icmp = request(
            Egress::Outbound("edge".into()),
            Probe::Icmp {
                host: "127.0.0.1".into(),
            },
        );
        assert!(format!("{:#}", collect(Some(&app), icmp).await.unwrap_err()).contains("ICMP"));
        let missing = request(
            Egress::Outbound("missing".into()),
            Probe::Icmp {
                host: "127.0.0.1".into(),
            },
        );
        assert!(collect(Some(&app), missing).await.is_err());
    }

    #[tokio::test]
    async fn icmp_echo_reaches_loopback() {
        let mut probe = request(
            Egress::System,
            Probe::Icmp {
                host: "127.0.0.1".into(),
            },
        );
        probe.count = 1;
        match collect(None, probe).await {
            Ok(events) => {
                let Event::Attempt(attempt) = &events[1] else {
                    panic!()
                };
                assert!(attempt.error.is_none(), "{attempt:?}");
            }
            // Unix hosts may forbid unprivileged ICMP sockets (net.ipv4.ping_group_range).
            Err(error) if cfg!(unix) => eprintln!("skipped: {error:#}"),
            Err(error) => panic!("{error:#}"),
        }
    }
}
