use anyhow::{bail, Context, Result};
use astra_ipc::platform::LocalStream as UnixStream;
use astra_ipc::{RouteRequest, RouteResponse, API_VERSION};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};

/// Idle channels are borrowed by availability. A single deadline includes queueing and retry.
pub struct RuleClient {
    socket_path: String,
    idle: Mutex<Vec<Option<UnixStream>>>,
    available: Arc<Semaphore>,
    next_id: AtomicU64,
    deadline: Duration,
    revision: Arc<AtomicU64>,
    observer: Option<tokio::task::JoinHandle<()>>,
}

struct Lease<'a> {
    owner: &'a RuleClient,
    stream: Option<UnixStream>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.owner.idle.lock().unwrap().push(self.stream.take());
    }
}

impl RuleClient {
    pub fn new(socket_path: String, pool_size: usize, timeout_ms: u64) -> Self {
        let revision = Arc::new(AtomicU64::new(0));
        let observer = None;
        Self {
            revision,
            observer,
            socket_path,
            idle: Mutex::new((0..pool_size).map(|_| None).collect()),
            available: Arc::new(Semaphore::new(pool_size)),
            next_id: AtomicU64::new(1),
            deadline: Duration::from_millis(timeout_ms),
        }
    }

    pub fn start_observer(&mut self) {
        if self.observer.is_none() {
            self.observer = Some(tokio::spawn(observe(
                self.socket_path.clone(),
                self.revision.clone(),
            )));
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub async fn decide(&self, mut req: RouteRequest) -> Result<RouteResponse> {
        req.api_version = API_VERSION;
        req.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        timeout(self.deadline, async {
            let permit = self.available.clone().acquire_owned().await?;
            let mut lease = Lease {
                owner: self,
                stream: self
                    .idle
                    .lock()
                    .unwrap()
                    .pop()
                    .expect("permit owns a channel"),
                _permit: permit,
            };
            for _ in 0..2 {
                // The stream is not returned to the pool until a full valid reply arrives.
                // Cancellation, partial I/O and timeouts therefore always close it.
                let mut stream = match lease.stream.take() {
                    Some(stream) => stream,
                    None => UnixStream::connect(&self.socket_path)
                        .await
                        .context("Rule Core connect failed")?,
                };
                let result = async {
                    astra_ipc::send(&mut stream, &req).await?;
                    astra_ipc::receive::<RouteResponse>(&mut stream).await
                }
                .await;
                match result {
                    Ok(response)
                        if response.id == req.id && response.api_version == API_VERSION =>
                    {
                        lease.stream = Some(stream);
                        return Ok(response);
                    }
                    Ok(_) => bail!("Rule Core response ID or protocol version mismatch"),
                    Err(err) => tracing::debug!(error = %err, "IPC error; reconnecting"),
                }
            }
            bail!("Rule Core unavailable: fail closed")
        })
        .await
        .context("IPC decision deadline exceeded")?
    }
}

impl Drop for RuleClient {
    fn drop(&mut self) {
        if let Some(observer) = &self.observer {
            observer.abort();
        }
    }
}
async fn observe(path: String, revision: Arc<AtomicU64>) {
    loop {
        let result: Result<()> = async {
            let mut stream = UnixStream::connect(&path).await?;
            astra_ipc::send(
                &mut stream,
                &RouteRequest {
                    api_version: API_VERSION,
                    id: 0,
                    target: astra_ipc::Target {
                        host: String::new(),
                        port: 0,
                    },
                    network: astra_ipc::Network::Tcp,
                    inbound: astra_ipc::Inbound::Socks5,
                },
            )
            .await?;
            loop {
                let response: RouteResponse = astra_ipc::receive(&mut stream).await?;
                anyhow::ensure!(
                    response.api_version == API_VERSION && response.id == 0,
                    "invalid revision notification"
                );
                revision.store(response.revision, Ordering::Release);
            }
        }
        .await;
        revision.store(0, Ordering::Release);
        if let Err(error) = result {
            tracing::debug!(%error, "rule revision subscription disconnected");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_ipc::{Inbound, Network, RouteAction, Target};
    fn request() -> RouteRequest {
        RouteRequest {
            api_version: API_VERSION,
            id: 0,
            target: Target {
                host: "example.com".into(),
                port: 443,
            },
            network: Network::Tcp,
            inbound: Inbound::Socks5,
        }
    }

    #[tokio::test]
    async fn timeout_discards_channel_and_returns_capacity() {
        let client = RuleClient::new("/nonexistent/astra-test.sock".into(), 1, 30);
        let (local, mut remote) = astra_ipc::platform::stream_pair().await.unwrap();
        client.idle.lock().unwrap()[0] = Some(local);
        let server = tokio::spawn(async move {
            let _: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
            let mut byte = [0];
            use tokio::io::AsyncReadExt;
            assert_eq!(remote.read(&mut byte).await.unwrap(), 0);
        });
        let err = client.decide(request()).await.unwrap_err();
        assert!(err.to_string().contains("deadline"));
        assert_eq!(client.available.available_permits(), 1);
        assert!(client.idle.lock().unwrap()[0].is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn busy_channel_does_not_block_an_idle_channel() {
        let client = Arc::new(RuleClient::new("unused".into(), 2, 500));
        let (fast, mut fast_peer) = astra_ipc::platform::stream_pair().await.unwrap();
        let (slow, mut slow_peer) = astra_ipc::platform::stream_pair().await.unwrap();
        *client.idle.lock().unwrap() = vec![Some(fast), Some(slow)];
        let (started, received) = tokio::sync::oneshot::channel();
        let slow_server = tokio::spawn(async move {
            let _: RouteRequest = astra_ipc::receive(&mut slow_peer).await.unwrap();
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let worker = {
            let client = client.clone();
            tokio::spawn(async move { client.decide(request()).await })
        };
        received.await.unwrap();
        let fast_server = tokio::spawn(async move {
            let req: RouteRequest = astra_ipc::receive(&mut fast_peer).await.unwrap();
            astra_ipc::send(
                &mut fast_peer,
                &RouteResponse {
                    api_version: API_VERSION,
                    revision: 1,
                    id: req.id,
                    action: RouteAction::Block,
                },
            )
            .await
            .unwrap();
        });
        assert_eq!(
            timeout(Duration::from_millis(100), client.decide(request()))
                .await
                .unwrap()
                .unwrap()
                .action,
            RouteAction::Block
        );
        worker.abort();
        let _ = worker.await;
        slow_server.abort();
        fast_server.await.unwrap();
        assert_eq!(client.available.available_permits(), 2);
    }
}
