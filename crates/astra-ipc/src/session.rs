//! Persistent framed RPC. A lost response is never automatically replayed.
use crate::progress;
use astra_platform::platform::LocalStream;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{io, path::PathBuf, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, oneshot},
};

#[derive(Debug, Serialize, Deserialize)]
pub enum Event<R> {
    Ready(u16),
    Progress(progress::Progress),
    Reply(R),
}
struct Call<Q, R> {
    request: Q,
    reply: oneshot::Sender<io::Result<R>>,
    progress: Option<progress::Sender>,
    timeout: Duration,
}
pub struct Peer<Q, R> {
    sender: mpsc::Sender<Call<Q, R>>,
}
impl<Q, R> Clone for Peer<Q, R> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}
impl<Q, R> Peer<Q, R>
where
    Q: Serialize + Send + Sync + 'static,
    R: DeserializeOwned + Send + 'static,
{
    pub fn new(path: PathBuf, hello: fn() -> Q) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Call<Q, R>>(32);
        tokio::spawn(async move {
            let mut connection: Option<LocalStream> = None;
            loop {
                let call = if let Some(stream) = connection.as_mut() {
                    tokio::select! {
                        biased;
                        _=stream.read_u8()=>{connection=None;continue;}
                        call=receiver.recv()=>call,
                    }
                } else {
                    receiver.recv().await
                };
                let Some(Call {
                    request,
                    mut reply,
                    progress,
                    timeout,
                }) = call
                else {
                    break;
                };
                if reply.is_closed() {
                    continue;
                }
                let result = tokio::select! {
                    biased;
                    _ = reply.closed() => {
                        // A cancelled caller must release its in-flight connection so
                        // the server can cancel connection-scoped work. Never reuse a
                        // stream whose request/response may be only partially framed.
                        // Already accepted mutations may still finish; do not replay them.
                        connection = None;
                        continue;
                    }
                    result = tokio::time::timeout(timeout, async {
                    if connection.is_none() {
                        if let Some(progress) = &progress { progress.send_replace(Some(progress::Progress::new("Connecting", path.display().to_string()))); }
                        let mut stream = tokio::time::timeout(Duration::from_secs(3), astra_platform::platform::connect_trusted(&path)).await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timeout"))??;
                        crate::send_management(&mut stream, &hello()).await?;
                        let event: Event<R> = tokio::time::timeout(Duration::from_secs(3), crate::receive_management(&mut stream)).await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "session handshake timeout"))??;
                        match event { Event::Ready(version) if version == crate::API_VERSION => {}, _ => return Err(io::Error::other("Update/restart the manager and cores: persistent sessions are unavailable")) }
                        connection = Some(stream);
                    }
                    let stream = connection.as_mut().unwrap();
                    crate::send_management(stream, &request).await?;
                    loop {
                        match crate::receive_management::<Event<R>>(stream).await? {
                            Event::Progress(event) => { if let Some(sender) = &progress { sender.send_replace(Some(event)); } }
                            Event::Reply(reply) => return Ok(reply),
                            Event::Ready(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected session greeting")),
                        }
                    }
                    }) => result.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out; it was not replayed; refresh state before retrying"))),
                };
                if result.is_err() {
                    connection = None;
                }
                let _ = reply.send(result);
            }
        });
        Self { sender }
    }
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
    pub async fn request(&self, request: Q, timeout: Duration) -> io::Result<R> {
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Call {
                request,
                reply,
                timeout,
                progress: progress::sender(),
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session worker stopped"))?;
        receiver
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "session worker stopped"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_platform::platform::LocalListener;
    async fn cancelled_call_closes_connection(during_handshake: bool) {
        let path = std::env::temp_dir().join(format!(
            "astra-cancel-{}-{during_handshake}.sock",
            std::process::id()
        ));
        let listener = LocalListener::bind(&path).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                0
            );
            if !during_handshake {
                crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
                    .await
                    .unwrap();
                assert_eq!(
                    crate::receive_management::<u8>(&mut stream).await.unwrap(),
                    1
                );
            }
            started_tx.send(()).unwrap();
            // No server-side timeout or reply can close this connection for the caller.
            assert!(stream.read_u8().await.is_err());
            closed_tx.send(()).unwrap();
            drop(stream);

            // The next call must use a fresh session, with no replay of the cancelled request.
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                0
            );
            crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
                .await
                .unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                2
            );
            crate::send_management(&mut stream, &Event::Reply(2u8))
                .await
                .unwrap();
        });
        let peer = Peer::<u8, u8>::new(path.clone(), || 0);
        let first_peer = peer.clone();
        let first =
            tokio::spawn(async move { first_peer.request(1, Duration::from_secs(60)).await });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("dropping the call must close its connection without waiting for a reply")
            .unwrap();
        assert_eq!(peer.request(2, Duration::from_secs(2)).await.unwrap(), 2);
        server.await.unwrap();
        astra_platform::platform::remove_socket(&path);
    }

    #[tokio::test]
    async fn cancelling_during_handshake_closes_connection_and_reconnects() {
        cancelled_call_closes_connection(true).await;
    }

    #[tokio::test]
    async fn cancelling_after_send_closes_connection_and_reconnects() {
        cancelled_call_closes_connection(false).await;
    }

    #[tokio::test]
    async fn cancelling_queued_call_does_not_send_it_or_close_active_connection() {
        let path = std::env::temp_dir().join(format!("astra-queued-{}.sock", std::process::id()));
        let listener = LocalListener::bind(&path).unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                0
            );
            crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
                .await
                .unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                1
            );
            started_tx.send(()).unwrap();
            release_rx.await.unwrap();
            crate::send_management(&mut stream, &Event::Reply(1u8))
                .await
                .unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                3
            );
            crate::send_management(&mut stream, &Event::Reply(3u8))
                .await
                .unwrap();
        });
        let peer = Peer::<u8, u8>::new(path.clone(), || 0);
        let first_peer = peer.clone();
        let first =
            tokio::spawn(async move { first_peer.request(1, Duration::from_secs(2)).await });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let mut queued = Box::pin(peer.request(2, Duration::from_secs(2)));
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        drop(queued);
        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert_eq!(peer.request(3, Duration::from_secs(2)).await.unwrap(), 3);
        server.await.unwrap();
        astra_platform::platform::remove_socket(&path);
    }

    #[tokio::test]
    async fn reuses_one_connection_and_does_not_replay_a_lost_mutation() {
        let path = std::env::temp_dir().join(format!("astra-peer-{}.sock", std::process::id()));
        let listener = LocalListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                0
            );
            crate::send_management(&mut stream, &Event::<u8>::Ready(crate::API_VERSION))
                .await
                .unwrap();
            for value in [1u8, 2] {
                assert_eq!(
                    crate::receive_management::<u8>(&mut stream).await.unwrap(),
                    value
                );
                crate::send_management(&mut stream, &Event::Reply(value))
                    .await
                    .unwrap();
            }
            assert_eq!(
                crate::receive_management::<u8>(&mut stream).await.unwrap(),
                3
            );
            drop(stream); // Mutation ran, but its reply was lost.
            assert!(
                tokio::time::timeout(Duration::from_millis(150), listener.accept())
                    .await
                    .is_err()
            );
        });
        let peer = Peer::<u8, u8>::new(path.clone(), || 0);
        assert_eq!(peer.request(1, Duration::from_secs(2)).await.unwrap(), 1);
        assert_eq!(peer.request(2, Duration::from_secs(2)).await.unwrap(), 2);
        assert!(peer.request(3, Duration::from_secs(2)).await.is_err());
        server.await.unwrap();
        astra_platform::platform::remove_socket(&path);
    }
}
