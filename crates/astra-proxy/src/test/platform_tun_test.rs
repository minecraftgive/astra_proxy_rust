use super::*;
use tokio::task::JoinHandle;
#[test]
#[cfg(not(windows))]
fn gateway_routes_do_not_become_on_link_routes_on_macos() {
    let args = route(
        "203.0.113.8/32",
        Some("en0"),
        Some("192.168.1.1".parse().unwrap()),
        true,
    );
    if cfg!(target_os = "macos") {
        assert!(!args.iter().any(|value| value == "-interface"));
    }
    let capture = route("0.0.0.0/1", Some("utun20"), None, true);
    assert!(capture.iter().any(|value| value
        == if cfg!(target_os = "macos") {
            "-interface"
        } else {
            "dev"
        }));
}
#[test]
fn udp_packets_preserve_empty_payload_and_ip_family() {
    for (source, destination) in [
        ("198.18.0.2:1234", "1.1.1.1:53"),
        ("[fd00::2]:1234", "[2606:4700:4700::1111]:53"),
    ] {
        for body in [&b""[..], &b"dns-query"[..]] {
            let packet =
                udp_packet(body, source.parse().unwrap(), destination.parse().unwrap()).unwrap();
            let parsed = etherparse::SlicedPacket::from_ip(&packet).unwrap();
            let Some(etherparse::TransportSlice::Udp(udp)) = parsed.transport else {
                panic!("UDP required")
            };
            assert_eq!(udp.source_port(), 1234);
            assert_eq!(udp.destination_port(), 53);
            assert_eq!(udp.payload(), body);
        }
    }
}
async fn proxy() -> (Current, JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = server.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut jobs = JoinSet::new();
        loop {
            tokio::select! {
                Some(_) = jobs.join_next(), if !jobs.is_empty() => {},
                client = server.accept() => {
                    let (mut client, _) = client.unwrap();
                    jobs.spawn(async move {
                        let mut greeting = [0; 3]; client.read_exact(&mut greeting).await.unwrap();
                        client.write_all(&[5,0]).await.unwrap();
                        let mut header = [0;4];client.read_exact(&mut header).await.unwrap();
                        let mut address=[0;6];
                        if header[3]==1 {client.read_exact(&mut address).await.unwrap();}
                        else {assert_eq!(header[3],3);let size=client.read_u8().await.unwrap() as usize;let mut domain=vec![0;size];client.read_exact(&mut domain).await.unwrap();assert_eq!(domain,b"fake.example");client.read_exact(&mut address[4..]).await.unwrap();}
                        if header[1] == 1 {
                            if header[3]==1 {assert_eq!(&address[..4], &[203,0,113,8]);}
                            client.write_all(&[5,0,0,1,0,0,0,0,0,0]).await.unwrap();
                            let (mut read, mut write)=client.into_split();
                            let _=tokio::io::copy(&mut read,&mut write).await;
                        } else {
                            assert_eq!(header[1],3);
                            let udp=tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                            let mut reply=vec![5,0,0,1,127,0,0,1];reply.extend_from_slice(&udp.local_addr().unwrap().port().to_be_bytes());
                            client.write_all(&reply).await.unwrap();
                            let mut packet=vec![0;65536];let mut byte=[0];
                            loop {
                                tokio::select! {
                                    _=client.read(&mut byte)=>break,
                                    data=udp.recv_from(&mut packet)=>{let (n,peer)=data.unwrap();udp.send_to(&packet[..n],peer).await.unwrap();}
                                }
                            }
                        }
                    });
                }
            }
        }
    });
    let path = std::env::temp_dir().join(format!("astra-stack-{}-{port}.toml", std::process::id()));
    std::fs::write(&path,format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='selected'\nserver='127.0.0.1:{port}'\n")).unwrap();
    let app = crate::App::load(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    (Arc::new(arc_swap::ArcSwap::from_pointee(app)), task)
}
#[tokio::test]
async fn raw_udp_stack_uses_selected_proxy_including_empty_packets() {
    let (current, proxy) = proxy().await;
    let (mut stack, _, udp, _) = netstack_smoltcp::StackBuilder::default()
        .enable_udp(true)
        .enable_tcp(false)
        .enable_icmp(false)
        .build()
        .unwrap();
    let (send, mut replies) = mpsc::channel(4);
    let dispatch = tokio::spawn(udp_dispatch(udp.unwrap(), send, current));
    let source = "198.18.0.2:3456".parse().unwrap();
    let destination = "203.0.113.8:53".parse().unwrap();
    for payload in [vec![], b"dns-query".to_vec(), vec![42; 1200]] {
        stack
            .send(udp_packet(&payload, source, destination).unwrap())
            .await
            .unwrap();
        let reply = tokio::time::timeout(std::time::Duration::from_secs(3), replies.recv())
            .await
            .unwrap()
            .unwrap();
        let parsed = etherparse::SlicedPacket::from_ip(&reply).unwrap();
        let Some(etherparse::TransportSlice::Udp(packet)) = parsed.transport else {
            panic!("expected UDP")
        };
        assert_eq!(packet.source_port(), 53);
        assert_eq!(packet.destination_port(), 3456);
        assert_eq!(packet.payload(), payload);
    }
    dispatch.abort();
    proxy.abort();
}
fn tcp_packet(destination: [u8; 4], sequence: u32, ack: Option<u32>, body: &[u8]) -> Vec<u8> {
    let builder = etherparse::PacketBuilder::ipv4([192, 0, 2, 2], destination, 64)
        .tcp(3456, 443, sequence, 65535);
    let builder = if let Some(ack) = ack {
        builder.ack(ack)
    } else {
        builder.syn()
    };
    let mut packet = vec![];
    builder.write(&mut packet, body).unwrap();
    packet
}
#[tokio::test]
async fn raw_tcp_stack_handshake_and_payload_use_selected_proxy() {
    tcp_round_trip(false, b"tun-echo").await;
}
#[tokio::test]
async fn fake_ip_tcp_stack_restores_domain_and_proxies_payload() {
    tcp_round_trip(true, b"tun-echo").await;
}
#[tokio::test]
async fn sniffed_http_keeps_original_ip_and_replays_payload_through_tcp_stack() {
    tcp_round_trip(false, b"GET / HTTP/1.1\r\nHost: qq.com\r\n\r\nbody").await;
}
async fn tcp_round_trip(fake: bool, payload: &[u8]) {
    let (current, proxy) = proxy().await;
    let destination = if fake {
        let text = format!(
            "{}\n[dns]\nlisten=[]\nfake_ip=true\n",
            current.load().source_text
        );
        let app =
            crate::App::from_text(&std::env::temp_dir().join("astra-fake-tcp.toml"), text).unwrap();
        let query = astra_dns::wire::query("fake.example", hickory_proto::rr::RecordType::A)
            .unwrap()
            .to_vec()
            .unwrap();
        let reply =
            hickory_proto::op::Message::from_vec(&app.dns_answer(&query).await.unwrap()).unwrap();
        let hickory_proto::rr::RData::A(ip) = reply.answers[0].data else {
            panic!()
        };
        current.store(Arc::new(app));
        ip.0.octets()
    } else {
        [203, 0, 113, 8]
    };
    let (stack, runner, _, tcp) = netstack_smoltcp::StackBuilder::default()
        .enable_tcp(true)
        .enable_udp(false)
        .build()
        .unwrap();
    let mut jobs = JoinSet::new();
    jobs.spawn(async move {
        runner.unwrap().await.unwrap();
    });
    jobs.spawn(async move {
        tcp_dispatch(tcp.unwrap(), current).await.unwrap();
    });
    let (mut sink, mut packets) = stack.split();
    sink.send(tcp_packet(destination, 1000, None, &[]))
        .await
        .unwrap();
    let syn_ack = tokio::time::timeout(std::time::Duration::from_secs(3), packets.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let parsed = etherparse::SlicedPacket::from_ip(&syn_ack).unwrap();
    let Some(etherparse::TransportSlice::Tcp(tcp)) = parsed.transport else {
        panic!("expected SYN ACK")
    };
    assert!(tcp.syn() && tcp.ack());
    assert_eq!(tcp.acknowledgment_number(), 1001);
    let acknowledgment = tcp.sequence_number().wrapping_add(1);
    sink.send(tcp_packet(destination, 1001, Some(acknowledgment), &[]))
        .await
        .unwrap();
    sink.send(tcp_packet(destination, 1001, Some(acknowledgment), payload))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let packet = packets.next().await.unwrap().unwrap();
            let parsed = etherparse::SlicedPacket::from_ip(&packet).unwrap();
            if let Some(etherparse::TransportSlice::Tcp(tcp)) = parsed.transport {
                if !tcp.payload().is_empty() {
                    assert_eq!(tcp.payload(), payload);
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    proxy.abort();
}
