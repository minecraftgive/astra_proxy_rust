use super::*;
use astra_dns::wire;
use hickory_proto::{
    op::{Message, ResponseCode},
    rr::{rdata::A, RData, Record, RecordType},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};

pub(super) fn app(settings: &str) -> App {
    App::from_text(
        &std::env::temp_dir().join("astra-dns-test.toml"),
        format!("[dns]\n{settings}\n"),
    )
    .unwrap()
}
fn response(raw: &[u8], truncated: bool) -> Vec<u8> {
    let query = Message::from_vec(raw).unwrap();
    let mut reply = wire::response(&query, ResponseCode::NoError);
    reply.metadata.truncation = truncated;
    if !truncated {
        reply.answers.push(Record::from_rdata(
            query.queries[0].name().clone(),
            30,
            RData::A(A("127.0.0.1".parse().unwrap())),
        ));
    }
    reply.to_vec().unwrap()
}

// Windows can reserve a UDP port even when TCP can bind the same number.
async fn dual_protocol_listeners() -> (TcpListener, UdpSocket) {
    for _ in 0..128 {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        match UdpSocket::bind(tcp.local_addr().unwrap()).await {
            Ok(udp) => return (tcp, udp),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("bind test DNS UDP socket: {error}"),
        }
    }
    panic!("could not allocate a test port available for both TCP and UDP");
}

