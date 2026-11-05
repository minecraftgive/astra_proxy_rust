use super::*;

#[test]
fn defaults_match_upstream() {
    let cfg = parse_opts("").unwrap();
    assert_eq!(cfg.key, "it's a secrect");
    assert_eq!(cfg.crypt, "aes");
    assert_eq!(cfg.mode, "fast");
    assert_eq!(cfg.conn, 1);
    assert_eq!(cfg.scavenge_ttl, 600);
    assert_eq!(cfg.mtu, 1350);
    assert_eq!(cfg.snd_wnd, 128);
    assert_eq!(cfg.rcv_wnd, 512);
    assert_eq!(cfg.data_shard, 10);
    assert_eq!(cfg.parity_shard, 3);
    assert_eq!(cfg.sock_buf, 4194304);
    assert_eq!(cfg.smux_ver, 1);
    assert_eq!(cfg.smux_buf, 4194304);
    assert_eq!(cfg.frame_size, 8192);
    assert_eq!(cfg.stream_buf, 2097152);
    assert_eq!(cfg.keep_alive, 10);
    // fast preset
    assert_eq!(
        (cfg.nodelay, cfg.interval, cfg.resend, cfg.nc),
        (0, 30, 2, 1)
    );
}

#[test]
fn manual_mode_keeps_knobs() {
    let cfg = parse_opts("mode=manual;nodelay=1;interval=20;resend=5;nc=0").unwrap();
    assert_eq!(
        (cfg.nodelay, cfg.interval, cfg.resend, cfg.nc),
        (1, 20, 5, 0)
    );
    // interval is clamped into upstream's [10, 5000] even under manual
    let cfg = parse_opts("mode=manual;interval=7").unwrap();
    assert_eq!(cfg.interval, 10);
    let cfg = parse_opts("mode=manual;interval=9999").unwrap();
    assert_eq!(cfg.interval, 5000);
    // preset modes clobber them, matching upstream FillDefaults order
    let cfg = parse_opts("nodelay=1;interval=7;mode=fast3").unwrap();
    assert_eq!(
        (cfg.nodelay, cfg.interval, cfg.resend, cfg.nc),
        (1, 10, 2, 1)
    );
    // manual with unset knobs inherits the zero-state fills
    // (`interval`→50, `resend`/`nc` stay 0), not the `fast` preset —
    // upstream fills a zeroed Config once.
    let cfg = parse_opts("mode=manual").unwrap();
    assert_eq!(
        (cfg.nodelay, cfg.interval, cfg.resend, cfg.nc),
        (0, 50, 0, 0)
    );
}

#[test]
fn rejects_bad_numbers_and_smuxver() {
    assert!(parse_opts("conn=notanumber").is_err());
    assert!(parse_opts("conn=99999").is_err()); // u16 overflow
    assert!(parse_opts("smuxver=2").is_err());
    assert!(parse_opts("dscp=256").is_err());
    // `sm4` must fail loudly — silently substituting AES guarantees a
    // dead session against a real sm4 peer.
    assert!(parse_opts("crypt=sm4").is_err());
    // Reed-Solomon shard counts that would overflow a usize add must
    // still parse (the guard is a saturating warn, not a panic).
    assert!(parse_opts(&format!("datashard={};parityshard=1", usize::MAX)).is_ok());
}

#[test]
fn bools_and_unknowns() {
    let cfg = parse_opts("nocomp=true;bogus=1").unwrap();
    assert!(cfg.no_comp);
    let cfg = parse_opts("nocomp=yes").unwrap();
    assert!(cfg.no_comp);
}

// Session-pool tests — a scripted `SocketIo` is enough to keep a smux
// session alive (or kill it) without any wire traffic.

use meow_transport::kcptun::SocketIo;
use std::io;
use std::task::{Context, Poll};
use tokio::io::ReadBuf;

