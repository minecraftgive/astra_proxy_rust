//! Server-only REALITY authentication and TLS record boundaries for Vision.
use crate::{
    config::{Reality, Transport},
    net::{App, Stream},
    protocol::{authority_target, Inbound},
};
use aes_gcm::{
    aead::{AeadInPlace, KeyInit},
    Aes256Gcm, Nonce, Tag,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use sha2::{Sha256, Sha512};
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context as Cx, Poll},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio_rustls::rustls;

#[derive(Clone, Default)]
pub struct RawSwitch {
    pub read: Arc<AtomicBool>,
    pub write: Arc<AtomicBool>,
}
pub struct RealityState {
    private: [u8; 32],
    options: Reality,
    key: Vec<u8>,
    cert: Vec<u8>,
    public: Vec<u8>,
    replay: Arc<Mutex<HashMap<[u8; 32], Instant>>>,
}
impl RealityState {
    pub fn validate(options: &Reality) -> Result<()> {
        let key = URL_SAFE_NO_PAD.decode(&options.private_key)?;
        ensure!(
            key.len() == 32 && key.iter().any(|b| *b != 0),
            "REALITY private_key must encode 32 nonzero bytes"
        );
        ensure!(
            !options.server_names.is_empty()
                && options.server_names.len() <= 16
                && options
                    .server_names
                    .iter()
                    .all(|n| !n.is_empty() && n.len() <= 253),
            "REALITY server_names"
        );
        ensure!(
            !options.short_ids.is_empty() && options.short_ids.len() <= 256,
            "REALITY short_ids"
        );
        for id in &options.short_ids {
            ensure!(
                id.len() <= 16 && id.len() % 2 == 0,
                "REALITY short_id must contain 0..16 hex digits"
            );
            hex::decode(id)?;
        }
        ensure!(
            (1..=600).contains(&options.max_time_diff_secs),
            "REALITY time window must be 1..600 seconds"
        );
        authority_target(&options.target)?;
        Ok(())
    }
    pub fn new(options: Reality, previous: Option<&Self>) -> Result<Arc<Self>> {
        Self::validate(&options)?;
        let private = URL_SAFE_NO_PAD
            .decode(&options.private_key)?
            .try_into()
            .unwrap();
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
        let public = key.public_key_raw().to_vec();
        let cert = rcgen::CertificateParams::new(options.server_names.clone())?
            .self_signed(&key)?
            .der()
            .to_vec();
        Ok(Arc::new(Self {
            private,
            options,
            key: key.serialize_der(),
            cert,
            public,
            replay: previous.map(|p| p.replay.clone()).unwrap_or_default(),
        }))
    }
    fn authenticate(&self, hello: &[u8]) -> Result<Vec<u8>> {
        let (name, share) = parse_hello(hello)?;
        ensure!(
            self.options.server_names.iter().any(|v| v == &name),
            "REALITY server name"
        );
        let shared = x25519_dalek::StaticSecret::from(self.private)
            .diffie_hellman(&x25519_dalek::PublicKey::from(share));
        ensure!(shared.was_contributory(), "invalid key share");
        let mut key = [0; 32];
        hkdf::Hkdf::<Sha256>::new(Some(&hello[6..26]), shared.as_bytes())
            .expand(b"REALITY", &mut key)
            .map_err(|_| anyhow::anyhow!("HKDF"))?;
        let sid: [u8; 32] = hello[39..71].try_into().unwrap();
        let mut aad = hello.to_vec();
        aad[39..71].fill(0);
        let mut payload = sid[..16].to_vec();
        Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .decrypt_in_place_detached(
                Nonce::from_slice(&hello[26..38]),
                &aad,
                &mut payload,
                Tag::from_slice(&sid[16..]),
            )
            .map_err(|_| anyhow::anyhow!("REALITY authentication"))?;
        let stamp = u64::from(u32::from_be_bytes(payload[4..8].try_into().unwrap()));
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        ensure!(
            now.abs_diff(stamp) <= self.options.max_time_diff_secs,
            "REALITY timestamp"
        );
        ensure!(
            self.options.short_ids.iter().any(|value| {
                let bytes = hex::decode(value).unwrap();
                let mut id = [0; 8];
                id[..bytes.len()].copy_from_slice(&bytes);
                crate::protocol::secret(&id, &payload[8..16])
            }),
            "REALITY short_id"
        );
        {
            let mut replay = self.replay.lock().unwrap();
            replay.retain(|_, at| at.elapsed().as_secs() <= self.options.max_time_diff_secs * 2);
            ensure!(
                replay.len() < 65536 && !replay.contains_key(&sid),
                "REALITY replay"
            );
            replay.insert(sid, Instant::now());
        }
        let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(&key).unwrap();
        mac.update(&self.public);
        let signature = mac.finalize().into_bytes();
        let mut cert = self.cert.clone();
        let offset = cert
            .len()
            .checked_sub(64)
            .context("certificate signature")?;
        cert[offset..].copy_from_slice(&signature);
        Ok(cert)
    }
    fn tls(&self, certificate: Vec<u8>, alpn: Vec<Vec<u8>>) -> Result<Arc<rustls::ServerConfig>> {
        let mut provider = rustls::crypto::aws_lc_rs::default_provider();
        provider.kx_groups = vec![rustls::crypto::aws_lc_rs::kx_group::X25519];
        let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(RealityCertificate(Arc::new(
                rustls::sign::CertifiedKey::new(
                    vec![certificate.into()],
                    Arc::new(RealitySigningKey(
                        rustls::crypto::aws_lc_rs::sign::any_supported_type(
                            &rustls::pki_types::PrivatePkcs8KeyDer::from(self.key.clone()).into(),
                        )?,
                    )),
                ),
            ))));
        config.alpn_protocols = alpn;
        Ok(Arc::new(config))
    }
}

