mod body;
mod conn;
pub mod header;
pub mod server;
mod kdf;
mod packet;

use async_trait::async_trait;
use meow_common::{
    AdapterType, MeowError, Metadata, ProxyAdapter, ProxyConn, ProxyHealth, ProxyPacketConn, Result,
};
use smol_str::SmolStr;
use std::sync::Arc;
use tracing::debug;

use crate::transport_chain::TransportChain;
pub use header::Security;

pub struct VmessAdapter {
    name: SmolStr,
    server: SmolStr,
    port: u16,
    addr_str: SmolStr,
    cmd_key: [u8; 16],
    security: Security,
    udp: bool,
    transport: Arc<TransportChain>,
    dialer: Arc<dyn crate::dialer::TcpDialer>,
    health: ProxyHealth,
    /// sing-mux compatible connection multiplexing (optional).
    #[cfg(feature = "mux")]
    mux: Option<Arc<crate::mux::MuxClient>>,
}

impl VmessAdapter {
    #[allow(
        clippy::too_many_arguments,
        reason = "dialer param for pluggable TcpDialer"
    )]
    pub fn new(
        name: &str,
        server: &str,
        port: u16,
        uuid_bytes: [u8; 16],
        security: Security,
        udp: bool,
        transport: TransportChain,
        dialer: Arc<dyn crate::dialer::TcpDialer>,
    ) -> Self {
        Self {
            name: SmolStr::from(name),
            server: SmolStr::from(server),
            port,
            addr_str: SmolStr::from(format!("{server}:{port}")),
            cmd_key: header::cmd_key(&uuid_bytes),
            security,
            udp,
            transport: Arc::new(transport),
            dialer,
            health: ProxyHealth::new(),
            #[cfg(feature = "mux")]
            mux: None,
        }
    }

    /// Enable connection multiplexing.  Two wire protocols share one
    /// connection pool (protocol picked by the `protocol` mux option):
    ///
    /// * sing-mux (smux/yamux/h2mux) — the session's VMess request targets
    ///   the reserved mux destination (sp.mux.sing-box.arpa:444) and a mux
    ///   request header follows; server must be sing-box / mihomo with
    ///   multiplex enabled on the VMess inbound.
    /// * muxcool — the session's VMess request itself is the signaling
    ///   (CommandMux 0x03, no address); server must be Xray, or sing-box /
    ///   mihomo (sing-vmess routes CommandMux to HandleMuxConnection, no
    ///   inbound config needed).
    #[cfg(feature = "mux")]
    pub fn with_mux(mut self, options: crate::mux::MuxOptions) -> Self {
        use crate::mux::{MuxClient, MUX_DESTINATION_FQDN, MUX_DESTINATION_PORT};
        use std::sync::Arc as StdArc;

        let transport = Arc::clone(&self.transport);
        let server = self.server.clone();
        let cmd_key = self.cmd_key;
        let security = self.security;
        let port = self.port;
        let protocol = options.protocol;
        let dialer = Arc::clone(&self.dialer);

        let dial: crate::mux::DialFn = StdArc::new(move || {
            let transport = Arc::clone(&transport);
            let server = server.clone();
            let dialer = Arc::clone(&dialer);
            Box::pin(async move {
                let sealed = match protocol {
                    crate::mux::Protocol::MuxCool => {
                        header::seal_mux_request_header(&cmd_key, security)
                    }
                    _ => {
                        let metadata = Metadata {
                            host: MUX_DESTINATION_FQDN.into(),
                            dst_port: MUX_DESTINATION_PORT,
                            ..Default::default()
                        };
                        header::seal_request_header(&cmd_key, security, &metadata, false)
                    }
                }
                .map_err(MeowError::Proxy)?;
                // Mux session dial — `internal: false`: a shared mux conn
                // exists to serve user streams regardless of which dial
                // triggered its establishment.
                dial_vmess(
                    &transport,
                    &server,
                    port,
                    dialer.as_ref(),
                    sealed,
                    security,
                    false,
                )
                .await
            })
        });
        self.mux = Some(MuxClient::new(dial, options));
        self
    }

    /// Dial a raw TCP + transport-chain stream to the VMess server, run the
    /// VMess request header exchange for the given destination, and return
    /// the encrypted duplex.
    async fn dial_to(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        let sealed = header::seal_request_header(&self.cmd_key, self.security, metadata, false)
            .map_err(MeowError::Proxy)?;
        dial_vmess(
            &self.transport,
            &self.server,
            self.port,
            self.dialer.as_ref(),
            sealed,
            self.security,
            metadata.is_internal(),
        )
        .await
    }
}

/// Dial a raw TCP + transport-chain stream to the VMess server, run the
/// VMess request header exchange for the given destination, and return the
/// encrypted duplex.  Shared by the plain dial path and the sing-mux
/// session dialer (which targets the reserved mux destination).
async fn dial_vmess(
    transport: &TransportChain,
    server: &str,
    port: u16,
    dialer: &dyn crate::dialer::TcpDialer,
    sealed: header::SealedHeader,
    security: Security,
    internal: bool,
) -> Result<Box<dyn ProxyConn>> {
    let stream = dialer
        .dial(server, port, internal)
        .await
        .map_err(MeowError::Io)?;
    vmess_over(transport, stream, sealed, security).await
}

