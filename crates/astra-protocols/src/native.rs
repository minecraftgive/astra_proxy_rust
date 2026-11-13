//! In-process Rust protocol codecs and composable transports. No child processes.
use crate::{config::OutboundConfig, outbound::Stream};
use anyhow::{bail, Context, Result};
use base64::Engine;
use meow_common::{Metadata, ProxyAdapter};
use meow_proxy::{
    dialer::{DirectDialer, TcpDialer},
    TransportChain,
};
use meow_transport::tls::{RealityConfig, TlsConfig, TlsLayer};
use std::{collections::HashMap, path::Path, sync::Arc};

pub use astra_model::config::protocol::{
    NodeConfig, Protocol, TlsOptions, TransportKind, TransportOptions,
};

pub enum NativeOutbound {
    Tuic(crate::tuic::Client),
    Legacy {
        config: OutboundConfig,
        dialer: Arc<dyn TcpDialer>,
    },
    Adapter(Arc<dyn ProxyAdapter>),
    Trojan {
        server: String,
        port: u16,
        password_hash: String,
        transport: TransportChain,
        dialer: Arc<dyn TcpDialer>,
    },
}
pub type Registry = HashMap<String, Arc<NativeOutbound>>;

pub fn compile(configs: &[OutboundConfig], base: &Path) -> Result<Registry> {
    configs
        .iter()
        .map(|config| {
            Ok((
                config.tag().to_owned(),
                Arc::new(compile_one(config, base, Arc::new(DirectDialer))?),
            ))
        })
        .collect()
}

pub fn compile_one(
    config: &OutboundConfig,
    base: &Path,
    dialer: Arc<dyn TcpDialer>,
) -> Result<NativeOutbound> {
    if config.requires_direct_entry() && dialer.is_proxy() {
        bail!("{}: QUIC nodes must be the first hop", config.tag());
    }
    match config {
        OutboundConfig::Socks5 {
            tag,
            server,
            username,
            password,
            ..
        } => {
            let endpoint = crate::http::authority_target(server)?;
            Ok(NativeOutbound::Adapter(Arc::new(
                meow_proxy::Socks5Adapter::new(
                    tag,
                    &endpoint.host,
                    endpoint.port,
                    username.clone().zip(password.clone()),
                    false,
                    false,
                    dialer,
                )
                .with_udp(true),
            )))
        }
        OutboundConfig::Native { tag, config, .. } => {
            validate_common(config).with_context(|| format!("outbound {tag}"))?;
            match config.protocol {
                Protocol::Tuic => Ok(NativeOutbound::Tuic(crate::tuic::Client::new(
                    config, base,
                )?)),
                Protocol::Trojan => {
                    use sha2::{Digest, Sha224};
                    anyhow::ensure!(config.tls.is_some(), "Trojan requires TLS");
                    anyhow::ensure!(config.mux.is_none(), "Trojan mux is not yet supported");
                    Ok(NativeOutbound::Trojan {
                        server: config.server.clone(),
                        port: config.port,
                        password_hash: hex::encode(Sha224::digest(
                            required(&config.password, "password")?.as_bytes(),
                        )),
                        transport: transport(config, base)?,
                        dialer,
                    })
                }
                _ => Ok(NativeOutbound::Adapter(build(tag, config, base, dialer)?)),
            }
        }
        _ => Ok(NativeOutbound::Legacy {
            config: config.clone(),
            dialer,
        }),
    }
}

pub fn supports_udp(outbound: &NativeOutbound) -> bool {
    match outbound {
        NativeOutbound::Adapter(adapter) => adapter.support_udp(),
        NativeOutbound::Legacy { .. } => false,
        NativeOutbound::Tuic(_) | NativeOutbound::Trojan { .. } => true,
    }
}

pub async fn connect(outbound: &NativeOutbound, target: &astra_model::Target) -> Result<Stream> {
    use crate::stable_write::StableWrite;
    // meow transports (gRPC, h2) require identical bytes when a pending write is retried.
    match outbound {
        NativeOutbound::Tuic(client) => client.connect(target).await,
        NativeOutbound::Legacy { config, dialer } => {
            crate::outbound::open_with_dialer(target, config, dialer.as_ref()).await
        }
        NativeOutbound::Adapter(adapter) => Ok(Box::new(StableWrite::new(
            adapter.dial_tcp(&metadata(target)).await?,
        ))),
        NativeOutbound::Trojan { .. } => Ok(Box::new(StableWrite::new(
            crate::trojan::trojan_stream(outbound, target, 1).await?,
        ))),
    }
}

