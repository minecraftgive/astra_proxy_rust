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
#[path = "test/session_test.rs"]
mod tests;
