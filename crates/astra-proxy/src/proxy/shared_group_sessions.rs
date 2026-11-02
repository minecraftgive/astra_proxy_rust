//! Per-group epochs are shared across configuration generations. Only connections
//! whose resolved path contains a switched group observe its cancellation.
use std::{collections::HashMap, sync::Mutex};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::watch,
};

#[derive(Default)]
pub(crate) struct Sessions(Mutex<HashMap<String, watch::Sender<u64>>>);
pub(crate) struct Lease(Vec<(watch::Receiver<u64>, u64)>, pub Option<String>);
impl Sessions {
    pub fn lease(&self, path: &[String]) -> Lease {
        let mut groups = self.0.lock().unwrap();
        groups.retain(|_, sender| sender.receiver_count() > 0);
        Lease(
            path.iter()
                .map(|name| {
                    let sender = groups
                        .entry(name.clone())
                        .or_insert_with(|| watch::channel(0).0);
                    let receiver = sender.subscribe();
                    let epoch = *receiver.borrow();
                    (receiver, epoch)
                })
                .collect(),
            None,
        )
    }
    pub fn disconnect(&self, group: &str, reason: &str) {
        let groups = self.0.lock().unwrap();
        if let Some(sender) = groups.get(group) {
            sender.send_modify(|v| *v = v.wrapping_add(1));
        }
        tracing::info!(%group, %reason, "closing previous proxy group connections");
    }
}
impl Lease {
    pub async fn cancelled(&self) {
        if self.0.is_empty() {
            std::future::pending::<()>().await;
            return;
        }
        let waits: Vec<_> = self
            .0
            .iter()
            .map(|(receiver, epoch)| {
                let mut receiver = receiver.clone();
                let epoch = *epoch;
                Box::pin(async move {
                    loop {
                        if *receiver.borrow_and_update() != epoch {
                            return;
                        }
                        if receiver.changed().await.is_err() {
                            std::future::pending::<()>().await;
                        }
                    }
                }) as Pin<Box<dyn Future<Output = ()> + Send>>
            })
            .collect();
        futures_util::future::select_all(waits).await;
    }
    pub fn wrap(self, mut upstream: crate::outbound::Stream) -> crate::outbound::Stream {
        if self.0.is_empty() {
            return upstream;
        }
        let (client, mut relay) = tokio::io::duplex(32 * 1024);
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = self.cancelled() => {},
                _ = tokio::io::copy_bidirectional(&mut upstream, &mut relay) => {},
            }
        });
        Box::new(Stream {
            inner: client,
            task,
        })
    }
}
struct Stream {
    inner: tokio::io::DuplexStream,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn cancellation_is_scoped_wakes_idle_sockets_and_does_not_cancel_future_connections() {
        let sessions = Sessions::default();
        let (upstream, mut remote) = tokio::io::duplex(64);
        let mut client = sessions
            .lease(&["parent".into(), "child".into()])
            .wrap(Box::new(upstream));
        let unrelated = sessions.lease(&["other".into()]);
        client.write_all(b"hello").await.unwrap();
        let mut bytes = [0; 5];
        remote.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"hello");
        sessions.disconnect("child", "failure");
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), remote.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), unrelated.cancelled())
                .await
                .is_err()
        );
        let future = sessions.lease(&["child".into()]);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), future.cancelled())
                .await
                .is_err()
        );
    }
}
