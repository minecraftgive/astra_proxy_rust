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
#[path = "../test/vmess_packet_test.rs"]
mod tests;
