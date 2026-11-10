//! Real forwarding through mixed listeners and the owner-only management API.
use astra_ipc::control;
use astra_model::runtime::Command;
use astra_model::runtime::Mode;
use astra_model::runtime::Snapshot;
use std::{
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct Core {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
    address: SocketAddr,
    source: String,
}
impl Drop for Core {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // A separate CLI runtime can request Stop even when this guard is
            // dropped while the test's current-thread runtime is unwinding.
            let mut stop = std::process::Command::new(env!("CARGO_BIN_EXE_astra-proxy"));
            stop.arg("--stop")
                .arg(&self.socket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            astra_platform::platform::background_command(&mut stop, false);
            let _ = stop.status();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.child.try_wait().ok().flatten().is_none()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
        // Never destroy the only recovery snapshot after a failed shutdown.
        let journal = self.socket.with_extension("system-proxy-state.toml");
        if journal.exists() {
            eprintln!(
                "system proxy recovery snapshot retained at {}",
                journal.display()
            );
        } else {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}
impl Core {
    async fn start(a: SocketAddr, b: SocketAddr, authenticated: bool) -> Self {
        // Leave room for the control/lifecycle sockets under macOS's long TMPDIR.
        let temp = if cfg!(target_os = "macos") {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let root = temp.join(format!(
            "art-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let auth = if authenticated {
            "authentication=['alice:correct']\n"
        } else {
            ""
        };
        let source = format!("standalone=true\nstandalone_outbound='main'\nmixed_listen='{address}'\n{auth}[[outbounds]]\nkind='http'\ntag='a'\nserver='{a}'\n[[outbounds]]\nkind='http'\ntag='b'\nserver='{b}'\n[proxy_groups.main]\nmode='select'\nmembers=['a','b']\nselected='a'\n[proxy_groups.PROXY]\nmode='select'\nmembers=['a','b']\nselected='b'\n");
        std::fs::write(root.join("config/proxy.toml"), &source).unwrap();
        let socket = root.join("state/core.sock");
        let child = Self::spawn(&root, &socket);
        let core = Self {
            child,
            root,
            socket,
            address,
            source,
        };
        core.ready().await;
        core
    }
    fn spawn(root: &std::path::Path, socket: &std::path::Path) -> Child {
        let log = std::fs::File::create(root.join("core.log")).unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_astra-proxy"));
        command
            .arg(root.join("config/proxy.toml"))
            .env("ASTRA_CONTROL_SOCKET", socket)
            .env("ASTRA_RUNTIME_DIR", root.join("runtime"))
            .env("ASTRA_LOG_LEVEL", "info")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        astra_platform::platform::background_command(&mut command, false);
        command.spawn().unwrap()
    }
    async fn ready(&self) {
        for _ in 0..100 {
            if control::request(&self.socket, astra_model::control::Command::Status)
                .await
                .is_ok()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        panic!(
            "core did not become ready: {}",
            std::fs::read_to_string(self.root.join("core.log")).unwrap()
        );
    }
    async fn command(&self, command: Command) -> Snapshot {
        control::runtime(&self.socket, command).await.unwrap()
    }
    fn unchanged(&self) {
        assert_eq!(
            std::fs::read_to_string(self.root.join("config/proxy.toml")).unwrap(),
            self.source
        );
        assert_eq!(
            std::fs::read_dir(self.root.join("config")).unwrap().count(),
            1
        );
    }
}

#[tokio::test]
async fn ordinary_core_exposes_dns_telemetry_and_refreshes_it_after_reload() {
    let mut core = Core::start(
        "127.0.0.1:9".parse().unwrap(),
        "127.0.0.1:9".parse().unwrap(),
        false,
    )
    .await;
    let endpoint = astra_model::host_control::endpoint(&core.socket);
    assert!(
        astra_ipc::host::request_json_control(
            &endpoint,
            astra_model::host_control::Command::Telemetry
        )
        .await
        .is_err(),
        "ordinary mode must not depend on a resident host"
    );
    let sample = control::runtime_telemetry(&core.socket)
        .await
        .unwrap()
        .unwrap();
    let disabled: astra_model::runtime::RuntimeView = serde_json::from_value(sample).unwrap();
    assert!(!disabled.telemetry.unwrap().dns_resolver.unwrap().enabled);

    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    core.source.push_str(&format!("\n[dns]\nlisten=['127.0.0.1:{port}']\nfake_ip=true\nnameserver=['https://resolver.example/dns-query']\nfallback=['tls://1.1.1.1']\nplaintext_bypass=true\n"));
    std::fs::write(core.root.join("config/proxy.toml"), &core.source).unwrap();
    control::request(
        &core.socket,
        astra_model::control::Command::Reload { path: None },
    )
    .await
    .unwrap();
    let query = astra_dns::wire::query("ordinary.example", hickory_proto::rr::RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    let mut dns = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    dns.write_u16(query.len() as u16).await.unwrap();
    dns.write_all(&query).await.unwrap();
    let size = dns.read_u16().await.unwrap();
    let mut answer = vec![0; size as usize];
    dns.read_exact(&mut answer).await.unwrap();
    drop(dns);
    let sample: astra_model::runtime::RuntimeView = serde_json::from_value(
        control::runtime_telemetry(&core.socket)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let meta = sample.telemetry.as_ref().unwrap();
    assert_eq!(meta.coverage, astra_model::runtime::Coverage::Complete);
    assert_eq!(meta.sample_age_ms, 0);
    let resolver = meta.dns_resolver.as_ref().unwrap();
    assert!(resolver.enabled && resolver.fake_ip);
    assert_eq!(resolver.nameservers[0].transport, "DoH");
    assert_eq!(resolver.listen[0].port(), port);
    assert_eq!(
        meta.dns.as_ref().unwrap().records[0].domain,
        "ordinary.example"
    );
    assert_eq!(meta.dns.as_ref().unwrap().records[0].source, "fake_ip");
    // Old Snapshot consumers still use the same bincode command and layout.
    assert_eq!(
        core.command(Command::Snapshot).await.pid,
        sample.snapshot.pid
    );
    core.source = core
        .source
        .replace("https://resolver.example/dns-query", "tcp://1.1.1.1:53");
    std::fs::write(core.root.join("config/proxy.toml"), &core.source).unwrap();
    control::request(
        &core.socket,
        astra_model::control::Command::Reload { path: None },
    )
    .await
    .unwrap();
    let next: astra_model::runtime::RuntimeView = serde_json::from_value(
        control::runtime_telemetry(&core.socket)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let next = next.telemetry.unwrap();
    assert_eq!(next.host_boot_id, meta.host_boot_id);
    assert!(next.active_generation > meta.active_generation);
    assert!(next.sample_sequence > meta.sample_sequence);
    assert!(next.sample_elapsed_ms >= meta.sample_elapsed_ms);
    assert_eq!(next.dns_resolver.unwrap().nameservers[0].transport, "TCP");
}
struct Server {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn header(stream: &mut TcpStream) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192);
            bytes.push(stream.read_u8().await.unwrap());
        }
        bytes
    })
    .await
    .unwrap()
}
async fn server(marker: u8, proxy: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                if proxy {
                    let headers = header(&mut socket).await;
                    assert!(headers.starts_with(b"CONNECT "));
                    socket
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                }
                let mut buffer = [0u8; 1024];
                while let Ok(count) = socket.read(&mut buffer).await {
                    if count == 0 {
                        break;
                    }
                    if socket.write_all(&[marker]).await.is_err()
                        || socket.write_all(&buffer[..count]).await.is_err()
                    {
                        break;
                    }
                }
            });
        }
    });
    Server { address, task }
}
async fn socks(core: &Core, destination: SocketAddr, authenticated: bool) -> TcpStream {
    let mut socket = TcpStream::connect(core.address).await.unwrap();
    socket
        .write_all(&[5, 1, if authenticated { 2 } else { 0 }])
        .await
        .unwrap();
    let mut response = [0; 2];
    socket.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [5, if authenticated { 2 } else { 0 }]);
    if authenticated {
        socket.write_all(b"\x01\x05alice\x07correct").await.unwrap();
        socket.read_exact(&mut response).await.unwrap();
        assert_eq!(response, [1, 0]);
    }
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&destination.port().to_be_bytes());
    socket.write_all(&request).await.unwrap();
    let mut response = [0; 10];
    socket.read_exact(&mut response).await.unwrap();
    assert_eq!(&response[..2], &[5, 0]);
    socket
}
async fn exchange(socket: &mut TcpStream, marker: u8) {
    socket.write_all(b"hello").await.unwrap();
    let mut reply = [0; 6];
    tokio::time::timeout(Duration::from_secs(3), socket.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, [marker, b'h', b'e', b'l', b'l', b'o']);
}

