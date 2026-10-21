use super::*;
use hickory_proto::op::ResponseCode;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{self, pki_types::PrivatePkcs8KeyDer, RootCertStore};

#[tokio::test]
async fn doh_http2_uses_dns_wire_format_post() {
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    let (client, server) = tokio::io::duplex(16384);
    let worker = tokio::spawn(async move {
        let service = hyper::service::service_fn(
            |request: http::Request<hyper::body::Incoming>| async move {
                assert_eq!(request.method(), http::Method::POST);
                assert_eq!(request.uri().path(), "/dns-query");
                assert_eq!(
                    request.headers()[http::header::CONTENT_TYPE],
                    "application/dns-message"
                );
                let raw = request.into_body().collect().await.unwrap().to_bytes();
                let query = Message::from_vec(&raw).unwrap();
                let reply = wire::response(&query, ResponseCode::NoError)
                    .to_vec()
                    .unwrap();
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .header("Content-Type", "application/dns-message")
                        .body(Full::new(Bytes::from(reply)))
                        .unwrap(),
                )
            },
        );
        let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .serve_connection(hyper_util::rt::TokioIo::new(server), service)
            .await;
    });
    let upstream = Upstream::parse("https://dns.example/dns-query").unwrap();
    let query = wire::query("h2.example", RecordType::A).unwrap();
    let raw = tokio::time::timeout(
        Duration::from_secs(3),
        doh(client, &upstream, query.to_vec().unwrap(), true),
    )
    .await
    .unwrap()
    .unwrap();
    wire::validate_response(&query, &Message::from_vec(&raw).unwrap()).unwrap();
    worker.abort();
}

#[tokio::test]
async fn dot_and_doh_exchange_over_verified_tls_and_reject_untrusted_certificates() {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let mut roots = RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let app = crate::dns::tests::app("");
    for scheme in ["tls", "https"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = acceptor.clone();
        let worker = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(socket).await.unwrap();
            let query = if scheme == "tls" {
                let size = stream.read_u16().await.unwrap() as usize;
                let mut raw = vec![0; size];
                stream.read_exact(&mut raw).await.unwrap();
                raw
            } else {
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                    assert!(header.len() < 8192);
                }
                let header = String::from_utf8(header).unwrap().to_ascii_lowercase();
                assert!(header.starts_with("post /dns-query http/1.1"));
                assert!(header.contains("content-type: application/dns-message"));
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap();
                let mut raw = vec![0; length];
                stream.read_exact(&mut raw).await.unwrap();
                raw
            };
            let query = Message::from_vec(&query).unwrap();
            let reply = wire::response(&query, ResponseCode::NoError)
                .to_vec()
                .unwrap();
            if scheme == "tls" {
                stream.write_u16(reply.len() as u16).await.unwrap();
            } else {
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\n\r\n",reply.len()).as_bytes()).await.unwrap();
            }
            stream.write_all(&reply).await.unwrap();
            stream.flush().await.unwrap();
        });
        let upstream = Upstream::parse(&format!("{scheme}://{address}/")).unwrap();
        let query = wire::query("verified.example", RecordType::A).unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(3),
            exchange_tls(
                &app,
                &upstream,
                query.to_vec().unwrap(),
                Some(connector.clone()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        wire::validate_response(&query, &Message::from_vec(&reply).unwrap()).unwrap();
        worker.await.unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(socket).await.is_err());
    });
    let upstream = Upstream::parse(&format!("tls://{address}")).unwrap();
    assert!(exchange(
        &app,
        &upstream,
        wire::query("untrusted.example", RecordType::A)
            .unwrap()
            .to_vec()
            .unwrap()
    )
    .await
    .is_err());
    worker.await.unwrap();
}
