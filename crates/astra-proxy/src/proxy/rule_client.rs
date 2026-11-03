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
mod tests {
    use super::*;
    use astra_model::Inbound;
    use astra_model::Network;
    use astra_model::RouteAction;
    use astra_model::Target;
    fn request() -> RouteRequest {
        RouteRequest {
            destination_ip: None,
            api_version: API_VERSION,
            id: 0,
            target: Target {
                host: "example.com".into(),
                port: 443,
            },
            network: Network::Tcp,
            inbound: Inbound::Socks5,
            process: None,
        }
    }

    #[tokio::test]
    async fn poisoned_idle_pool_discards_channels_without_panicking_during_lease_drop() {
        let client = RuleClient::new("/nonexistent/astra-poison-test.sock".into(), 2, 100);
        let (cached, mut peer) = astra_platform::platform::stream_pair().await.unwrap();
        *client.idle() = vec![Some(cached)];
        let lease = Lease {
            owner: &client,
            stream: None, // a request owns its stream outside the idle lease
            _permit: client.available.clone().acquire_owned().await.unwrap(),
        };
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _idle = client.idle.lock().unwrap();
            panic!("injected idle pool update panic");
        }))
        .is_err());
        let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _lease = lease;
            panic!("original request panic");
        }))
        .unwrap_err();
        assert_eq!(
            original.downcast_ref::<&str>(),
            Some(&"original request panic")
        );
        use tokio::io::AsyncReadExt;
        assert_eq!(
            timeout(Duration::from_secs(1), peer.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(!client.idle.is_poisoned());
        assert_eq!(client.available.available_permits(), 2);
        // The cache can have fewer entries than permits after repair. A cache
        // miss must reconnect/fail closed, never panic or invent a direct route.
        let (first, second) = tokio::join!(client.decide(request()), client.decide(request()));
        assert!(first.is_err() && second.is_err());
        assert_eq!(client.available.available_permits(), 2);
        assert!(client.idle().len() <= client.capacity);
    }

    #[tokio::test]
    async fn routing_context_uses_original_destination_only_for_transparent_inbounds() {
        let app = crate::App::from_text(
            &std::env::temp_dir().join("astra-route-context.toml"),
            "rule_socket='unused'\nipc_connections=1\nsocks5_listen='127.0.0.1:1080'\n".into(),
        )
        .unwrap();
        let cases = [
            (Inbound::Socks5, "qq.com", "127.0.0.1:1080", None),
            (Inbound::HttpConnect, "qq.com", "127.0.0.1:8080", None),
            (
                Inbound::Tun,
                "qq.com",
                "203.0.113.8:443",
                Some("203.0.113.8"),
            ),
            (
                Inbound::Socks5,
                "203.0.113.8",
                "127.0.0.1:1080",
                Some("203.0.113.8"),
            ),
        ];
        let (local, mut remote) = astra_platform::platform::stream_pair().await.unwrap();
        app.rule_client.as_ref().unwrap().idle.lock().unwrap()[0] = Some(local);
        let server = tokio::spawn(async move {
            for (inbound, host, _, ip) in cases {
                let req: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
                assert_eq!(req.inbound, inbound);
                assert_eq!(req.target.host, host);
                assert_eq!(req.destination_ip, ip.map(|ip| ip.parse().unwrap()));
                astra_ipc::send(
                    &mut remote,
                    &RouteResponse {
                        api_version: API_VERSION,
                        id: req.id,
                        revision: 1,
                        action: RouteAction::Block,
                    },
                )
                .await
                .unwrap();
            }
        });
        for (inbound, host, destination, _) in cases {
            assert_eq!(
                app.route_connection(
                    &Target {
                        host: host.into(),
                        port: 443
                    },
                    inbound,
                    Network::Tcp,
                    None,
                    Some(destination.parse().unwrap()),
                )
                .await
                .unwrap(),
                RouteAction::Block
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fake_ip_restores_domain_before_rule_ipc_for_tcp_and_udp() {
        let text="rule_socket='unused'\nipc_connections=1\nsocks5_listen='127.0.0.1:1080'\n[dns]\nlisten=[]\nfake_ip=true\n";
        let app = crate::App::from_text(
            &std::env::temp_dir().join("astra-dns-rule.toml"),
            text.into(),
        )
        .unwrap();
        let (local, mut remote) = astra_platform::platform::stream_pair().await.unwrap();
        app.rule_client.as_ref().unwrap().idle.lock().unwrap()[0] = Some(local);
        let server = tokio::spawn(async move {
            for network in [Network::Tcp, Network::Udp] {
                let request: RouteRequest = astra_ipc::receive(&mut remote).await.unwrap();
                assert_eq!(request.target.host, "blocked.example");
                assert_eq!(request.network, network);
                astra_ipc::send(
                    &mut remote,
                    &RouteResponse {
                        api_version: API_VERSION,
                        id: request.id,
                        revision: 1,
                        action: RouteAction::Block,
                    },
                )
                .await
                .unwrap();
            }
        });
        let query = astra_dns::wire::query("blocked.example", hickory_proto::rr::RecordType::A)
            .unwrap()
            .to_vec()
            .unwrap();
        let reply =
            hickory_proto::op::Message::from_vec(&app.dns_answer(&query).await.unwrap()).unwrap();
        let hickory_proto::rr::RData::A(ip) = reply.answers[0].data else {
            panic!()
        };
        let target = app
            .restore_target(&Target {
                host: ip.0.to_string(),
                port: 443,
            })
            .unwrap();
        for network in [Network::Tcp, Network::Udp] {
            assert_eq!(
                app.route_network(&target, Inbound::Tun, network)
                    .await
                    .unwrap(),
                RouteAction::Block
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn timeout_discards_channel_and_returns_capacity() {
        let client = RuleClient::new("/nonexistent/astra-test.sock".into(), 1, 30);
        let (local, mut remote) = astra_platform::platform::stream_pair().await.unwrap();
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
        let (fast, mut fast_peer) = astra_platform::platform::stream_pair().await.unwrap();
        let (slow, mut slow_peer) = astra_platform::platform::stream_pair().await.unwrap();
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
