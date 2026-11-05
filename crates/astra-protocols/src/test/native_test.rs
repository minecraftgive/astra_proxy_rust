use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    time::{timeout, Duration},
};

#[tokio::test]
async fn closing_socks_udp_closes_upstream_control_connection() {
    timeout(Duration::from_secs(3), async {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut control, _) = tcp.accept().await.unwrap();
            assert_eq!(control.read_u8().await.unwrap(), 5);
            let count = control.read_u8().await.unwrap();
            let mut methods = vec![0; count as usize];
            control.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&0));
            control.write_all(&[5, 0]).await.unwrap();
            let mut request = [0; 10];
            control.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..4], &[5, 3, 0, 1]);
            let mut response = vec![5, 0, 0, 1, 127, 0, 0, 1];
            response.extend_from_slice(&relay.local_addr().unwrap().port().to_be_bytes());
            control.write_all(&response).await.unwrap();
            let mut byte = [0];
            assert_eq!(
                control.read(&mut byte).await.unwrap(),
                0,
                "control socket survived UDP close"
            );
        });
        let configs = [OutboundConfig::Socks5 {
            tag: "socks".into(),
            server: address.to_string(),
            username: None,
            password: None,
        }];
        let registry = compile(&configs, Path::new(".")).unwrap();
        let packet = connect_udp(
            &registry["socks"],
            &astra_model::Target {
                host: "example.test".into(),
                port: 53,
            },
        )
        .await
        .unwrap();
        packet.close().unwrap();
        // Keep the packet object alive: explicit close must release control immediately.
        peer.await.unwrap();
        drop(packet);
    })
    .await
    .unwrap();
}