pub enum Accepted {
    Proxy(Stream, Option<RawSwitch>),
    Cover(Stream, tokio::net::TcpStream),
}
pub async fn accept(mut stream: Stream, inbound: &Inbound, app: Arc<App>) -> Result<Accepted> {
    let mut prefix = vec![];
    let config = if let Some(reality) = &inbound.reality {
        let mut hello = vec![];
        // Parse a bounded ClientHello, retaining every original record for fallback.
        loop {
            let mut header = [0; 5];
            stream.read_exact(&mut header).await?;
            ensure!(header[0] == 22 && header[1] == 3, "TLS ClientHello record");
            let n = u16::from_be_bytes([header[3], header[4]]) as usize;
            ensure!(
                n > 0 && n <= 18432 && prefix.len() + n + 5 <= 65536,
                "ClientHello size limit"
            );
            let mut body = vec![0; n];
            stream.read_exact(&mut body).await?;
            prefix.extend(header);
            prefix.extend(&body);
            hello.extend(body);
            if hello.len() >= 4 {
                let n = (usize::from(hello[1]) << 16)
                    | (usize::from(hello[2]) << 8)
                    | usize::from(hello[3]);
                ensure!(n <= 65532, "ClientHello size limit");
                if hello.len() >= n + 4 {
                    ensure!(hello.len() == n + 4, "unexpected extra handshake messages");
                    break;
                }
            }
        }
        match reality.authenticate(&hello) {
            Ok(cert) => {
                let alpn = if matches!(
                    inbound.options.transport,
                    Transport::Grpc | Transport::Xhttp
                ) {
                    vec![b"h2".to_vec()]
                } else {
                    vec![b"http/1.1".to_vec()]
                };
                Some(reality.tls(cert, alpn)?)
            }
            Err(_) => {
                let target = authority_target(&reality.options.target)?;
                let mut remote = app.connect(&target).await?;
                remote.write_all(&prefix).await?;
                return Ok(Accepted::Cover(stream, remote));
            }
        }
    } else {
        inbound.tls.clone()
    };
    if let Some(config) = config {
        let raw = RawSwitch::default();
        let io = RecordIo {
            inner: stream,
            prefix: std::io::Cursor::new(prefix),
            header: [0; 5],
            header_pos: 0,
            header_delivered: 0,
            left: 0,
        };
        let tls = tokio_rustls::TlsAcceptor::from(config).accept(io).await?;
        Ok(Accepted::Proxy(
            Box::new(SecurityStream {
                tls,
                raw: raw.clone(),
            }),
            Some(raw),
        ))
    } else {
        Ok(Accepted::Proxy(stream, None))
    }
}
fn take<'a>(data: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = at.checked_add(n).context("ClientHello overflow")?;
    ensure!(end <= data.len(), "truncated ClientHello");
    let out = &data[*at..end];
    *at = end;
    Ok(out)
}
fn number(data: &[u8], at: &mut usize) -> Result<usize> {
    let b = take(data, at, 2)?;
    Ok(usize::from(u16::from_be_bytes(b.try_into().unwrap())))
}
fn parse_hello(hello: &[u8]) -> Result<(String, [u8; 32])> {
    ensure!(
        hello.len() >= 75 && hello[0] == 1 && hello[38] == 32,
        "REALITY ClientHello format"
    );
    let mut at = 71;
    let n = number(hello, &mut at)?;
    take(hello, &mut at, n)?;
    let n = take(hello, &mut at, 1)?[0] as usize;
    take(hello, &mut at, n)?;
    let n = number(hello, &mut at)?;
    let extensions = take(hello, &mut at, n)?;
    ensure!(at == hello.len(), "ClientHello trailing data");
    let mut at = 0;
    let mut name = None;
    let mut share = None;
    while at < extensions.len() {
        let kind = number(extensions, &mut at)?;
        let n = number(extensions, &mut at)?;
        let value = take(extensions, &mut at, n)?;
        let mut i = 0;
        match kind {
            0 => {
                let n = number(value, &mut i)?;
                ensure!(
                    n == value.len() - 2 && take(value, &mut i, 1)? == [0],
                    "server name list"
                );
                let n = number(value, &mut i)?;
                let host = std::str::from_utf8(take(value, &mut i, n)?)?.to_string();
                ensure!(name.replace(host).is_none(), "duplicate server name");
            }
            51 => {
                let n = number(value, &mut i)?;
                ensure!(n == value.len() - 2, "key share list");
                while i < value.len() {
                    let group = number(value, &mut i)?;
                    let n = number(value, &mut i)?;
                    let key = take(value, &mut i, n)?;
                    if group == 29 {
                        ensure!(key.len() == 32 && share.is_none(), "X25519 key share");
                        share = Some(key.try_into().unwrap());
                    }
                }
            }
            _ => {}
        }
    }
    Ok((
        name.context("missing server name")?,
        share.context("missing X25519 key share")?,
    ))
}
/// Never read ahead into the first raw Vision record after an outer TLS record.
struct RecordIo {
    inner: Stream,
    prefix: std::io::Cursor<Vec<u8>>,
    header: [u8; 5],
    header_pos: usize,
    header_delivered: usize,
    left: usize,
}
impl AsyncRead for RecordIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.prefix.position() < (self.prefix.get_ref().len() as u64) {
            return Pin::new(&mut self.prefix).poll_read(cx, buf);
        }
        if self.header_pos < 5 {
            while self.header_pos < 5 {
                let this = &mut *self;
                let mut out = ReadBuf::new(&mut this.header[this.header_pos..]);
                std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, &mut out))?;
                let n = out.filled().len();
                if n == 0 {
                    return if self.header_pos == 0 {
                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()))
                    };
                }
                self.header_pos += n;
            }
            self.left = u16::from_be_bytes([self.header[3], self.header[4]]) as usize;
            if self.left > 18432 {
                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
            }
        }
        if self.header_delivered < 5 {
            let n = buf.remaining().min(5 - self.header_delivered);
            buf.put_slice(&self.header[self.header_delivered..self.header_delivered + n]);
            self.header_delivered += n;
            if self.header_delivered == 5 && self.left == 0 {
                self.header_pos = 0;
                self.header_delivered = 0;
            }
            return Poll::Ready(Ok(()));
        }
        let n = buf.remaining().min(self.left);
        let mut out = ReadBuf::new(buf.initialize_unfilled_to(n));
        std::task::ready!(Pin::new(&mut self.inner).poll_read(cx, &mut out))?;
        let n = out.filled().len();
        if n == 0 && self.left > 0 {
            return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
        }
        buf.advance(n);
        self.left -= n;
        if self.left == 0 {
            self.header_pos = 0;
            self.header_delivered = 0;
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for RecordIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Cx<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
struct SecurityStream {
    tls: tokio_rustls::server::TlsStream<RecordIo>,
    raw: RawSwitch,
}
impl AsyncRead for SecurityStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.raw.read.load(Ordering::Acquire) {
            Pin::new(&mut self.tls.get_mut().0.inner).poll_read(cx, b)
        } else {
            Pin::new(&mut self.tls).poll_read(cx, b)
        }
    }
}
impl AsyncWrite for SecurityStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Cx<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        if self.raw.write.load(Ordering::Acquire) {
            Pin::new(&mut self.tls.get_mut().0.inner).poll_write(cx, b)
        } else {
            Pin::new(&mut self.tls).poll_write(cx, b)
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        if self.raw.write.load(Ordering::Acquire) {
            Pin::new(&mut self.tls.get_mut().0.inner).poll_flush(cx)
        } else {
            Pin::new(&mut self.tls).poll_flush(cx)
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        if self.raw.write.load(Ordering::Acquire) {
            Pin::new(&mut self.tls.get_mut().0.inner).poll_shutdown(cx)
        } else {
            Pin::new(&mut self.tls).poll_shutdown(cx)
        }
    }
}

