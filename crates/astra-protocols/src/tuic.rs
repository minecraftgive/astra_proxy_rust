//! Native TUIC v5 over QUIC. Wire specification: tuic-protocol/tuic SPEC.md.
use crate::{native::NodeConfig, outbound::Stream};
use anyhow::{bail, Context, Result};
use astra_ipc::Target;
use std::{
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::Mutex,
    task::JoinHandle,
};
use tokio_rustls::rustls;

pub struct Client {
    server: String,
    port: u16,
    server_name: String,
    uuid: [u8; 16],
    password: Vec<u8>,
    config: quinn::ClientConfig,
    udp_native: bool,
    heartbeat: Duration,
    session: Mutex<Option<Arc<Session>>>,
}

struct Session {
    connection: quinn::Connection,
    _endpoint: quinn::Endpoint,
    active: AtomicUsize,
    udp: std::sync::Mutex<std::collections::HashMap<u16, tokio::sync::mpsc::Sender<QueuedPacket>>>,
    next_association: std::sync::atomic::AtomicU16,
    udp_budget: Arc<tokio::sync::Semaphore>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"client closed");
        for task in self.tasks.get_mut().unwrap() {
            task.abort();
        }
    }
}

impl Client {
    pub fn new(cfg: &NodeConfig, base: &Path) -> Result<Self> {
        let tls = cfg.tls.as_ref().context("TUIC requires TLS settings")?;
        if tls.reality_public_key.is_some()
            || tls.reality_short_id.is_some()
            || tls.fingerprint.is_some()
            || tls.cert_pin_sha256.is_some()
            || cfg.mux.is_some()
            || cfg.transport.kind != crate::native::TransportKind::Tcp
        {
            bail!("TUIC uses its own QUIC transport; REALITY, fingerprint and mux are unsupported");
        }
        rustls::pki_types::ServerName::try_from(tls.server_name.clone())
            .context("invalid TUIC server name")?;
        if cfg.cipher.is_some()
            || cfg.transport.path.is_some()
            || cfg.transport.host.is_some()
            || cfg.transport.service_name.is_some()
            || !cfg.transport.headers.is_empty()
        {
            bail!("TUIC does not use cipher or HTTP transport options");
        }
        let uuid = *uuid::Uuid::parse_str(cfg.uuid.as_deref().context("TUIC uuid is required")?)?
            .as_bytes();
        let password = cfg
            .password
            .as_ref()
            .filter(|p| !p.is_empty())
            .context("TUIC password is required")?
            .as_bytes()
            .to_vec();
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(ca) = &tls.ca_file {
            let pem = std::fs::read(base.join(ca)).context("read TUIC CA")?;
            let certificates: Vec<_> =
                rustls_pemfile::certs(&mut pem.as_slice()).collect::<std::io::Result<_>>()?;
            if certificates.is_empty() {
                bail!("TUIC CA file contains no certificates");
            }
            for cert in certificates {
                roots.add(cert)?;
            }
        }
        let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        crypto.alpn_protocols = vec![b"h3".to_vec()];
        let mut config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
        ));
        let mut transport = quinn::TransportConfig::default();
        let idle_secs = cfg.tuic_idle_timeout_secs.unwrap_or(15);
        if !(3..=3600).contains(&idle_secs) {
            bail!("TUIC idle timeout must be 3..3600 seconds");
        }
        transport.max_idle_timeout(Some(Duration::from_secs(idle_secs).try_into()?));
        transport.max_concurrent_uni_streams(64u32.into());
        transport.max_concurrent_bidi_streams(0u32.into());
        transport.datagram_receive_buffer_size(Some(512 * 1024));
        transport.datagram_send_buffer_size(512 * 1024);
        match cfg.congestion_control.as_deref().unwrap_or("cubic") {
            "cubic" => {}
            "bbr" => {
                transport.congestion_controller_factory(Arc::new(
                    quinn::congestion::BbrConfig::default(),
                ));
            }
            "new_reno" => {
                transport.congestion_controller_factory(Arc::new(
                    quinn::congestion::NewRenoConfig::default(),
                ));
            }
            _ => bail!("TUIC congestion_control must be cubic, bbr or new_reno"),
        }
        let udp_native = match cfg.tuic_udp_mode.as_deref().unwrap_or("native") {
            "native" => true,
            "quic" => false,
            _ => bail!("TUIC tuic_udp_mode must be native or quic"),
        };
        config.transport_config(Arc::new(transport));
        Ok(Self {
            server: cfg.server.clone(),
            port: cfg.port,
            server_name: tls.server_name.clone(),
            uuid,
            password,
            config,
            udp_native,
            heartbeat: Duration::from_secs(idle_secs / 3),
            session: Mutex::new(None),
        })
    }

    async fn session(&self) -> Result<Arc<Session>> {
        let mut cached = self.session.lock().await;
        if let Some(session) = cached
            .as_ref()
            .filter(|s| s.connection.close_reason().is_none())
        {
            return Ok(session.clone());
        }
        let addresses: Vec<_> = tokio::net::lookup_host((self.server.as_str(), self.port))
            .await?
            .collect();
        let mut last_error = None;
        for address in addresses {
            let bind: SocketAddr = if address.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            }
            .parse()?;
            let mut endpoint = quinn::Endpoint::client(bind)?;
            endpoint.set_default_client_config(self.config.clone());
            let connection = match tokio::time::timeout(
                Duration::from_secs(5),
                endpoint.connect(address, &self.server_name)?,
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    last_error = Some(e.to_string());
                    continue;
                }
                Err(e) => {
                    last_error = Some(e.to_string());
                    continue;
                }
            };
            let mut token = [0; 32];
            connection
                .export_keying_material(&mut token, &self.uuid, &self.password)
                .map_err(|_| anyhow::anyhow!("TUIC TLS exporter failed"))?;
            let mut auth = connection.open_uni().await?;
            let mut header = vec![5, 0];
            header.extend_from_slice(&self.uuid);
            header.extend_from_slice(&token);
            auth.write_all(&header).await?;
            auth.finish()?;
            let session = Arc::new(Session {
                connection,
                _endpoint: endpoint,
                active: AtomicUsize::new(0),
                udp: std::sync::Mutex::new(std::collections::HashMap::new()),
                next_association: std::sync::atomic::AtomicU16::new(0),
                udp_budget: Arc::new(tokio::sync::Semaphore::new(2 * 1024 * 1024)),
                tasks: std::sync::Mutex::new(Vec::new()),
            });
            let weak = Arc::downgrade(&session);
            let heartbeat_period = self.heartbeat;
            let heartbeat = tokio::spawn(async move {
                let mut interval = tokio::time::interval(heartbeat_period);
                loop {
                    interval.tick().await;
                    let Some(session) = weak.upgrade() else {
                        break;
                    };
                    if session.connection.close_reason().is_some() {
                        break;
                    }
                    if session.active.load(Ordering::Relaxed) > 0 {
                        let _ = session
                            .connection
                            .send_datagram(bytes::Bytes::from_static(&[5, 4]));
                    }
                }
            });
            session.tasks.lock().unwrap().push(heartbeat);
            let dispatcher = tokio::spawn(dispatch(
                session.connection.clone(),
                Arc::downgrade(&session),
            ));
            session.tasks.lock().unwrap().push(dispatcher);
            tracing::debug!(server = %self.server, port = self.port, "TUIC QUIC session established");
            *cached = Some(session.clone());
            return Ok(session);
        }
        bail!(
            "TUIC connection failed: {}",
            last_error.unwrap_or_else(|| "server has no addresses".into())
        )
    }

    pub async fn connect(&self, target: &Target) -> Result<Stream> {
        let session = self.session().await?;
        let (mut send, recv) = session.connection.open_bi().await?;
        let mut header = vec![5, 1];
        encode_address(&mut header, target)?;
        send.write_all(&header).await?;
        session.active.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Tunnel {
            send,
            recv,
            session,
        }))
    }
}

