use crate::config::Config;
use anyhow::{bail, ensure, Context, Result};
use astra_model::Target;
use bytes::Bytes;
use futures_util::FutureExt;
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::{mpsc, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> Io for T {}
pub type Stream = Box<dyn Io>;
#[derive(Default)]
pub struct Stats {
    pub active: AtomicU64,
    pub accepted: AtomicU64,
    pub rejected: AtomicU64,
    pub upload: AtomicU64,
    pub download: AtomicU64,
}
pub struct App {
    pub config: Config,
    pub stats: Arc<Stats>,
    pub sessions: Arc<Semaphore>,
    pub udp: Arc<Semaphore>,
    pub bytes: Arc<Semaphore>,
}
impl App {
    pub fn new(
        config: Config,
        stats: Arc<Stats>,
        sessions: Arc<Semaphore>,
        udp: Arc<Semaphore>,
        bytes: Arc<Semaphore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            stats,
            sessions,
            udp,
            bytes,
        })
    }
    pub fn session(&self) -> Result<Session> {
        let permit = self
            .sessions
            .clone()
            .try_acquire_owned()
            .context("session limit reached")?;
        self.stats.active.fetch_add(1, Ordering::Relaxed);
        self.stats.accepted.fetch_add(1, Ordering::Relaxed);
        Ok(Session {
            stats: self.stats.clone(),
            _permit: permit,
        })
    }
    fn allowed(&self, ip: IpAddr) -> bool {
        if self.config.deny_cidrs.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        let ip = embedded_ipv4(ip);
        if self.config.deny_cidrs.iter().any(|net| net.contains(&ip)) {
            return false;
        }
        if self.config.allow_private_targets {
            return !ip.is_unspecified() && !ip.is_multicast();
        }
        public_destination(ip)
    }
    pub async fn resolve(&self, target: &Target) -> Result<Vec<SocketAddr>> {
        ensure!(
            !target.host.is_empty() && target.host.len() <= 253 && target.port > 0,
            "invalid destination"
        );
        let addresses = tokio::time::timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            tokio::net::lookup_host((target.host.as_str(), target.port)),
        )
        .await??;
        let addresses: Vec<_> = addresses
            .filter(|a| self.allowed(a.ip()))
            .take(16)
            .collect();
        ensure!(
            !addresses.is_empty(),
            "destination blocked by server egress policy"
        );
        Ok(addresses)
    }
    pub async fn connect(&self, target: &Target) -> Result<TcpStream> {
        tokio::time::timeout(
            Duration::from_secs(self.config.connect_timeout_secs),
            async {
                let mut error = None;
                for address in self.resolve(target).await? {
                    match TcpStream::connect(address).await {
                        Ok(s) => {
                            s.set_nodelay(true)?;
                            return Ok(s);
                        }
                        Err(e) => error = Some(e),
                    }
                }
                Err(error.context("no destination addresses")?.into())
            },
        )
        .await?
    }
    pub async fn datagram_socket(&self, target: &Target) -> Result<UdpSocket> {
        let address = self
            .resolve(target)
            .await?
            .into_iter()
            .next()
            .context("no UDP address")?;
        let s = UdpSocket::bind(if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        s.connect(address).await?;
        Ok(s)
    }
    pub fn packet(
        &self,
        association: u32,
        target: Target,
        payload: impl Into<Bytes>,
    ) -> Result<Packet> {
        let payload = payload.into();
        ensure!(payload.len() <= 65507, "UDP datagram exceeds 65507 bytes");
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(payload.len().max(1) as u32)
            .context("UDP buffer budget exhausted")?;
        Ok(Packet {
            association,
            target,
            payload,
            _budget: permit,
        })
    }
}

