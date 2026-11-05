use anyhow::{bail, Context, Result};
use astra_ipc::API_VERSION;
use astra_model::RouteRequest;
use astra_model::RouteResponse;
use astra_platform::platform::LocalStream as UnixStream;
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
    capacity: usize,
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
        let mut idle = self.owner.idle();
        if idle.len() < self.owner.capacity {
            idle.push(self.stream.take());
        }
    }
}

impl RuleClient {
    fn idle(&self) -> std::sync::MutexGuard<'_, Vec<Option<UnixStream>>> {
        match self.idle.lock() {
            Ok(idle) => idle,
            Err(poisoned) => {
                // Only idle transport resources live here; discard their frames.
                // Semaphore permits independently bound in-flight requests, so a
                // missing cached channel can simply be reconnected when needed.
                let mut idle = poisoned.into_inner();
                idle.clear();
                self.idle.clear_poison();
                tracing::error!("rule IPC idle pool update panicked; discarded cached channels");
                idle
            }
        }
    }
    pub fn new(socket_path: String, pool_size: usize, timeout_ms: u64) -> Self {
        let revision = Arc::new(AtomicU64::new(0));
        let observer = None;
        Self {
            revision,
            observer,
            socket_path,
            idle: Mutex::new((0..pool_size).map(|_| None).collect()),
            capacity: pool_size,
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

    pub async fn decide(&self, req: RouteRequest) -> Result<RouteResponse> {
        let operation = astra_observe::operation!("rule_ipc", "规则 IPC 决策（含排队与重试）");
        let result = self.decide_inner(req).await;
        operation.finish(result.is_ok());
        result
    }

    async fn decide_inner(&self, mut req: RouteRequest) -> Result<RouteResponse> {
        req.api_version = API_VERSION;
        req.id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let result = timeout(self.deadline, async {
            let queue = astra_observe::operation!("rule_ipc.queue", "规则 IPC 连接池排队");
            let permit = self.available.clone().acquire_owned().await;
            queue.finish(permit.is_ok());
            let permit = permit?;
            let mut lease = Lease {
                owner: self,
                stream: self.idle().pop().flatten(),
                _permit: permit,
            };
            for attempt in 0..2 {
                if attempt > 0 {
                    astra_observe::counter!("rule_ipc.retries", "规则 IPC：重试").add(1);
                }
                // The stream is not returned to the pool until a full valid reply arrives.
                // Cancellation, partial I/O and timeouts therefore always close it.
                let mut stream = match lease.stream.take() {
                    Some(stream) => stream,
                    None => {
                        astra_observe::counter!(
                            "rule_ipc.connect_attempts",
                            "规则 IPC：新连接尝试"
                        )
                        .add(1);
                        if attempt > 0 {
                            astra_observe::counter!(
                                "rule_ipc.reconnects",
                                "规则 IPC：I/O 失败后重连尝试"
                            )
                            .add(1);
                        }
                        let mut stream =
                            astra_platform::platform::connect_trusted(&self.socket_path)
                                .await
                                .context("Rule Core connect failed")?;
                        astra_ipc::exchange_core_info(&mut stream, env!("CARGO_PKG_VERSION"))
                            .await?;
                        stream
                    }
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
                    Err(err) => {
                        astra_observe::counter!("rule_ipc.io_errors", "规则 IPC：读写错误").add(1);
                        tracing::debug!(error = %err, "IPC error; reconnecting");
                    }
                }
            }
            bail!("Rule Core unavailable: fail closed")
        })
        .await;
        if result.is_err() {
            astra_observe::counter!("rule_ipc.timeouts", "规则 IPC：决策超时（含排队）").add(1);
        }
        result.context("IPC decision deadline exceeded")?
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
            let mut stream = astra_platform::platform::connect_trusted(&path).await?;
            astra_ipc::send(
                &mut stream,
                &RouteRequest {
                    destination_ip: None,
                    api_version: API_VERSION,
                    id: 0,
                    target: astra_model::Target {
                        host: String::new(),
                        port: 0,
                    },
                    network: astra_model::Network::Tcp,
                    inbound: astra_model::Inbound::Socks5,
                    process: None,
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
#[path = "../test/proxy_rule_client_test.rs"]
mod tests;