fn encode_address(bytes: &mut Vec<u8>, target: &Target) -> Result<()> {
    if target.port == 0 {
        bail!("TUIC target port is zero");
    }
    bytes.extend(crate::wire::encode(target, 2)?);
    Ok(())
}

struct Tunnel {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    session: Arc<Session>,
}
impl Drop for Tunnel {
    fn drop(&mut self) {
        self.session.active.fetch_sub(1, Ordering::Relaxed);
    }
}
impl AsyncRead for Tunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}
impl AsyncWrite for Tunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}

const MAX_UDP: usize = 65507;
const MAX_WIRE: usize = MAX_UDP + 270;

impl Client {
    pub async fn connect_udp(
        &self,
        target: &Target,
    ) -> Result<Box<dyn meow_common::ProxyPacketConn>> {
        let session = self.session().await?;
        let (send, receive) = tokio::sync::mpsc::channel(256);
        let id = {
            let mut udp = session.udp.lock().unwrap();
            if udp.len() >= 4096 {
                bail!("TUIC association limit reached");
            }
            loop {
                let id = session.next_association.fetch_add(1, Ordering::Relaxed);
                if let std::collections::hash_map::Entry::Vacant(entry) = udp.entry(id) {
                    entry.insert(send);
                    break id;
                }
            }
        };
        session.active.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(PacketConnection {
            session,
            id,
            target: target.clone(),
            native: self.udp_native,
            next_packet: std::sync::atomic::AtomicU16::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            reader: Mutex::new(PacketReader {
                receive,
                fragments: std::collections::HashMap::new(),
            }),
        }))
    }
}

