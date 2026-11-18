mod https;
mod tcp;
#[cfg(test)]
#[path = "../../test/dns_upstream_test.rs"]
mod tests;
mod tls;
mod udp;
use super::DnsRoute;
use crate::App;
use anyhow::{Context, Result};
use astra_dns::{wire, Upstream};
use astra_model::Inbound;
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
    if let Some(local) = dns.lookup_magic(host) {
        return local?
            .into_iter()
            .next()
            .context("Magic DNS bootstrap has no usable address");
    }
    if let Some(ip) = dns.bootstrap_cached(host) {
        return Ok(ip);
    }
    let config = &dns.config;
    let mut servers = config.direct_bootstrap();
    if servers.is_empty() && config.plaintext_bypass {
        servers = config
            .bootstrap
            .iter()
            .map(|ip| Upstream::parse(&ip.to_string()))
            .collect::<Result<_>>()?;
    }
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
        .context(format!("direct DNS bootstrap for {host} failed")))
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
        let bytes = exchange_at(app, server, literal, query.to_vec()?, None, None).await?;
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
pub(super) async fn exchange(
    app: &App,
    upstream: &Upstream,
    query: Vec<u8>,
    route: Option<&DnsRoute>,
) -> Result<Vec<u8>> {
    let exchange = exchange_tls(app, upstream, query, None, route);
    match route {
        Some((_, lease)) => tokio::select! {
            biased;
            _ = lease.cancelled() => anyhow::bail!("DNS route changed during query"),
            result = exchange => result,
        },
        None => exchange.await,
    }
}
async fn exchange_tls(
    app: &App,
    upstream: &Upstream,
    query: Vec<u8>,
    connector: Option<tokio_rustls::TlsConnector>,
    route: Option<&DnsRoute>,
) -> Result<Vec<u8>> {
    let ip = bootstrap(app, &upstream.host).await?;
    exchange_at(app, upstream, ip, query, connector, route).await
}
async fn exchange_at(
    app: &App,
    upstream: &Upstream,
    ip: IpAddr,
    query: Vec<u8>,
    connector: Option<tokio_rustls::TlsConnector>,
    route: Option<&DnsRoute>,
) -> Result<Vec<u8>> {
    let address = SocketAddr::new(ip, upstream.port);
    let dial = Target {
        host: ip.to_string(),
        port: upstream.port,
    };
    if upstream.scheme == "udp" {
        let bytes = if let Some((action, lease)) = route {
            let connection = crate::udp::open_planned(
                app,
                &dial,
                Inbound::Tun,
                "127.0.0.1:0".parse()?,
                action.clone(),
                lease.clone(),
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
    let stream: crate::outbound::Stream = match route {
        Some((action, lease)) => app.connect_planned(&dial, action, lease).await?,
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
