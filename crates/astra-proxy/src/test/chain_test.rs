use super::*;
use std::{net::SocketAddr, sync::Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
};

struct Server {
    addr: SocketAddr,
    targets: Arc<Mutex<Vec<Target>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn socks_server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let log = targets.clone();
    let app = Arc::new(
        App::from_text(
            std::path::Path::new("test-chain-server.toml"),
            "standalone=true\nstandalone_outbound='DIRECT'\nsocks5_listen='127.0.0.1:1080'\n"
                .into(),
        )
        .unwrap(),
    );
    let task = tokio::spawn(async move {
        let mut sessions = tokio::task::JoinSet::new();
        loop {
            let (mut stream, peer) = listener.accept().await.unwrap();
            let log = log.clone();
            let app = app.clone();
            sessions.spawn(async move {
                let Ok((command, target)) =
                    astra_protocols::socks5::read_socks_request(&mut stream).await
                else {
                    return;
                };
                log.lock().unwrap().push(target.clone());
                if command == 3 {
                    let _ = proxy::udp::associate_stream(stream, target, app, peer, addr).await;
                } else {
                    let mut remote = TcpStream::connect((target.host.as_str(), target.port))
                        .await
                        .unwrap();
                    astra_protocols::socks5::reply_socks5(&mut stream, 0)
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut remote).await;
                }
            });
        }
    });
    Server {
        addr,
        targets,
        task,
    }
}
fn node(name: &str, server: SocketAddr, front: Option<&str>) -> String {
    format!(
        "\n[[outbounds]]\nkind='socks5'\ntag='{name}'\nserver='{server}'\n{}",
        front
            .map(|name| format!("dialer_proxy='{name}'\n"))
            .unwrap_or_default()
    )
}
fn app(nodes: &str, groups: &str) -> App {
    App::from_text(std::path::Path::new("test-chain.toml"), format!("standalone=true\nstandalone_outbound='exit'\nsocks5_listen='127.0.0.1:1080'\nconnect_timeout_secs=2\n{nodes}\n{groups}")).unwrap()
}
#[tokio::test]
async fn chain_four_hops_and_relay_preserve_actual_path() {
    timeout(Duration::from_secs(10), async {
        let a = socks_server().await;
        let b = socks_server().await;
        let c = socks_server().await;
        let d = socks_server().await;
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = Target {
            host: "127.0.0.1".into(),
            port: echo.local_addr().unwrap().port(),
        };
        let worker = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut read, mut write) = stream.into_split();
                    let _ = tokio::io::copy(&mut read, &mut write).await;
                });
            }
        });
        let nodes = node("entry", a.addr, None)
            + &node("r1", b.addr, Some("entry"))
            + &node("r2", c.addr, Some("r1"))
            + &node("exit", d.addr, Some("r2"));
        let app = app(
            &nodes,
            "[proxy_groups.chain]\nmode='relay'\nmembers=['entry','r1','r2','exit']\n",
        );
        for action in ["exit", "chain"] {
            let mut stream = app
                .connect_tracked(
                    &target,
                    &RouteAction::Proxy(action.into()),
                    Inbound::Socks5,
                    None,
                )
                .await
                .unwrap();
            stream.write_all(b"chain!").await.unwrap();
            let mut bytes = [0; 6];
            stream.read_exact(&mut bytes).await.unwrap();
            assert_eq!(&bytes, b"chain!");
            let paths = app.chain_telemetry();
            let path = paths.connections.values().next().unwrap();
            assert_eq!(
                path.hops
                    .iter()
                    .map(|hop| hop.name.as_str())
                    .collect::<Vec<_>>(),
                ["entry", "r1", "r2", "exit"]
            );
            assert_eq!(path.hops.len(), 4);
        }
        // One extra full-chain health probe is allowed; every observed path
        // must still traverse all four proxies in order.
        assert!(a
            .targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.port == b.addr.port()));
        assert!(b
            .targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.port == c.addr.port()));
        assert!(c
            .targets
            .lock()
            .unwrap()
            .iter()
            .all(|t| t.port == d.addr.port()));
        assert!(d
            .targets
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.port == target.port));
        worker.abort();
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn chain_udp_uses_front_for_control_and_datagrams() {
    timeout(Duration::from_secs(8), async {
        let a = socks_server().await;
        let b = socks_server().await;
        let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = echo.local_addr().unwrap();
        let app = app(
            &(node("entry", a.addr, None) + &node("exit", b.addr, Some("entry"))),
            "",
        );
        let target = Target {
            host: addr.ip().to_string(),
            port: addr.port(),
        };
        let conn = proxy::udp::open_action(
            &app,
            &target,
            Inbound::Socks5,
            "127.0.0.1:1".parse().unwrap(),
            RouteAction::Proxy("exit".into()),
        )
        .await
        .unwrap();
        conn.send(b"udp chain", addr).await.unwrap();
        let mut buffer = [0; 512];
        let (n, peer) = echo.recv_from(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..n], b"udp chain");
        echo.send_to(&buffer[..n], peer).await.unwrap();
        let (n, _) = conn.recv(&mut buffer).await.unwrap();
        assert_eq!(&buffer[..n], b"udp chain");
        assert!(a
            .targets
            .lock()
            .unwrap()
            .iter()
            .any(|t| t.port == b.addr.port()));
        assert!(
            a.targets.lock().unwrap().len() >= 2,
            "front must carry TCP control and UDP association"
        );
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn chain_failure_never_dials_exit_directly() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front = closed.local_addr().unwrap();
    drop(closed);
    let exit = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let app = app(
        &(node("entry", front, None) + &node("exit", exit.local_addr().unwrap(), Some("entry"))),
        "",
    );
    assert!(app
        .connect(
            &Target {
                host: "127.0.0.1".into(),
                port: 80
            },
            &RouteAction::Proxy("exit".into())
        )
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(50), exit.accept())
        .await
        .is_err());
}

