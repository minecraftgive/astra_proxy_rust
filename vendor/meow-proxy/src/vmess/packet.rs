//! One VMess body record per datagram; never relay packets through a byte stream.
use super::{body::BodyCipher, header, Security};
use async_trait::async_trait;
use meow_common::{MeowError, ProxyPacketConn, Result};
use meow_transport::Stream;
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::{
    io::{ReadHalf, WriteHalf},
    sync::Mutex,
};

type Transport = Box<dyn Stream>;
struct Reader {
    stream: ReadHalf<Transport>,
    cipher: BodyCipher,
    header: Option<([u8; 16], [u8; 16], u8)>,
    packet: Vec<u8>,
}
struct Writer {
    stream: WriteHalf<Transport>,
    cipher: BodyCipher,
}
pub(super) struct VmessPacketConn {
    reader: Mutex<Reader>,
    writer: Mutex<Writer>,
    poisoned: AtomicBool,
    security: Security,
}
impl VmessPacketConn {
    pub fn new(stream: Transport, security: Security, sealed: &header::SealedHeader) -> Self {
        let (reader, writer) = tokio::io::split(stream);
        let (key, iv) = header::response_body_keys(&sealed.req_key, &sealed.req_iv);
        Self {
            reader: Mutex::new(Reader {
                stream: reader,
                cipher: BodyCipher::from_response_keys(security, &key, &iv),
                header: Some((key, iv, sealed.resp_v)),
                packet: Vec::new(),
            }),
            writer: Mutex::new(Writer {
                stream: writer,
                cipher: BodyCipher::new_writer(security, &sealed.req_key, &sealed.req_iv),
            }),
            poisoned: AtomicBool::new(false),
            security,
        }
    }
}
#[async_trait]
impl ProxyPacketConn for VmessPacketConn {
    async fn write_packet(&self, buf: &[u8], _addr: &SocketAddr) -> Result<usize> {
        crate::check_not_desynced(&self.poisoned)?;
        // A zero-length plaintext record is the terminator in security=none.
        // Encrypted empty datagrams have a nonzero authentication-tag length.
        if buf.len() > 65507 || (buf.is_empty() && matches!(self.security, Security::None)) {
            return Err(MeowError::Proxy(
                "vmess UDP: unsupported datagram length".into(),
            ));
        }
        let mut state = self.writer.lock().await;
        crate::check_not_desynced(&self.poisoned)?;
        let mut guard = crate::PoisonOnIncomplete::new(&self.poisoned);
        let Writer { stream, cipher } = &mut *state;
        cipher
            .write_record(stream, buf)
            .await
            .map_err(MeowError::Io)?;
        guard.complete = true;
        Ok(buf.len())
    }
    async fn read_packet(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        crate::check_not_desynced(&self.poisoned)?;
        let mut state = self.reader.lock().await;
        crate::check_not_desynced(&self.poisoned)?;
        // Codec reads are not resumable after cancellation. Retire the session
        // rather than treating the remaining ciphertext as another packet.
        let mut guard = crate::PoisonOnIncomplete::new(&self.poisoned);
        let Reader {
            stream,
            cipher,
            header,
            packet,
        } = &mut *state;
        if let Some((key, iv, version)) = header.take() {
            header::read_aead_response_header(stream, &key, &iv, version)
                .await
                .map_err(MeowError::Io)?;
        }
        if cipher
            .read_record_into(stream, packet)
            .await
            .map_err(MeowError::Io)?
            .is_none()
        {
            return Err(MeowError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "vmess UDP closed",
            )));
        }
        guard.complete = true;
        if packet.len() > buf.len() {
            return Err(MeowError::Proxy(
                "vmess UDP: packet exceeds receive buffer".into(),
            ));
        }
        buf[..packet.len()].copy_from_slice(packet);
        // The request binds the destination; body records have no addresses.
        Ok((packet.len(), SocketAddr::from(([0, 0, 0, 0], 0))))
    }
    fn local_addr(&self) -> Result<SocketAddr> {
        Err(MeowError::NotSupported(
            "VMess UDP uses a stream transport".into(),
        ))
    }
    fn close(&self) -> Result<()> {
        self.poisoned.store(true, Ordering::Relaxed);
        // The owning packet connection drops both transport halves.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn pair(
        security: Security,
        capacity: usize,
    ) -> (
        VmessPacketConn,
        tokio::io::DuplexStream,
        header::SealedHeader,
    ) {
        let sealed = header::seal_request_header(
            &[1; 16],
            security,
            &meow_common::Metadata {
                host: "example.com".into(),
                dst_port: 53,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        let (client, peer) = tokio::io::duplex(capacity);
        (
            VmessPacketConn::new(Box::new(client), security, &sealed),
            peer,
            sealed,
        )
    }

    #[tokio::test]
    async fn client_first_packets_keep_boundaries_and_drain_short_buffers() {
        for security in [Security::Aes128Gcm, Security::ChaCha20Poly1305] {
            let (conn, mut peer, sealed) = pair(security, 64);
            let server = tokio::spawn(async move {
                let mut cipher =
                    BodyCipher::server_mirror(security, &sealed.req_key, &sealed.req_iv);
                // Wait for the client first, as actual UDP echo services do.
                let first = cipher.read_record(&mut peer).await.unwrap().unwrap();
                assert_eq!(first, b"first");
                let response = header::tests::seal_response_header(
                    &sealed.req_key,
                    &sealed.req_iv,
                    sealed.resp_v,
                );
                for chunk in response.chunks(3) {
                    peer.write_all(chunk).await.unwrap();
                }
                for data in [vec![42; 8192], vec![], b"last".to_vec()] {
                    cipher.write_record(&mut peer, &data).await.unwrap();
                }
            });
            tokio::time::timeout(Duration::from_secs(3), async {
                conn.write_packet(b"first", &"127.0.0.1:53".parse().unwrap())
                    .await
                    .unwrap();
                let mut short = [0; 4];
                assert!(conn
                    .read_packet(&mut short)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("receive buffer"));
                assert_eq!(conn.read_packet(&mut short).await.unwrap().0, 0);
                assert_eq!(conn.read_packet(&mut short).await.unwrap().0, 4);
                assert_eq!(&short, b"last");
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn canceled_partial_header_and_write_retire_session() {
        let (conn, mut peer, _) = pair(Security::Aes128Gcm, 8);
        peer.write_all(&[1]).await.unwrap();
        let mut packet = [0; 64];
        assert!(
            tokio::time::timeout(Duration::from_millis(20), conn.read_packet(&mut packet))
                .await
                .is_err()
        );
        let addr = "127.0.0.1:53".parse().unwrap();
        assert!(conn.write_packet(b"next", &addr).await.is_err());
        assert!(conn.read_packet(&mut packet).await.is_err());
        drop(conn);
        assert_eq!(peer.read(&mut packet).await.unwrap(), 0);

        let (conn, mut peer, _) = pair(Security::ChaCha20Poly1305, 8);
        // A partially written frame cannot be resumed by a new packet.
        assert!(tokio::time::timeout(
            Duration::from_millis(20),
            conn.write_packet(&[42; 100], &addr)
        )
        .await
        .is_err());
        assert!(conn.write_packet(b"next", &addr).await.is_err());
        drop(conn);
        let mut written = Vec::new();
        peer.read_to_end(&mut written).await.unwrap();
        assert_eq!(written.len(), 8);
    }
}
