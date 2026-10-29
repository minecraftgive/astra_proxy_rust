use crate::config::Config;
use anyhow::{bail, ensure, Context, Result};
use astra_ipc::Target;
use bytes::Bytes;
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
    pub fn packet(&self, association: u32, target: Target, payload: Vec<u8>) -> Result<Packet> {
        ensure!(payload.len() <= 65507, "UDP datagram exceeds 65507 bytes");
        let permit = self
            .bytes
            .clone()
            .try_acquire_many_owned(payload.len().max(1) as u32)
            .context("UDP buffer budget exhausted")?;
        Ok(Packet {
            association,
            target,
            payload: payload.into(),
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
#[cfg(test)]
mod tests {
    use super::*;
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
        let mut c = Config::default();
        c.allow_private_targets = true;
        c.deny_cidrs = vec!["127.0.0.0/8".parse().unwrap()];
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
    let download = async {
        let mut buf = [0; 16384];
        loop {
            let n = rr.read(&mut buf).await?;
            if n == 0 {
                cw.shutdown().await?;
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::timeout(idle, async {
                cw.write_all(&buf[..n]).await?;
                cw.flush().await
            })
            .await??;
            app.stats.download.fetch_add(n as u64, Ordering::Relaxed);
            activity.send_replace(tokio::time::Instant::now());
        }
    };
    let watchdog = async {
        loop {
            let deadline = *updates.borrow_and_update() + idle;
            tokio::select! {_=tokio::time::sleep_until(deadline)=>bail!("TCP idle timeout"),r=updates.changed()=>{r?;}}
        }
    };
    tokio::select! {r=async{tokio::try_join!(upload,download)?;Ok(())}=>r,r=watchdog=>r}
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