fn deliver(weak: &std::sync::Weak<Session>, packet: bytes::Bytes) {
    if packet.len() < 4 || packet.len() > MAX_WIRE || packet[..2] != [5, 2] {
        return;
    }
    let id = u16::from_be_bytes([packet[2], packet[3]]);
    if let Some(session) = weak.upgrade() {
        if let Some(send) = session.udp.lock().unwrap().get(&id) {
            if let Ok(permit) = session
                .udp_budget
                .clone()
                .try_acquire_many_owned(packet.len() as u32)
            {
                let _ = send.try_send(QueuedPacket {
                    bytes: packet,
                    _permit: permit,
                });
            }
        }
    }
}

async fn dispatch(connection: quinn::Connection, session: std::sync::Weak<Session>) {
    let mut streams = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            data = connection.read_datagram() => match data {
                Ok(data) => deliver(&session, data),
                Err(_) => break,
            },
            stream = connection.accept_uni() => match stream {
                Ok(mut stream) => {
                    if streams.len() >= 32 { let _ = stream.stop(0u32.into()); continue; }
                    let session = session.clone();
                    streams.spawn(async move {
                        if let Ok(Ok(packet)) = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(MAX_WIRE)).await {
                            deliver(&session, packet.into());
                        }
                    });
                }
                Err(_) => break,
            },
            Some(_) = streams.join_next(), if !streams.is_empty() => {},
        }
    }
    // Wake every pending UDP reader when the QUIC connection dies.
    if let Some(session) = session.upgrade() {
        session.udp.lock().unwrap().clear();
    }
}

struct PacketConnection {
    session: Arc<Session>,
    id: u16,
    target: Target,
    native: bool,
    next_packet: std::sync::atomic::AtomicU16,
    closed: std::sync::atomic::AtomicBool,
    reader: Mutex<PacketReader>,
}
impl PacketConnection {
    fn shutdown(&self) {
        if self.closed.swap(true, Ordering::Relaxed) {
            return;
        }
        self.session.udp.lock().unwrap().remove(&self.id);
        self.session.active.fetch_sub(1, Ordering::Relaxed);
        let connection = self.session.connection.clone();
        let id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), async move {
                    let mut stream = connection.open_uni().await?;
                    let mut command = vec![5, 3];
                    command.extend_from_slice(&id.to_be_bytes());
                    stream.write_all(&command).await?;
                    stream.finish()?;
                    Ok::<_, anyhow::Error>(())
                })
                .await;
            });
        }
    }
}
impl Drop for PacketConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}
fn proxy_error(error: impl std::fmt::Display) -> meow_common::MeowError {
    meow_common::MeowError::Proxy(format!("TUIC: {error}"))
}