#[tokio::test]
async fn udp_upstream_retries_truncated_responses_over_tcp() {
    let (tcp, udp) = dual_protocol_listeners().await;
    let address = tcp.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let mut query = vec![0; 4096];
        let (size, peer) = udp.recv_from(&mut query).await.unwrap();
        udp.send_to(&response(&query[..size], true), peer)
            .await
            .unwrap();
        let (mut stream, _) = tcp.accept().await.unwrap();
        let size = stream.read_u16().await.unwrap() as usize;
        let mut query = vec![0; size];
        stream.read_exact(&mut query).await.unwrap();
        let reply = response(&query, false);
        stream.write_u16(reply.len() as u16).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    });
    let app = app(&format!("nameserver=['udp://{address}']"));
    let query = wire::query("truncate.example", RecordType::A).unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(3),
        app.dns_answer(&query.to_vec().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!Message::from_vec(&reply).unwrap().metadata.truncation);
    worker.await.unwrap();
}
#[tokio::test]
async fn local_udp_and_tcp_dns_listener_reload_keeps_fake_address_ownership() {
    let (probe, udp_probe) = dual_protocol_listeners().await;
    let address = probe.local_addr().unwrap();
    drop(probe);
    drop(udp_probe);
    let app = app("fake_ip=true");
    let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
    let guard = Prepared::bind(&[address], None)
        .await
        .unwrap()
        .start(current.clone());
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.connect(address).await.unwrap();
    let query = wire::query("listener.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    client.send(&query).await.unwrap();
    let mut reply = vec![0; 4096];
    let size = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut reply))
        .await
        .unwrap()
        .unwrap();
    let reply = Message::from_vec(&reply[..size]).unwrap();
    let RData::A(ip) = reply.answers[0].data else {
        panic!()
    };
    let mut next = super::tests::app("fake_ip=true\ncache_capacity=128");
    next.dns
        .as_mut()
        .unwrap()
        .inherit(current.load().dns.as_ref().unwrap())
        .unwrap();
    current.store(Arc::new(next));
    let prepared = Prepared::bind(&[address], Some(&guard)).await.unwrap();
    drop(guard);
    let _guard = prepared.start(current.clone());
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let reply = super::upstream::framed(tcp, &query).await.unwrap();
    let reply = Message::from_vec(&reply).unwrap();
    assert_eq!(reply.answers[0].data, RData::A(ip));
    assert_eq!(
        current
            .load()
            .dns
            .as_ref()
            .unwrap()
            .restore(ip.0.into())
            .unwrap()
            .as_deref(),
        Some("listener.example")
    );
}
#[tokio::test]
async fn tun_dns_interception_and_fake_udp_direct_egress_preserve_virtual_source() {
    let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let worker = tokio::spawn(async move {
        let mut query = vec![0; 4096];
        loop {
            let (size, peer) = upstream.recv_from(&mut query).await.unwrap();
            calls.fetch_add(1, Ordering::SeqCst);
            upstream
                .send_to(&response(&query[..size], false), peer)
                .await
                .unwrap();
        }
    });
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let echo_port = echo.local_addr().unwrap().port();
    let echo_task = tokio::spawn(async move {
        let mut bytes = vec![0; 1024];
        let (size, peer) = echo.recv_from(&mut bytes).await.unwrap();
        echo.send_to(&bytes[..size], peer).await.unwrap();
    });
    // Use a manual DIRECT group to avoid requiring a separate rule process.
    let source=format!("standalone=true\nstandalone_outbound='direct-group'\nsocks5_listen='127.0.0.1:1080'\n[proxy_groups.direct-group]\nmembers=['DIRECT']\n[dns]\nlisten=[]\nfake_ip=true\nnameserver=['udp://{upstream_address}']\n");
    let source = format!("max_udp_flows=1\nudp_targets_per_association=1\n{source}");
    let app = App::from_text(&std::env::temp_dir().join("astra-dns-udp.toml"), source).unwrap();
    let current = Arc::new(arc_swap::ArcSwap::from_pointee(app));
    let (send, receive) = tokio::sync::mpsc::channel(4);
    let (output, mut replies) = tokio::sync::mpsc::channel(4);
    let dispatcher = tokio::spawn(crate::packet::dispatch(
        receive,
        output,
        current.clone(),
        astra_ipc::Inbound::Tun,
    ));
    let source = "192.0.2.2:40000".parse().unwrap();
    let dns_dest = "8.8.8.8:53".parse().unwrap();
    let query = wire::query("udp-echo.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    send.send((query, source, dns_dest)).await.unwrap();
    let (reply, from, to) = tokio::time::timeout(Duration::from_secs(2), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(from, dns_dest);
    assert_eq!(to, source);
    let reply = Message::from_vec(&reply).unwrap();
    let RData::A(ip) = reply.answers[0].data else {
        panic!()
    };
    let fake = std::net::SocketAddr::new(ip.0.into(), echo_port);
    send.send((b"fake-udp-echo".to_vec(), source, fake))
        .await
        .unwrap();
    let (reply, from, to) = tokio::time::timeout(Duration::from_secs(3), replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, b"fake-udp-echo");
    assert_eq!(from, fake);
    assert_eq!(to, source);
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "direct fake egress must resolve real IP without querying its own fake listener"
    );
    echo_task.await.unwrap();
    // Keep the UDP echo flow alive and its sole permit occupied. DNS still
    // works and uses the new configuration on the same client tuple.
    let text = current
        .load()
        .source_text
        .replace("fake_ip=true", "fake_ip=true\nfake_ip_filter=['+.example']");
    let mut next = App::from_text(&std::env::temp_dir().join("astra-dns-udp.toml"), text).unwrap();
    next.dns
        .as_mut()
        .unwrap()
        .inherit(current.load().dns.as_ref().unwrap())
        .unwrap();
    let previous = current.load_full();
    current.store(Arc::new(next));
    previous.egress_signal.send_replace(true);
    tokio::time::timeout(Duration::from_secs(2), previous.udp_permits.idle())
        .await
        .unwrap();
    let query = wire::query("udp-echo.example", RecordType::A)
        .unwrap()
        .to_vec()
        .unwrap();
    send.send((query, source, dns_dest)).await.unwrap();
    let (reply, _, _) = tokio::time::timeout(Duration::from_secs(2), replies.recv())
        .await
        .unwrap()
        .unwrap();
    let reply = Message::from_vec(&reply).unwrap();
    assert_eq!(
        reply.answers[0].data,
        RData::A(A("127.0.0.1".parse().unwrap()))
    );
    dispatcher.abort();
    worker.abort();
}
