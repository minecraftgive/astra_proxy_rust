use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn app() -> App {
    App::from_text(&std::env::temp_dir().join("astra-egress-session.toml"),"standalone=true\nstandalone_outbound='direct'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct]\nmembers=['DIRECT']\n".into()).unwrap()
}
#[test]
fn failed_system_proxy_rollback_keeps_both_causes_and_requires_shutdown() {
    let mut system = system_proxy::Guard::default();
    let consistency = std::sync::OnceLock::new();
    let error = failed_system_proxy_switch(
        anyhow::anyhow!("candidate rejected"),
        Ok(system_proxy::Guard::default()),
        &mut system,
        &consistency,
    );
    assert_eq!(error.to_string(), "candidate rejected");
    assert!(consistency.get().is_none());
    let error = failed_system_proxy_switch(
        anyhow::anyhow!("candidate rejected"),
        Err(anyhow::anyhow!("previous settings unavailable")),
        &mut system,
        &consistency,
    );
    let message = error.to_string();
    assert!(message.contains("candidate rejected"));
    assert!(message.contains("previous settings unavailable"));
    assert!(message.contains("outcome unknown"));
    assert_eq!(consistency.get(), Some(&message));
}
#[tokio::test]
async fn runtime_reports_a_panicked_listener_instead_of_healthy_state() {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap().to_string();
    let mut state = State {
        consistency: Default::default(),
        hosted: None,
        generation: 1,
        path: PathBuf::new(),
        text: String::new(),
        current: Arc::new(ArcSwap::from_pointee(app())),
        listeners: HashMap::from([(
            Key(Inbound::Socks5, address.clone()),
            Listener {
                task: Task::spawn(format!("SOCKS5 {address}"), async {
                    panic!("injected accept panic")
                }),
                socket: listener,
            },
        )]),
        tunnel: None,
        tproxy_udp: None,
        dns: None,
        system: Default::default(),
        system_journal: PathBuf::new(),
        group_worker: None,
        outbound_interface: None,
        elevated_tun: false,
    };
    let error = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(error) = state.task_failure().await {
                break error;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(error.contains(&address) && error.contains("injected accept panic"));
}
#[tokio::test]
async fn manager_detachment_does_not_request_core_shutdown() {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut receiver = Some(receiver);
    drop(sender);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), lifeline_closed(&mut receiver))
            .await
            .is_err()
    );
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut receiver = Some(receiver);
    sender.send(()).unwrap();
    tokio::time::timeout(Duration::from_millis(20), lifeline_closed(&mut receiver))
        .await
        .unwrap();
}
async fn client(proxy: std::net::SocketAddr, target: u16) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
    stream.write_all(&[5, 1, 0]).await.unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 0]);
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&target.to_be_bytes());
    stream.write_all(&request).await.unwrap();
    let mut reply = [0; 10];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0);
    stream
}
#[tokio::test]
async fn egress_change_closes_old_tcp_sessions_and_accepts_fresh_ones() {
    let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {Some(_)=jobs.join_next(),if !jobs.is_empty()=>{},accepted=echo.accept()=>{let (mut stream,_)=accepted.unwrap();jobs.spawn(async move{let (mut r,mut w)=stream.split();let _=tokio::io::copy(&mut r,&mut w).await;});}}
        }
    });
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let current = Arc::new(ArcSwap::from_pointee(app()));
    let previous = current.load_full();
    let listener_task = tokio::spawn(accept(listener, Inbound::Socks5, current.clone()));
    let mut old = client(address, echo_port).await;
    old.write_all(b"a").await.unwrap();
    assert_eq!(old.read_u8().await.unwrap(), b'a');
    current.store(Arc::new(app()));
    previous.egress_signal.send_replace(true);
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), old.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut fresh = client(address, echo_port).await;
    fresh.write_all(b"b").await.unwrap();
    assert_eq!(fresh.read_u8().await.unwrap(), b'b');
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        current.load_full().egress_cancelled()
    )
    .await
    .is_err());
    listener_task.abort();
    echo_task.abort();
}