/// IPv6 forms that carry an IPv4 destination (mapped, IPv4-compatible, NAT64
/// well-known prefix, 6to4) are judged as that IPv4 address, so they cannot
/// be used to reach private or metadata IPv4 ranges.
fn embedded_ipv4(ip: IpAddr) -> IpAddr {
    let IpAddr::V6(v6) = ip else { return ip };
    if let Some(v4) = v6.to_ipv4_mapped() {
        return IpAddr::V4(v4);
    }
    let (s, o) = (v6.segments(), v6.octets());
    if s[..6] == [0; 6] || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if s[0] == 0x2002 {
        return IpAddr::V4(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    ip
}
fn public_destination(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && o[0] != 0
                && o[0] < 240
                // 100.64.0.0/10 shared/CGNAT space, which also hosts cloud
                // metadata endpoints such as Alibaba Cloud 100.100.100.200.
                && !(o[0] == 100 && (o[1] & 0xc0) == 64)
                // 192.0.0.0/24 IETF protocol assignments.
                && !(o[0] == 192 && o[1] == 0 && o[2] == 0)
                // 198.18.0.0/15 benchmarking, commonly used for Fake IP.
                && !(o[0] == 198 && (o[1] & 0xfe) == 18)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_unique_local()
                && !ip.is_unicast_link_local()
                // 64:ff9b:1::/48 local-use NAT64.
                && !(s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1)
        }
    }
}
pub struct Session {
    stats: Arc<Stats>,
    _permit: OwnedSemaphorePermit,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::Relaxed);
    }
}
pub async fn relay(client: Stream, remote: TcpStream, app: Arc<App>) -> Result<()> {
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut rr, mut rw) = remote.into_split();
    let idle = Duration::from_secs(app.config.idle_timeout_secs);
    let (activity, mut updates) = tokio::sync::watch::channel(tokio::time::Instant::now());
    let upload = async {
        let mut buf = [0; 16384];
        loop {
            let n = cr.read(&mut buf).await?;
            if n == 0 {
                rw.shutdown().await?;
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::timeout(idle, rw.write_all(&buf[..n])).await??;
            app.stats.upload.fetch_add(n as u64, Ordering::Relaxed);
            activity.send_replace(tokio::time::Instant::now());
        }
    };
    let download = download(&mut rr, &mut cw, &app.stats, &activity, idle);
    let watchdog = async {
        loop {
            let deadline = *updates.borrow_and_update() + idle;
            tokio::select! {_=tokio::time::sleep_until(deadline)=>bail!("TCP idle timeout"),r=updates.changed()=>{r?;}}
        }
    };
    tokio::select! {r=async{tokio::try_join!(upload,download)?;Ok(())}=>r,r=watchdog=>r}
}

async fn download<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    stats: &Stats,
    activity: &tokio::sync::watch::Sender<tokio::time::Instant>,
    idle: Duration,
) -> Result<()> {
    let mut buf = [0; 16384];
    let mut next = None;
    let mut unflushed = 0;
    let mut deadline = None;
    loop {
        let n = match next.take() {
            Some(result) => result?,
            None => reader.read(&mut buf).await?,
        };
        if n == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        let batch_deadline = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + idle);
        tokio::time::timeout_at(batch_deadline, async {
            writer.write_all(&buf[..n]).await?;
            unflushed += n;
            if n == buf.len() && unflushed < 64 * 1024 {
                next = reader.read(&mut buf).now_or_never();
                if matches!(next, Some(Ok(count)) if count > 0) {
                    return Ok::<(), std::io::Error>(());
                }
            }
            writer.flush().await?;
            stats
                .download
                .fetch_add(unflushed as u64, Ordering::Relaxed);
            activity.send_replace(tokio::time::Instant::now());
            unflushed = 0;
            deadline = None;
            Ok(())
        })
        .await??;
    }
}