#[async_trait::async_trait]
impl meow_common::ProxyPacketConn for PacketConnection {
    async fn write_packet(&self, data: &[u8], _address: &SocketAddr) -> meow_common::Result<usize> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(proxy_error("association closed"));
        }
        if data.len() > MAX_UDP {
            return Err(proxy_error("UDP packet too large"));
        }
        let mut address = Vec::new();
        encode_address(&mut address, &self.target).map_err(proxy_error)?;
        let capacity = if self.native {
            self.session
                .connection
                .max_datagram_size()
                .and_then(|mtu| mtu.checked_sub(10 + address.len()))
                .filter(|n| *n > 0)
                .ok_or_else(|| proxy_error("QUIC datagrams unavailable"))?
        } else {
            MAX_UDP
        };
        let packet_id = self.next_packet.fetch_add(1, Ordering::Relaxed);
        let packets = crate::datagram::encode(
            u32::from(self.id),
            packet_id,
            &self.target,
            data,
            crate::accept::Protocol::Tuic,
            capacity + 10 + address.len(),
        )
        .map_err(proxy_error)?;
        for packet in packets {
            if self.native {
                self.session
                    .connection
                    .send_datagram_wait(packet.into())
                    .await
                    .map_err(proxy_error)?;
            } else {
                let mut stream = self
                    .session
                    .connection
                    .open_uni()
                    .await
                    .map_err(proxy_error)?;
                stream.write_all(&packet).await.map_err(proxy_error)?;
                stream.finish().map_err(proxy_error)?;
            }
        }
        Ok(data.len())
    }
    async fn read_packet(&self, buf: &mut [u8]) -> meow_common::Result<(usize, SocketAddr)> {
        let mut reader = self.reader.lock().await;
        loop {
            reader.expire();
            let packet =
                match tokio::time::timeout(Duration::from_secs(5), reader.receive.recv()).await {
                    Ok(Some(packet)) => packet,
                    Ok(None) => return Err(proxy_error("QUIC session closed")),
                    Err(_) => continue,
                };
            let Ok(fragment) = decode_packet(&packet.bytes) else {
                continue;
            };
            let Some((target, payload)) = reader.accept(fragment) else {
                continue;
            };
            if payload.len() > buf.len() {
                return Err(proxy_error("UDP receive buffer too small"));
            }
            let ip = match target.host.parse::<IpAddr>() {
                Ok(ip) => ip,
                Err(_)
                    if target.host.eq_ignore_ascii_case(&self.target.host)
                        && target.port == self.target.port =>
                {
                    "0.0.0.0".parse().unwrap()
                }
                Err(_) => continue,
            };
            buf[..payload.len()].copy_from_slice(&payload);
            return Ok((payload.len(), SocketAddr::new(ip, target.port)));
        }
    }
    fn local_addr(&self) -> meow_common::Result<SocketAddr> {
        self.session._endpoint.local_addr().map_err(proxy_error)
    }
    fn close(&self) -> meow_common::Result<()> {
        self.shutdown();
        Ok(())
    }
}

