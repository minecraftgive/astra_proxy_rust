//! Opt-in end-to-end tests: real client framing, Agent persistence and rebuilt
//! core subprocesses; every destination is a controlled loopback fixture.
use super::*;
use astra_model::nettool::{AddressFamily, Egress, Probe, Request};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};

struct Fixtures {
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixtures {
    async fn dns(&mut self, last: u8) -> (SocketAddr, Arc<AtomicUsize>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        self.tasks.push(tokio::spawn(async move {
            let mut input = [0; 4096];
            while let Ok((size, peer)) = socket.recv_from(&mut input).await {
                seen.fetch_add(1, Ordering::SeqCst);
                if size < 17 {
                    continue;
                }
                let mut end = 12;
                while end < size && input[end] != 0 {
                    end += 1 + usize::from(input[end]);
                }
                end += 5;
                if end > size {
                    continue;
                }
                let mut output = vec![input[0], input[1], 0x81, 0x80, 0, 1, 0, 1, 0, 0, 0, 0];
                output.extend_from_slice(&input[12..end]);
                output.extend([0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 192, 0, 2, last]);
                let _ = socket.send_to(&output, peer).await;
            }
        }));
        (address, count)
    }
    async fn tcp(&mut self, address: &str, http: bool) -> SocketAddr {
        let listener = TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        self.tasks.push(tokio::spawn(async move {
            while let Ok((mut stream,_))=listener.accept().await {
                if !http {continue;}
                let mut bytes=vec![];
                loop { let mut buf=[0;1024];let Ok(size)=stream.read(&mut buf).await else {break;};if size==0 {break;}bytes.extend_from_slice(&buf[..size]);if bytes.windows(4).any(|b|b==b"\r\n\r\n"){break;} }
                let body="safe fixture content ".repeat(3000);
                let head=format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nSet-Cookie: COOKIE_SECRET\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len());
                let _=stream.write_all(head.as_bytes()).await;let _=stream.write_all(body.as_bytes()).await;
            }
        }));
        address
    }
    async fn tls(&mut self) -> (SocketAddr, Arc<AtomicUsize>) {
        use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer};
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let application = Arc::new(AtomicUsize::new(0));
        let observed = application.clone();
        self.tasks.push(tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                if let Ok(Ok(mut tls)) =
                    tokio::time::timeout(Duration::from_secs(5), acceptor.accept(stream)).await
                {
                    let mut bytes = [0; 4096];
                    if let Ok(Ok(size)) =
                        tokio::time::timeout(Duration::from_millis(500), tls.read(&mut bytes)).await
                    {
                        observed.fetch_add(size, Ordering::SeqCst);
                    }
                }
            }
        }));
        (address, application)
    }
    async fn stun(&mut self) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        self.tasks.push(tokio::spawn(async move {
            let mut bytes = [0; 4096];
            while let Ok((size, peer)) = socket.recv_from(&mut bytes).await {
                if size < 20 {
                    continue;
                }
                let SocketAddr::V4(peer) = peer else {
                    continue;
                };
                let mut reply = vec![1, 1, 0, 12];
                reply.extend_from_slice(&bytes[4..20]);
                reply.extend([0, 0x20, 0, 8, 0, 1]);
                reply.extend((peer.port() ^ 0x2112).to_be_bytes());
                reply.extend(
                    peer.ip()
                        .octets()
                        .into_iter()
                        .zip([0x21, 0x12, 0xa4, 0x42])
                        .map(|(a, b)| a ^ b),
                );
                let _ = socket.send_to(&reply, peer).await;
            }
        }));
        address
    }
}
fn project(id: &str, path: &str, probe: Probe) -> v::NetworkBatchProject {
    v::NetworkBatchProject {
        id: id.into(),
        path_id: path.into(),
        request: Request {
            config: String::new(),
            egress: Egress::System,
            probe,
            count: 1,
            interval_ms: 0,
            timeout_ms: 5000,
        }
        .into(),
    }
}
async fn collect(
    client: &Client,
    id: &str,
) -> anyhow::Result<(v::NetworkReport, Vec<serde_json::Value>, usize)> {
    tokio::time::timeout(Duration::from_secs(35), async {
        let (mut cursor, mut items, mut pages) = (None, vec![], 0);
        loop {
            let report = client.report(id, cursor.clone()).await?;
            pages += 1;
            cursor = Some(report.cursor.clone());
            items.extend(report.items.iter().map(|item| item.data.clone()));
            if report.state.terminal() && !report.has_more {
                return Ok((report, items, pages));
            }
            if !report.has_more {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "requires freshly built astra-proxy binary; all probe destinations are loopback"]
async fn real_agent_and_core_batch_pages_all_tools_and_cancels_without_more_packets() {
    let directory = crate::import::tests::Fixture::new();
    let bin = std::env::var_os("ASTRA_NETWORK_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    assert!(
        bin.join("astra-proxy").exists(),
        "build astra-proxy before this opt-in test"
    );
    let state = directory.0.join("state");
    let endpoint = directory.0.join("ipc.sock");
    let manager = tokio::spawn(astra_manager::run_config(
        astra_management::config::ManagerConfig {
            state_dir: state,
            config_dir: directory.0.join("configs"),
            runtime_dir: Some(directory.0.join("run")),
            bin_dir: bin,
            local_ipc: astra_management::config::LocalIpc {
                endpoint: Some(endpoint.clone()),
            },
            ..Default::default()
        },
    ));
    let mut fixtures = Fixtures { tasks: vec![] };
    let client = Client::for_target(endpoint.clone(), None);
    let result: anyhow::Result<()> =
        async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !endpoint.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            let config = directory.write(
                "proxy.toml",
                "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='unused'\nserver='127.0.0.1:9'\n",
            );
            client
                .request(v::RequestBody::Execute {
                    operation_id: "add-profile".into(),
                    command: v::Command::Add {
                        name: "fixture".into(),
                        kind: v::InstanceKind::Proxy,
                        config,
                    },
                })
                .await?;
            let (dns_a, count_a) = fixtures.dns(1).await;
            let (dns_b, count_b) = fixtures.dns(2).await;
            let tcp = fixtures.tcp("127.0.0.1:0", false).await;
            let ipv6 = fixtures.tcp("[::1]:0", false).await;
            let http = fixtures.tcp("127.0.0.1:0", true).await;
            let (tls, application) = fixtures.tls().await;
            let stun = fixtures.stun().await;
            let dns = |server: SocketAddr| Probe::Dns {
                name: "fixture.invalid".into(),
                record: "A".into(),
                server: format!("udp://{server}"),
            };
            let mut projects = vec![
                project("dns-a", "path-a", dns(dns_a)),
                project("dns-b", "path-b", dns(dns_b)),
                project(
                    "tcp",
                    "path-a",
                    Probe::Tcp {
                        host: tcp.ip().to_string(),
                        port: tcp.port(),
                    },
                ),
                project(
                    "v6",
                    "path-a",
                    Probe::Tcp {
                        host: ipv6.ip().to_string(),
                        port: ipv6.port(),
                    },
                ),
                project(
                    "tls",
                    "path-a",
                    Probe::Tls {
                        host: tls.ip().to_string(),
                        port: tls.port(),
                        server_name: Some("localhost".into()),
                        compare_bundled: true,
                    },
                ),
                project(
                    "http",
                    "path-a",
                    Probe::Http {
                        url: format!("http://{http}/private?token=URL_SECRET"),
                        method: "GET".into(),
                        compare_bundled: false,
                    },
                ),
                project(
                    "failed-https",
                    "path-a",
                    Probe::Http {
                        url: format!(
                            "https://localhost:{}/private?token=TLS_URL_SECRET#bootstrap={}",
                            tls.port(),
                            tls.ip()
                        ),
                        method: "GET".into(),
                        compare_bundled: true,
                    },
                ),
                project(
                    "stun",
                    "path-a",
                    Probe::Stun {
                        server: stun.to_string(),
                    },
                ),
            ];
            projects[3].request.family = Some(AddressFamily::Ipv6);
            let batch = v::NetworkBatchRequest {
                projects,
                comparison: true,
            };
            let accepted = client
                .submit_batch("real-batch".into(), "fixture".into(), batch)
                .await?;
            assert!(matches!(accepted.state, v::TaskState::Running));
            let (report, events, pages) = collect(&client, "real-batch").await?;
            anyhow::ensure!(
                matches!(
                    report.state,
                    v::NetworkState::Complete | v::NetworkState::Partial
                ),
                "{report:?}"
            );
            anyhow::ensure!(
                !report.truncated && !report.expired,
                "Unexpected lost evidence: state={:?} truncated={} expired={} cursor={} message={} sizes={:?}", report.state,report.truncated,report.expired,report.cursor,report.message,events.iter().map(|e|(e["event"].clone(),e["detail"]["event"].clone(),serde_json::to_vec(e).unwrap().len())).collect::<Vec<_>>()
            );
            let serialized = serde_json::to_string(&events)?;
            assert!(
                serialized.len() > 64 * 1024 && pages > 1,
                "exercise real pagination"
            );
            for secret in ["COOKIE_SECRET", "URL_SECRET", "TLS_URL_SECRET", "/private"] {
                assert!(!serialized.contains(secret), "export leaked {secret}");
            }
            for id in [
                "dns-a",
                "dns-b",
                "tcp",
                "v6",
                "tls",
                "http",
                "failed-https",
                "stun",
            ] {
                assert!(
                    events.iter().any(|event| event["event"] == "project_status"
                        && event["project_id"] == id
                        && event["state"] == "complete"),
                    "project {id} failed: {events:?}"
                );
            }
            assert_eq!(count_a.load(Ordering::SeqCst), 2);
            assert_eq!(count_b.load(Ordering::SeqCst), 2);
            let details = events
                .iter()
                .filter(|event| event["event"] == "project")
                .map(|event| &event["detail"])
                .collect::<Vec<_>>();
            assert!(details.iter().any(|event| event["event"] == "stun"
                && event["observation"]["response_received"] == true));
            assert!(details.iter().any(|event| event["event"] == "tcp"
                && event["result"]["family"] == "ipv6"
                && event["result"]["outcome"] == "connected"));
            assert!(details
                .iter()
                .any(|event| event["event"] == "tls" && event["detail"]["kind"] == "certificate"));
            assert!(details.iter().any(|event| event["event"] == "tls"
                && event["detail"]["kind"] == "handshake"
                && event["detail"]["bundled_comparison"]["offline"] == true));
            assert_eq!(
                application.load(Ordering::SeqCst),
                0,
                "failed TLS must not send HTTP"
            );
            let analysis = events
                .iter()
                .find(|event| event["event"] == "analysis")
                .expect("backend analysis retained");
            assert!(
                analysis["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| finding["kind"] == "dns_difference"),
                "two actual resolver paths must feed the shared rules: {analysis}"
            );
            // Restore from None on an already-terminal paged report, then retain the
            // original operation without ever resubmitting it.
            let (_, restored, _) = collect(&client, "real-batch").await?;
            assert_eq!(events, restored);
            let mut long: v::NetworkDiagnosticsRequest = Request {
                config: String::new(),
                egress: Egress::System,
                probe: dns(dns_a),
                count: 50,
                interval_ms: 100,
                timeout_ms: 1000,
            }
            .into();
            long.family = Some(AddressFamily::Ipv4);
            let before = count_a.load(Ordering::SeqCst);
            client
                .submit("cancel-real".into(), "fixture".into(), long)
                .await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                while count_a.load(Ordering::SeqCst) == before {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            let cancelled = client
                .cancel("cancel-real", "cancel-receipt".into())
                .await?;
            assert_eq!(cancelled.state, v::NetworkState::Cancelled);
            let after = count_a.load(Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(350)).await;
            assert_eq!(
                after,
                count_a.load(Ordering::SeqCst),
                "no more packets after cancellation acknowledged"
            );
            Ok(())
        }
        .await;
    let _ = client
        .request(v::RequestBody::Execute {
            operation_id: "shutdown-live".into(),
            command: v::Command::Shutdown,
        })
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(8), manager).await;
    result.unwrap();
}

/// A private, pre-provisioned remote fixture is required; this test creates no
/// listener, credentials, instance, route or TUN on either machine.
#[tokio::test]
#[ignore = "requires ASTRA_REMOTE_FIXTURE manifest for an isolated Controller and remote test Agent"]
async fn remote_controller_routes_shared_client_tools_pages_and_cancel_to_target_agent() {
    let file = std::env::var("ASTRA_REMOTE_FIXTURE").expect("private remote fixture manifest");
    let fixture: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    let value = |key: &str| {
        fixture[key]
            .as_str()
            .unwrap_or_else(|| panic!("missing {key}"))
            .to_owned()
    };
    let agent = value("agent_id");
    let instance = value("instance");
    let client = Client::for_target(value("endpoint").into(), Some(agent.clone()));
    let Availability::Available { capabilities, .. } = client.availability().await.unwrap() else {
        panic!("remote Agent must advertise new tools")
    };
    assert!(capabilities
        .iter()
        .any(|capability| capability == v::NETWORK_BATCH_CAPABILITY));
    let v::ResponseBody::NetworkState(snapshot) = client
        .request(v::RequestBody::Query(v::Query::NetworkState))
        .await
        .unwrap()
    else {
        panic!("target network snapshot")
    };
    assert_eq!(snapshot.agent_id, agent);
    assert!(!snapshot.interfaces.is_empty());
    assert!(snapshot
        .interfaces
        .iter()
        .all(|interface| !interface.source.is_empty()));
    let dns = value("dns");
    let tcp: SocketAddr = value("tcp").parse().unwrap();
    let ipv6: SocketAddr = value("ipv6").parse().unwrap();
    let http = value("http");
    let stun = value("stun");
    let query = || Probe::Dns {
        name: "fixture.invalid".into(),
        record: "A".into(),
        server: format!("udp://{dns}"),
    };
    let mut projects = vec![
        project("dns", "remote", query()),
        project(
            "tcp",
            "remote",
            Probe::Tcp {
                host: tcp.ip().to_string(),
                port: tcp.port(),
            },
        ),
        project(
            "v6",
            "remote",
            Probe::Tcp {
                host: ipv6.ip().to_string(),
                port: ipv6.port(),
            },
        ),
        project(
            "http",
            "remote",
            Probe::Http {
                url: format!("http://{http}/private?token=REMOTE_URL_SECRET"),
                method: "GET".into(),
                compare_bundled: false,
            },
        ),
        project("stun", "remote", Probe::Stun { server: stun }),
    ];
    projects[2].request.family = Some(AddressFamily::Ipv6);
    let suffix = astra_ipc::progress::now_ms();
    let id = format!("remote-live-{suffix}");
    client
        .submit_batch(
            id.clone(),
            instance.clone(),
            v::NetworkBatchRequest {
                projects,
                comparison: false,
            },
        )
        .await
        .unwrap();
    let (report, events, pages) = collect(&client, &id).await.unwrap();
    assert_eq!(report.state, v::NetworkState::Complete, "{report:?}");
    assert!(!report.truncated && !report.expired);
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(
        serialized.len() > 64 * 1024 && pages > 1,
        "remote fixture must exercise paging"
    );
    assert!(!serialized.contains("REMOTE_URL_SECRET") && !serialized.contains("/private"));
    for name in ["dns", "tcp", "v6", "http", "stun"] {
        assert!(
            events.iter().any(|event| event["event"] == "project_status"
                && event["project_id"] == name
                && event["state"] == "complete"),
            "project {name} did not complete"
        );
    }
    let (_, restored, _) = collect(&client, &id).await.unwrap();
    assert_eq!(events, restored);
    assert!(matches!(
        client.task(&id).await.unwrap().state,
        v::TaskState::Complete
    ));
    let cancel_id = format!("remote-cancel-{suffix}");
    let request: v::NetworkDiagnosticsRequest = Request {
        config: String::new(),
        egress: Egress::System,
        probe: query(),
        count: 50,
        interval_ms: 500,
        timeout_ms: 1000,
    }
    .into();
    client
        .submit(cancel_id.clone(), instance, request)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let report = client.report(&cancel_id, None).await.unwrap();
            if report
                .items
                .iter()
                .any(|item| item.data["event"] == "attempt")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    let cancelled = client
        .cancel(&cancel_id, format!("remote-cancel-receipt-{suffix}"))
        .await
        .unwrap();
    assert_eq!(cancelled.state, v::NetworkState::Cancelled);
    let (before, partial, _) = collect(&client, &cancel_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let (after, again, _) = collect(&client, &cancel_id).await.unwrap();
    assert_eq!(before.cursor, after.cursor);
    assert_eq!(partial, again);
    assert!(
        partial
            .iter()
            .filter(|event| event["event"] == "attempt")
            .count()
            < 50
    );
    println!("remote accepted: agent={} sampled_at={} interfaces={} batch={} pages={} evidence_bytes={} cancelled={}",snapshot.agent_id,snapshot.sampled_at_ms,snapshot.interfaces.len(),id,pages,serialized.len(),cancel_id);
}

/// This relay forwards every request to the real old service. Counting Execute
/// frames proves that capability rejection happens before a new command is sent.
async fn counted_relay(
    endpoint: PathBuf,
    target: PathBuf,
    fixtures: &mut Fixtures,
) -> Arc<AtomicUsize> {
    let listener = astra_platform::platform::LocalListener::bind(&endpoint).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    fixtures.tasks.push(tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let request: v::Request = astra_management::client::receive(&mut stream)
                .await
                .unwrap();
            if matches!(request.body, v::RequestBody::Execute { .. }) {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            let response = ManagerClient::new(target.clone())
                .request_v2(request)
                .await
                .unwrap();
            astra_management::client::send(&mut stream, &response)
                .await
                .unwrap();
        }
    }));
    count
}

#[tokio::test]
#[ignore = "requires ASTRA_OLD_NETWORK_BIN release directory and a freshly built current core"]
async fn real_mixed_versions_keep_legacy_tools_and_reject_unsupported_probes() {
    let old = PathBuf::from(
        std::env::var_os("ASTRA_OLD_NETWORK_BIN").expect("old release binary directory"),
    );
    let new = std::env::var_os("ASTRA_NETWORK_TEST_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    assert!(
        old.join("astra-manager").exists()
            && old.join("astra-proxy").exists()
            && new.join("astra-proxy").exists()
    );
    for (old_agent, old_core) in [(true, true), (false, true), (true, false)] {
        let directory = crate::import::tests::Fixture::new();
        astra_platform::storage::private_dir(&directory.0).unwrap();
        let endpoint = directory.0.join("agent.sock");
        let config = astra_management::config::ManagerConfig {
            state_dir: directory.0.join("state"),
            config_dir: directory.0.join("configs"),
            runtime_dir: Some(directory.0.join("run")),
            bin_dir: if old_core { old.clone() } else { new.clone() },
            local_ipc: astra_management::config::LocalIpc {
                endpoint: Some(endpoint.clone()),
            },
            ..Default::default()
        };
        let config_file = directory.write("agent.toml", toml::to_string(&config).unwrap());
        let mut child = if old_agent {
            Some(
                tokio::process::Command::new(old.join("astra-manager"))
                    .arg("--config")
                    .arg(config_file)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            )
        } else {
            None
        };
        let manager = if old_agent {
            None
        } else {
            Some(tokio::spawn(astra_manager::run_config(config)))
        };
        let direct = Client::for_target(endpoint.clone(), None);
        let mut fixtures = Fixtures { tasks: vec![] };
        let result = check_mixed(&directory, &endpoint, &mut fixtures, old_agent, old_core).await;
        let shutdown = tokio::time::timeout(
            Duration::from_secs(8),
            direct.request(v::RequestBody::Execute {
                operation_id: "shutdown-mixed".into(),
                command: v::Command::Shutdown,
            }),
        )
        .await;
        if let Some(child) = &mut child {
            if tokio::time::timeout(Duration::from_secs(8), child.wait())
                .await
                .is_err()
            {
                let _ = child.kill().await;
            }
        }
        if let Some(manager) = manager {
            let _ = tokio::time::timeout(Duration::from_secs(8), manager).await;
        }
        result.unwrap();
        shutdown.unwrap().unwrap();
    }
}

async fn check_mixed(
    directory: &crate::import::tests::Fixture,
    endpoint: &Path,
    fixtures: &mut Fixtures,
    old_agent: bool,
    old_core: bool,
) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !endpoint.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let relay = directory.0.join("relay.sock");
    let executes = counted_relay(relay.clone(), endpoint.to_owned(), fixtures).await;
    let client = Client::for_target(relay.clone(), None);
    anyhow::ensure!(
        matches!(client.availability().await?, Availability::Legacy { .. }) == old_agent,
        "actual Agent capability class"
    );
    let reserved = TcpListener::bind("127.0.0.1:0").await?;
    let config=directory.write("proxy.toml",format!("standalone=true\nsocks5_listen='{}'\n[[outbounds]]\nkind='socks5'\ntag='unused'\nserver='127.0.0.1:9'\n",reserved.local_addr()?));
    client
        .request(v::RequestBody::Execute {
            operation_id: "add-mixed".into(),
            command: v::Command::Add {
                name: "mixed".into(),
                kind: v::InstanceKind::Proxy,
                config,
            },
        })
        .await?;
    let (dns, packets) = fixtures.dns(7).await;
    let tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
    let tcp = tcp_listener.local_addr()?;
    let tcp_count = Arc::new(AtomicUsize::new(0));
    let tcp_seen = tcp_count.clone();
    fixtures.tasks.push(tokio::spawn(async move {
        while let Ok((stream, _)) = tcp_listener.accept().await {
            tcp_seen.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    }));
    let tls = TcpListener::bind("127.0.0.1:0").await?;
    let tls_address = tls.local_addr()?;
    let tls_probe = Probe::Tls {
        host: tls_address.ip().to_string(),
        port: tls_address.port(),
        server_name: Some("localhost".into()),
        compare_bundled: false,
    };
    let tls_request = project("tls", "system", tls_probe).request;
    if old_agent {
        let before = executes.load(Ordering::SeqCst);
        let error = client
            .submit("unsupported-mixed".into(), "mixed".into(), tls_request)
            .await
            .unwrap_err()
            .to_string();
        anyhow::ensure!(error.contains("Unsupported"), "{error}");
        for kind in [crate::nettools::Kind::Http, crate::nettools::Kind::Icmp] {
            let mut view = crate::nettools::View::new(&relay, "mixed".into(), &[]);
            view.set_kind(kind);
            view.target = if kind == crate::nettools::Kind::Http {
                format!("https://{tls_address}/")
            } else {
                "127.0.0.1".into()
            };
            view.count = 1;
            view.start();
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    view.poll();
                    if !view.running() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await?;
            anyhow::ensure!(
                view.error
                    .as_ref()
                    .is_some_and(|error| error.contains("版本过旧")),
                "legacy UI must explain rejection: {:?}",
                view.error
            );
            anyhow::ensure!(
                view.attempts.is_empty() && view.summary.is_none() && view.operation_id.is_none(),
                "legacy new probe has no execution evidence"
            );
        }
        anyhow::ensure!(
            executes.load(Ordering::SeqCst) == before,
            "new HTTP/ICMP must not send Execute to old Agent"
        );
    } else {
        client
            .submit("unsupported-mixed".into(), "mixed".into(), tls_request)
            .await?;
        let (report, events, _) = collect(&client, "unsupported-mixed").await?;
        anyhow::ensure!(
            report.state == v::NetworkState::Failed,
            "old core must fail new probe: {report:?}"
        );
        anyhow::ensure!(
            format!("{} {}", report.message, serde_json::to_string(&events)?)
                .contains("Unable to confirm diagnostic executable capabilities"),
            "capability rejection remains explicit: {report:?} {events:?}"
        );
        anyhow::ensure!(
            !events.iter().any(|event| event["event"] == "attempt"),
            "no new-tool attempt on old core"
        );
    }
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(100), tls.accept())
            .await
            .is_err(),
        "unsupported TLS never contacts service"
    );
    for (id, probe) in [
        (
            "legacy-tcp",
            Probe::Tcp {
                host: tcp.ip().to_string(),
                port: tcp.port(),
            },
        ),
        (
            "legacy-dns",
            Probe::Dns {
                name: "mixed.invalid".into(),
                record: "A".into(),
                server: format!("udp://{dns}"),
            },
        ),
    ] {
        let request = project(id, "system", probe).request;
        if !old_agent {
            let new_id = format!("unconfirmed-{id}");
            client
                .submit(new_id.clone(), "mixed".into(), request.clone())
                .await?;
            let (report, events, _) = collect(&client, &new_id).await?;
            anyhow::ensure!(
                report.state == v::NetworkState::Failed
                    && report
                        .message
                        .contains("Unable to confirm diagnostic executable capabilities")
                    && events.is_empty(),
                "new API never silently assumes a legacy executable capability: {report:?}"
            );
        }
        let reply = client
            .request(v::RequestBody::Execute {
                operation_id: id.into(),
                command: v::Command::NetTool {
                    instance: "mixed".into(),
                    request: request.request,
                },
            })
            .await?;
        let v::ResponseBody::NetTool(events) = reply else {
            anyhow::bail!("legacy response type");
        };
        anyhow::ensure!(
            events.iter().any(|event| event["event"] == "attempt"
                && event["error"].is_null()
                && event["rtt_ms"].is_number()),
            "{id}: {events:?}"
        );
        anyhow::ensure!(
            events.iter().any(|event| event["event"] == "done"),
            "legacy Done retained"
        );
        anyhow::ensure!(
            !events.iter().any(|event| event["event"] == "dns"
                || event["event"] == "tcp"
                || event["event"] == "tls"),
            "legacy stream must not contain new variants"
        );
    }
    anyhow::ensure!(
        packets.load(Ordering::SeqCst) == 1,
        "exactly one legacy DNS query"
    );
    anyhow::ensure!(
        tcp_count.load(Ordering::SeqCst) == 1,
        "exactly one legacy TCP connection"
    );
    if old_agent && !old_core {
        // Real old Agent readiness persists the new Core's Identity using
        // its old bincode decoder before it can commit the service.
        drop(reserved);
        client
            .request(v::RequestBody::Execute {
                operation_id: "start-new-core".into(),
                command: v::Command::StartSilent {
                    instance: "mixed".into(),
                },
            })
            .await?;
        let record_path = directory.0.join("run/processes/mixed.json");
        let record_path = if record_path.exists() {
            record_path
        } else {
            directory.0.join("state/processes/mixed.json")
        };
        let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&record_path)?)?;
        let control = record["endpoint"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("persisted control endpoint"))?;
        let identity = astra_ipc::control::identity(Path::new(control)).await?;
        anyhow::ensure!(
            identity.version == 1
                && identity.capabilities.as_ref().is_some_and(|caps| caps
                    .iter()
                    .any(|cap| cap == "network_diagnostics_stream_v1")),
            "new core identity must advertise capabilities"
        );
        anyhow::ensure!(
            record["identity"]["process"]["pid"] == identity.process.pid
                && record["identity"]["boot_id"] == identity.boot_id
                && record["identity"]["version"] == 1,
            "old decoder persisted the same core identity"
        );
        anyhow::ensure!(
            record["identity"]["capabilities"].is_null(),
            "the actual old Agent persisted only legacy identity fields"
        );
        let second = astra_ipc::control::identity(Path::new(control)).await?;
        anyhow::ensure!(
            identity == second,
            "fresh identity reads must remain stable"
        );
        println!("old Agent committed current Core Identity version={} with {} capabilities visible only to new decoder",identity.version,identity.capabilities.as_ref().map_or(0,Vec::len));
        anyhow::ensure!(
            matches!(
                client
                    .request(v::RequestBody::Query(v::Query::Runtime {
                        instance: "mixed".into()
                    }))
                    .await?,
                v::ResponseBody::Runtime(_)
            ),
            "old Agent can read new Core runtime"
        );
    }
    println!("mixed versions accepted: agent={} core={} legacy TCP/DNS success; unsupported probes sent no packets",if old_agent{"old"}else{"new"},if old_core{"old"}else{"new"});
    Ok(())
}
