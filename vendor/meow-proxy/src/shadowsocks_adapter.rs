use crate::dialer::UdpTarget;
#[cfg(feature = "ech-tls-tunnel")]
use crate::ech_tls_tunnel::{self, EchTlsTunnelConfig};
#[cfg(feature = "ss-plugins")]
use crate::gost_plugin;
#[cfg(feature = "ss-plugins")]
use crate::jls_plugin;
#[cfg(feature = "kcptun")]
use crate::kcptun_plugin;
#[cfg(feature = "ss-plugins")]
use crate::restls_plugin;
#[cfg(feature = "ss-plugins")]
use crate::shadow_tls_plugin;
#[cfg(feature = "ss-plugins")]
use crate::v2ray_plugin::{self, V2rayPluginConfig};
use async_trait::async_trait;
use meow_common::atomic::{checked_increment, AtomicU};
use meow_common::{
    AdapterType, MeowError, Metadata, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn, Result,
};
#[cfg(feature = "ss-plugins")]
use meow_transport::simple_obfs::client::{HttpObfs, TlsObfs};
#[cfg(feature = "ss-plugins")]
use meow_transport::tls::TlsLayer;
#[cfg(feature = "ss-plugins")]
use shadowsocks::config::Mode;
use shadowsocks::config::{ServerAddr, ServerConfig, ServerType};
use shadowsocks::context::{Context, SharedContext};
use shadowsocks::crypto::CipherKind;
#[cfg(feature = "ss-plugins")]
use shadowsocks::plugin::{Plugin, PluginConfig, PluginMode};
use shadowsocks::relay::udprelay::crypto_io::{decrypt_server_payload, encrypt_client_payload};
use shadowsocks::relay::udprelay::options::UdpSocketControlData;
use shadowsocks::relay::udprelay::proxy_socket::UdpSocketType;
use shadowsocks::relay::udprelay::{DatagramReceive, DatagramSend, DatagramSocket, ProxySocket};
use shadowsocks::relay::Address;
use shadowsocks::ProxyClientStream;
use smol_str::SmolStr;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;
use tracing::debug;
#[cfg(feature = "ss-plugins")]
use tracing::warn;

/// Built-in (native, no external process) simple-obfs configuration.
#[cfg(feature = "ss-plugins")]
#[derive(Debug, Clone)]
pub enum BuiltinObfs {
    /// HTTP simple-obfs with the configured fake `Host` header.
    Http { host: String },
    /// TLS simple-obfs with the configured fake SNI server name.
    Tls { server: String },
}

/// Which plugin (if any) the adapter uses when dialing outbound.
///
/// Layered on top of the SS cipher stream:
/// * `None` — direct TCP to `server:port`.
/// * `External` — SIP003 subprocess (e.g. `obfs-local` via shadowsocks-rust's
///   `Plugin::start`); `server_config` is rewritten to point at the local
///   listener the subprocess exposes.
/// * `Obfs` — native simple-obfs codec wraps the TCP stream before SS encryption.
/// * `V2ray` — native v2ray-plugin websocket (+ optional TLS) transport wraps
///   the TCP stream before SS encryption.
/// * `Gost` — native gost-plugin websocket (+ optional TLS and single-stream
///   smux) transport wraps the TCP stream before SS encryption.
/// * `ShadowTls` — native shadow-tls v1/v2/v3 (cover-TLS record transport).
/// * `EchTlsTunnel` — `ech-tls-tunnel` plugin (TLS-in-TLS with ECH).
/// * `Restls` — native restls (record-level TLS handshake + tagged records).
/// * `Jls` — native jls (record-level TLS 1.3 with random-field auth).
/// * `Kcptun` — native kcptun (KCP-over-UDP + smux pool; UDP via UoT).
#[allow(
    clippy::large_enum_variant,
    reason = "external-plugin boxing would add an indirection to the overwhelmingly-common no-plugin arm; the variant spread is bounded by in-tree plugins"
)]
enum PluginKind {
    None,
    /// External SIP003 plugin subprocess. The `Plugin` handle keeps the
    /// subprocess alive for the adapter's lifetime.
    #[cfg(feature = "ss-plugins")]
    External(#[allow(dead_code)] Plugin),
    #[cfg(feature = "ss-plugins")]
    Obfs(BuiltinObfs),
    #[cfg(feature = "ss-plugins")]
    V2ray(V2rayPluginConfig, Option<TlsLayer>),
    /// Native gost-plugin websocket (+ optional TLS and smux) transport.
    /// `WsLayer` is built at construction so a malformed headers/path
    /// config fails once at startup, not per dial. The TLS layer is a
    /// `ReloadableTlsLayer` — file-sourced mTLS cert/key PEMs are re-stat
    /// per dial and hot-reloaded on change (issue #621).
    #[cfg(feature = "ss-plugins")]
    Gost(
        gost_plugin::GostPluginConfig,
        Option<gost_plugin::ReloadableTlsLayer>,
        meow_transport::ws::WsLayer,
    ),
    /// Native shadow-tls transport (v1/v2/v3). `TlsLayer` is built at
    /// construction so a malformed TLS config fails once at startup.
    #[cfg(feature = "ss-plugins")]
    ShadowTls(shadow_tls_plugin::ShadowTlsConfig, TlsLayer),
    /// Native restls transport — the record-level TLS client needs no
    /// `TlsLayer`; `RestlsPluginConfig` is validated at construction.
    #[cfg(feature = "ss-plugins")]
    Restls(restls_plugin::RestlsPluginConfig),
    /// Native jls transport — record-level TLS 1.3 with hello-random
    /// authentication; `JlsConfig` is validated at construction.
    #[cfg(feature = "ss-plugins")]
    Jls(jls_plugin::JlsPluginConfig),
    /// Native kcptun transport — KCP-over-UDP + smux session pool.
    /// `Arc` because the pooled client outlives a single dial.
    #[cfg(feature = "kcptun")]
    Kcptun(Arc<kcptun_plugin::KcptunClient>),
    #[cfg(feature = "ech-tls-tunnel")]
    EchTlsTunnel(EchTlsTunnelConfig, TlsLayer),
}

impl PluginKind {
    /// Why this plugin refuses UDP relay, if it does — one place so the
    /// `dial_udp` gate can't drift as variants accrete.
    fn udp_block_reason(&self) -> Option<&'static str> {
        match self {
            #[cfg(feature = "ss-plugins")]
            PluginKind::V2ray(..) => Some("v2ray-plugin does not support UDP relay"),
            #[cfg(feature = "ss-plugins")]
            PluginKind::Gost(..) => Some("gost-plugin does not support UDP relay"),
            #[cfg(feature = "ss-plugins")]
            PluginKind::ShadowTls(..) => Some("shadow-tls does not support UDP relay"),
            #[cfg(feature = "ss-plugins")]
            PluginKind::Restls(..) => Some("restls does not support UDP relay"),
            #[cfg(feature = "ss-plugins")]
            PluginKind::Jls(..) => Some("jls does not support UDP relay"),
            #[cfg(feature = "ech-tls-tunnel")]
            PluginKind::EchTlsTunnel(..) => Some("ech-tls-tunnel does not support UDP relay"),
            _ => None,
        }
    }
}

/// Dial-relevant SS state, shared (Arc) between the adapter and any mux
/// session so the SIP003 plugin handle stays alive for both.
struct SsCore {
    #[cfg(feature = "ss-plugins")]
    server: SmolStr,
    #[cfg(feature = "ss-plugins")]
    port: u16,
    server_config: ServerConfig,
    context: shadowsocks::context::SharedContext,
    plugin: PluginKind,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
}

pub struct ShadowsocksAdapter {
    name: SmolStr,
    core: Arc<SsCore>,
    addr_str: SmolStr,
    support_udp: bool,
    health: ProxyHealth,
    /// sing-mux compatible connection multiplexing (optional).
    #[cfg(feature = "mux")]
    mux: Option<Arc<crate::mux::MuxClient>>,
}

