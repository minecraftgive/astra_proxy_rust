use astra_ipc::Target;
use astra_protocols::{
    accept::{Request, User},
    http, socks5, trojan, vless, wire,
};
use sha2::{Digest, Sha224};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn user() -> User {
    User {
        name: "test".into(),
        password: Some("secret".into()),
        uuid: Some(uuid::Uuid::from_bytes([7; 16])),
    }
}
fn target() -> Target {
    Target {
        host: "example.test".into(),
        port: 443,
    }
}
fn tcp(request: Request) -> Target {
    match request {
        Request::Tcp(t) => t,
        _ => panic!("expected TCP"),
    }
}

#[tokio::test]
async fn socks_client_and_server_preserve_tunnel_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            tcp(socks5::accept(&mut stream, &[user()]).await.unwrap()),
            target()
        );
        stream
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        stream.write_all(b"hello").await.unwrap();
    });
    let mut stream = socks5::socks5(&address, Some("test"), Some("secret"), &target())
        .await
        .unwrap();
    let mut data = [0; 5];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"hello");
    server.await.unwrap();
}

#[tokio::test]
async fn http_client_and_server_preserve_server_first_payload() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(
            http::accept(&mut stream, &[user()]).await.unwrap(),
            target()
        );
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nhello")
            .await
            .unwrap();
    });
    let mut stream = http::http_connect(&address, Some("test"), Some("secret"), &target())
        .await
        .unwrap();
    let mut data = [0; 5];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"hello");
    server.await.unwrap();
}

#[tokio::test]
async fn trojan_shared_encoder_authenticates_and_rejects_wrong_secret() {
    for password in ["secret", "wrong"] {
        let hash = hex::encode(Sha224::digest(password.as_bytes()));
        let bytes = trojan::request(&hash, 1, &target()).unwrap();
        let result = trojan::accept(&mut bytes.as_slice(), &[user()]).await;
        if password == "secret" {
            assert_eq!(tcp(result.unwrap()), target());
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn vless_accepts_valid_identity_and_rejects_unknown_identity() {
    for id in [[7u8; 16], [8u8; 16]] {
        let mut bytes = vec![0];
        bytes.extend(id);
        bytes.extend([0, 1]);
        bytes.extend(target().port.to_be_bytes());
        let address = wire::encode(&target(), 1).unwrap();
        bytes.extend(&address[..address.len() - 2]);
        let result = vless::accept(&mut bytes.as_slice(), &[user()], false, None).await;
        if id[0] == 7 {
            let (request, flow) = result.unwrap();
            assert_eq!(tcp(request), target());
            assert!(flow.is_none());
        } else {
            assert!(result.is_err());
        }
    }
}

#[tokio::test]
async fn all_address_formats_round_trip_ipv4_ipv6_and_domains() {
    for style in 0..=2 {
        for host in ["127.0.0.1", "::1", "example.test"] {
            let target = Target {
                host: host.into(),
                port: 443,
            };
            let encoded = wire::encode(&target, style).unwrap();
            assert_eq!(
                wire::address(&mut encoded.as_slice(), style, false)
                    .await
                    .unwrap(),
                target
            );
        }
    }
    for host in [String::new(), "a".repeat(254)] {
        assert!(wire::encode(&Target { host, port: 443 }, 0).is_err());
    }
}
