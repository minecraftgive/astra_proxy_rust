mod https;
mod tcp;
#[cfg(test)]
#[path = "../../test/dns_upstream_test.rs"]
mod tests;
mod tls;
mod udp;
use crate::App;
use anyhow::{Context, Result};
use astra_dns::{wire, Upstream};
use astra_model::Inbound;
use astra_model::Network;
use astra_model::Target;
use hickory_proto::{op::Message, rr::RecordType};
use https::exchange as doh;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
pub(crate) use tcp::exchange as framed;
pub(crate) use tls::connector as tls_connector;
use udp::direct as udp_direct;

async fn bootstrap(app: &App, host: &str) -> Result<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    let dns = app.resolver().context("DNS disabled")?;
    if let Some(ip) = dns.bootstrap_cached(host) {
        return Ok(ip);
    }
    let config = &dns.config;
    let (servers, label) = if !config.proxy_server_nameserver.is_empty() {
        (config.proxy_bootstrap(), "proxy DNS bootstrap")
    } else if config.plaintext_bypass {
        let servers = config
            .bootstrap
            .iter()
            .map(|ip| Upstream::parse(&ip.to_string()))
            .collect::<Result<Vec<_>>>()?;
        (servers, "plaintext DNS bootstrap")
    } else {
        (config.encrypted_bootstrap(), "encrypted DNS bootstrap")
    };
    // One unreachable bootstrap server must not consume the entire exchange
    // deadline when another literal server is available.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(config.query_timeout_ms);
    let mut last = None;
    for (index, server) in servers.iter().enumerate() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let budget = remaining / (servers.len() - index) as u32;
        match tokio::time::timeout(budget, bootstrap_from(app, server, host, config.ipv6)).await {
            Ok(Ok((ip, ttl))) => {
                dns.bootstrap_store(host, ip, ttl);
                return Ok(ip);
            }
            Ok(Err(error)) => last = Some(error),
            Err(error) => last = Some(error.into()),
        }
    }
    Err(last
        .unwrap_or_else(|| anyhow::anyhow!("no bootstrap servers"))
        .context(format!("{label} for {host} failed")))
}

async fn bootstrap_from(
    app: &App,
    server: &Upstream,
    host: &str,
    ipv6: bool,
) -> Result<(IpAddr, u32)> {
    let literal = server
        .host
        .parse::<IpAddr>()
        .context("bootstrap server requires a literal IP")?;
    for kind in [RecordType::A, RecordType::AAAA] {
        if kind == RecordType::AAAA && !ipv6 {
            break;
        }
        let query = wire::query(host, kind)?;
        let bytes = exchange_at(app, server, literal, query.to_vec()?, None, true).await?;
        let reply = Message::from_vec(&bytes)?;
        wire::validate_response(&query, &reply)?;
        anyhow::ensure!(
            reply.metadata.response_code == hickory_proto::op::ResponseCode::NoError,
            "bootstrap server returned {:?}",
            reply.metadata.response_code
        );
        let address = wire::address_answers(&reply).next();
        if let Some(found) = address {
            return Ok(found);
        }
    }
    anyhow::bail!("no usable bootstrap address")
}

fn interface(app: &App) -> Option<std::sync::Arc<str>> {
    meow_common::outbound_iface::outbound_interface().or_else(|| {
        app.config
            .tun
            .as_ref()?
            .interface
            .as_deref()
            .map(Into::into)
    })
}
pub(super) async fn exchange(app: &App, upstream: &Upstream, query: Vec<u8>) -> Result<Vec<u8>> {
    exchange_tls(app, upstream, query, None, false).await
}
/// Same as exchange() but never routes through proxies (respect_rules).
pub(super) async fn exchange_direct(
    app: &App,
    upstream: &Upstream,
    query: Vec<u8>,
) -> Result<Vec<u8>> {
    exchange_tls(app, upstream, query, None, true).await
}
async fn exchange_tls(
    app: &App,
    upstream: &Upstream,
    query: Vec<u8>,
    connector: Option<tokio_rustls::TlsConnector>,
    direct: bool,
) -> Result<Vec<u8>> {
    let ip = bootstrap(app, &upstream.host).await?;
    exchange_at(app, upstream, ip, query, connector, direct).await
}
async fn exchange_at(
    app: &App,
    upstream: &Upstream,
    ip: IpAddr,
    query: Vec<u8>,
    connector: Option<tokio_rustls::TlsConnector>,
    direct: bool,
) -> Result<Vec<u8>> {
    let dns = app.resolver().context("DNS disabled")?;
    let address = SocketAddr::new(ip, upstream.port);
    // Route using upstream hostname, dial its bootstrapped literal IP. This
    // avoids resolving the DNS server through itself.
    let target = Target {
        host: upstream.host.clone(),
        port: upstream.port,
    };
    let dial = Target {
        host: ip.to_string(),
        port: upstream.port,
    };
    let network = if upstream.scheme == "udp" {
        Network::Udp
    } else {
        Network::Tcp
    };
    let action = if dns.config.respect_rules && !direct && !dns.config.always_direct(upstream) {
        Some(app.route_network(&target, Inbound::Tun, network).await?)
    } else {
        None
    };
    if upstream.scheme == "udp" {
        // The UDP helper routes once; to preserve hostname policy select an
        // explicit action rather than resolving the literal address again.
        let bytes = if let Some(action) = &action {
            let connection = crate::udp::open_action(
                app,
                &dial,
                Inbound::Tun,
                "127.0.0.1:0".parse()?,
                action.clone(),
            )
            .await?;
            connection.send(&query, address).await?;
            let mut bytes = vec![0; 65535];
            let (size, _) = connection.recv(&mut bytes).await?;
            bytes.truncate(size);
            bytes
        } else {
            udp_direct(app, address, &query).await?
        };
        if !Message::from_vec(&bytes)?.metadata.truncation {
            return Ok(bytes);
        }
        // RFC 7766: retry truncated UDP responses using framed TCP.
    }
    let stream: crate::outbound::Stream = match action {
        Some(action) => app.connect(&dial, &action).await?,
        None => Box::new(crate::egress::connect(&dial, interface(app).as_deref()).await?),
    };
    match upstream.scheme.as_str() {
        "udp" | "tcp" => framed(stream, &query).await,
        "tls" | "https" => {
            let connector = match connector {
                Some(connector) => connector,
                None => tls_connector(upstream.scheme == "https")?,
            };
            let name =
                tokio_rustls::rustls::pki_types::ServerName::try_from(upstream.host.clone())?;
            let tls = connector
                .connect(name, stream)
                .await
                .context("DNS upstream TLS verification/handshake failed")?;
            if upstream.scheme == "tls" {
                framed(tls, &query).await
            } else {
                let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
                doh(tls, upstream, query, h2).await
            }
        }
        _ => anyhow::bail!("unsupported DNS upstream"),
    }
}