/// Swallows outbound datagrams, never receives — a live-but-silent
/// endpoint the smux session's reader parks on.
struct SilentSocket;
impl SocketIo for SilentSocket {
    fn poll_send(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_recv(&mut self, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

/// `poll_recv` errors immediately — the smux reader exits on the first
/// poll and the session is marked dead.
struct FailSocket;
impl SocketIo for FailSocket {
    fn poll_send(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_recv(&mut self, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::other("fail")))
    }
}

struct SilentDialer;
#[async_trait]
impl TcpDialer for SilentDialer {
    async fn dial(&self, _: &str, _: u16, _: bool) -> io::Result<Box<dyn Stream>> {
        Err(io::Error::other("unused in pool tests"))
    }
    async fn dial_udp_endpoint(&self, _: SocketAddr) -> io::Result<Box<dyn SocketIo>> {
        Ok(Box::new(SilentSocket))
    }
}

/// Fails once then serves `SilentSocket`s — used to observe dead-session
/// eviction and redial.
struct FailOnceDialer(AtomicUsize);
#[async_trait]
impl TcpDialer for FailOnceDialer {
    async fn dial(&self, _: &str, _: u16, _: bool) -> io::Result<Box<dyn Stream>> {
        Err(io::Error::other("unused in pool tests"))
    }
    async fn dial_udp_endpoint(&self, _: SocketAddr) -> io::Result<Box<dyn SocketIo>> {
        if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            Ok(Box::new(FailSocket))
        } else {
            Ok(Box::new(SilentSocket))
        }
    }
}

fn client(opts: &str, dialer: Arc<dyn TcpDialer>) -> KcptunClient {
    // A literal IP skips DNS entirely; the fake dialer never touches it.
    KcptunClient::new(parse_opts(opts).unwrap(), "127.0.0.1", 8388, dialer)
}

#[tokio::test]
async fn pool_grows_to_conn_then_round_robins() {
    let c = client("conn=2;nocomp=true", Arc::new(SilentDialer));
    let a = c.pick().await.unwrap();
    let b = c.pick().await.unwrap();
    assert!(!Arc::ptr_eq(&a, &b));
    // Pool is full — subsequent picks rotate over the pooled sessions.
    assert!(Arc::ptr_eq(&c.pick().await.unwrap(), &a));
    assert!(Arc::ptr_eq(&c.pick().await.unwrap(), &b));
}

#[tokio::test]
async fn dead_session_is_evicted_and_redialed() {
    let c = client(
        "conn=1;nocomp=true",
        Arc::new(FailOnceDialer(AtomicUsize::new(0))),
    );
    let dead = c.pick().await.unwrap();
    // Let the smux reader task observe the socket error and mark dead —
    // poll for it instead of racing a fixed sleep on a loaded runner.
    for _ in 0..50 {
        if dead.is_dead() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(dead.is_dead());
    let live = c.pick().await.unwrap();
    assert!(!Arc::ptr_eq(&dead, &live));
    assert!(!live.is_dead());
    assert_eq!(c.pool.lock().len(), 1);
}

#[tokio::test]
async fn autoexpire_rotates_session() {
    let c = client("conn=1;nocomp=true;autoexpire=1", Arc::new(SilentDialer));
    let first = c.pick().await.unwrap();
    assert!(Arc::ptr_eq(&c.pick().await.unwrap(), &first));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let second = c.pick().await.unwrap();
    assert!(!Arc::ptr_eq(&first, &second));
}

/// Silent socket that records its own drop — i.e. its session's
/// teardown.
struct DropFlagSocket(Arc<std::sync::atomic::AtomicBool>);
impl SocketIo for DropFlagSocket {
    fn poll_send(&mut self, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_recv(&mut self, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}
impl Drop for DropFlagSocket {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Hands out [`DropFlagSocket`]s; the first dial parks on `gate`.
struct GatedDialer {
    gate: tokio::sync::Notify,
    dropped: Mutex<Vec<Arc<std::sync::atomic::AtomicBool>>>,
}
#[async_trait]
impl TcpDialer for GatedDialer {
    async fn dial(&self, _: &str, _: u16, _: bool) -> io::Result<Box<dyn Stream>> {
        Err(io::Error::other("unused in pool tests"))
    }
    async fn dial_udp_endpoint(&self, _: SocketAddr) -> io::Result<Box<dyn SocketIo>> {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first = {
            let mut dropped = self.dropped.lock();
            dropped.push(Arc::clone(&flag));
            dropped.len() == 1
        };
        if first {
            self.gate.notified().await;
        }
        Ok(Box::new(DropFlagSocket(flag)))
    }
}

async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Issue #695: `reset` closes every pooled session (their sockets may
/// predate a new outbound binding) and the next stream dials afresh.
#[tokio::test]
async fn reset_closes_pooled_sessions_and_redials() {
    let c = client("conn=2;nocomp=true", Arc::new(SilentDialer));
    let a = c.pick().await.unwrap();
    let b = c.pick().await.unwrap();
    assert_eq!(c.reset(), 2);
    assert!(a.is_dead() && b.is_dead(), "pooled sessions are closed");
    assert!(c.pool.lock().is_empty());
    let fresh = c.pick().await.unwrap();
    assert!(!fresh.is_dead());
    assert!(!Arc::ptr_eq(&fresh, &a) && !Arc::ptr_eq(&fresh, &b));
    assert_eq!(c.pool.lock().len(), 1);
    assert_eq!(c.reset(), 1);
}

/// Issue #695: a session whose dial straddles a reset is closed rather
/// than pooled or handed out, and the pick redials once.
#[tokio::test]
async fn reset_discards_a_session_dialled_across_it() {
    let dialer = Arc::new(GatedDialer {
        gate: tokio::sync::Notify::new(),
        dropped: Mutex::new(Vec::new()),
    });
    let c = Arc::new(client(
        "conn=1;nocomp=true",
        Arc::clone(&dialer) as Arc<dyn TcpDialer>,
    ));
    let pick = tokio::spawn({
        let c = Arc::clone(&c);
        async move { c.pick().await }
    });
    wait_until("the first dial", || dialer.dropped.lock().len() == 1).await;
    assert_eq!(c.reset(), 0, "nothing pooled yet");
    dialer.gate.notify_one();
    let session = pick.await.unwrap().unwrap();
    assert!(!session.is_dead());
    let flags = dialer.dropped.lock().clone();
    assert_eq!(flags.len(), 2, "the straddling dial is redialled");
    wait_until("the stale session's teardown", || {
        flags[0].load(Ordering::SeqCst)
    })
    .await;
    assert!(!flags[1].load(Ordering::SeqCst));
    assert_eq!(c.pool.lock().len(), 1);
}