// REALITY deliberately uses Ed25519 even when a browser-shaped hello omits it,
// matching XTLS/REALITY's server handshake. Only authenticated hellos reach here.
#[derive(Debug)]
struct RealitySigningKey(Arc<dyn rustls::sign::SigningKey>);
impl rustls::sign::SigningKey for RealitySigningKey {
    fn choose_scheme(
        &self,
        _: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        self.0.choose_scheme(&[rustls::SignatureScheme::ED25519])
    }
    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        self.0.algorithm()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options() -> Reality {
        Reality {
            private_key: URL_SAFE_NO_PAD.encode([8; 32]),
            server_names: vec!["example.test".into()],
            short_ids: vec!["0102030405060708".into()],
            target: "example.test:443".into(),
            max_time_diff_secs: 60,
        }
    }
    fn hello(name: &str, short: [u8; 8], time: u64) -> Vec<u8> {
        let client = x25519_dalek::StaticSecret::from(rand::random::<[u8; 32]>());
        let public = x25519_dalek::PublicKey::from(&client);
        let server = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([8; 32]));
        let mut body = vec![1, 0, 0, 0, 3, 3];
        body.extend(rand::random::<[u8; 32]>());
        body.push(32);
        body.resize(71, 0);
        body.extend([0, 2, 0x13, 1, 1, 0]);
        let mut name_ext = vec![];
        name_ext.extend(((name.len() + 3) as u16).to_be_bytes());
        name_ext.push(0);
        name_ext.extend((name.len() as u16).to_be_bytes());
        name_ext.extend(name.as_bytes());
        let mut extensions = vec![0, 0];
        extensions.extend((name_ext.len() as u16).to_be_bytes());
        extensions.extend(name_ext);
        extensions.extend([0, 51, 0, 38, 0, 36, 0, 29, 0, 32]);
        extensions.extend(public.as_bytes());
        body.extend((extensions.len() as u16).to_be_bytes());
        body.extend(extensions);
        let len = body.len() - 4;
        body[1] = (len >> 16) as u8;
        body[2] = (len >> 8) as u8;
        body[3] = len as u8;
        let shared = client.diffie_hellman(&server);
        let mut key = [0; 32];
        hkdf::Hkdf::<Sha256>::new(Some(&body[6..26]), shared.as_bytes())
            .expand(b"REALITY", &mut key)
            .unwrap();
        let mut payload = vec![1, 0, 0, 0];
        payload.extend((time as u32).to_be_bytes());
        payload.extend(short);
        let tag = Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .encrypt_in_place_detached(Nonce::from_slice(&body[26..38]), &body, &mut payload)
            .unwrap();
        body[39..55].copy_from_slice(&payload);
        body[55..71].copy_from_slice(&tag);
        body
    }
    #[test]
    fn reality_authenticates_and_rejects_replay_time_sni_and_short_id() {
        let state = RealityState::new(options(), None).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let valid = hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now);
        assert!(state.authenticate(&valid).is_ok());
        assert!(state
            .authenticate(&valid)
            .unwrap_err()
            .to_string()
            .contains("replay"));
        let reloaded = RealityState::new(options(), Some(&state)).unwrap();
        assert!(reloaded.authenticate(&valid).is_err());
        assert!(state
            .authenticate(&hello("other.test", [1, 2, 3, 4, 5, 6, 7, 8], now))
            .is_err());
        assert!(state
            .authenticate(&hello("example.test", [9; 8], now))
            .is_err());
        assert!(state
            .authenticate(&hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now - 120))
            .is_err());
        for end in 0..valid.len() {
            assert!(parse_hello(&valid[..end]).is_err(), "truncation {end}");
        }
        let mut damaged = hello("example.test", [1, 2, 3, 4, 5, 6, 7, 8], now);
        damaged[39] ^= 1;
        assert!(state.authenticate(&damaged).is_err());
    }
    #[tokio::test]
    async fn record_gate_handles_tiny_reads_without_consuming_raw_successor() {
        let (mut input, inner) = tokio::io::duplex(128);
        let first = [23, 3, 3, 0, 3, 1, 2, 3];
        let raw = [23, 3, 3, 0, 2, 9, 8];
        input.write_all(&first).await.unwrap();
        input.write_all(&raw).await.unwrap();
        let mut gate = RecordIo {
            inner: Box::new(inner),
            prefix: std::io::Cursor::new(vec![]),
            header: [0; 5],
            header_pos: 0,
            header_delivered: 0,
            left: 0,
        };
        let mut received = vec![];
        for _ in 0..first.len() {
            received.push(gate.read_u8().await.unwrap());
        }
        assert_eq!(received, first);
        let mut next = [0; 7];
        gate.inner.read_exact(&mut next).await.unwrap();
        assert_eq!(next, raw);
    }
}
#[derive(Debug)]
struct RealityCertificate(Arc<rustls::sign::CertifiedKey>);
impl rustls::server::ResolvesServerCert for RealityCertificate {
    fn resolve(
        &self,
        _: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
}