impl ShadowsocksAdapter {
    #[allow(
        clippy::too_many_arguments,
        reason = "SS node params are flat config fields; a builder adds indirection without fewer call-site args"
    )]
    pub fn new(
        name: &str,
        server: &str,
        port: u16,
        password: &str,
        cipher: &str,
        udp: bool,
        plugin_name: Option<&str>,
        plugin_opts: Option<&str>,
        // Node-level `client-fingerprint` (uTLS profile) — consumed by the
        // `shadow-tls` plugin's cover handshake; ignored by other plugins.
        client_fingerprint: Option<&str>,
        dialer: Arc<dyn crate::dialer::TcpDialer>,
    ) -> Result<Self> {
        #[cfg(not(feature = "ss-plugins"))]
        let _ = (plugin_opts, client_fingerprint);
        let cipher_kind = cipher
            .parse::<CipherKind>()
            .map_err(|_| MeowError::Config(format!("unknown cipher: {cipher}")))?;
        #[allow(unused_mut)] // Plugin builds replace the remote endpoint with the plugin endpoint.
        let mut server_config = ServerConfig::new((server, port), password, cipher_kind)
            .map_err(|e| MeowError::Config(format!("invalid ss config: {e}")))?;
        let context = Context::new_shared(ServerType::Local);
        let addr_str = SmolStr::from(format!("{server}:{port}"));

        let plugin = match plugin_name {
            #[cfg(feature = "ss-plugins")]
            Some(p) if is_builtin_obfs_plugin(p) => {
                let cfg = parse_obfs_opts(plugin_opts, server)?;
                debug!("SS '{}' using built-in simple-obfs ({:?})", name, cfg);
                PluginKind::Obfs(cfg)
            }
            #[cfg(feature = "ss-plugins")]
            Some("v2ray-plugin") => {
                let mut cfg = v2ray_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                if cfg.host.is_empty() {
                    cfg.host = server.to_string();
                }
                debug!(
                    "SS '{}' using built-in v2ray-plugin: tls={} host={} path={} mux={}",
                    name, cfg.tls, cfg.host, cfg.path, cfg.mux
                );
                let tls = v2ray_plugin::build_tls_layer(&cfg)?;
                PluginKind::V2ray(cfg, tls)
            }
            #[cfg(feature = "ss-plugins")]
            Some("gost-plugin") => {
                let cfg = gost_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in gost-plugin: tls={} host={} path={} mux={}",
                    name, cfg.tls, cfg.host, cfg.path, cfg.mux
                );
                let tls = gost_plugin::build_tls_layer(&cfg)?;
                let ws = gost_plugin::build_ws_layer(&cfg)?;
                PluginKind::Gost(cfg, tls, ws)
            }
            #[cfg(feature = "ss-plugins")]
            Some("shadow-tls") => {
                let cfg = shadow_tls_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in shadow-tls: host={} version={} alpn={:?}",
                    name, cfg.host, cfg.version, cfg.alpn
                );
                let tls = shadow_tls_plugin::build_tls_layer(&cfg, client_fingerprint)?;
                PluginKind::ShadowTls(cfg, tls)
            }
            #[cfg(feature = "ss-plugins")]
            Some("restls") => {
                let cfg = restls_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in restls: host={} version-hint={}",
                    name, cfg.host, cfg.version_hint
                );
                // `client-fingerprint` selects a uTLS profile upstream; our
                // record-level client crafts a fixed-shape ClientHello.
                if client_fingerprint.is_some() {
                    warn!("SS '{name}': client-fingerprint has no effect on restls");
                }
                PluginKind::Restls(cfg)
            }
            #[cfg(feature = "ss-plugins")]
            Some("jls") => {
                let cfg = jls_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in jls: host={} username={}",
                    name, cfg.host, cfg.username
                );
                // `client-fingerprint` selects a uTLS profile upstream; our
                // record-level client crafts a fixed-shape ClientHello.
                if client_fingerprint.is_some() {
                    warn!("SS '{name}': client-fingerprint has no effect on jls");
                }
                PluginKind::Jls(cfg)
            }
            #[cfg(feature = "kcptun")]
            Some("kcptun") => {
                let cfg = kcptun_plugin::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in kcptun: crypt={} mode={} conn={} nocomp={}",
                    name, cfg.crypt, cfg.mode, cfg.conn, cfg.no_comp
                );
                PluginKind::Kcptun(Arc::new(kcptun_plugin::KcptunClient::new(
                    cfg,
                    server,
                    port,
                    Arc::clone(&dialer),
                )))
            }
            #[cfg(feature = "ech-tls-tunnel")]
            Some("ech-tls-tunnel") => {
                let cfg = ech_tls_tunnel::parse_opts(plugin_opts.unwrap_or(""))?;
                debug!(
                    "SS '{}' using built-in ech-tls-tunnel: sni={} path={} ech_config_len={}",
                    name,
                    cfg.sni,
                    cfg.path,
                    cfg.ech_config.len()
                );
                let tls = ech_tls_tunnel::build_tls_layer(&cfg)?;
                PluginKind::EchTlsTunnel(cfg, tls)
            }
            #[cfg(feature = "ss-plugins")]
            Some(pname) => {
                // SIP003u: a `mode=tcp_and_udp` / `udp_only` token in plugin-opts
                // selects whether UDP relay is tunneled through the plugin. The
                // token is consumed here (not forwarded to the plugin process)
                // and translated to the shadowsocks plugin transport mode.
                // Default stays TcpOnly, so UDP keeps going direct to the server
                // for plugins that don't speak SIP003u (unchanged behaviour).
                let (plugin_mode, filtered_opts) = extract_sip003_plugin_mode(plugin_opts);
                let plugin_config = PluginConfig {
                    plugin: pname.to_string(),
                    plugin_opts: filtered_opts,
                    plugin_args: vec![],
                    plugin_mode,
                };
                let started =
                    Plugin::start(&plugin_config, server_config.addr(), PluginMode::Client)
                        .map_err(|e| {
                            MeowError::Config(format!("failed to start ss plugin '{pname}': {e}"))
                        })?;
                server_config.set_plugin_addr(ServerAddr::SocketAddr(started.local_addr()));
                server_config.set_plugin(plugin_config);
                debug!("SS plugin '{}' started on {}", pname, started.local_addr());
                PluginKind::External(started)
            }
            #[cfg(not(feature = "ss-plugins"))]
            Some(_) => {
                return Err(MeowError::Config(
                    "Shadowsocks plugins were not enabled in this build".into(),
                ));
            }
            None => PluginKind::None,
        };

        let core = Arc::new(SsCore {
            #[cfg(feature = "ss-plugins")]
            server: SmolStr::from(server),
            #[cfg(feature = "ss-plugins")]
            port,
            server_config,
            context,
            plugin,
            dialer,
        });

        Ok(Self {
            name: SmolStr::from(name),
            core,
            addr_str,
            support_udp: udp,
            health: ProxyHealth::new(),
            #[cfg(feature = "mux")]
            mux: None,
        })
    }

    /// Enable sing-mux compatible connection multiplexing.  The session's
    /// SS stream targets the reserved mux destination; the server must be a
    /// sing-box / mihomo SS inbound with multiplex enabled (a plain
    /// ss-server does not speak sing-mux).  Xray Mux.Cool is VLESS-only.
    #[cfg(feature = "mux")]
    pub fn with_mux(mut self, options: crate::mux::MuxOptions) -> Self {
        use crate::mux::{MuxClient, MUX_DESTINATION_FQDN, MUX_DESTINATION_PORT};
        use std::sync::Arc as StdArc;

        let core = Arc::clone(&self.core);
        let dial: crate::mux::DialFn = StdArc::new(move || {
            let core = Arc::clone(&core);
            Box::pin(async move {
                let addr = Address::DomainNameAddress(
                    MUX_DESTINATION_FQDN.to_string(),
                    MUX_DESTINATION_PORT,
                );
                core.dial_tcp_stream(addr, false).await
            })
        });
        self.mux = Some(MuxClient::new(dial, options));
        self
    }

    /// Whether an external SIP003 plugin owns the UDP leg: `udp_external_addr`
    /// is then the plugin's local listener, whose outbound belongs to the
    /// subprocess — chaining it through `dialer-proxy` would make the *front*
    /// dial `127.0.0.1:<plugin-port>` on its own loopback while our plugin
    /// still listens locally. Same contract as the TCP path, which dials the
    /// plugin listener directly under `dialer-proxy`.
    fn plugin_owns_udp(&self) -> bool {
        #[cfg(feature = "ss-plugins")]
        {
            matches!(self.core.plugin, PluginKind::External(_))
                && self
                    .core
                    .server_config
                    .plugin()
                    .is_some_and(|p| p.plugin_mode.enable_udp())
        }
        #[cfg(not(feature = "ss-plugins"))]
        false
    }

    /// Whether the plain SS UDP association must ride the `dialer-proxy`
    /// chain — false on a direct dialer, and false when an external plugin
    /// owns the UDP leg (its endpoint is the local plugin listener, which
    /// is never chain-routed).
    fn udp_via_chain(&self) -> bool {
        self.core.dialer.is_proxy() && !self.plugin_owns_udp()
    }

    /// Dial the SS server's UDP endpoint through the `dialer-proxy` front.
    ///
    /// A [`UdpTarget::Name`] asks the front to resolve the name itself —
    /// matching the view it used for the TCP leg (issue #657). A front
    /// that cannot carry a domain target answers `ErrorKind::Unsupported`
    /// and we fall back to a locally-resolved literal per candidate — the
    /// pre-#657 behavior, still fail-closed through the same chain.
    ///
    /// Returns the conn plus the *effective* target (the literal a
    /// fallback dial bound to), which the conn uses for its source filter.
    async fn dial_udp_via_front(
        &self,
        target: UdpTarget,
        internal: bool,
    ) -> Result<(Arc<dyn ProxyPacketConn>, UdpTarget)> {
        match self
            .core
            .dialer
            .dial_udp_conn(target.clone(), internal)
            .await
        {
            Ok(conn) => Ok((conn, target)),
            // The dialer maps front `NotSupported`/`UdpNotSupported` to
            // `ErrorKind::Unsupported` — a capability refusal, not a
            // transport failure: `NotSupported` stays exempt from
            // dead-marking (a UDP-less front must not cost the node its
            // health), and every candidate would fail the same way.
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => match &target {
                UdpTarget::Addr(_) => Err(MeowError::NotSupported(format!(
                    "ss udp via dialer-proxy {target}: {e}"
                ))),
                UdpTarget::Name { host, port } => {
                    let candidates =
                        meow_common::resolve_host_all(host, *port)
                            .await
                            .map_err(|e| {
                                MeowError::io_with(&format!("ss udp lookup {host}:{port}"), e)
                            })?;
                    let mut last_err = None;
                    for remote in candidates {
                        match self
                            .core
                            .dialer
                            .dial_udp_conn(UdpTarget::Addr(remote), internal)
                            .await
                        {
                            Ok(conn) => return Ok((conn, UdpTarget::Addr(remote))),
                            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                                return Err(MeowError::NotSupported(format!(
                                    "ss udp via dialer-proxy {remote}: {e}"
                                )));
                            }
                            Err(e) => {
                                last_err = MeowError::prefer_errno(
                                    last_err,
                                    MeowError::io_with(
                                        &format!("ss udp via dialer-proxy {remote}"),
                                        e,
                                    ),
                                );
                            }
                        }
                    }
                    Err(last_err.unwrap_or_else(|| {
                        MeowError::Proxy("ss udp via dialer-proxy: no candidates".into())
                    }))
                }
            },
            Err(e) => Err(MeowError::io_with(
                &format!("ss udp via dialer-proxy {target}"),
                e,
            )),
        }
    }
}