pub async fn connect_udp(
    outbound: &NativeOutbound,
    target: &astra_model::Target,
) -> Result<Box<dyn meow_common::ProxyPacketConn>> {
    match outbound {
        NativeOutbound::Tuic(client) => client.connect_udp(target).await,
        NativeOutbound::Legacy { .. } => bail!("outbound does not support UDP"),
        NativeOutbound::Adapter(adapter) => {
            if !adapter.support_udp() {
                bail!("outbound does not support UDP");
            }
            let mut metadata = metadata(target);
            metadata.network = meow_common::Network::Udp;
            Ok(adapter.dial_udp(&metadata).await?)
        }
        NativeOutbound::Trojan { .. } => {
            let stream = crate::trojan::trojan_stream(outbound, target, 3).await?;
            let bound_name = target
                .host
                .parse::<std::net::IpAddr>()
                .is_err()
                .then(|| (target.host.as_str().into(), target.port));
            Ok(Box::new(meow_proxy::trojan::TrojanPacketConn::new(
                stream, bound_name,
            )))
        }
    }
}

pub fn metadata(target: &astra_model::Target) -> Metadata {
    let ip = target.host.parse().ok();
    Metadata {
        dst_ip: ip,
        host: if ip.is_some() {
            "".into()
        } else {
            target.host.as_str().into()
        },
        dst_port: target.port,
        ..Metadata::default()
    }
}

pub(crate) fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("{name} is required"))
}

fn validate_common(cfg: &NodeConfig) -> Result<()> {
    if !matches!(cfg.protocol, Protocol::Tuic)
        && (cfg.tuic_udp_mode.is_some()
            || cfg.congestion_control.is_some()
            || cfg.tuic_idle_timeout_secs.is_some())
    {
        bail!("TUIC-specific transport options require TUIC");
    }
    if cfg.server.is_empty() || cfg.port == 0 {
        bail!("server and nonzero port are required");
    }
    if cfg.encryption.is_some() && !matches!(cfg.protocol, Protocol::Vless) {
        bail!("encryption is only valid for VLESS");
    }
    if cfg.flow.is_some() && !matches!(cfg.protocol, Protocol::Vless) {
        bail!("flow is only valid for VLESS");
    }
    if cfg.transport.mode.is_some() && cfg.transport.kind != TransportKind::Xhttp {
        bail!("mode is only valid for XHTTP");
    }
    if !cfg.transport.headers.is_empty()
        && matches!(cfg.transport.kind, TransportKind::Tcp | TransportKind::Grpc)
    {
        bail!("extra headers are unsupported for this transport");
    }
    if !matches!(cfg.protocol, Protocol::Hysteria2)
        && (cfg.pin_sha256.is_some() || cfg.obfs_password.is_some())
    {
        bail!("pin_sha256 and obfs_password are Hysteria2 options");
    }
    Ok(())
}

fn build(
    tag: &str,
    cfg: &NodeConfig,
    base: &Path,
    dialer: Arc<dyn TcpDialer>,
) -> Result<Arc<dyn ProxyAdapter>> {
    let mux = cfg
        .mux
        .as_deref()
        .map(|s| {
            let protocol =
                meow_proxy::mux::Protocol::parse(s).context("unsupported mux protocol")?;
            Ok::<_, anyhow::Error>(meow_proxy::mux::MuxOptions {
                protocol,
                ..Default::default()
            })
        })
        .transpose()?;
    match cfg.protocol {
        Protocol::Tuic => bail!("TUIC must use its native QUIC path"),
        Protocol::Trojan => bail!("Trojan must use the native transport path"),
        Protocol::Vless => crate::vless::build(tag, cfg, base, dialer, mux),
        Protocol::Vmess => crate::vmess::build(tag, cfg, base, dialer, mux),
        Protocol::Shadowsocks => crate::shadowsocks::build(tag, cfg, base, dialer, mux),
        Protocol::Hysteria2 => crate::hysteria2::build(tag, cfg, base, dialer, mux),
    }
}

// Match mihomo/Xray auto selection for our single HTTP/2 transport.
// Split download settings are not exposed by Astra.
fn effective_xhttp_mode(mode: Option<&str>, reality: bool) -> &str {
    match mode {
        Some("auto") => {
            if reality {
                "stream-one"
            } else {
                "packet-up"
            }
        }
        None => "stream-one", // Preserve existing Astra configuration semantics.
        Some(mode) => mode,
    }
}