#[tokio::test]
async fn modes_selection_traffic_disconnect_and_restart_never_modify_profile() {
    let a = server(b'A', true).await;
    let b = server(b'B', true).await;
    let direct = server(b'D', false).await;
    let mut core = Core::start(a.address, b.address, false).await;
    let mut first = socks(&core, direct.address, false).await;
    exchange(&mut first, b'A').await;
    core.command(Command::Select {
        group: "main".into(),
        member: "b".into(),
    })
    .await;
    let mut second = socks(&core, direct.address, false).await;
    exchange(&mut second, b'B').await;
    exchange(&mut first, b'A').await; // Existing sockets keep their original outbound.
    assert!(control::runtime(
        &core.socket,
        Command::Select {
            group: "main".into(),
            member: "missing".into()
        }
    )
    .await
    .is_err());
    core.command(Command::SetMode(Mode::Direct)).await;
    let mut third = socks(&core, direct.address, false).await;
    exchange(&mut third, b'D').await;
    let snapshot = core.command(Command::Snapshot).await;
    assert_eq!(snapshot.active, 3);
    assert!(snapshot.upload >= 20 && snapshot.download >= 24);
    let id = snapshot
        .connections
        .iter()
        .find(|c| c.outbound == "DIRECT")
        .unwrap()
        .id;
    core.command(Command::Disconnect(Some(id))).await;
    let closed = tokio::time::timeout(Duration::from_secs(2), third.read_u8())
        .await
        .unwrap();
    assert!(closed.is_err());
    core.command(Command::SetMode(Mode::Global)).await;
    let mut fourth = socks(&core, direct.address, false).await;
    exchange(&mut fourth, b'B').await;
    let test = core.command(Command::TestGroup("main".into())).await;
    assert!(test.groups.iter().any(|g| g.name == "main"));
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_astra-proxy"))
        .arg("--runtime")
        .arg(&core.socket)
        .arg("snapshot")
        .output()
        .unwrap();
    assert!(cli.status.success());
    let cli_snapshot: Snapshot = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(cli_snapshot.mode, Mode::Global);
    // These tests must never change the desktop's system proxy or create a TUN.
    assert!(!core.command(Command::SystemProxy(false)).await.system_proxy);
    assert!(!core.command(Command::Tun(false)).await.tun);
    let previous = test.pid;
    core.unchanged();
    drop((first, second, third, fourth));
    control::request(&core.socket, astra_model::control::Command::Stop)
        .await
        .unwrap();
    assert!(core.child.wait().unwrap().success());
    core.child = Core::spawn(&core.root, &core.socket);
    core.ready().await;
    let snapshot = core.command(Command::Snapshot).await;
    assert_ne!(snapshot.pid, previous);
    assert_eq!(snapshot.mode, Mode::Global);
    assert_eq!(
        snapshot
            .groups
            .iter()
            .find(|g| g.name == "main")
            .unwrap()
            .selected
            .as_deref(),
        Some("b")
    );
    assert_eq!(snapshot.active, 0);
    core.unchanged();
    let second_path = core.root.join("config/second.toml");
    let second_source = core.source.replace("selected='a'", "selected='b'");
    std::fs::write(&second_path, &second_source).unwrap();
    control::request(
        &core.socket,
        astra_model::control::Command::Reload {
            path: Some(second_path.clone()),
        },
    )
    .await
    .unwrap();
    let switched = core.command(Command::Snapshot).await;
    assert_eq!(
        switched.mode,
        Mode::Rule,
        "a different profile must not inherit the old profile's mode override"
    );
    assert_eq!(
        switched
            .groups
            .iter()
            .find(|g| g.name == "main")
            .unwrap()
            .selected
            .as_deref(),
        Some("b")
    );
    core.command(Command::SetMode(Mode::Direct)).await;
    control::request(
        &core.socket,
        astra_model::control::Command::Reload {
            path: Some(core.root.join("config/proxy.toml")),
        },
    )
    .await
    .unwrap();
    assert_eq!(core.command(Command::Snapshot).await.mode, Mode::Global);
    assert_eq!(
        std::fs::read_to_string(&second_path).unwrap(),
        second_source
    );
    assert_eq!(
        std::fs::read_to_string(core.root.join("config/proxy.toml")).unwrap(),
        core.source
    );
    core.command(Command::Reset).await;
    assert_eq!(core.command(Command::Snapshot).await.mode, Mode::Rule);
}