impl SsCore {
    /// Layer the SS crypto codec for `addr` over `transport`, an established
    /// (possibly plugin-wrapped) stream to this server. Every TCP dial goes
    /// through here, so every SS connection gets [`SsConn`]'s
    /// server-first handling of the deferred request header.
    fn ss_conn<T>(&self, transport: T, addr: Address) -> Box<dyn ProxyConn>
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        Box::new(SsConn::new(ProxyClientStream::from_stream(
            Arc::clone(&self.context),
            transport,
            &self.server_config,
            addr,
        )))
    }

    /// Dial a raw (or plugin-transported) TCP stream to the SS server and
    /// wrap it in the SS crypto codec for the given target address.
    ///
    /// `internal` is the caller's [`Metadata::is_internal`] marker — mux
    /// session dials pass `false` because a shared mux conn exists to serve
    /// user streams regardless of which dial triggered its establishment.
    async fn dial_tcp_stream(&self, addr: Address, internal: bool) -> Result<Box<dyn ProxyConn>> {
        match &self.plugin {
            #[cfg(feature = "ss-plugins")]
            PluginKind::Obfs(obfs) => {
                // Open a raw TCP connection to the SS server, wrap it in the
                // simple-obfs codec, then layer the SS crypto stream on top.
                let tcp = self
                    .dialer
                    .dial(&self.server, self.port, internal)
                    .await
                    .map_err(|e| MeowError::io_with("ss obfs tcp connect", e))?;

                match obfs.clone() {
                    BuiltinObfs::Http { host } => {
                        let wrapped = HttpObfs::new(tcp, host, self.port)
                            .map_err(|e| MeowError::Config(format!("ss obfs: {e}")))?;
                        Ok(self.ss_conn(wrapped, addr))
                    }
                    BuiltinObfs::Tls { server } => {
                        let wrapped = TlsObfs::new(tcp, server)
                            .map_err(|e| MeowError::Config(format!("ss obfs: {e}")))?;
                        Ok(self.ss_conn(wrapped, addr))
                    }
                }
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::V2ray(cfg, tls) => {
                let transport = v2ray_plugin::dial(
                    cfg,
                    tls.as_ref(),
                    &self.server,
                    self.port,
                    &*self.dialer,
                    internal,
                )
                .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Gost(cfg, tls, ws) => {
                let transport = gost_plugin::dial(
                    cfg,
                    tls.as_ref(),
                    ws,
                    &self.server,
                    self.port,
                    &*self.dialer,
                    internal,
                )
                .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::ShadowTls(cfg, tls) => {
                let transport = shadow_tls_plugin::dial(
                    cfg,
                    tls,
                    &self.server,
                    self.port,
                    &*self.dialer,
                    internal,
                )
                .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Restls(cfg) => {
                let transport =
                    restls_plugin::dial(cfg, &self.server, self.port, &*self.dialer, internal)
                        .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Jls(cfg) => {
                let transport =
                    jls_plugin::dial(cfg, &self.server, self.port, &*self.dialer, internal).await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "kcptun")]
            PluginKind::Kcptun(client) => {
                // A pooled smux stream over the KCP/UDP transport — the SS
                // crypto layer sits on top exactly like a TCP dial.
                let transport = client.open_stream().await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ech-tls-tunnel")]
            PluginKind::EchTlsTunnel(cfg, tls) => {
                let transport = ech_tls_tunnel::dial(
                    cfg,
                    tls,
                    &self.server,
                    self.port,
                    &*self.dialer,
                    internal,
                )
                .await?;
                Ok(self.ss_conn(transport, addr))
            }
            PluginKind::None => {
                // Dial the remote SS server through the pluggable dialer
                // (direct or via ``dialer-proxy``).  ``connect_tcp_host`` is
                // resolver-aware and SocketProtector-aware (Android
                // ``VpnService.protect(fd)``), and ``DirectDialer`` preserves
                // both of those properties.
                let tcp = match self.server_config.tcp_external_addr() {
                    ServerAddr::SocketAddr(sa) => self.dialer.dial_addr(*sa, internal).await,
                    ServerAddr::DomainName(host, port) => {
                        self.dialer.dial(host, *port, internal).await
                    }
                }
                .map_err(|e| MeowError::io_with("ss tcp connect", e))?;
                Ok(self.ss_conn(tcp, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::External(_) => {
                // SIP003 plugin subprocess: ``tcp_external_addr`` returns the
                // plugin's local listener (typically 127.0.0.1:<port>).
                // Always dial directly — the plugin is a local process and
                // must NOT be tunnelled through ``dialer-proxy``.
                let tcp = match self.server_config.tcp_external_addr() {
                    ServerAddr::SocketAddr(sa) => meow_common::connect_tcp(*sa).await,
                    ServerAddr::DomainName(host, port) => {
                        meow_common::connect_tcp_host(host, *port).await
                    }
                }
                .map_err(|e| MeowError::io_with("ss plugin tcp connect", e))?;
                Ok(self.ss_conn(tcp, addr))
            }
        }
    }

    /// Run the SS crypto handshake over `stream`, which must already
    /// terminate at this SS server — relay groups pass a chained stream in
    /// through `connect_over` instead of a fresh `dialer.dial`.
    async fn tcp_stream_over(
        &self,
        stream: Box<dyn meow_transport::Stream>,
        addr: Address,
    ) -> Result<Box<dyn ProxyConn>> {
        match &self.plugin {
            PluginKind::None => Ok(self.ss_conn(stream, addr)),
            #[cfg(feature = "ss-plugins")]
            PluginKind::Obfs(obfs) => {
                let stream = match obfs.clone() {
                    BuiltinObfs::Http { host } => Box::new(
                        HttpObfs::new(stream, host, self.port)
                            .map_err(|e| MeowError::Config(format!("ss obfs: {e}")))?,
                    )
                        as Box<dyn meow_transport::Stream>,
                    BuiltinObfs::Tls { server } => Box::new(
                        TlsObfs::new(stream, server)
                            .map_err(|e| MeowError::Config(format!("ss obfs: {e}")))?,
                    )
                        as Box<dyn meow_transport::Stream>,
                };
                Ok(self.ss_conn(stream, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::V2ray(cfg, tls) => {
                let transport = v2ray_plugin::handshake_over(
                    cfg,
                    tls.as_ref(),
                    &self.server,
                    self.port,
                    stream,
                )
                .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ech-tls-tunnel")]
            PluginKind::EchTlsTunnel(cfg, tls) => {
                let transport =
                    ech_tls_tunnel::handshake_over(cfg, tls, &self.server, self.port, stream)
                        .await?;
                Ok(self.ss_conn(transport, addr))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Gost(..) => {
                // gost (ws+tls+smux) could terminate on a relay-supplied
                // stream once it grows a `handshake_over` split — its `dial`
                // currently owns the TCP dial itself. Until then, fail
                // loudly rather than send unwrapped traffic.
                Err(MeowError::NotSupported(
                    "ss: gost-plugin transport does not yet support \
                     terminating on a relay-supplied stream"
                        .into(),
                ))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::ShadowTls(..) => {
                // shadow-tls could terminate on a relay-supplied stream once
                // it grows a `handshake_over` split — its `dial` currently
                // owns the TCP dial itself. Until then, fail loudly rather
                // than send unwrapped traffic.
                Err(MeowError::NotSupported(
                    "ss: shadow-tls transport does not yet support \
                     terminating on a relay-supplied stream"
                        .into(),
                ))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Restls(..) => {
                // restls could terminate on a relay-supplied stream once it
                // grows a `handshake_over` split — its `dial` currently owns
                // the TCP dial itself. Until then, fail loudly rather than
                // send unwrapped traffic.
                Err(MeowError::NotSupported(
                    "ss: restls transport does not yet support terminating \
                     on a relay-supplied stream"
                        .into(),
                ))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::Jls(..) => {
                // jls could terminate on a relay-supplied stream once it
                // grows a `handshake_over` split — its `dial` currently owns
                // the TCP dial itself. Until then, fail loudly rather than
                // send unwrapped traffic.
                Err(MeowError::NotSupported(
                    "ss: jls transport does not yet support terminating on \
                     a relay-supplied stream"
                        .into(),
                ))
            }
            #[cfg(feature = "kcptun")]
            PluginKind::Kcptun(_) => {
                // KCP is UDP-only — it can never ride a relay TCP stream.
                Err(MeowError::NotSupported(
                    "ss: kcptun is a UDP transport; it cannot terminate on \
                     a relay-supplied stream"
                        .into(),
                ))
            }
            #[cfg(feature = "ss-plugins")]
            PluginKind::External(_) => {
                // A SIP003 subprocess owns its outbound leg (it dials the
                // real server itself and we only reach its local listener).
                // The relay-supplied stream already terminates at the real
                // SS server, so plugin obfuscation cannot be applied to it —
                // fail loudly rather than send un-obfuscated traffic.
                Err(MeowError::NotSupported(
                    "ss: external SIP003 plugin owns its outbound leg; \
                     it cannot terminate on a relay-supplied stream"
                        .into(),
                ))
            }
        }
    }
}

/// SIP003u: extract the plugin transport mode from SIP003 `plugin-opts`.
///
/// A `mode=tcp_and_udp` / `mode=udp_only` / `mode=tcp_only` token selects
/// whether UDP relay is tunneled through the SIP003 plugin. Any *other* `mode=`
/// value (e.g. simple-obfs `mode=tls`, v2ray-plugin `mode=quic`) is a
/// plugin-specific option and is left in place. Returns the resolved
/// [`Mode`] (default [`Mode::TcpOnly`], preserving the pre-SIP003u behaviour
/// where UDP bypasses the plugin) and the opts string with a recognised
/// transport-mode token removed so it is not forwarded to the plugin process.
#[cfg(feature = "ss-plugins")]
fn extract_sip003_plugin_mode(opts: Option<&str>) -> (Mode, Option<String>) {
    let Some(opts) = opts else {
        return (Mode::TcpOnly, None);
    };
    let mut mode = Mode::TcpOnly;
    let mut kept: Vec<&str> = Vec::new();
    for tok in opts.split(';') {
        if let Some((k, v)) = tok.split_once('=') {
            if k.trim().eq_ignore_ascii_case("mode") {
                match v.trim().to_ascii_lowercase().as_str() {
                    "tcp_and_udp" => {
                        mode = Mode::TcpAndUdp;
                        continue;
                    }
                    "udp_only" => {
                        mode = Mode::UdpOnly;
                        continue;
                    }
                    "tcp_only" => {
                        mode = Mode::TcpOnly;
                        continue;
                    }
                    // Plugin-specific mode (obfs tls/http, v2ray quic/ws) — keep.
                    _ => {}
                }
            }
        }
        kept.push(tok);
    }
    let filtered = if kept.is_empty() {
        None
    } else {
        Some(kept.join(";"))
    };
    (mode, filtered)
}

/// Returns true if the given plugin name selects the built-in simple-obfs.
/// Accepts both `obfs` (Go mihomo's short name) and `simple-obfs` (the
/// original SIP003 binary name some users still write).
#[cfg(feature = "ss-plugins")]
pub fn is_builtin_obfs_plugin(name: &str) -> bool {
    matches!(name, "obfs" | "simple-obfs")
}

/// Whether `name` resolves to an in-process plugin — anything else is
/// spawned as an external SIP003 subprocess.  Must stay in sync with the
/// dispatch match in `ShadowsocksAdapter::new` (single source of truth
/// for `meow-config`'s dialer-injection gate, which cannot see the
/// feature-gated `ech-tls-tunnel`/`kcptun` arms directly).
#[cfg(feature = "ss-plugins")]
pub fn is_builtin_sip003_plugin(name: &str) -> bool {
    is_builtin_obfs_plugin(name)
        || matches!(
            name,
            "v2ray-plugin" | "gost-plugin" | "shadow-tls" | "restls" | "jls"
        )
        || (cfg!(feature = "ech-tls-tunnel") && name == "ech-tls-tunnel")
        || (cfg!(feature = "kcptun") && name == "kcptun")
}

/// Parses `plugin-opts` (already serialized to SIP003 `key=value;...` form) for
/// the built-in simple-obfs plugin.
///
/// Accepted keys (alias-tolerant — both YAML-style `mode`/`host` and SIP003
/// native `obfs`/`obfs-host` work):
///
/// * `mode` / `obfs` → `http` or `tls` (case-insensitive). REQUIRED.
/// * `host` / `obfs-host` → fake `Host:` (HTTP) or fake SNI (TLS).
///   Falls back to the SS server name if absent or empty.
///
/// Unknown keys are silently ignored to stay forward-compatible with the
/// upstream Go reference.
#[cfg(feature = "ss-plugins")]
pub(crate) fn parse_obfs_opts(plugin_opts: Option<&str>, server: &str) -> Result<BuiltinObfs> {
    let opts = plugin_opts.unwrap_or("").trim();
    let mut mode: Option<String> = None;
    let mut host: Option<String> = None;
    for part in opts.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (part, ""),
        };
        match k {
            "obfs" | "mode" => mode = Some(v.to_ascii_lowercase()),
            "obfs-host" | "host" => host = Some(v.to_string()),
            _ => {}
        }
    }
    let mode = mode.ok_or_else(|| {
        MeowError::Config("simple-obfs plugin-opts must specify mode=http or mode=tls".to_string())
    })?;
    // An empty `host=` is treated as "not set" — fall back to the server.
    let host = host
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| server.to_string());
    // `host` lands verbatim in the emitted `Host:` header (http mode) or
    // SNI (tls mode) — reject bytes that could inject into either. The
    // opt arrives verbatim from provider payloads too (issue #648).
    if !meow_transport::simple_obfs::client::is_valid_obfs_host(&host) {
        return Err(MeowError::Config(
            "simple-obfs plugin-opts host is empty, over 253 bytes, or contains whitespace/control bytes".to_string(),
        ));
    }
    match mode.as_str() {
        "http" => Ok(BuiltinObfs::Http { host }),
        "tls" => Ok(BuiltinObfs::Tls { server: host }),
        other => Err(MeowError::Config(format!(
            "simple-obfs unsupported mode '{other}': expected 'http' or 'tls'"
        ))),
    }
}

/// How long [`SsConn`] waits for the caller's first payload before it sends
/// the Shadowsocks request header on its own.
///
/// `ProxyClientStream` defers the request header (salt/IV + target address)
/// to the first `poll_write` and coalesces it with that payload, so a
/// client-first connection opens with one chunk carrying both. A
/// server-first protocol (SMTP, FTP, POP3, IMAP, MySQL, VNC) writes nothing
/// until the server's banner arrives, and meow's relay never issues an empty
/// write — without a deadline the SS server waits for the address while the
/// client waits for the banner, forever.
///
/// 200 ms is mihomo's pre-dial client peek: `handleTCPConn` gives the client
/// 200 ms to send its first bytes, then writes whatever it got — possibly
/// nothing — to a conn that `N.NeedHandshake`, and an empty write sends the
/// SS header alone (`tunnel/tunnel.go:543-551,582-604` @ MetaCubeX/mihomo
/// 88dcbf7f). shadowsocks-rust's `sslocal` does the same with a 500 ms wait
/// and `write(&[])` (`crates/shadowsocks-service/src/local/utils.rs:40-67`
/// @ f23366dc). mihomo's window overlaps the dial; this one starts at the
/// relay's first read, after it. Only server-first connections pay it: a
/// client that writes inside the window keeps the coalesced header.
const SS_HEADER_WINDOW: Duration = Duration::from_millis(200);

/// Progress of the request header that `ProxyClientStream` defers to the
/// first `poll_write`.
enum HeaderState {
    /// Nothing written yet; the header waits for the caller's first payload.
    /// The window timer is armed by the first `poll_read` and boxed so
    /// [`SsConn`] stays `Unpin`; a connection that writes first never
    /// allocates it.
    Deferred(Option<Pin<Box<Sleep>>>),
    /// Header-only write (`poll_write(&[])`) in flight. Under backpressure
    /// the crate keeps the encrypted header in its own `Connecting` buffer
    /// and the next call resumes it.
    Writing,
    /// Header written; flushing it through plugin transports that buffer
    /// (TLS, WebSocket, shadow-tls, ...).
    Flushing,
    /// Nothing left to do: the header went out, alone or coalesced with the
    /// first payload, or the write half shut down before it was needed.
    Done,
}

/// [`ProxyConn`] over a Shadowsocks client stream that also sends the
/// deferred request header for server-first protocols: when the remote side
/// has been read for [`SS_HEADER_WINDOW`] and the caller has written
/// nothing, it writes the header alone. Like the VLESS
/// (`vless/conn.rs` `poll_flush_deferred_header`) and Snell v6
/// (`snell/v6.rs` `poll_send_request`) adapters, the deferred header is
/// flushed from `poll_read`, which the relay polls from the start.
struct SsConn<T> {
    inner: ProxyClientStream<T>,
    header: HeaderState,
}

impl<T: AsyncRead + AsyncWrite + Unpin> SsConn<T> {
    fn new(inner: ProxyClientStream<T>) -> Self {
        Self {
            inner,
            header: HeaderState::Deferred(None),
        }
    }

    /// Drive a started header-only write through write and flush. Returns
    /// `Ready(Ok(()))` straight away unless the state is `Writing` or
    /// `Flushing`.
    fn poll_send_header(&mut self, cx: &mut std::task::Context<'_>) -> Poll<std::io::Result<()>> {
        loop {
            match self.header {
                HeaderState::Writing => {
                    // An empty first write makes the crate send the salt +
                    // address (+ AEAD-2022 padding) as a chunk of its own —
                    // its documented hook for protocols that wait for a
                    // server hello (shadowsocks-rust#232). While this is
                    // pending the crate ignores the buffer argument and
                    // resumes its own.
                    ready!(Pin::new(&mut self.inner).poll_write(cx, &[]))?;
                    self.header = HeaderState::Flushing;
                }
                HeaderState::Flushing => {
                    ready!(Pin::new(&mut self.inner).poll_flush(cx))?;
                    self.header = HeaderState::Done;
                }
                HeaderState::Deferred(_) | HeaderState::Done => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for SsConn<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        loop {
            match &mut this.header {
                HeaderState::Done => break,
                HeaderState::Deferred(timer) => {
                    let timer =
                        timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(SS_HEADER_WINDOW)));
                    if timer.as_mut().poll(cx).is_pending() {
                        break;
                    }
                    debug!(
                        "ss: client silent for {}ms, sending request header alone",
                        SS_HEADER_WINDOW.as_millis()
                    );
                    this.header = HeaderState::Writing;
                }
                HeaderState::Writing | HeaderState::Flushing => {
                    if this.poll_send_header(cx)?.is_pending() {
                        break;
                    }
                }
            }
        }
        // Read even while the header is pending: a server close or reset
        // still surfaces, and the read waker is registered next to the
        // timer's or the header write's.
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for SsConn<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        if let HeaderState::Deferred(_) = this.header {
            if !buf.is_empty() {
                // First payload: the crate coalesces the header with it.
                // Leave `Deferred` before the call, because the crate commits
                // the header to this write even when it returns `Pending`.
                // A timer-driven empty write after that would complete the
                // crate's buffered `header ‖ buf` and report 0, and the
                // caller's retry would send `buf` a second time.
                this.header = HeaderState::Done;
                return Pin::new(&mut this.inner).poll_write(cx, buf);
            }
            // An explicit empty write asks for the header now.
            this.header = HeaderState::Writing;
        }
        // Finish a header-only write first: until it completes the crate
        // ignores `buf` and would report it written.
        ready!(this.poll_send_header(cx))?;
        if buf.is_empty() {
            // The header is out. Forwarding would make the crate emit an
            // empty AEAD chunk.
            return Poll::Ready(Ok(0));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        ready!(this.poll_send_header(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if let HeaderState::Deferred(_) = this.header {
            // The caller closed without sending anything: no header, as
            // before (sslocal also gives up on EOF). Dropping the timer
            // keeps a later read from writing after the shutdown.
            this.header = HeaderState::Done;
        }
        ready!(this.poll_send_header(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static> ProxyConn for SsConn<T> {}

/// Per-association SIP022 client session for AEAD-2022 UDP (§3.2.2).
///
/// The socket's `send`/`recv` shortcuts substitute an all-zero control —
/// `client_session_id = 0` and `packet_id = 0` on every datagram — which a
/// spec-conforming 2022 server drops from the second packet on (its
/// per-session replay window is mandatory, §3.2.4). That made SS-2022 UDP
/// outbound unusable against real servers. A working association mints one
/// random session ID, counts packets up, and validates the reply direction
/// the same way the server validates us.
///
/// Ciphers outside the AEAD-2022 category carry no session fields: the
/// control is ignored on encrypt and `recv` yields `None`, so this state is
/// simply unused there.
struct SsUdpSession {
    /// Random non-zero session ID the server keys its relay session on.
    client_session_id: u64,
    /// Client→server packet counter for this session.
    next_packet_id: AtomicU,
    /// Server→client replay tracker: a sliding window over each server
    /// session's packet IDs (§3.2.4 applies to clients too). `Mutex`
    /// because `read_packet` takes `&self`.
    server: std::sync::Mutex<ServerReplyTracker>,
}

/// Interleaved replies under several live server sessions (e.g. a UDP load
/// balancer fanning one VIP out to several ssserver backends, each minting
/// its own ID for our client session, or a server-side association expiry
/// re-keying mid-association) must each keep their window — resetting one
/// shared window on every flap would re-accept replayed packet IDs. §3.2.4
/// has clients remember at least the current and previous server sessions.
#[derive(Default)]
struct ServerReplyTracker {
    /// server_session_id → its reply packet-ID window. Bounded by
    /// [`MAX_TRACKED_SERVER_SESSIONS`]: past it the *least recently used*
    /// window is evicted — never the whole table. A full `clear()` would let
    /// an on-path attacker replay N distinct recently-captured server
    /// session IDs to wipe every window at once, re-opening all of them to
    /// replays; per-entry eviction bounds that blast radius to a single
    /// session per injected datagram.
    windows: std::collections::HashMap<u64, TrackedWindow>,
    /// Monotonic access stamp feeding LRU eviction.
    tick: u64,
}

/// A replay window plus the last time it filtered a reply.
struct TrackedWindow {
    window: meow_common::ReplayWindow,
    last_used: u64,
}

const MAX_TRACKED_SERVER_SESSIONS: usize = 4;

impl SsUdpSession {
    /// Mint a fresh session. `generate_nonce` fills via `random_iv_or_salt`,
    /// which already guarantees a non-zero buffer.
    fn new(ctx: &SharedContext, method: CipherKind) -> Self {
        let mut buf = [0u8; 8];
        ctx.generate_nonce(method, &mut buf, false);
        Self {
            client_session_id: u64::from_be_bytes(buf),
            next_packet_id: AtomicU::new(0),
            server: std::sync::Mutex::new(ServerReplyTracker::default()),
        }
    }

    /// The control for the next client→server datagram, or `None` once the
    /// packet-ID space is exhausted. IDs are pre-incremented (sslocal
    /// parity: the first datagram carries 1). On 32-bit targets `AtomicU`
    /// is u32 and the space runs out after 2^32 datagrams; `checked_add`
    /// refuses the wrap rather than reusing IDs the server's replay window
    /// would drop. The caller errors the association so the next datagram
    /// re-dials under a freshly minted session — the same recovery shape
    /// as sslocal's socket reset + session renewal.
    fn send_control(&self) -> Option<UdpSocketControlData> {
        let mut control = UdpSocketControlData::default();
        control.client_session_id = self.client_session_id;
        control.packet_id = checked_increment(&self.next_packet_id)?;
        Some(control)
    }

    /// Validate a server→client control (§3.2.3/§3.2.4): the echoed client
    /// session ID must be ours and the server session ID non-zero (`0` is
    /// the "no session" sentinel every implementation reserves — ssserver
    /// generates IDs in a non-zero loop, sing-box uses it as the empty
    /// marker); each server session ID keeps its own replay window (server
    /// restarts legitimately re-key sessions, and interleaved sessions must
    /// not reset each other's window); the packet ID must not be a replay
    /// or older than its session's window.
    fn accept_reply(&self, control: &UdpSocketControlData) -> bool {
        if control.client_session_id != self.client_session_id || control.server_session_id == 0 {
            return false;
        }
        let mut tracker = self
            .server
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.tick += 1;
        let tick = tracker.tick;
        let windows = &mut tracker.windows;
        if !windows.contains_key(&control.server_session_id)
            && windows.len() >= MAX_TRACKED_SERVER_SESSIONS
        {
            if let Some(&oldest) = windows
                .iter()
                .min_by_key(|(_, w)| w.last_used)
                .map(|(id, _)| id)
            {
                windows.remove(&oldest);
            }
        }
        windows
            .entry(control.server_session_id)
            .and_modify(|t| t.last_used = tick)
            .or_insert_with(|| TrackedWindow {
                window: meow_common::ReplayWindow::new(),
                last_used: tick,
            })
            .window
            .check_and_set(control.packet_id)
    }
}

// Wrapper for SS UDP packet transport. The crypto layer is identical on
// both arms — only how ciphertext datagrams reach the SS server's UDP
// endpoint differs.
enum SsUdpIo {
    /// Raw connected UDP socket to the server (no `dialer-proxy`).
    Raw(ProxySocket<TokioUdpDatagram>),
    /// Front-proxied UDP association — mihomo's `proxyDialer.ListenPacket`
    /// shape: `dialer-proxy` turns the SS server's UDP endpoint into the
    /// front's association destination, and the SS crypto layer runs
    /// directly over the returned `ProxyPacketConn` (no `ProxySocket`/poll
    /// bridge — `ProxySocket`'s client path only ever uses connected
    /// send/recv, which `ProxyPacketConn` already is).
    ///
    /// `remote` is the SS server's UDP endpoint: bound-at-dial conns
    /// (VLESS, mux, name-bound fronts) ignore the per-packet addr;
    /// per-packet conns (SOCKS5) encode it — either way it must be the
    /// server endpoint, not the inner SS target (that travels encrypted
    /// inside the payload). A [`UdpTarget::Name`] keeps the server domain
    /// so the front resolves it with the same view as the TCP leg.
    Chained {
        conn: Arc<dyn ProxyPacketConn>,
        remote: UdpTarget,
        context: SharedContext,
        method: CipherKind,
        key: Box<[u8]>,
        identity_keys: Arc<Vec<bytes::Bytes>>,
    },
}

struct SsPacketConn {
    io: SsUdpIo,
    session: SsUdpSession,
    /// This adapter as a *front* for a domain-carrying association request
    /// (issue #657): the SS payload's target stamps the name so the server
    /// resolves it — the caller's per-packet `SocketAddr` arg is advisory.
    write_target: Option<Address>,
}

impl SsPacketConn {
    fn chained(
        conn: Arc<dyn ProxyPacketConn>,
        remote: UdpTarget,
        core: &SsCore,
        session: SsUdpSession,
        write_target: Option<Address>,
    ) -> Self {
        Self {
            io: SsUdpIo::Chained {
                conn,
                remote,
                context: Arc::clone(&core.context),
                method: core.server_config.method(),
                key: core.server_config.key().into(),
                identity_keys: core.server_config.clone_identity_keys(),
            },
            session,
            write_target,
        }
    }
}

#[async_trait]
impl ProxyPacketConn for SsPacketConn {
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        match &self.io {
            SsUdpIo::Raw(socket) => {
                use shadowsocks::relay::udprelay::proxy_socket::ProxySocketError;
                loop {
                    let (n, addr, _raw_len, control) = match socket.recv_with_ctrl(buf).await {
                        Ok(v) => v,
                        // A single undecryptable / malformed datagram must not kill
                        // the association: the reply task exits on Err, forcing a
                        // redial per stray packet (an on-path attacker replaying
                        // captured ciphertexts could churn sessions). Protocol-level
                        // rejects are per-datagram — drop and keep reading, matching
                        // how sslocal survives recv errors.
                        Err(
                            e @ (ProxySocketError::ProtocolError(_)
                            | ProxySocketError::ProtocolErrorWithPeer(..)),
                        ) => {
                            debug!("ss udp: dropped malformed reply datagram: {e}");
                            continue;
                        }
                        Err(e) => return Err(MeowError::Proxy(format!("ss udp recv: {e}"))),
                    };
                    // §3.2.4: a reply stamped for another session, or a replayed /
                    // out-of-window server packet ID, is dropped — keep reading for
                    // a valid datagram instead of surfacing the junk one.
                    if let Some(c) = &control {
                        if !self.session.accept_reply(c) {
                            continue;
                        }
                    }
                    let sock_addr = match addr {
                        Address::SocketAddress(sa) => sa,
                        Address::DomainNameAddress(ref host, port) => {
                            // Some peers echo the requested domain. Accept only our
                            // bound name and preserve remote DNS via an unspecified IP.
                            match &self.write_target {
                                Some(Address::DomainNameAddress(expected, expected_port))
                                    if host.eq_ignore_ascii_case(expected) && port == *expected_port =>
                                        std::net::SocketAddr::from(([0, 0, 0, 0], port)),
                                _ => continue,
                            }
                        },
                    };
                    return Ok((n, sock_addr));
                }
            }
            SsUdpIo::Chained {
                conn,
                remote,
                context,
                method,
                key,
                ..
            } => loop {
                // Transport errors propagate (the association is dead);
                // decrypt failures are per-datagram — same drop-and-continue
                // as the Raw arm's ProtocolError handling above.
                let (rn, outer_src) = conn.read_packet(buf).await?;
                // Per-packet fronts (SOCKS5 UDP) report the real wire source
                // and can deliver datagrams from arbitrary remotes — restore
                // the connected-socket filter the Raw arm had. Bound conns
                // report `remote` or an unspecified addr (no source info);
                // unspecified skips the check rather than breaking them.
                // `src_matches` compares canonically for literal targets and
                // by port only for a `Name` — a name-bound front resolved it
                // itself, so the wire source legitimately differs from any
                // local resolution (issue #657). The unspecified exemption is
                // canonical too: `::ffff:0.0.0.0` must not sneak past as a
                // "real" remote.
                if !remote.src_matches(outer_src) && !outer_src.ip().to_canonical().is_unspecified()
                {
                    debug!("ss udp: dropped chained reply from {outer_src} (expected {remote})");
                    continue;
                }
                let Ok((n, addr, control)) =
                    decrypt_server_payload(context, *method, key, &mut buf[..rn])
                else {
                    debug!("ss udp: dropped malformed reply datagram (chained)");
                    continue;
                };
                if let Some(c) = &control {
                    if !self.session.accept_reply(c) {
                        continue;
                    }
                }
                let Address::SocketAddress(sa) = addr else {
                    continue;
                };
                return Ok((n, sa));
            },
        }
    }

    async fn write_packet(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize> {
        // A front-side domain association stamps its bound name as the
        // decrypted target; otherwise the caller's per-packet addr rules.
        let arg_target;
        let target = match &self.write_target {
            Some(t) => t,
            None => {
                arg_target = Address::SocketAddress(*addr);
                &arg_target
            }
        };
        // An exhausted packet-ID space kills the association: the tunnel
        // drops the session on this error and the next datagram re-dials
        // under a fresh `SsUdpSession` — sslocal's recovery on counter
        // overflow (socket reset + session renewal).
        let Some(control) = self.session.send_control() else {
            return Err(MeowError::Proxy(
                "ss udp: client packet-ID space exhausted".into(),
            ));
        };
        match &self.io {
            SsUdpIo::Raw(socket) => {
                // ProxySocket::send_with_ctrl returns the encrypted packet size
                // (with protocol overhead), but callers expect the payload size.
                socket
                    .send_with_ctrl(target, &control, buf)
                    .await
                    .map_err(|e| MeowError::Proxy(format!("ss udp send: {e}")))?;
            }
            SsUdpIo::Chained {
                conn,
                remote,
                context,
                method,
                key,
                identity_keys,
            } => {
                // Same bytes `ProxySocket::send_with_ctrl` emits — the SS
                // header + target are inside the ciphertext; the wire
                // datagram always goes to `remote` (the SS server's UDP
                // endpoint bound by the front's association).
                let mut send_buf = bytes::BytesMut::with_capacity(buf.len() + 256);
                encrypt_client_payload(
                    context,
                    *method,
                    key,
                    target,
                    &control,
                    identity_keys,
                    buf,
                    &mut send_buf,
                );
                // AEAD-2022 overhead (header + EIH + up to 900 B padding +
                // addr + tag) can push ciphertext past u16::MAX for a legal
                // max-size UDP payload. Every stream-framed front encodes
                // datagrams with a u16 length — a wrap would desync the
                // front's stream permanently, so error the association like
                // EMSGSIZE does on the raw socket.
                if send_buf.len() > u16::MAX as usize {
                    return Err(MeowError::Proxy(format!(
                        "ss udp: chained datagram {}B exceeds u16 frame limit",
                        send_buf.len()
                    )));
                }
                // A datagram conn must be atomic — a short write means the
                // front truncated ciphertext, which no retry can repair.
                // `write_dst` is the literal for `Addr` targets and an
                // advisory placeholder for a name-bound front conn (it
                // stamps its own resolved target).
                let sent = conn.write_packet(&send_buf, &remote.write_dst()).await?;
                if sent != send_buf.len() {
                    return Err(MeowError::Proxy(format!(
                        "ss udp: front conn truncated datagram ({sent}/{} B)",
                        send_buf.len()
                    )));
                }
            }
        }
        Ok(buf.len())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        match &self.io {
            SsUdpIo::Raw(socket) => socket.local_addr().map_err(MeowError::Io),
            // Stream-framed front conns (VLESS/Trojan) have no bound UDP
            // socket — they report unspecified, which callers only log.
            SsUdpIo::Chained { conn, .. } => conn.local_addr(),
        }
    }

    fn close(&self) -> Result<()> {
        match &self.io {
            SsUdpIo::Raw(_) => Ok(()),
            SsUdpIo::Chained { conn, .. } => conn.close(),
        }
    }
}

/// The shadowsocks crate asserts `domain.len() <= u8::MAX` when serializing
/// a `DomainNameAddress`, so an oversized provider/config hostname panics
/// instead of erroring — refuse first. `NotSupported` also lets a chained
/// `dial_udp_conn` caller fall back to a locally-resolved `UdpTarget::Addr`
/// (issue #657).
fn parse_address(metadata: &Metadata) -> Result<Address> {
    if !metadata.host.is_empty() || metadata.dst_ip.is_none() {
        if metadata.host.len() > u8::MAX as usize {
            return Err(MeowError::NotSupported(format!(
                "ss: domain target exceeds 255 bytes ({}B)",
                metadata.host.len()
            )));
        }
        return Ok(Address::DomainNameAddress(
            metadata.host.to_string(),
            metadata.dst_port,
        ));
    }
    Ok(Address::SocketAddress(SocketAddr::new(
        metadata.dst_ip.expect("checked above"),
        metadata.dst_port,
    )))
}

#[async_trait]
impl ProxyAdapter for ShadowsocksAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Shadowsocks
    }

    fn addr(&self) -> &str {
        &self.addr_str
    }

    fn support_udp(&self) -> bool {
        // The plain SS UDP association rides the front proxy's own
        // `dial_udp` under `dialer-proxy` (mihomo `proxyDialer.ListenPacket`),
        // so a proxy dialer is fine *when the front advertises UDP* — a
        // point-in-time snapshot for group fronts; `dial_udp` re-checks at
        // dial and fails closed on a front that cannot carry UDP. An
        // external SIP003 plugin's UDP leg (its local listener) bypasses
        // the chain by design, same as TCP — capability is unconditional
        // there.
        //
        // Two consumers read this advertisement: the rule probe treats a
        // UDP flow matched to a `!support_udp` target as ineligible and
        // skips to the next rule (tunnel.rs `RouteTargetProbe`), and
        // LoadBalance filters members by it.  It is NOT the enforcement
        // point — `dial_udp` re-checks the chain and refuses on its own,
        // so a stale optimistic snapshot cannot open a raw socket.  Keep
        // the two in sync.
        let plain_udp_ok = self.support_udp
            && self.core.plugin.udp_block_reason().is_none()
            && (!self.udp_via_chain() || self.core.dialer.supports_udp());
        // kcptun's UDP path is UDP-over-TCP over a pooled smux stream — its
        // datagrams go through `dial_udp_endpoint`, which tunnels them under
        // `dialer-proxy` — so the chain is *safe*, but it still needs the
        // front to carry UDP at all: advertise only when it reports so.
        #[cfg(feature = "kcptun")]
        let kcptun_udp_ok = self.support_udp
            && matches!(self.core.plugin, PluginKind::Kcptun(_))
            && self.core.dialer.supports_udp();
        #[cfg(not(feature = "kcptun"))]
        let kcptun_udp_ok = false;
        plain_udp_ok || kcptun_udp_ok || {
            #[cfg(feature = "mux")]
            {
                self.mux.as_ref().is_some_and(|mux| mux.supports_udp())
            }
            #[cfg(not(feature = "mux"))]
            {
                false
            }
        }
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        let addr = parse_address(metadata)?;
        debug!("SS connecting to {} via {}", addr, self.addr_str);

        #[cfg(feature = "mux")]
        if let Some(mux) = &self.mux {
            let conn = mux.open_stream_for(metadata, "ss").await?;
            return Ok(Box::new(conn));
        }

        self.core
            .dial_tcp_stream(addr, metadata.is_internal())
            .await
    }

    /// Run the SS handshake over an existing stream (relay chain).
    ///
    /// The stream already terminates at this SS server, so the configured
    /// obfs/v2ray-plugin/ech transport still applies on top of it — only
    /// the raw dial is skipped.  Mux pooling is bypassed (single-use
    /// stream), and external SIP003 plugins fail loudly because they own
    /// their outbound leg.
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        let addr = parse_address(metadata)?;
        debug!("SS connecting to {} via relay stream", addr);

        #[cfg(feature = "mux")]
        if self.mux.is_some() {
            debug!("SS mux bypassed on relay-supplied stream (single-use)");
        }

        self.core.tcp_stream_over(Box::new(stream), addr).await
    }

    #[cfg_attr(not(feature = "mux"), allow(unused_variables))]
    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        #[cfg(feature = "mux")]
        if let Some(mux) = &self.mux {
            if mux.supports_udp() {
                debug!(
                    "SS mux UDP connecting to {} via {}",
                    metadata.remote_address(),
                    self.addr_str
                );
            }
            if let Some(conn) = mux.open_packet_stream_for(metadata, "ss").await? {
                return Ok(conn);
            }
        }

        // kcptun has no UDP relay on the wire — upstream forces
        // `UDPOverTCP`: one pooled smux stream carries an SS request to the
        // magic UoT target, then legacy `addr ‖ len ‖ payload` framing.
        // Safe before the `is_proxy` refusal below: the underlying KCP
        // datagrams went through `dial_udp_endpoint`, which tunnels them
        // through the front proxy's UDP relay rather than binding a raw
        // socket.
        #[cfg(feature = "kcptun")]
        if let PluginKind::Kcptun(client) = &self.core.plugin {
            // The UoT per-packet address is `SocketAddr`-keyed — a domain
            // association target cannot ride it. Refuse so the dialer layer
            // falls back to a locally-resolved `UdpTarget::Addr`, which the
            // kcptun path stamps per packet as before.
            if let Some((host, _)) = metadata.domain_udp_target() {
                return Err(MeowError::NotSupported(format!(
                    "ss kcptun: cannot carry domain UDP target {host}"
                )));
            }
            debug!(
                "SS UDP over kcptun UoT connecting to {} via {}",
                metadata.remote_address(),
                self.addr_str
            );
            let stream = client.open_stream().await?;
            let target = Address::DomainNameAddress(
                kcptun_plugin::UOT_MAGIC_HOST.to_string(),
                kcptun_plugin::UOT_MAGIC_PORT,
            );
            let ss_stream = ProxyClientStream::from_stream(
                Arc::clone(&self.core.context),
                stream,
                &self.core.server_config,
                target,
            );
            return Ok(Box::new(kcptun_plugin::UotPacketConn::new(Box::new(
                ss_stream,
            ))));
        }

        // TCP-transport plugins (v2ray/gost/shadow-tls/…) refuse UDP
        // regardless of chaining — checked before the `is_proxy` branch so
        // the refusal names the plugin, not the chain.
        if let Some(reason) = self.core.plugin.udp_block_reason() {
            return Err(MeowError::NotSupported(reason.into()));
        }

        // This adapter as a *front*: a host-only UDP destination — the
        // dialer layer's encoding of a chained `UdpTarget::Name`
        // (issue #657) — binds the association to the name: the SS
        // payload's target stamps the domain and the server resolves it
        // with its own view. The mux arm above carries it natively on
        // bound flows; the kcptun arm cannot and refuses below. The same
        // `DomainNameAddress` length contract as `parse_address` applies —
        // refuse an oversized name so the caller falls back to a literal
        // `UdpTarget::Addr` instead of tripping the crate's assert.
        let write_target = match metadata.domain_udp_target() {
            Some((host, port)) if host.len() <= u8::MAX as usize => {
                Some(Address::DomainNameAddress(host.to_string(), port))
            }
            Some((host, _)) => {
                return Err(MeowError::NotSupported(format!(
                    "ss: domain UDP target exceeds 255 bytes ({}B)",
                    host.len()
                )));
            }
            None => None,
        };

        // Snapshot pre-flight BEFORE resolving: a front that advertises no
        // UDP is a capability refusal (`NotSupported` is exempt from
        // dead-marking — the node itself isn't broken), and skipping the
        // resolve keeps a refused dial from emitting a pointless local DNS
        // lookup whose failure would masquerade as `Proxy` (dead-markable).
        // The authoritative check stays the real dial below: groups may
        // rotate members between this query and it.
        if self.udp_via_chain() && !self.core.dialer.supports_udp() {
            return Err(MeowError::NotSupported(
                "ss: `dialer-proxy` front reports no UDP support; \
                 refusing rather than leaking the real source path"
                    .into(),
            ));
        }

        // mihomo `proxyDialer.ListenPacket`: under `dialer-proxy` the SS UDP
        // association rides the front proxy's own `dial_udp` — the
        // association's wire destination is the SS server's UDP endpoint and
        // the SS crypto layer wraps the per-packet targets inside. Reached
        // after the mux branch above (which tunnels UDP over `dialer.dial()`)
        // and skipped for external plugins (their UDP endpoint is the local
        // plugin listener — never chained).
        //
        // A domain-form server address dials as `UdpTarget::Name` — the
        // front resolves it with the same view as the control/TCP leg
        // (issue #657); a front that cannot carry a domain refuses and
        // `dial_udp_via_front` falls back to a local resolution.
        //
        // Fail-closed (Class A, ADR-0002): a front that cannot carry UDP
        // surfaces its error and the raw-socket path below stays unreachable
        // on this branch — no silent real-source egress.
        if self.udp_via_chain() {
            let target = match self.core.server_config.udp_external_addr() {
                ServerAddr::SocketAddr(sa) => UdpTarget::Addr(*sa),
                ServerAddr::DomainName(host, port) => UdpTarget::named(host, *port),
            };
            let (conn, bound) = self
                .dial_udp_via_front(target, metadata.is_internal())
                .await?;
            debug!("SS UDP chained to {bound} via {}", self.addr_str);
            let session = SsUdpSession::new(&self.core.context, self.core.server_config.method());
            return Ok(Box::new(SsPacketConn::chained(
                conn,
                bound,
                &self.core,
                session,
                write_target,
            )));
        }

        // The SS server's UDP endpoint for the raw path. `udp_external_addr`
        // returns a literal `SocketAddr` for the standard path and the SIP003
        // plugin's local listener for external plugins (where the connect is
        // loopback — protect is harmless).
        let candidates = match self.core.server_config.udp_external_addr() {
            ServerAddr::SocketAddr(sa) => vec![*sa],
            ServerAddr::DomainName(host, port) => meow_common::resolve_host_all(host, *port)
                .await
                .map_err(|e| MeowError::io_with(&format!("ss udp lookup {host}:{port}"), e))?,
        };

        // Hand-roll the UDP bind+connect so the installed
        // `meow_common::SocketProtector` sees the fd before bind — otherwise
        // the upstream `shadowsocks::ProxySocket::connect` path binds via
        // plain tokio and the Android `VpnService.protect(fd)` hook never
        // fires, looping outbound UDP back into our own VPN tunnel.
        //
        // Try candidates in resolver order rather than committing to the
        // first one: on a single-stack network the resolver can still order
        // the unreachable family first (AAAA on an IPv4-only path), and a
        // UDP connect() to an unreachable family fails immediately with
        // ENETUNREACH — so falling through to the next candidate is cheap
        // and keeps UDP relay alive where TcpStream::connect's built-in
        // multi-address loop already keeps TCP alive.
        let mut connected = None;
        let mut last_err: Option<MeowError> = None;
        for remote in candidates {
            let bind_addr: SocketAddr = if remote.is_ipv4() {
                "0.0.0.0:0".parse().expect("static")
            } else {
                "[::]:0".parse().expect("static")
            };
            let udp = match meow_common::bind_udp(bind_addr).await {
                Ok(udp) => udp,
                Err(e) => {
                    last_err = MeowError::prefer_errno(
                        last_err,
                        MeowError::io_with(&format!("ss udp bind for {remote}"), e),
                    );
                    continue;
                }
            };
            match udp.connect(remote).await {
                Ok(()) => {
                    connected = Some((udp, remote));
                    break;
                }
                Err(e) => {
                    last_err = MeowError::prefer_errno(
                        last_err,
                        MeowError::io_with(&format!("ss udp connect {remote}"), e),
                    );
                }
            }
        }
        let Some((udp, remote)) = connected else {
            return Err(last_err
                .unwrap_or_else(|| MeowError::Proxy("ss udp connect: no candidates".into())));
        };
        let socket = ProxySocket::<TokioUdpDatagram>::from_socket(
            UdpSocketType::Client,
            Arc::clone(&self.core.context),
            &self.core.server_config,
            TokioUdpDatagram(udp),
        );
        // One SIP022 client session per association: a fresh random session
        // ID + packet counter, minted from the shared context's CSPRNG.
        let session = SsUdpSession::new(&self.core.context, self.core.server_config.method());
        debug!("SS UDP connected via {}", remote);
        Ok(Box::new(SsPacketConn {
            io: SsUdpIo::Raw(socket),
            session,
            write_target,
        }))
    }

    /// Issue #695: close every pooled mux session and every pooled kcptun
    /// KCP/smux session so the next dial opens fresh sockets. An external
    /// SIP003 plugin's own upstream sockets live in a subprocess meow does
    /// not control and are not reset.
    fn reset_sessions(&self) {
        #[cfg(feature = "mux")]
        if let Some(mux) = &self.mux {
            mux.reset();
        }
        #[cfg(feature = "kcptun")]
        if let PluginKind::Kcptun(client) = &self.core.plugin {
            client.reset();
        }
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

// ─── Tokio UDP datagram adapter ─────────────────────────────────────────────
//
// `ProxySocket::<S>::from_socket` accepts any `S` that implements
// `DatagramSocket + DatagramSend + DatagramReceive`. The upstream
// `shadowsocks` crate ships these impls only for its own
// `shadowsocks::net::UdpSocket`, whose constructors all bind the underlying
// `tokio::net::UdpSocket` internally — bypassing our protect hook.
//
// `TokioUdpDatagram` is a thin newtype over `tokio::net::UdpSocket` that
// implements the three traits as straight delegates, so the SS UDP adapter
// can bind the fd through `meow_common::bind_udp` (firing the
// `SocketProtector`) and then hand the connected socket to the SS codec.

struct TokioUdpDatagram(tokio::net::UdpSocket);

impl DatagramSocket for TokioUdpDatagram {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.0.local_addr()
    }
}

impl DatagramReceive for TokioUdpDatagram {
    fn poll_recv(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.0.poll_recv(cx, buf)
    }
    fn poll_recv_from(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<SocketAddr>> {
        self.0.poll_recv_from(cx, buf)
    }
    fn poll_recv_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.0.poll_recv_ready(cx)
    }
}

impl DatagramSend for TokioUdpDatagram {
    fn poll_send(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.poll_send(cx, buf)
    }
    fn poll_send_to(
        &self,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0.poll_send_to(cx, buf, target)
    }
    fn poll_send_ready(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.0.poll_send_ready(cx)
    }
}

#[cfg(test)]
#[path = "test/shadowsocks_adapter_test.rs"]
mod tests;

/// [`SsConn`]'s deferred-header handling against an in-process
/// `ProxyServerStream` over an in-memory duplex. The clock is paused, so it
/// only moves when every task is idle (or on `advance`): the window expires
/// at exactly [`SS_HEADER_WINDOW`] of virtual time.
#[cfg(test)]
#[path = "test/shadowsocks_adapter_ss_conn_test.rs"]
mod ss_conn_tests;