#[tokio::test]
async fn chain_vless_udp_crosses_a_tcp_only_http_entry() {
    timeout(Duration::from_secs(8), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let entry = listener.local_addr().unwrap();
        let front = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![];
            while !request.ends_with(b"\r\n\r\n") { request.push(stream.read_u8().await.unwrap()); }
            let text = String::from_utf8(request).unwrap();
            let target = astra_protocols::http::authority_target(text.split_whitespace().nth(1).unwrap()).unwrap();
            let mut exit = TcpStream::connect((target.host.as_str(), target.port)).await.unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut exit).await;
            target
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); let exit = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let user: astra_protocols::accept::User = serde_json::from_value(serde_json::json!({"name":"test", "uuid":"00000000-0000-4000-8000-000000000001"})).unwrap();
            let (request, _) = astra_protocols::vless::accept(&mut stream, &[user], false, None).await.unwrap();
            assert!(matches!(request, astra_protocols::accept::Request::Udp(_)));
            stream.write_all(&[0, 0]).await.unwrap();
            let size = stream.read_u16().await.unwrap(); let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap(); assert_eq!(bytes, b"remote-domain");
            stream.write_u16(size).await.unwrap(); stream.write_all(&bytes).await.unwrap();
        });
        let app = app(&format!("[[outbounds]]\nkind='http'\ntag='entry'\nserver='{entry}'\n[[outbounds]]\nkind='native'\ntag='exit'\ndialer_proxy='entry'\n[outbounds.config]\nprotocol='vless'\nserver='127.0.0.1'\nport={}\nuuid='00000000-0000-4000-8000-000000000001'\n", exit.port()), "");
        let target = Target { host: "remote-only.invalid".into(), port: 53 };
        let conn = proxy::udp::open_action(&app, &target, Inbound::Socks5, "127.0.0.1:1".parse().unwrap(), RouteAction::Proxy("exit".into())).await.unwrap();
        conn.send(b"remote-domain", "0.0.0.0:53".parse().unwrap()).await.unwrap();
        let mut bytes = [0; 64]; let (n, _) = conn.recv(&mut bytes).await.unwrap(); assert_eq!(&bytes[..n], b"remote-domain");
        drop(conn); server.await.unwrap();
        assert_eq!(front.await.unwrap().port, exit.port());
    }).await.unwrap();
}

#[test]
fn chain_rejects_quic_later_hops_and_tcp_only_udp_prefix() {
    let text = "[[outbounds]]\nkind='socks5'\ntag='entry'\nserver='127.0.0.1:1'\n[[outbounds]]\nkind='native'\ntag='exit'\n[outbounds.config]\nprotocol='tuic'\nserver='127.0.0.1'\nport=2\nuuid='00000000-0000-4000-8000-000000000001'\npassword='secret'\n[proxy_groups.chain]\nmode='relay'\nmembers=['entry','exit']\n";
    let config: ProxyConfig = toml::from_str(&format!(
        "standalone=true\nstandalone_outbound='exit'\nsocks5_listen='127.0.0.1:1080'\n{text}"
    ))
    .unwrap();
    assert!(config
        .validate_structure()
        .unwrap_err()
        .to_string()
        .contains("QUIC"));
    let app = app("[[outbounds]]\nkind='http'\ntag='entry'\nserver='127.0.0.1:1'\n[[outbounds]]\nkind='socks5'\ntag='exit'\nserver='127.0.0.1:2'\ndialer_proxy='entry'\n", "");
    let adapter = app
        .chain_adapter(&["entry".into(), "exit".into()], None, vec![])
        .unwrap();
    assert!(!native::supports_udp(&adapter));
}

#[tokio::test]
async fn chain_front_group_switch_cancels_lease_and_changes_future_paths() {
    let nodes = node("entry", "127.0.0.1:1".parse().unwrap(), None)
        + &node("other", "127.0.0.1:2".parse().unwrap(), None)
        + &node("exit", "127.0.0.1:3".parse().unwrap(), Some("front"));
    let app = app(&nodes, "[proxy_groups.front]\nmode='select'\nmembers=['entry','other']\nselected='entry'\ndisconnect_on_manual=true\n");
    let (_, lease) = app
        .resolve_session(&RouteAction::Proxy("exit".into()))
        .await
        .unwrap();
    assert_eq!(lease.3, ["entry", "exit"]);
    assert!(lease.4.contains(&"front".into()));
    let mut overrides = (*app.overrides.load_full()).clone();
    overrides.selected.insert("front".into(), "other".into());
    app.groups.commit_overrides(&app, overrides, Some("front"));
    timeout(Duration::from_millis(100), lease.cancelled())
        .await
        .unwrap();
    let (_, next) = app
        .resolve_session(&RouteAction::Proxy("exit".into()))
        .await
        .unwrap();
    assert_eq!(next.3, ["other", "exit"]);
    assert_eq!(lease.3, ["entry", "exit"]);
}
