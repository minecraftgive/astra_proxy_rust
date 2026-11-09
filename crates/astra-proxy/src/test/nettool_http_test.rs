use super::*;
use astra_model::nettool::{DetailedRequest, Egress, Probe, Request};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn request() -> DetailedRequest {
    Request {
        config: String::new(),
        egress: Egress::System,
        probe: Probe::Http {
            url: "http://127.0.0.1".into(),
            method: "GET".into(),
            compare_bundled: false,
        },
        count: 1,
        interval_ms: 0,
        timeout_ms: 1000,
    }
    .into()
}
fn record(events: &Mutex<Vec<Event>>, event: Event) -> Result<()> {
    assert!(serde_json::to_vec(&event)?.len() < 56 * 1024);
    events.lock().unwrap().push(event);
    Ok(())
}
async fn fixture(
    response: String,
    requests: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests_seen = vec![];
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            requests_seen.push(String::from_utf8(header).unwrap());
            let _ = stream.write_all(response.as_bytes()).await;
        }
        requests_seen
    });
    (format!("http://{address}"), task)
}
#[tokio::test]
async fn get_and_head_are_bounded_and_authentication_fields_are_redacted() {
    let body = r#"{"ip":"192.0.2.9","access_token":"body-secret","nested":{"password":"password-secret"}}"#;
    for method in ["GET", "HEAD"] {
        let response=format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nSet-Cookie: cookie-secret\r\nX-Api-Key: header-secret\r\nConnection: close\r\n\r\n{}",body.len(),if method=="GET"{body}else{""});
        let (url, server) = fixture(response, 1).await;
        let request = request();
        let route = Route::new(&request).unwrap();
        let events = Mutex::new(vec![]);
        run(
            &route,
            1,
            &format!("{url}/private-path?token=query-secret"),
            method,
            false,
            false,
            &|event| record(&events, event),
        )
        .await
        .unwrap();
        let seen = server.await.unwrap();
        assert!(seen[0].starts_with(&format!(
            "{method} /private-path?token=query-secret HTTP/1.1"
        )));
        let saved = serde_json::to_string(&events.into_inner().unwrap()).unwrap();
        for secret in [
            "body-secret",
            "password-secret",
            "cookie-secret",
            "header-secret",
            "query-secret",
            "private-path",
        ] {
            assert!(!saved.contains(secret), "{saved}");
        }
    }
}
#[tokio::test]
async fn redirects_stop_after_five_and_unknown_response_headers_are_hidden() {
    let response="HTTP/1.1 302 Found\r\nLocation: /again?token=redirect-secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned();
    let (url, server) = fixture(response, 6).await;
    let request = request();
    let route = Route::new(&request).unwrap();
    let events = Mutex::new(vec![]);
    assert!(run(&route, 1, &url, "GET", false, false, &|event| record(
        &events, event
    ))
    .await
    .unwrap_err()
    .to_string()
    .contains("limit"));
    assert_eq!(server.await.unwrap().len(), 6);
    let events = events.into_inner().unwrap();
    assert_eq!(events.iter().filter(|e|matches!(e,Event::Http {detail,..} if matches!(detail.as_ref(),HttpDetail::Redirect {followed:true,..}))).count(),5);
    assert!(!serde_json::to_string(&events)
        .unwrap()
        .contains("redirect-secret"));
}
#[test]
fn url_and_preview_redaction_preserve_no_embedded_credentials() {
    assert!(Endpoint::parse("https://user:password@localhost/").is_err());
    let url = Url::parse("https://localhost/").unwrap();
    let (location, _) = safe_header(
        "location",
        "https://user:secret@example.test/path?token=secret",
        &url,
    );
    assert_eq!(location, "https://example.test/[path and query omitted]");
    let (body, redacted) = preview("text/html", b"<input name=token value=secret>", false);
    assert!(redacted);
    assert!(!body.contains("value=secret"));
    let (body, redacted) = preview(
        "application/json",
        br#"{"next":"https://user:secret@example.test/path?token=secret"}"#,
        false,
    );
    assert!(redacted);
    assert!(!body.contains("secret"));
}

#[test]
fn json_array_redaction_visits_every_item_after_the_first_change() {
    let mut data = serde_json::json!([
        {"token":"first-secret"}, {"password":"second-secret"},
        [{"url":"https://example.test/private?token=third-secret"}]
    ]);
    assert!(redact_json(&mut data, 0));
    let text = data.to_string();
    for secret in ["first-secret", "second-secret", "third-secret"] {
        assert!(!text.contains(secret));
    }
}

