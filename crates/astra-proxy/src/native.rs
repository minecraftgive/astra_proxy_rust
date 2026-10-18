//! In-process Rust protocol codecs and composable transports. No child processes.
use crate::{config::OutboundConfig, outbound::Stream};
use anyhow::{bail, Context, Result};
use base64::Engine;
use meow_common::{Metadata, ProxyAdapter};
use meow_proxy::{dialer::DirectDialer, TransportChain};
use meow_transport::tls::{RealityConfig, TlsConfig, TlsLayer};
use serde::Deserialize;
use std::{collections::HashMap, path::Path, sync::Arc};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tuic,
    Vless,
    Vmess,
    Shadowsocks,
    Hysteria2,
    Trojan,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub protocol: Protocol,
    pub server: String,
    pub port: u16,
    pub uuid: Option<String>,
    pub password: Option<String>,
    pub cipher: Option<String>,
    pub encryption: Option<String>,
    pub flow: Option<String>,
    pub tls: Option<TlsOptions>,
    #[serde(default)]
    pub transport: TransportOptions,
    pub mux: Option<String>,
    pub obfs_password: Option<String>,
    pub pin_sha256: Option<String>,
    pub tuic_udp_mode: Option<String>,
    pub tuic_idle_timeout_secs: Option<u64>,
    pub congestion_control: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsOptions {
    pub server_name: String,
    pub ca_file: Option<String>,
    pub fingerprint: Option<String>,
    pub cert_pin_sha256: Option<String>,
    pub reality_public_key: Option<String>,
    pub reality_short_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransportOptions {
    pub kind: TransportKind,
    pub path: Option<String>,
    pub host: Option<String>,
    pub service_name: Option<String>,
    pub mode: Option<String>,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    #[default]
    Tcp,
    Ws,
    Grpc,
    HttpUpgrade,
    Xhttp,
}

pub enum NativeOutbound {
    Tuic(crate::tuic::Client),
    Adapter(Arc<dyn ProxyAdapter>),
    Trojan {
        server: String,
        port: u16,
        password_hash: String,
        transport: TransportChain,
    },
}
pub type Registry = HashMap<String, NativeOutbound>;

pub fn compile(configs: &[OutboundConfig], base: &Path) -> Result<Registry> {
    let mut registry = Registry::new();
    for config in configs {
        if let OutboundConfig::Socks5 {
            tag,
            server,
            username,
            password,
        } = config
        {
            let (host, port) = server
                .rsplit_once(':')
                .context("SOCKS5 server requires host:port")?;
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let port: u16 = port.parse().context("invalid SOCKS5 server port")?;
            if host.is_empty() || port == 0 {
                bail!("invalid SOCKS5 server");
            }
            registry.insert(
                tag.clone(),
                NativeOutbound::Adapter(Arc::new(
                    meow_proxy::Socks5Adapter::new(
                        tag,
                        host,
                        port,
                        username.clone().zip(password.clone()),
                        false,
                        false,
                        Arc::new(DirectDialer),
                    )
                    .with_udp(true),
                )),
            );
        }
        if let OutboundConfig::Native { tag, config } = config {
            validate_common(config).with_context(|| format!("outbound {tag}"))?;
            let outbound = if matches!(config.protocol, Protocol::Tuic) {
                NativeOutbound::Tuic(crate::tuic::Client::new(config, base)?)
            } else if matches!(config.protocol, Protocol::Trojan) {
                use sha2::{Digest, Sha224};
                if config.tls.is_none() {
                    bail!("Trojan requires TLS");
                }
                if config.mux.is_some() {
                    bail!("Trojan mux is not yet supported");
                }
                NativeOutbound::Trojan {
                    server: config.server.clone(),
                    port: config.port,
                    password_hash: hex::encode(Sha224::digest(
                        required(&config.password, "password")?.as_bytes(),
                    )),
                    transport: transport(config, base)?,
                }
            } else {
                NativeOutbound::Adapter(
                    build(tag, config, base).with_context(|| format!("outbound {tag}"))?,
                )
            };
            registry.insert(tag.clone(), outbound);
        }
    }
    Ok(registry)
}

pub async fn connect(outbound: &NativeOutbound, target: &astra_ipc::Target) -> Result<Stream> {
    match outbound {
        NativeOutbound::Tuic(client) => client.connect(target).await,
        NativeOutbound::Adapter(adapter) => {
            Ok(Box::new(adapter.dial_tcp(&metadata(target)).await?))
        }
        NativeOutbound::Trojan { .. } => Ok(Box::new(trojan_stream(outbound, target, 1).await?)),
    }
}

async fn trojan_stream(
    outbound: &NativeOutbound,
    target: &astra_ipc::Target,
    command: u8,
) -> Result<Box<dyn meow_transport::Stream>> {
    let NativeOutbound::Trojan {
        server,
        port,
        password_hash,
        transport,
    } = outbound
    else {
        bail!("not a Trojan outbound");
    };
    use tokio::io::AsyncWriteExt;
    let tcp = tokio::net::TcpStream::connect((server.as_str(), *port)).await?;
    tcp.set_nodelay(true)?;
    let mut stream = transport.connect(Box::new(tcp)).await?;
    let mut header = password_hash.as_bytes().to_vec();
    header.extend_from_slice(b"\r\n");
    header.push(command);
    crate::outbound::append_socks_address(&mut header, target)?;
    header.extend_from_slice(b"\r\n");
    stream.write_all(&header).await?;
    stream.flush().await?;
    Ok(stream)
}

pub async fn connect_udp(
    outbound: &NativeOutbound,
    target: &astra_ipc::Target,
) -> Result<Box<dyn meow_common::ProxyPacketConn>> {
    match outbound {
        NativeOutbound::Tuic(client) => client.connect_udp(target).await,
        NativeOutbound::Adapter(adapter) => {
            if !adapter.support_udp() {
                bail!("outbound does not support UDP");
            }
            let mut metadata = metadata(target);
            metadata.network = meow_common::Network::Udp;
            Ok(adapter.dial_udp(&metadata).await?)
        }
        NativeOutbound::Trojan { .. } => {
            let stream = trojan_stream(outbound, target, 3).await?;
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

pub fn metadata(target: &astra_ipc::Target) -> Metadata {
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

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
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

fn build(tag: &str, cfg: &NodeConfig, base: &Path) -> Result<Arc<dyn ProxyAdapter>> {
    let dialer = Arc::new(DirectDialer);
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
        Protocol::Vless => {
            let id = uuid::Uuid::parse_str(required(&cfg.uuid, "uuid")?)?;
            let flow = match cfg.flow.as_deref() {
                None | Some("") => None,
                Some("xtls-rprx-vision") => {
                    if cfg.tls.is_none() {
                        bail!("Vision requires TLS/REALITY");
                    }
                    if cfg.transport.kind != TransportKind::Tcp
                        && !cfg.encryption.as_deref().is_some_and(|v| {
                            v.starts_with("mlkem768x25519plus.native.")
                                || v.starts_with("mlkem768x25519plus.xorpub.")
                        })
                    {
                        bail!(
                            "Vision over an HTTP transport requires native/xorpub VLESS Encryption"
                        );
                    }
                    if mux.is_some() {
                        bail!("Vision with mux is not supported in this release");
                    }
                    Some(meow_proxy::VlessFlow::XtlsRprxVision)
                }
                Some(_) => bail!("unsupported VLESS flow"),
            };
            let mut adapter = meow_proxy::VlessAdapter::new(
                tag,
                &cfg.server,
                cfg.port,
                *id.as_bytes(),
                flow,
                true,
                transport(cfg, base)?,
                dialer,
            );
            let encryption =
                meow_proxy::parse_client_encryption(cfg.encryption.as_deref().unwrap_or("none"))
                    .map_err(anyhow::Error::msg)?;
            adapter.set_encryption(encryption.map(Arc::new));
            if let Some(mux) = mux {
                adapter = adapter.with_mux(mux);
            }
            Ok(Arc::new(adapter))
        }
        Protocol::Vmess => {
            let id = uuid::Uuid::parse_str(required(&cfg.uuid, "uuid")?)?;
            let security = match cfg.cipher.as_deref().unwrap_or("auto") {
                "auto" => meow_proxy::vmess::header::auto_security(),
                "aes-128-gcm" => meow_proxy::vmess::Security::Aes128Gcm,
                "chacha20-poly1305" => meow_proxy::vmess::Security::ChaCha20Poly1305,
                _ => bail!("unsupported VMess cipher"),
            };
            let mut adapter = meow_proxy::VmessAdapter::new(
                tag,
                &cfg.server,
                cfg.port,
                *id.as_bytes(),
                security,
                true,
                transport(cfg, base)?,
                dialer,
            );
            if let Some(mux) = mux {
                adapter = adapter.with_mux(mux);
            }
            Ok(Arc::new(adapter))
        }
        Protocol::Shadowsocks => {
            if cfg.tls.is_some() || cfg.transport.kind != TransportKind::Tcp {
                bail!("Shadowsocks transport options are not supported yet");
            }
            if mux.is_some() {
                bail!("Shadowsocks mux is not supported yet");
            }
            Ok(Arc::new(meow_proxy::ShadowsocksAdapter::new(
                tag,
                &cfg.server,
                cfg.port,
                required(&cfg.password, "password")?,
                required(&cfg.cipher, "cipher")?,
                true,
                None,
                None,
                None,
                dialer,
            )?))
        }
        Protocol::Hysteria2 => {
            if cfg.transport.kind != TransportKind::Tcp || mux.is_some() {
                bail!("Hysteria2 uses its own QUIC transport");
            }
            if cfg.tls.as_ref().is_some_and(|t| {
                t.ca_file.is_some() || t.reality_public_key.is_some() || t.cert_pin_sha256.is_some()
            }) {
                bail!("Hysteria2 uses system roots or pin_sha256, not ca_file/REALITY");
            }
            let options = meow_proxy::Hy2Options {
                name: tag.into(),
                server: cfg.server.clone(),
                port: cfg.port,
                password: required(&cfg.password, "password")?.into(),
                sni: cfg.tls.as_ref().map(|t| t.server_name.clone()),
                skip_cert_verify: false,
                udp: true,
                up_bps: 0,
                down_bps: 0,
                obfs: cfg
                    .obfs_password
                    .as_ref()
                    .map(|_| meow_proxy::Hy2Obfs::Salamander),
                obfs_password: cfg.obfs_password.clone(),
                ports: None,
                hop_interval: None,
                fingerprint: cfg.pin_sha256.clone(),
                fast_open: false,
            };
            Ok(Arc::new(
                meow_proxy::Hy2Adapter::new(options).map_err(anyhow::Error::msg)?,
            ))
        }
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

fn transport(cfg: &NodeConfig, base: &Path) -> Result<TransportChain> {
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
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, UdpSocket},
        time::{timeout, Duration},
    };

    #[tokio::test]
    async fn closing_socks_udp_closes_upstream_control_connection() {
        timeout(Duration::from_secs(3), async {
            let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = tcp.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut control, _) = tcp.accept().await.unwrap();
                assert_eq!(control.read_u8().await.unwrap(), 5);
                let count = control.read_u8().await.unwrap();
                let mut methods = vec![0; count as usize];
                control.read_exact(&mut methods).await.unwrap();
                assert!(methods.contains(&0));
                control.write_all(&[5, 0]).await.unwrap();
                let mut request = [0; 10];
                control.read_exact(&mut request).await.unwrap();
                assert_eq!(&request[..4], &[5, 3, 0, 1]);
                let mut response = vec![5, 0, 0, 1, 127, 0, 0, 1];
                response.extend_from_slice(&relay.local_addr().unwrap().port().to_be_bytes());
                control.write_all(&response).await.unwrap();
                let mut byte = [0];
                assert_eq!(
                    control.read(&mut byte).await.unwrap(),
                    0,
                    "control socket survived UDP close"
                );
            });
            let configs = [OutboundConfig::Socks5 {
                tag: "socks".into(),
                server: address.to_string(),
                username: None,
                password: None,
            }];
            let registry = compile(&configs, Path::new(".")).unwrap();
            let packet = connect_udp(
                &registry["socks"],
                &astra_ipc::Target {
                    host: "example.test".into(),
                    port: 53,
                },
            )
            .await
            .unwrap();
            packet.close().unwrap();
            // Keep the packet object alive: explicit close must release control immediately.
            peer.await.unwrap();
            drop(packet);
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn auto_selects_peer_compatible_xhttp_mode() {
        assert_eq!(effective_xhttp_mode(Some("auto"), false), "packet-up");
        assert_eq!(effective_xhttp_mode(Some("auto"), true), "stream-one");
        assert_eq!(effective_xhttp_mode(None, false), "stream-one");
        assert_eq!(effective_xhttp_mode(Some("stream-up"), false), "stream-up");
    }

    #[test]
    fn pin_and_encryption_validation_fail_early() {
        let source = r#"
rule_socket = "/tmp/astra-test-config.sock"
socks5_listen = "127.0.0.1:1080"
[[outbounds]]
kind = "native"
tag = "test"
[outbounds.config]
protocol = "vless"
server = "localhost"
port = 443
uuid = "00000000-0000-0000-0000-000000000001"
flow = "xtls-rprx-vision"
encryption = "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA"
[outbounds.config.tls]
server_name = "localhost"
cert_pin_sha256 = "1111111111111111111111111111111111111111111111111111111111111111"
[outbounds.config.transport]
kind = "xhttp"
mode = "auto"
"#;
        let config: crate::config::ProxyConfig = toml::from_str(source).unwrap();
        compile(&config.outbounds, Path::new(".")).unwrap();
        for (before, after) in [
            (
                "1111111111111111111111111111111111111111111111111111111111111111",
                "ab",
            ),
            (
                "mlkem768x25519plus.native.0rtt.AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA",
                "bad-encryption",
            ),
        ] {
            let config: crate::config::ProxyConfig =
                toml::from_str(&source.replace(before, after)).unwrap();
            assert!(compile(&config.outbounds, Path::new(".")).is_err());
        }
    }
}
