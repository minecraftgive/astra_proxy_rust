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
use astra_model::RouteAction;
use astra_model::Target;
use hickory_proto::{
    op::Message,
    rr::{RData, RecordType},
};
use https::exchange as doh;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
pub(crate) use tcp::exchange as framed;
pub(crate) use tls::connector as tls_connector;
use udp::direct as udp_direct;

async fn bootstrap(app: &App, host: &str, direct: bool) -> Result<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    let dns = app.resolver().context("DNS disabled")?;
    if let Some(ip) = dns.bootstrap_cached(host) {
        return Ok(ip);
    }
    let (ip, ttl) = if dns.config.plaintext_bypass {
        plaintext_bootstrap(app, host).await?
    } else {
        encrypted_bootstrap(app, host, direct).await?
    };
    dns.bootstrap_store(host, ip, ttl);
    Ok(ip)
}
fn first_a(query: &Message, bytes: &[u8]) -> Result<(IpAddr, u32)> {
    let reply = Message::from_vec(bytes)?;
    wire::validate_response(query, &reply)?;
    reply
        .answers
        .iter()
        .find_map(|record| {
            if let RData::A(ip) = record.data {
                Some((IpAddr::V4(ip.0), record.ttl))
            } else {
                None
            }
        })
        .context("bootstrap has no A answer")
}
/// Resolve an upstream hostname only through IP-literal DoH/DoT upstreams;
/// there is no plaintext fallback. Boxed as `Send` because exchange() calls
/// back into bootstrap (literal-IP upstreams return before recursing).
fn encrypted_bootstrap<'a>(
    app: &'a App,
    host: &'a str,
    direct: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(IpAddr, u32)>> + Send + 'a>> {
    Box::pin(async move {
        let dns = app.resolver().context("DNS disabled")?;
        let mut last = None;
        for upstream in dns.config.encrypted_bootstrap() {
            let query = wire::query(host, RecordType::A)?;
            let result = tokio::time::timeout(
                Duration::from_millis(dns.config.query_timeout_ms),
                exchange_tls(app, &upstream, query.to_vec()?, None, direct),
            )
            .await;
            match result {
                Ok(Ok(bytes)) => match first_a(&query, &bytes) {
                    Ok(found) => return Ok(found),
                    Err(error) => last = Some(error),
                },
                Ok(Err(error)) => last = Some(error),
                Err(error) => last = Some(error.into()),
            }
        }
        Err(last
            .unwrap_or_else(|| {
                anyhow::anyhow!("no IP-literal DoH/DoT upstream for encrypted bootstrap")
            })
            .context(format!("encrypted DNS bootstrap for {host} failed")))
    })
}
/// Legacy UDP/53 bootstrap, reachable only with dns.plaintext_bypass = true.
async fn plaintext_bootstrap(app: &App, host: &str) -> Result<(IpAddr, u32)> {
    let dns = app.resolver().context("DNS disabled")?;
    let mut last = None;
    for ip in &dns.config.bootstrap {
        let query = wire::query(host, RecordType::A)?;
        let target = SocketAddr::new(*ip, 53);
        let result = tokio::time::timeout(
            Duration::from_millis(dns.config.query_timeout_ms / 2),
            async { first_a(&query, &udp_direct(app, target, &query.to_vec()?).await?) },
        )
        .await;
        match result {
            Ok(Ok(found)) => return Ok(found),
            Ok(Err(error)) => last = Some(error),
            Err(error) => last = Some(error.into()),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("DNS bootstrap failed")))
}
fn interface(app: &App) -> Option<&str> {
    app.config
        .tun
        .as_ref()
        .and_then(|tun| tun.interface.as_deref())
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
    let ip = bootstrap(app, &upstream.host, direct).await?;
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
    let action = if dns.config.respect_rules && !direct {
        app.route_network(&target, Inbound::Tun, network).await?
    } else {
        RouteAction::Direct
    };
    let mut stream = None;
    if upstream.scheme == "udp" {
        // The UDP helper routes once; to preserve hostname policy select an
        // explicit action rather than resolving the literal address again.
        let bytes = if action == RouteAction::Direct {
            udp_direct(app, address, &query).await?
        } else {
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
        };
        if !Message::from_vec(&bytes)?.metadata.truncation {
            return Ok(bytes);
        }
        // RFC 7766: retry truncated UDP responses using framed TCP.
        stream = Some(app.connect(&dial, &action).await?);
    }
    let stream = match stream {
        Some(stream) => stream,
        None => app.connect(&dial, &action).await?,
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
