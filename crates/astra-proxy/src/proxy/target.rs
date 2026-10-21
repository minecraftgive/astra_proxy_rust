//! Preserve the observed address while attaching names used for rule matching.
use crate::App;
use anyhow::Result;
use astra_ipc::Target;
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) fn select(
    app: &App,
    original: &Target,
    sniffed: Option<String>,
) -> Result<(Target, Target)> {
    let synthetic = original.host.parse().is_ok_and(|ip| app.synthetic_ip(ip));
    let mapped = app.restore_target(original);
    let route = if synthetic {
        match mapped {
            Ok(target) => target,
            Err(error) => match sniffed {
                Some(host) => Target {
                    host,
                    port: original.port,
                },
                None => return Err(error),
            },
        }
    } else {
        let mut target = mapped?;
        if let Some(host) = sniffed {
            target.host = host;
        }
        target
    };
    let dial = if synthetic {
        route.clone()
    } else {
        original.clone()
    };
    Ok((route, dial))
}

pub(crate) async fn preface<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> Result<(Vec<u8>, Option<String>)> {
    use astra_protocols::sniff::{self, Result as Inspection};
    let mut bytes = Vec::with_capacity(2048);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match sniff::inspect(&bytes) {
            Inspection::Host(host) => return Ok((bytes, Some(host))),
            Inspection::Unknown => return Ok((bytes, None)),
            Inspection::NeedMore => {}
        }
        if bytes.len() >= sniff::LIMIT {
            return Ok((bytes, None));
        }
        let mut buffer = [0; 2048];
        let capacity = buffer.len().min(sniff::LIMIT - bytes.len());
        match tokio::time::timeout_at(deadline, stream.read(&mut buffer[..capacity])).await {
            Ok(Ok(0)) | Err(_) => return Ok((bytes, None)),
            Ok(Ok(size)) => bytes.extend_from_slice(&buffer[..size]),
            Ok(Err(error)) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn app() -> App {
        App::from_text(
            &std::env::temp_dir().join("astra-target-test.toml"),
            "socks5_listen='127.0.0.1:1080'\nrule_socket='unused'\n[dns]\nlisten=[]\nfake_ip=true\n".into(),
        ).unwrap()
    }

    #[test]
    fn names_affect_rules_without_changing_real_dial_address() {
        let app = app();
        let original = Target {
            host: "203.0.113.8".into(),
            port: 443,
        };
        let (route, dial) = select(&app, &original, Some("qq.com".into())).unwrap();
        assert_eq!(route.host, "qq.com");
        assert_eq!(dial, original);
        let fake = Target {
            host: "198.18.0.8".into(),
            port: 443,
        };
        assert!(select(&app, &fake, None).is_err());
        let (route, dial) = select(&app, &fake, Some("qq.com".into())).unwrap();
        assert_eq!(route.host, "qq.com");
        assert_eq!(dial, route);
        assert!(app.restore_fake_target(&fake).is_err());
    }

    #[tokio::test]
    async fn preface_preserves_headers_and_body_for_forwarding() {
        let payload = b"GET / HTTP/1.1\r\nHost: QQ.COM\r\nContent-Length: 4\r\n\r\nbody";
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(&payload[..17]).await.unwrap();
        let writer = tokio::spawn(async move {
            tokio::task::yield_now().await;
            client.write_all(&payload[17..]).await.unwrap();
        });
        let (prefix, name) = preface(&mut server).await.unwrap();
        let mut rest = vec![];
        server.read_to_end(&mut rest).await.unwrap();
        assert_eq!([prefix, rest].concat(), payload);
        assert_eq!(name.as_deref(), Some("qq.com"));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn server_first_protocol_wait_is_bounded_and_partial_bytes_are_retained() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(b"GET ").await.unwrap();
        let (prefix, name) =
            tokio::time::timeout(std::time::Duration::from_secs(1), preface(&mut server))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(prefix, b"GET ");
        assert_eq!(name, None);
    }
}