pub(crate) fn transport(cfg: &NodeConfig, base: &Path) -> Result<TransportChain> {
    let mut chain = TransportChain::empty();
    if let Some(tls) = &cfg.tls {
        let mut options = TlsConfig::new(&tls.server_name);
        options.fingerprint = tls.fingerprint.clone();
        if let Some(pin) = &tls.cert_pin_sha256 {
            if tls.reality_public_key.is_some() {
                bail!("certificate pinning cannot be combined with REALITY");
            }
            if tls.ca_file.is_some() {
                bail!("certificate pinning replaces CA verification; omit ca_file");
            }
            options.cert_pin = Some(
                hex::decode(pin)
                    .context("invalid certificate SHA-256 hex")?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("certificate SHA-256 must be 32 bytes"))?,
            );
        }
        if let Some(fp) = &tls.fingerprint {
            if !meow_transport::tls::is_supported_fingerprint(fp) {
                bail!("unsupported TLS fingerprint: {fp}");
            }
        }
        options.alpn = match cfg.transport.kind {
            TransportKind::Grpc | TransportKind::Xhttp => vec!["h2".into()],
            TransportKind::Ws | TransportKind::HttpUpgrade => vec!["http/1.1".into()],
            TransportKind::Tcp => vec!["h2".into(), "http/1.1".into()],
        };
        if let Some(ca) = &tls.ca_file {
            let pem = std::fs::read(base.join(ca)).context("read custom CA")?;
            options.additional_roots = rustls_pemfile::certs(&mut pem.as_slice())
                .map(|r| r.map(|c| c.to_vec()))
                .collect::<std::io::Result<_>>()?;
            if options.additional_roots.is_empty() {
                bail!("custom CA file contains no certificates");
            }
        }
        if let Some(key) = &tls.reality_public_key {
            if matches!(
                cfg.transport.kind,
                TransportKind::Ws | TransportKind::HttpUpgrade
            ) {
                bail!("REALITY requires TCP, gRPC or XHTTP");
            }
            let public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(key)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("REALITY public key must be 32 bytes"))?;
            let short = hex::decode(tls.reality_short_id.as_deref().unwrap_or(""))?;
            if short.len() > 8 {
                bail!("REALITY short ID must be at most 8 bytes");
            }
            let mut short_id = [0; 8];
            short_id[..short.len()].copy_from_slice(&short);
            options.reality = Some(RealityConfig {
                public_key,
                short_id,
                support_x25519_mlkem768: false,
            });
        } else if tls.reality_short_id.is_some() {
            bail!("REALITY short ID requires a public key");
        }
        chain.push(Box::new(TlsLayer::new(&options)?));
    }
    let t = &cfg.transport;
    let host = t
        .host
        .clone()
        .or_else(|| cfg.tls.as_ref().map(|t| t.server_name.clone()))
        .unwrap_or_else(|| cfg.server.clone());
    let path = t.path.clone().unwrap_or_else(|| "/".into());
    if !path.starts_with('/') || t.headers.len() > 64 {
        bail!("invalid transport path or too many headers");
    }
    let headers = t
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    match t.kind {
        TransportKind::Tcp => {}
        TransportKind::Ws => chain.push(Box::new(meow_transport::ws::WsLayer::new(
            meow_transport::ws::WsConfig {
                path,
                host_header: Some(host),
                extra_headers: headers,
                ..Default::default()
            },
        )?)),
        TransportKind::Grpc => chain.push(Box::new(meow_transport::grpc::GrpcLayer::new(
            meow_transport::grpc::GrpcConfig {
                service_name: t
                    .service_name
                    .clone()
                    .unwrap_or_else(|| "GunService".into()),
                authority: host,
            },
        ))),
        TransportKind::HttpUpgrade => chain.push(Box::new(
            meow_transport::httpupgrade::HttpUpgradeLayer::new(
                meow_transport::httpupgrade::HttpUpgradeConfig {
                    path,
                    host_header: Some(host),
                    extra_headers: headers,
                },
            ),
        )),
        TransportKind::Xhttp => {
            let mode = effective_xhttp_mode(
                t.mode.as_deref(),
                cfg.tls
                    .as_ref()
                    .is_some_and(|tls| tls.reality_public_key.is_some()),
            );
            if matches!(mode, "stream-up" | "packet-up") {
                let layer = crate::xhttp::StreamUp {
                    path,
                    host,
                    scheme: if cfg.tls.is_some() { "https" } else { "http" },
                    headers,
                };
                if mode == "packet-up" {
                    chain.push(Box::new(crate::xhttp::PacketUp(layer)));
                } else {
                    chain.push(Box::new(layer));
                }
                return Ok(chain);
            }
            if mode != "stream-one" {
                bail!("XHTTP mode {mode} is not implemented yet; supported: stream-one, stream-up, packet-up");
            }
            chain.push(Box::new(meow_transport::xhttp::XhttpLayer::new(
                meow_transport::xhttp::XhttpConfig {
                    path,
                    hosts: vec![host],
                    scheme: if cfg.tls.is_some() { "https" } else { "http" }.into(),
                    extra_headers: headers,
                    ..Default::default()
                },
            )));
        }
    }
    Ok(chain)
}

#[cfg(test)]
#[path = "test/native_test.rs"]
mod tests;

#[cfg(test)]
#[path = "test/native_configuration_test.rs"]
mod configuration_tests;
