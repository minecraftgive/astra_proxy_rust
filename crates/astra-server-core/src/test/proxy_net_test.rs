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