/// Apply the transport chain and run the VMess request exchange on
/// `stream`, which must already terminate at this adapter's server —
/// `dial_tcp` obtains it from `dialer.dial`, `connect_over` receives it
/// from the relay chain.
async fn vmess_over(
    transport: &TransportChain,
    stream: Box<dyn meow_transport::Stream>,
    sealed: header::SealedHeader,
    security: Security,
) -> Result<Box<dyn ProxyConn>> {
    use tokio::io::AsyncWriteExt;

    let mut stream = transport.connect(stream).await.map_err(|e| match e {
        // Io/RelayHop chains carry errno + capability classification that
        // dead-marking reads — do not stringify them away (issue #668).
        e @ (MeowError::Io(_) | MeowError::RelayHopFailed { .. }) => e,
        e => MeowError::Proxy(format!("vmess transport: {e}")),
    })?;

    stream
        .write_all(&sealed.bytes)
        .await
        .map_err(MeowError::Io)?;

    // The relay builds each half's body cipher internally — one AEAD key
    // schedule per direction, and a swapped pairing is unrepresentable
    // (issue #533).
    let duplex = conn::spawn_vmess_relay(
        stream,
        security,
        sealed.req_key,
        sealed.req_iv,
        sealed.resp_v,
    );
    Ok(Box::new(crate::stream_conn::StreamConn(Box::new(duplex))))
}

#[async_trait]
impl ProxyAdapter for VmessAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn adapter_type(&self) -> AdapterType {
        AdapterType::Vmess
    }

    fn addr(&self) -> &str {
        &self.addr_str
    }

    fn support_udp(&self) -> bool {
        #[cfg(feature = "mux")]
        if self.mux.as_ref().is_some_and(|mux| mux.supports_udp()) {
            return true;
        }
        self.udp
    }

    async fn dial_tcp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyConn>> {
        debug!(
            "VMess connecting to {} via {}",
            metadata.remote_address(),
            self.addr_str
        );

        #[cfg(feature = "mux")]
        if let Some(mux) = &self.mux {
            let conn = mux.open_stream_for(metadata, "vmess").await?;
            return Ok(Box::new(conn));
        }

        self.dial_to(metadata).await
    }

    /// Run the transport chain + VMess request exchange over an existing
    /// stream (relay chain).  Mux pooling is bypassed — the relay-supplied
    /// stream is single-use and cannot be re-dialled.
    async fn connect_over(
        &self,
        stream: Box<dyn ProxyConn>,
        metadata: &Metadata,
    ) -> Result<Box<dyn ProxyConn>> {
        #[cfg(feature = "mux")]
        if self.mux.is_some() {
            debug!("VMess mux bypassed on relay-supplied stream (single-use)");
        }
        let sealed = header::seal_request_header(&self.cmd_key, self.security, metadata, false)
            .map_err(MeowError::Proxy)?;
        vmess_over(&self.transport, Box::new(stream), sealed, self.security).await
    }

    #[cfg_attr(not(feature = "mux"), allow(unused_variables))]
    async fn dial_udp(&self, metadata: &Metadata) -> Result<Box<dyn ProxyPacketConn>> {
        #[cfg(feature = "mux")]
        if let Some(mux) = &self.mux {
            if let Some(conn) = mux.open_packet_stream_for(metadata, "vmess").await? {
                debug!(
                    "VMess mux UDP connecting to {} via {}",
                    metadata.remote_address(),
                    self.addr_str
                );
                return Ok(conn);
            }
        }

        if !self.udp {
            return Err(MeowError::NotSupported("vmess UDP disabled".into()));
        }
        use tokio::io::AsyncWriteExt;
        let sealed = header::seal_request_header(&self.cmd_key, self.security, metadata, true)
            .map_err(MeowError::Proxy)?;
        let stream = self
            .dialer
            .dial(&self.server, self.port, metadata.is_internal())
            .await
            .map_err(MeowError::Io)?;
        let mut stream = self.transport.connect(stream).await?;
        stream
            .write_all(&sealed.bytes)
            .await
            .map_err(MeowError::Io)?;
        stream.flush().await.map_err(MeowError::Io)?;
        // Response headers can arrive with the first reply; do not wait here.
        Ok(Box::new(packet::VmessPacketConn::new(
            stream,
            self.security,
            &sealed,
        )))
    }

    /// Issue #695: close every pooled mux session so the next dial opens a
    /// fresh physical connection (see `MuxClient::reset`).
    #[cfg(feature = "mux")]
    fn reset_sessions(&self) {
        if let Some(mux) = &self.mux {
            mux.reset();
        }
    }

    fn health(&self) -> &ProxyHealth {
        &self.health
    }
}

#[cfg(test)]
#[path = "../test/vmess_test.rs"]
mod tests;