#[tokio::test]
async fn mixed_listener_requires_socks_and_http_authentication_before_forwarding() {
    let a = server(b'A', true).await;
    let b = server(b'B', true).await;
    let direct = server(b'D', false).await;
    let core = Core::start(a.address, b.address, true).await;
    let mut denied = TcpStream::connect(core.address).await.unwrap();
    denied.write_all(&[5, 1, 0]).await.unwrap();
    let mut response = [0; 2];
    denied.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [5, 255]);
    let mut wrong = TcpStream::connect(core.address).await.unwrap();
    wrong.write_all(&[5, 1, 2]).await.unwrap();
    wrong.read_exact(&mut response).await.unwrap();
    wrong.write_all(b"\x01\x05alice\x05wrong").await.unwrap();
    wrong.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [1, 1]);
    let mut accepted = socks(&core, direct.address, true).await;
    exchange(&mut accepted, b'A').await;
    let mut http = TcpStream::connect(core.address).await.unwrap();
    http.write_all(
        format!(
            "CONNECT {} HTTP/1.1\r\nHost: {}\r\n\r\n",
            direct.address, direct.address
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    assert!(header(&mut http).await.starts_with(b"HTTP/1.1 407"));
    let mut http = TcpStream::connect(core.address).await.unwrap();
    // Base64 of alice:correct; it must never be forwarded to the destination.
    http.write_all(format!("CONNECT {} HTTP/1.1\r\nHost: {}\r\nProxy-Authorization: Basic YWxpY2U6Y29ycmVjdA==\r\n\r\n", direct.address, direct.address).as_bytes()).await.unwrap();
    assert!(header(&mut http).await.starts_with(b"HTTP/1.1 200"));
    exchange(&mut http, b'A').await;
    core.unchanged();
}

#[tokio::test]
async fn authenticated_mixed_udp_flows_report_bytes_and_can_be_closed_individually() {
    use tokio::net::UdpSocket;
    let a = server(b'A', true).await;
    let b = server(b'B', true).await;
    let core = Core::start(a.address, b.address, true).await;
    core.command(Command::SetMode(Mode::Direct)).await;
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let mut buffer = [0; 1024];
        loop {
            let Ok((count, peer)) = echo.recv_from(&mut buffer).await else {
                break;
            };
            let _ = echo.send_to(&buffer[..count], peer).await;
        }
    });
    let mut control_socket = TcpStream::connect(core.address).await.unwrap();
    control_socket.write_all(&[5, 1, 2]).await.unwrap();
    let mut method = [0; 2];
    control_socket.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 2]);
    control_socket
        .write_all(b"\x01\x05alice\x07correct")
        .await
        .unwrap();
    control_socket.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [1, 0]);
    control_socket
        .write_all(&[5, 3, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let mut response = [0; 10];
    control_socket.read_exact(&mut response).await.unwrap();
    assert_eq!(&response[..2], &[5, 0]);
    let relay: SocketAddr = format!(
        "127.0.0.1:{}",
        u16::from_be_bytes([response[8], response[9]])
    )
    .parse()
    .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut packet = vec![0, 0, 0, 1, 127, 0, 0, 1];
    packet.extend_from_slice(&target.port().to_be_bytes());
    packet.extend_from_slice(b"hello");
    let mut buffer = [0; 1024];
    socket.send_to(&packet, relay).await.unwrap();
    let (count, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[count - 5..count], b"hello");
    let snapshot = core.command(Command::Snapshot).await;
    let flow = snapshot
        .connections
        .iter()
        .find(|c| c.network == "Udp")
        .unwrap();
    assert_eq!((flow.upload, flow.download), (5, 5));
    let id = flow.id;
    core.command(Command::Disconnect(Some(id))).await;
    for _ in 0..50 {
        if core
            .command(Command::Snapshot)
            .await
            .connections
            .iter()
            .all(|c| c.id != id)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(core
        .command(Command::Snapshot)
        .await
        .connections
        .iter()
        .all(|c| c.id != id));
    socket.send_to(&packet, relay).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert!(core
        .command(Command::Snapshot)
        .await
        .connections
        .iter()
        .any(|c| c.network == "Udp" && c.id != id));
    core.unchanged();
    worker.abort();
}

#[tokio::test]
async fn missing_system_proxy_listener_rejects_runtime_change_without_touching_profile() {
    let a = server(b'a', true).await;
    let b = server(b'b', true).await;
    let mut core = Core::start(a.address, b.address, false).await;
    // Every supported platform now accepts both HTTP and SOCKS endpoints.
    // DNS-only is a valid runtime with no eligible system-proxy listener, so
    // validation must reject the request before touching desktop settings.
    let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dns_address = reserved.local_addr().unwrap();
    drop(reserved);
    core.source = core
        .source
        .lines()
        .filter(|line| !line.starts_with("mixed_listen="))
        .collect::<Vec<_>>()
        .join("\n");
    core.source
        .push_str(&format!("\n[dns]\nlisten=['{dns_address}']\n"));
    std::fs::write(core.root.join("config/proxy.toml"), &core.source).unwrap();
    control::request(
        &core.socket,
        astra_model::control::Command::Reload { path: None },
    )
    .await
    .unwrap();
    let before = core.command(Command::Snapshot).await;
    let error = control::runtime(&core.socket, Command::SystemProxy(true))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("listener"));
    let after = core.command(Command::Snapshot).await;
    assert_eq!(before.pid, after.pid);
    assert!(!after.system_proxy);
    assert!(!after.tun);
    core.unchanged();
}
