//! Persistent framed RPC. A lost response is never automatically replayed.
use crate::{platform::LocalStream, progress};
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
                let Some(call) = call else {
                    break;
                };
                if call.reply.is_closed() {
                    continue;
                }
                let result = tokio::time::timeout(call.timeout, async {
                    if connection.is_none() {
                        if let Some(progress) = &call.progress { progress.send_replace(Some(progress::Progress::new("Connecting", path.display().to_string()))); }
                        let mut stream = tokio::time::timeout(Duration::from_secs(3), LocalStream::connect(&path)).await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connection timeout"))??;
                        crate::send_management(&mut stream, &hello()).await?;
                        let event: Event<R> = tokio::time::timeout(Duration::from_secs(3), crate::receive_management(&mut stream)).await
                            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "session handshake timeout"))??;
                        match event { Event::Ready(version) if version == crate::API_VERSION => {}, _ => return Err(io::Error::other("Update/restart the manager and cores: persistent sessions are unavailable")) }
                        connection = Some(stream);
                    }
                    let stream = connection.as_mut().unwrap();
                    crate::send_management(stream, &call.request).await?;
                    loop {
                        match crate::receive_management::<Event<R>>(stream).await? {
                            Event::Progress(event) => { if let Some(sender) = &call.progress { sender.send_replace(Some(event)); } }
                            Event::Reply(reply) => return Ok(reply),
                            Event::Ready(_) => return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected session greeting")),
                        }
                    }
                }).await.unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out; it was not replayed; refresh state before retrying")));
                if result.is_err() {
                    connection = None;
                }
                let _ = call.reply.send(result);
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
    use crate::platform::LocalListener;
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
        crate::platform::remove_socket(&path);
    }
}
