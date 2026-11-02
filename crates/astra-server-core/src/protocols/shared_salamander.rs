//! Server-only QUIC socket adaptation. No external processes or UDP forwarder.
use blake2::{
    digest::{Update, VariableOutput},
    Blake2bVar,
};
use quinn::{AsyncUdpSocket, UdpPoller};
use std::{
    fmt,
    io::{self, IoSliceMut},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
pub struct Socket {
    inner: Arc<dyn AsyncUdpSocket>,
    password: Vec<u8>,
}
impl fmt::Debug for Socket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SalamanderServerSocket")
            .finish_non_exhaustive()
    }
}
impl Socket {
    pub fn new(inner: Arc<dyn AsyncUdpSocket>, password: &str) -> Arc<Self> {
        Arc::new(Self {
            inner,
            password: password.as_bytes().to_vec(),
        })
    }
    fn key(&self, salt: &[u8]) -> [u8; 32] {
        let mut hash = Blake2bVar::new(32).unwrap();
        hash.update(&self.password);
        hash.update(salt);
        let mut key = [0; 32];
        hash.finalize_variable(&mut key).unwrap();
        key
    }
}
impl AsyncUdpSocket for Socket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &quinn::udp::Transmit) -> io::Result<()> {
        let salt: [u8; 8] = rand::random();
        let key = self.key(&salt);
        let mut data = Vec::with_capacity(transmit.contents.len() + 8);
        data.extend(salt);
        data.extend(
            transmit
                .contents
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ key[i % 32]),
        );
        self.inner.try_send(&quinn::udp::Transmit {
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents: &data,
            segment_size: None,
            src_ip: transmit.src_ip,
        })
    }
    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [quinn::udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let n = std::task::ready!(self.inner.poll_recv(cx, bufs, meta))?;
        for (buf, info) in bufs.iter_mut().zip(meta.iter_mut()).take(n) {
            if info.len <= 8 {
                info.len = 0;
                continue;
            }
            let stride = info.stride.max(1);
            let mut read = 0;
            let mut written = 0;
            while read < info.len {
                let end = (read + stride).min(info.len);
                if end - read > 8 {
                    let key = self.key(&buf[read..read + 8]);
                    for i in read + 8..end {
                        buf[written] = buf[i] ^ key[(i - read - 8) % 32];
                        written += 1;
                    }
                }
                read = end;
            }
            info.len = written;
            info.stride = stride.saturating_sub(8).max(1);
        }
        Poll::Ready(Ok(n))
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