pub struct Packet {
    pub association: u32,
    pub target: Target,
    pub payload: Bytes,
    _budget: OwnedSemaphorePermit,
}
pub struct Hub {
    pub send: mpsc::Sender<Packet>,
    pub receive: mpsc::Receiver<Packet>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Hub {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Hub {
    pub fn new(app: Arc<App>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Packet>(32);
        let (reply, receive) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let mut flows = HashMap::<(u32, String, u16), mpsc::Sender<Packet>>::new();
            let mut tasks = JoinSet::new();
            let mut cleanup = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    Some(packet)=rx.recv()=>{
                        flows.retain(|_,tx|!tx.is_closed());let key=(packet.association,packet.target.host.clone(),packet.target.port);
                        if let Some(tx)=flows.get(&key){let _=tx.try_send(packet);continue}
                        if flows.len()>=64{continue}let Ok(permit)=app.udp.clone().try_acquire_owned()else{continue};
                        let(tx,mut input)=mpsc::channel(4);let _=tx.try_send(packet);flows.insert(key.clone(),tx);let app=app.clone();let reply=reply.clone();
                        tasks.spawn(async move{let _permit=permit;let target=Target{host:key.1,port:key.2};let Ok(socket)=app.datagram_socket(&target).await else{return};let mut buf=vec![0u8;65536];
                            loop{tokio::select!{
                                packet=input.recv()=>{let Some(packet)=packet else{break};if socket.send(&packet.payload).await.is_err(){break}app.stats.upload.fetch_add(packet.payload.len() as u64,Ordering::Relaxed);},
                                result=socket.recv(&mut buf)=>{let Ok(n)=result else{break};let Ok(peer)=socket.peer_addr()else{break};let Ok(packet)=app.packet(key.0,Target{host:peer.ip().to_string(),port:peer.port()},buf[..n].to_vec())else{continue};app.stats.download.fetch_add(n as u64,Ordering::Relaxed);if reply.send(packet).await.is_err(){break}},
                                _=tokio::time::sleep(Duration::from_secs(app.config.idle_timeout_secs))=>break,
                            }}
                        });
                    },
                    _=cleanup.tick()=>flows.retain(|_,tx|!tx.is_closed()),
                    Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
                    else=>break,
                }
            }
        });
        Self {
            send: tx,
            receive,
            task,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::Future,
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    #[derive(Default)]
    struct FlushWriter {
        bytes: Vec<u8>,
        flushes: usize,
        max_write: Option<usize>,
        fail_flush: bool,
    }
    impl AsyncWrite for FlushWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let n = self.max_write.unwrap_or(bytes.len()).min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..n]);
            Poll::Ready(Ok(n))
        }
        fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.flushes += 1;
            Poll::Ready(if self.fail_flush {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "flush failed"))
            } else {
                Ok(())
            })
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct SlowWriter {
        timer: Option<Pin<Box<tokio::time::Sleep>>>,
        bytes: usize,
    }
    impl AsyncWrite for SlowWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let timer = self
                .timer
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(Duration::from_millis(250))));
            if timer.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.timer = None;
            self.bytes += bytes.len();
            Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn download_batches_ready_blocks_and_flushes_at_eof() {
        for size in [16384usize, 16384 * 8, 16384 * 8 + 3] {
            let payload: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut writer = FlushWriter {
                max_write: Some(113),
                ..Default::default()
            };
            let stats = Stats::default();
            let (activity, _) = tokio::sync::watch::channel(tokio::time::Instant::now());
            download(
                &mut payload.as_slice(),
                &mut writer,
                &stats,
                &activity,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(writer.bytes, payload);
            assert_eq!(writer.flushes, size.div_ceil(64 * 1024));
            assert_eq!(stats.download.load(Ordering::Relaxed), size as u64);
        }
    }

    #[tokio::test]
    async fn download_does_not_count_a_batch_when_flush_fails() {
        let payload = vec![37; 16384 * 3];
        let mut writer = FlushWriter {
            max_write: Some(113),
            fail_flush: true,
            ..Default::default()
        };
        let stats = Stats::default();
        let initial = tokio::time::Instant::now();
        let (activity, updates) = tokio::sync::watch::channel(initial);
        let error = download(
            &mut payload.as_slice(),
            &mut writer,
            &stats,
            &activity,
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(writer.bytes, payload);
        assert_eq!(writer.flushes, 1);
        assert_eq!(stats.download.load(Ordering::Relaxed), 0);
        assert_eq!(*updates.borrow(), initial);
        assert!(!updates.has_changed().unwrap());
    }

    #[tokio::test]
    async fn download_keeps_the_first_write_deadline_for_a_batch() {
        let payload = vec![37; 16384 * 3];
        let mut writer = SlowWriter {
            timer: None,
            bytes: 0,
        };
        let stats = Stats::default();
        let initial = tokio::time::Instant::now();
        let (activity, updates) = tokio::sync::watch::channel(initial);
        let error = download(
            &mut payload.as_slice(),
            &mut writer,
            &stats,
            &activity,
            Duration::from_millis(400),
        )
        .await
        .unwrap_err();
        assert!(error.is::<tokio::time::error::Elapsed>());
        assert!(writer.bytes >= 16384 && writer.bytes < payload.len());
        assert_eq!(stats.download.load(Ordering::Relaxed), 0);
        assert_eq!(*updates.borrow(), initial);
        assert!(!updates.has_changed().unwrap());
    }

    #[tokio::test]
    async fn download_flushes_full_and_short_responses_before_waiting_for_more() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut source, mut reader) = tokio::io::duplex(32768);
            let (sink, mut client) = tokio::io::duplex(32768);
            let task = tokio::spawn(async move {
                let mut writer = tokio::io::BufWriter::with_capacity(65536, sink);
                let stats = Stats::default();
                let (activity, _) = tokio::sync::watch::channel(tokio::time::Instant::now());
                download(
                    &mut reader,
                    &mut writer,
                    &stats,
                    &activity,
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
                stats.download.load(Ordering::Relaxed)
            });
            for size in [16384, 7, 16384] {
                let payload = vec![61; size];
                source.write_all(&payload).await.unwrap();
                let mut response = vec![0; size];
                client.read_exact(&mut response).await.unwrap();
                assert_eq!(response, payload);
            }
            source.shutdown().await.unwrap();
            assert_eq!(task.await.unwrap(), 32775);
        })
        .await
        .unwrap();
    }

    fn app(config: Config) -> Arc<App> {
        App::new(
            config,
            Arc::default(),
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(1)),
            Arc::new(Semaphore::new(16)),
        )
    }
    #[test]
    fn egress_policy_handles_mapped_addresses_and_explicit_denials() {
        let a = app(Config::default());
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "::1",
            "::ffff:127.0.0.1",
            "fe80::1",
            "fd00::1",
            "224.0.0.1",
            "255.255.255.255",
            "100.100.100.200",
            "100.64.0.1",
            "198.18.0.1",
            "192.0.0.1",
            "::127.0.0.1",
            "64:ff9b::a00:1",
            "64:ff9b::6464:64c8",
            "2002:a00:1::1",
            "2002:7f00:1::1",
        ] {
            assert!(!a.allowed(ip.parse().unwrap()), "{ip}");
        }
        assert!(a.allowed("8.8.8.8".parse().unwrap()));
        assert!(a.allowed("64:ff9b::808:808".parse().unwrap()));
        assert!(a.allowed("2001:4860:4860::8888".parse().unwrap()));
        let c = Config {
            allow_private_targets: true,
            deny_cidrs: vec!["127.0.0.0/8".parse().unwrap()],
            ..Default::default()
        };
        let a = app(c);
        assert!(a.allowed("10.0.0.1".parse().unwrap()));
        assert!(!a.allowed("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!a.allowed("0.0.0.0".parse().unwrap()));
    }
    #[test]
    fn shared_limits_release_on_drop() {
        let a = app(Config::default());
        let session = a.session().unwrap();
        assert!(a.session().is_err());
        drop(session);
        assert!(a.session().is_ok());
        let p = a
            .packet(
                0,
                Target {
                    host: "example.test".into(),
                    port: 53,
                },
                vec![0; 16],
            )
            .unwrap();
        assert!(a.packet(0, p.target.clone(), vec![1]).is_err());
        drop(p);
        assert_eq!(a.bytes.available_permits(), 16);
    }
}