async fn https_fixture(
    response: String,
) -> (
    String,
    super::super::tls::TrustSnapshot,
    tokio::task::JoinHandle<String>,
) {
    let (acceptor, trust) = super::super::tls::tests::fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(stream).await.unwrap();
        let mut header = vec![];
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        let _ = stream.write_all(response.as_bytes()).await;
        String::from_utf8(header).unwrap()
    });
    (
        format!("https://localhost:{port}/#bootstrap=127.0.0.1"),
        trust,
        task,
    )
}
#[tokio::test]
async fn https_ip_is_separate_source_and_uses_verified_system_snapshot() {
    let body = "203.0.113.9\n";
    let (url,trust,server)=https_fixture(format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())).await;
    let request = request();
    let route = Route::new(&request).unwrap();
    route.tls.set_fixture(trust);
    let events = Mutex::new(vec![]);
    run(&route, 1, &url, "GET", true, true, &|event| {
        record(&events, event)
    })
    .await
    .unwrap();
    server.await.unwrap();
    assert!(events.lock().unwrap().iter().any(|event|matches!(event,Event::PublicIp {address,source,..} if address=="203.0.113.9"&&source=="https")));
}
#[tokio::test]
async fn https_downgrade_is_stopped_before_any_http_socket_is_opened() {
    let downgrade = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (url,trust,server)=https_fixture(format!("HTTP/1.1 302 Found\r\nLocation: http://{}/private?token=secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",downgrade.local_addr().unwrap())).await;
    let request = request();
    let route = Route::new(&request).unwrap();
    route.tls.set_fixture(trust);
    let events = Mutex::new(vec![]);
    assert!(run(&route, 1, &url, "GET", false, false, &|event| record(
        &events, event
    ))
    .await
    .unwrap_err()
    .to_string()
    .contains("downgrade"));
    server.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), downgrade.accept())
            .await
            .is_err()
    );
    assert!(events.lock().unwrap().iter().any(|event|matches!(event,Event::Http {detail,..} if matches!(detail.as_ref(),HttpDetail::Redirect {followed:false,reason,..} if reason.contains("downgrade")))));
}
#[tokio::test]
async fn tls_failure_retains_certificates_and_prevents_http_request_bytes() {
    let (acceptor, _) = super::super::tls::tests::fixture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        assert!(acceptor.accept(stream).await.is_err());
    });
    let request = request();
    let route = Route::new(&request).unwrap();
    route
        .tls
        .set_fixture(super::super::tls::TrustSnapshot::fixture(
            tokio_rustls::rustls::RootCertStore::empty(),
        ));
    let events = Mutex::new(vec![]);
    assert!(run(
        &route,
        1,
        &format!("https://localhost:{port}/#bootstrap=127.0.0.1"),
        "GET",
        false,
        false,
        &|event| record(&events, event)
    )
    .await
    .is_err());
    server.await.unwrap();
    assert!(events.lock().unwrap().iter().any(|event|matches!(event,Event::Tls {detail,..} if matches!(detail.as_ref(),astra_model::network_tls::TlsDetail::CertificateDer {..}))));
    assert!(!events.lock().unwrap().iter().any(|event|matches!(event,Event::Http {detail,..} if matches!(detail.as_ref(),HttpDetail::Response {..}))));
}
#[tokio::test]
async fn headers_and_body_limits_are_enforced_without_unbounded_evidence() {
    let body = "x".repeat(70 * 1024);
    let response=format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
    let (url, server) = fixture(response, 1).await;
    let request = request();
    let route = Route::new(&request).unwrap();
    let events = Mutex::new(vec![]);
    run(&route, 1, &url, "GET", false, false, &|event| {
        record(&events, event)
    })
    .await
    .unwrap();
    server.await.unwrap();
    let events = events.into_inner().unwrap();
    assert!(events.iter().any(|event|matches!(event,Event::Http {detail,..} if matches!(detail.as_ref(),HttpDetail::Response {body_truncated:true,preview_bytes:65536,..}))));
    assert!(
        events
            .iter()
            .filter_map(|event| {
                if let Event::Http { detail, .. } = event {
                    if let HttpDetail::Preview { text, .. } = detail.as_ref() {
                        return Some(text.len());
                    }
                }
                None
            })
            .sum::<usize>()
            <= 65536
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nX-Large: {}\r\nContent-Length: 0\r\n\r\n",
        "y".repeat(40 * 1024)
    );
    let (url, server) = fixture(response, 1).await;
    assert!(run(&route, 2, &url, "GET", false, false, &|_| Ok(()))
        .await
        .is_err());
    server.await.unwrap();
}