struct Fragment {
    id: u16,
    count: u8,
    index: u8,
    target: Option<Target>,
    payload: Vec<u8>,
}
struct Assembly {
    created: std::time::Instant,
    target: Option<Target>,
    parts: Vec<Option<Vec<u8>>>,
    bytes: usize,
}
struct QueuedPacket {
    bytes: bytes::Bytes,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

struct PacketReader {
    receive: tokio::sync::mpsc::Receiver<QueuedPacket>,
    fragments: std::collections::HashMap<u16, Assembly>,
}
impl PacketReader {
    fn expire(&mut self) {
        self.fragments
            .retain(|_, f| f.created.elapsed() < Duration::from_secs(5));
    }
    fn accept(&mut self, fragment: Fragment) -> Option<(Target, Vec<u8>)> {
        if fragment.count == 1 {
            return Some((fragment.target?, fragment.payload));
        }
        if !self.fragments.contains_key(&fragment.id) && self.fragments.len() >= 8 {
            return None;
        }
        if let Some(assembly) = self.fragments.get(&fragment.id) {
            if assembly.parts.len() != fragment.count as usize {
                self.fragments.remove(&fragment.id);
                return None;
            }
            if let Some(existing) = &assembly.parts[fragment.index as usize] {
                if *existing != fragment.payload
                    || (fragment.index == 0 && assembly.target != fragment.target)
                {
                    self.fragments.remove(&fragment.id);
                }
                return None;
            }
        }
        let buffered: usize = self.fragments.values().map(|f| f.bytes).sum();
        if buffered + fragment.payload.len() > MAX_UDP {
            self.fragments.remove(&fragment.id);
            return None;
        }
        let assembly = self
            .fragments
            .entry(fragment.id)
            .or_insert_with(|| Assembly {
                created: std::time::Instant::now(),
                target: None,
                parts: vec![None; fragment.count as usize],
                bytes: 0,
            });
        let part = &mut assembly.parts[fragment.index as usize];
        assembly.bytes += fragment.payload.len();
        *part = Some(fragment.payload);
        if fragment.index == 0 {
            assembly.target = fragment.target;
        }
        if assembly.parts.iter().any(Option::is_none) {
            return None;
        }
        let assembly = self.fragments.remove(&fragment.id)?;
        let mut payload = Vec::with_capacity(assembly.bytes);
        for part in assembly.parts {
            payload.extend(part?);
        }
        Some((assembly.target?, payload))
    }
}

fn decode_packet(packet: &[u8]) -> Result<Fragment> {
    if packet.len() < 11 || packet[..2] != [5, 2] {
        bail!("invalid TUIC UDP command");
    }
    let (count, index) = (packet[6], packet[7]);
    if count == 0 || index >= count {
        bail!("invalid TUIC fragment number");
    }
    let size = u16::from_be_bytes([packet[8], packet[9]]) as usize;
    let mut cursor = io::Cursor::new(&packet[11..]);
    let host = match packet[10] {
        255 if index != 0 => None,
        1 => {
            let mut bytes = [0; 4];
            std::io::Read::read_exact(&mut cursor, &mut bytes)?;
            Some(std::net::Ipv4Addr::from(bytes).to_string())
        }
        2 => {
            let mut bytes = [0; 16];
            std::io::Read::read_exact(&mut cursor, &mut bytes)?;
            Some(std::net::Ipv6Addr::from(bytes).to_string())
        }
        0 => {
            let mut length = [0];
            std::io::Read::read_exact(&mut cursor, &mut length)?;
            if length[0] == 0 || length[0] > 253 {
                bail!("empty TUIC domain");
            }
            let mut host = vec![0; length[0] as usize];
            std::io::Read::read_exact(&mut cursor, &mut host)?;
            Some(String::from_utf8(host)?)
        }
        _ => bail!("invalid TUIC address"),
    };
    let target = if let Some(host) = host {
        if index != 0 {
            bail!("TUIC continuation fragment must omit address");
        }
        let mut port = [0; 2];
        std::io::Read::read_exact(&mut cursor, &mut port)?;
        let port = u16::from_be_bytes(port);
        if port == 0 {
            bail!("invalid TUIC port");
        }
        Some(Target { host, port })
    } else {
        None
    };
    let start = 11 + cursor.position() as usize;
    if packet.len() != start + size || size > MAX_UDP {
        bail!("invalid TUIC packet length");
    }
    Ok(Fragment {
        id: u16::from_be_bytes([packet[4], packet[5]]),
        count,
        index,
        target,
        payload: packet[start..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(id: u16, count: u8, index: u8, target: Option<&Target>, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![5, 2, 0, 0];
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(&[count, index]);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        if let Some(target) = target {
            encode_address(&mut frame, target).unwrap();
        } else {
            frame.push(255);
        }
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn fragmented_udp_reorders_and_rejects_conflicts_without_unbounded_buffers() {
        let (_, receive) = tokio::sync::mpsc::channel(1);
        let mut reader = PacketReader {
            receive,
            fragments: std::collections::HashMap::new(),
        };
        let target = Target {
            host: "example.test".into(),
            port: 53,
        };
        let first = fragment(7, 2, 0, Some(&target), b"hello");
        let last = fragment(7, 2, 1, None, b" world");
        assert!(reader.accept(decode_packet(&last).unwrap()).is_none());
        assert!(reader.accept(decode_packet(&last).unwrap()).is_none());
        assert_eq!(
            reader.accept(decode_packet(&first).unwrap()).unwrap(),
            (target.clone(), b"hello world".to_vec())
        );
        assert!(reader.fragments.is_empty());
        reader.accept(decode_packet(&first).unwrap());
        let conflict = fragment(7, 3, 1, None, b"invalid");
        assert!(reader.accept(decode_packet(&conflict).unwrap()).is_none());
        assert!(reader.fragments.is_empty());
        for id in 0..20 {
            reader.accept(decode_packet(&fragment(id, 2, 0, Some(&target), b"x")).unwrap());
        }
        assert_eq!(reader.fragments.len(), 8);
        for assembly in reader.fragments.values_mut() {
            assembly.created = std::time::Instant::now() - Duration::from_secs(6);
        }
        reader.expire();
        assert!(reader.fragments.is_empty());
        let large = vec![0; MAX_UDP];
        reader.accept(decode_packet(&fragment(1, 2, 0, Some(&target), &large)).unwrap());
        assert!(reader
            .accept(decode_packet(&fragment(1, 2, 1, None, b"x")).unwrap())
            .is_none());
        assert!(reader.fragments.is_empty());
        let full = fragment(9, 2, 0, Some(&target), &large);
        reader.accept(decode_packet(&full).unwrap());
        reader.accept(decode_packet(&full).unwrap());
        let completed = reader
            .accept(decode_packet(&fragment(9, 2, 1, None, b"")).unwrap())
            .unwrap();
        assert_eq!(completed.1.len(), MAX_UDP);
    }

    #[test]
    fn udp_framing_validates_lengths_and_addresses() {
        for host in ["example.test", "127.0.0.1", "::1"] {
            let target = Target {
                host: host.into(),
                port: 53,
            };
            let frame = fragment(3, 1, 0, Some(&target), b"");
            assert_eq!(decode_packet(&frame).unwrap().target, Some(target));
            for end in 0..frame.len() {
                assert!(decode_packet(&frame[..end]).is_err());
            }
            let mut malformed = frame.clone();
            malformed[6] = 0;
            assert!(decode_packet(&malformed).is_err());
            malformed = frame.clone();
            malformed.push(0);
            assert!(decode_packet(&malformed).is_err());
        }
    }
}

use crate::accept::secret;
use anyhow::ensure;
use tokio::io::AsyncReadExt;
pub async fn authenticate(conn: &quinn::Connection, users: &[crate::accept::User]) -> Result<()> {
    let mut stream = conn.accept_uni().await?;
    ensure!(
        stream.read_u8().await? == 5 && stream.read_u8().await? == 0,
        "TUIC authentication header"
    );
    let mut id = [0; 16];
    let mut token = [0; 32];
    stream.read_exact(&mut id).await?;
    stream.read_exact(&mut token).await?;
    let user = users
        .iter()
        .find(|u| u.uuid.is_some_and(|v| secret(v.as_bytes(), &id)))
        .context("TUIC authentication failed")?;
    let mut expected = [0; 32];
    conn.export_keying_material(
        &mut expected,
        &id,
        user.password.as_ref().unwrap().as_bytes(),
    )
    .map_err(|_| anyhow::anyhow!("TLS exporter"))?;
    ensure!(secret(&expected, &token), "TUIC authentication failed");
    Ok(())
}

pub async fn accept_tcp<R: tokio::io::AsyncRead + Unpin>(recv: &mut R) -> Result<Target> {
    use crate::wire;

    ensure!(
        recv.read_u8().await? == 5 && recv.read_u8().await? == 1,
        "TUIC CONNECT header"
    );
    wire::address(recv, 2, false).await
}

pub(crate) async fn decode_datagram(
    bytes: &[u8],
    fragments: &mut crate::datagram::Fragments,
) -> Result<Option<(u32, Target, Vec<u8>)>> {
    if bytes.starts_with(&[5, 4]) {
        return Ok(None);
    }
    let packet = decode_packet(bytes)?;
    let association = u32::from(u16::from_be_bytes([bytes[2], bytes[3]]));
    fragments.insert(
        association,
        packet.id,
        packet.count,
        packet.index,
        packet.target,
        &packet.payload,
    )
}
