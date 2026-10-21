mod https;
mod tcp;
#[cfg(test)]
mod tests;
mod tls;
mod udp;
use crate::App;
use anyhow::{Context, Result};
use astra_dns::{wire, Upstream};
use astra_ipc::{Inbound, Network, RouteAction, Target};
use hickory_proto::{
    op::Message,
    rr::{RData, RecordType},
};
use https::exchange as doh;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
pub(super) use tcp::exchange as framed;
use tls::connector as tls_connector;
use udp::direct as udp_direct;

async fn bootstrap(app: &App, host: &str) -> Result<IpAddr> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    let dns = app.dns.as_ref().context("DNS disabled")?;
    let mut last = None;
    for ip in &dns.config.bootstrap {
        let query = wire::query(host, RecordType::A)?;
        let target = SocketAddr::new(*ip, 53);
        let result = tokio::time::timeout(
            Duration::from_millis(dns.config.query_timeout_ms / 2),
            async {
                let bytes = udp_direct(app, target, &query.to_vec()?).await?;
                let reply = Message::from_vec(&bytes)?;
                wire::validate_response(&query, &reply)?;
                reply
                    .answers
                    .iter()
                    .find_map(|record| {
                        if let RData::A(ip) = record.data {
                            Some(IpAddr::V4(ip.0))
                        } else {
                            None
                        }
                    })
                    .context("bootstrap has no A answer")
            },
        )
        .await;
        match result {
            Ok(Ok(ip)) => return Ok(ip),
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
    exchange_tls(app, upstream, query, None).await
}
async fn exchange_tls(
    app: &App,
    upstream: &Upstream,
    query: Vec<u8>,
    connector: Option<tokio_rustls::TlsConnector>,
) -> Result<Vec<u8>> {
    let ip = bootstrap(app, &upstream.host).await?;
    let dns = app.dns.as_ref().context("DNS disabled")?;
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
    let action = if dns.config.respect_rules {
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
