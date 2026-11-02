use anyhow::{Context, Result};
use astra_proxy::hosted::{self, Command};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn socks(port: u16, target: u16) -> Result<TcpStream> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).await?;
    socket.write_all(&[5, 1, 0]).await?;
    let mut hello = [0; 2];
    socket.read_exact(&mut hello).await?;
    anyhow::ensure!(hello == [5, 0], "SOCKS hello failed");
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend(target.to_be_bytes());
    socket.write_all(&request).await?;
    let mut reply = [0; 10];
    socket.read_exact(&mut reply).await?;
    anyhow::ensure!(reply[1] == 0, "SOCKS connect failed: {:?}", reply);
    Ok(socket)
}
async fn echo(socket: &mut TcpStream, byte: u8) -> Result<()> {
    socket.write_all(&[byte]).await?;
    anyhow::ensure!(socket.read_u8().await? == byte, "payload changed");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tcp_survives_three_generations_and_failed_prepare() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), scenario()).await?
}
async fn scenario() -> Result<()> {
    let directory = std::env::temp_dir().join(format!("ah-{}", uuid::Uuid::new_v4().simple()));
    private_test_directory(&directory)?;
    let control = directory.join("c.sock");
    let host = hosted::endpoint(&control);
    let proxy_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = listener.local_addr()?.port();
    let echo_task = tokio::spawn(async move {
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut socket,_) = accepted.unwrap();
                    jobs.spawn(async move {
                        let (mut read,mut write) = socket.split();
                        tokio::io::copy(&mut read,&mut write).await.unwrap();
                        write.write_all(b"end").await.unwrap();
                    });
                }
                Some(_) = jobs.join_next(), if !jobs.is_empty() => {}
            }
        }
    });
    let config = directory.join("proxy.toml");
    std::fs::write(&config,format!("standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:{proxy_port}'\n[proxy_groups.direct]\nmembers=['DIRECT']\n"))?;
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_astra-proxy"));
    let mut child = astra_ipc::platform::background_core(&binary)
        .arg(&config)
        .env("ASTRA_CONTROL_SOCKET", &control)
        .env("ASTRA_RUNTIME_DIR", directory.join("runtime"))
        .env("ASTRA_HOSTED_EXPERIMENTAL", "1")
        .kill_on_drop(true)
        .spawn()?;
    let result = async {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if astra_ipc::control::request(&control, astra_ipc::control::Command::Status)
                    .await
                    .is_ok()
                {
                    break;
                }
                anyhow::ensure!(child.try_wait()?.is_none(), "resident exited before ready");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let mut old = socks(proxy_port, target).await?;
        echo(&mut old, b'a').await?;
        anyhow::ensure!(
            hosted::request(
                &host,
                Command::Prepare {
                    directory: directory.join("missing"),
                    rule_socket: None
                }
            )
            .await
            .is_err(),
            "invalid candidate accepted"
        );
        echo(&mut old, b'b').await?;
        let mut generations = Vec::new();
        for _ in 0..2 {
            let generation = hosted::request(
                &host,
                Command::Prepare {
                    directory: binary.parent().unwrap().into(),
                    rule_socket: None,
                },
            )
            .await?
            .as_u64()
            .context("missing generation")?;
            hosted::request(&host, Command::Activate { generation }).await?;
            let mut stream = socks(proxy_port, target).await?;
            echo(&mut stream, b'c').await?;
            generations.push(stream);
            echo(&mut old, b'd').await?;
        }
        let status: hosted::Status =
            serde_json::from_value(hosted::request(&host, Command::Status).await?)?;
        anyhow::ensure!(
            status.generations.iter().filter(|g| g.flows > 0).count() == 3,
            "live generations were not preserved"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        echo(&mut old, b'e').await?;
        old.shutdown().await?;
        let mut trailer = Vec::new();
        old.read_to_end(&mut trailer).await?;
        anyhow::ensure!(trailer == b"end", "half-close lost response");
        drop(generations);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let _ = astra_ipc::control::request(&control, astra_ipc::control::Command::Stop).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    echo_task.abort();
    // Test-owned directory; never delete a caller-supplied path.
    let _ = std::fs::remove_dir_all(&directory);
    result
}

struct Resident {
    directory: PathBuf,
    control: PathBuf,
    host: PathBuf,
    port: u16,
    child: tokio::process::Child,
}
fn private_test_directory(path: &std::path::Path) -> std::io::Result<()> {
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
impl Resident {
    async fn start(extra: &str) -> Result<Self> {
        Self::start_with_group(extra, "members=['DIRECT']").await
    }
    async fn start_with_group(extra: &str, group: &str) -> Result<Self> {
        let directory = std::env::temp_dir().join(format!("ah-{}", uuid::Uuid::new_v4().simple()));
        private_test_directory(&directory)?;
        let control = directory.join("c.sock");
        let host = hosted::endpoint(&control);
        let port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
        let config = directory.join("proxy.toml");
        std::fs::write(&config, format!("standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:{port}'\n{extra}\n[proxy_groups.direct]\n{group}\n"))?;
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_astra-proxy"));
        let child = astra_ipc::platform::background_core(&binary)
            .arg(&config)
            .env("ASTRA_CONTROL_SOCKET", &control)
            .env("ASTRA_RUNTIME_DIR", directory.join("runtime"))
            .env("ASTRA_HOSTED_EXPERIMENTAL", "1")
            .kill_on_drop(true)
            .spawn()?;
        let mut resident = Self {
            directory,
            control,
            host,
            port,
            child,
        };
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if astra_ipc::control::request(
                    &resident.control,
                    astra_ipc::control::Command::Status,
                )
                .await
                .is_ok()
                {
                    break;
                }
                anyhow::ensure!(
                    resident.child.try_wait()?.is_none(),
                    "resident exited before ready"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(resident)
    }
    async fn prepare(&self) -> Result<u64> {
        let binary = PathBuf::from(env!("CARGO_BIN_EXE_astra-proxy"));
        hosted::request(
            &self.host,
            Command::Prepare {
                directory: binary.parent().unwrap().into(),
                rule_socket: None,
            },
        )
        .await?
        .as_u64()
        .context("missing candidate generation")
    }
    async fn status(&self) -> Result<hosted::Status> {
        Ok(serde_json::from_value(
            hosted::request(&self.host, Command::Status).await?,
        )?)
    }
    async fn stop(&mut self) {
        let _ = astra_ipc::control::request(&self.control, astra_ipc::control::Command::Stop).await;
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
        // This path is created above with a unique test-only name.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_manual_group_change_disconnects_current_and_draining_generations() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = listener.local_addr()?.port();
    let echo_task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await?;
            tasks.spawn(async move {
                let (mut read, mut write) = socket.split();
                tokio::io::copy(&mut read, &mut write).await
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, std::io::Error>(())
    });
    let mut core = Resident::start_with_group(
        "[proxy_groups.first]\nmembers=['DIRECT']\n[proxy_groups.second]\nmembers=['DIRECT']",
        "mode='select'\nmembers=['first','second']\nselected='first'\ndisconnect_on_manual=true",
    )
    .await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let mut old = socks(core.port, target).await?;
        echo(&mut old, b'a').await?;
        let generation = core.prepare().await?;
        hosted::request(&core.host, Command::Activate { generation }).await?;
        let mut current = socks(core.port, target).await?;
        echo(&mut current, b'b').await?;
        astra_ipc::control::runtime(
            &core.control,
            astra_ipc::runtime::Command::Select {
                group: "direct".into(),
                member: "second".into(),
            },
        )
        .await?;
        for stream in [&mut old, &mut current] {
            let ended = tokio::time::timeout(Duration::from_secs(5), stream.read_u8()).await?;
            anyhow::ensure!(
                ended.is_err(),
                "explicit group switch left an old connection alive"
            );
        }
        let mut fresh = socks(core.port, target).await?;
        echo(&mut fresh, b'c').await?;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.stop().await;
    echo_task.abort();
    result?
}

#[tokio::test(flavor = "multi_thread")]
async fn previous_host_snapshot_id_cannot_disconnect_a_restarted_worker() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = listener.local_addr()?.port();
    let echo_task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await?;
            tasks.spawn(async move {
                let (mut read, mut write) = socket.split();
                tokio::io::copy(&mut read, &mut write).await
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, std::io::Error>(())
    });
    async fn visible_id(core: &Resident) -> Result<u64> {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let snapshot = astra_ipc::control::runtime(
                    &core.control,
                    astra_ipc::runtime::Command::Snapshot,
                )
                .await?;
                if let Some(connection) = snapshot.connections.first() {
                    return Ok(connection.id);
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await?
    }
    let mut first = Resident::start("").await?;
    let previous = async {
        let mut stream = socks(first.port, target).await?;
        echo(&mut stream, b'a').await?;
        visible_id(&first).await
    }
    .await;
    first.stop().await;
    let previous = previous?;
    let mut restarted = Resident::start("").await?;
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = socks(restarted.port, target).await?;
        echo(&mut stream, b'b').await?;
        let current = visible_id(&restarted).await?;
        anyhow::ensure!(
            previous != current,
            "restarted host reused external connection identity"
        );
        anyhow::ensure!(
            astra_ipc::control::runtime(
                &restarted.control,
                astra_ipc::runtime::Command::Disconnect(Some(previous))
            )
            .await
            .is_err(),
            "stale host ID was accepted"
        );
        echo(&mut stream, b'c').await?;
        astra_ipc::control::runtime(
            &restarted.control,
            astra_ipc::runtime::Command::Disconnect(Some(current)),
        )
        .await?;
        let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_u8()).await?;
        anyhow::ensure!(
            closed.is_err(),
            "current opaque ID did not disconnect its worker flow"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    restarted.stop().await;
    echo_task.abort();
    result?
}

async fn udp_association(
    port: u16,
) -> Result<(TcpStream, tokio::net::UdpSocket, std::net::SocketAddr)> {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let mut control = TcpStream::connect(("127.0.0.1", port)).await?;
    control.write_all(&[5, 1, 0]).await?;
    let mut hello = [0; 2];
    control.read_exact(&mut hello).await?;
    anyhow::ensure!(hello == [5, 0], "SOCKS hello rejected");
    let mut request = vec![5, 3, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&socket.local_addr()?.port().to_be_bytes());
    control.write_all(&request).await?;
    let mut reply = [0; 10];
    control.read_exact(&mut reply).await?;
    anyhow::ensure!(
        reply[..8] == [5, 0, 0, 1, 127, 0, 0, 1],
        "UDP ASSOCIATE failed"
    );
    let relay =
        std::net::SocketAddr::from(([127, 0, 0, 1], u16::from_be_bytes([reply[8], reply[9]])));
    Ok((control, socket, relay))
}
fn udp_packet(target: u16, body: &[u8]) -> Vec<u8> {
    let mut packet = vec![0, 0, 0, 1, 127, 0, 0, 1];
    packet.extend_from_slice(&target.to_be_bytes());
    packet.extend_from_slice(body);
    packet
}
async fn udp_echo(
    socket: &tokio::net::UdpSocket,
    relay: std::net::SocketAddr,
    target: u16,
    body: &[u8],
) -> Result<()> {
    socket.send_to(&udp_packet(target, body), relay).await?;
    let mut reply = [0; 1024];
    let (count, from) =
        tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut reply)).await??;
    anyhow::ensure!(
        from == relay && count >= 10 && &reply[10..count] == body,
        "UDP payload or association changed"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn socks_udp_association_keeps_generation_and_global_quota_across_activation() -> Result<()> {
    let mut core = Resident::start("max_udp_flows=2\nudp_targets_per_association=2").await?;
    let mut echo_tasks = Vec::new();
    let mut targets = Vec::new();
    for _ in 0..3 {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        targets.push(socket.local_addr()?.port());
        echo_tasks.push(tokio::spawn(async move {
            let mut body = [0; 1024];
            loop {
                let (size, source) = socket.recv_from(&mut body).await?;
                socket.send_to(&body[..size], source).await?;
            }
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(())
        }));
    }
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let original = core.status().await?.active;
        let (old_control, old, old_relay) = udp_association(core.port).await?;
        udp_echo(&old, old_relay, targets[0], b"before").await?;
        let generation = core.prepare().await?;
        hosted::request(&core.host, Command::Activate { generation }).await?;
        udp_echo(&old, old_relay, targets[0], b"retained").await?;
        // A new destination on an existing association belongs to its old worker.
        udp_echo(&old, old_relay, targets[1], b"new-old-target").await?;
        let (new_control, new, new_relay) = udp_association(core.port).await?;
        let status = core.status().await?;
        anyhow::ensure!(
            status
                .generations
                .iter()
                .find(|g| g.generation == original)
                .is_some_and(|g| g.flows == 1),
            "old UDP association lost its generation"
        );
        new.send_to(&udp_packet(targets[2], b"quota-full"), new_relay)
            .await?;
        let mut response = [0; 1024];
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_millis(250), new.recv_from(&mut response))
                .await
                .is_err(),
            "new worker bypassed global UDP quota"
        );
        // Dropping the old control stream retires both targets and releases quota.
        drop(old_control);
        drop(old);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                new.send_to(&udp_packet(targets[2], b"after-release"), new_relay)
                    .await?;
                if let Ok(Ok((size, _))) =
                    tokio::time::timeout(Duration::from_millis(200), new.recv_from(&mut response))
                        .await
                {
                    anyhow::ensure!(
                        size >= 10 && &response[10..size] == b"after-release",
                        "invalid new UDP response"
                    );
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        drop(new_control);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.stop().await;
    for task in echo_tasks {
        task.abort();
    }
    result?
}

#[tokio::test(flavor = "multi_thread")]
async fn paused_activation_preserves_rollback_target_and_queues_new_connections() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = listener.local_addr()?.port();
    let echo_task = tokio::spawn(async move {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            let (mut socket, _) = listener.accept().await?;
            tasks.spawn(async move {
                let (mut read, mut write) = socket.split();
                tokio::io::copy(&mut read, &mut write).await
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, std::io::Error>(())
    });
    let mut core = Resident::start("").await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let original = core.status().await?.active;
        let generation = core.prepare().await?;
        hosted::request(&core.host, Command::Pause).await?;
        hosted::request(&core.host, Command::Activate { generation }).await?;
        let port = core.port;
        let mut queued = tokio::spawn(async move { socks(port, target).await });
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_millis(150), &mut queued)
                .await
                .is_err(),
            "paused admission accepted a connection"
        );
        tokio::time::sleep(Duration::from_millis(1300)).await;
        anyhow::ensure!(
            core.status()
                .await?
                .generations
                .iter()
                .any(|g| g.generation == original),
            "paused update reaped rollback generation"
        );
        hosted::request(
            &core.host,
            Command::SelectExisting {
                generation: original,
            },
        )
        .await?;
        hosted::request(&core.host, Command::Resume).await?;
        let mut old = tokio::time::timeout(Duration::from_secs(5), queued).await???;
        echo(&mut old, b'r').await?;
        anyhow::ensure!(
            core.status().await?.active == original,
            "rollback did not restore generation"
        );
        // Pausing admission never suspends established streams.
        hosted::request(&core.host, Command::Pause).await?;
        echo(&mut old, b'p').await?;
        hosted::request(&core.host, Command::Resume).await?;
        anyhow::ensure!(
            hosted::request(
                &core.host,
                Command::Prepare {
                    directory: core.directory.join("missing"),
                    rule_socket: None
                }
            )
            .await
            .is_err(),
            "invalid candidate accepted"
        );
        echo(&mut old, b'e').await?;
        let mut fresh = socks(core.port, target).await?;
        echo(&mut fresh, b'n').await?;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.stop().await;
    echo_task.abort();
    result?
}

#[tokio::test(flavor = "multi_thread")]
async fn fake_ip_queried_before_upgrade_is_resolved_by_new_worker() -> Result<()> {
    use hickory_proto::{
        op::{Message, ResponseCode},
        rr::{rdata::A, RData, Record, RecordType},
    };
    let upstream = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
    let upstream_address = upstream.local_addr()?;
    let dns_task = tokio::spawn(async move {
        let mut bytes = [0; 4096];
        loop {
            let (size, peer) = upstream.recv_from(&mut bytes).await?;
            let query = Message::from_vec(&bytes[..size])?;
            let mut reply = astra_dns::wire::response(&query, ResponseCode::NoError);
            if query.queries[0].query_type() == RecordType::A {
                anyhow::ensure!(
                    query.queries[0].name().to_ascii() == "cached-generation.test.",
                    "fake mapping restored the wrong hostname"
                );
                reply.answers.push(Record::from_rdata(
                    query.queries[0].name().clone(),
                    60,
                    RData::A(A("127.0.0.1".parse().unwrap())),
                ));
            }
            upstream.send_to(&reply.to_vec()?, peer).await?;
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    });
    let dns_port = TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let target = listener.local_addr()?.port();
    let echo_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let (mut read, mut write) = socket.split();
        tokio::io::copy(&mut read, &mut write).await
    });
    let mut core = Resident::start(&format!("[dns]\nlisten=['127.0.0.1:{dns_port}']\nfake_ip=true\nfake_ip_filter=[]\nipv6=false\nnameserver=['udp://{upstream_address}']")).await?;
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let query = astra_dns::wire::query("cached-generation.test", RecordType::A)?.to_vec()?;
        socket.send_to(&query, ("127.0.0.1", dns_port)).await?;
        let mut buffer = [0; 4096];
        let (size, _) = socket.recv_from(&mut buffer).await?;
        let reply = Message::from_vec(&buffer[..size])?;
        let fake = reply
            .answers
            .iter()
            .find_map(|record| match record.data {
                RData::A(A(ip)) => Some(ip),
                _ => None,
            })
            .context("missing Fake IP")?;
        anyhow::ensure!(
            fake.octets()[..2] == [198, 18],
            "DNS did not allocate Fake IP"
        );
        let generation = core.prepare().await?;
        hosted::request(&core.host, Command::Activate { generation }).await?;
        let mut proxy = TcpStream::connect(("127.0.0.1", core.port)).await?;
        proxy.write_all(&[5, 1, 0]).await?;
        let mut method = [0; 2];
        proxy.read_exact(&mut method).await?;
        anyhow::ensure!(method == [5, 0], "SOCKS negotiation failed");
        let mut request = vec![5, 1, 0, 1];
        request.extend_from_slice(&fake.octets());
        request.extend_from_slice(&target.to_be_bytes());
        proxy.write_all(&request).await?;
        let mut response = [0; 10];
        proxy.read_exact(&mut response).await?;
        anyhow::ensure!(response[1] == 0, "new worker lost old Fake IP mapping");
        echo(&mut proxy, b'f').await?;
        Ok::<_, anyhow::Error>(())
    })
    .await;
    core.stop().await;
    dns_task.abort();
    echo_task.abort();
    result?
}
