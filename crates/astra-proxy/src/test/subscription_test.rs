use super::*;
#[tokio::test]
async fn mihomo_download_follows_long_redirect_chain_and_decodes_gzip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for hop in 0..=5 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with(&format!("get /{hop}?token=private ")));
            assert!(request.contains("user-agent: mihomo/1.19.0\r\n"));
            assert!(request.contains("accept: */*\r\n"));
            assert!(request.contains("accept-encoding: gzip, deflate\r\n"));
            if hop < 5 {
                stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: /{}?token=private\r\nContent-Length: 0\r\n\r\n", hop + 1).as_bytes()).await.unwrap();
            } else {
                let data = [
                    31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 203, 203, 79, 73, 45, 6, 0, 252, 5, 61, 29,
                    5, 0, 0, 0,
                ];
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", data.len()).as_bytes()).await.unwrap();
                stream.write_all(&data).await.unwrap();
            }
        }
    });
    let app = App::from_text(
            Path::new("gzip-download.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:9'\n".into(),
        )
        .unwrap();
    assert_eq!(
        download(
            &app,
            &format!("http://{address}/0?token=private"),
            &Subscription::default().user_agent,
            "DIRECT"
        )
        .await
        .unwrap(),
        "nodes"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn download_uses_selected_proxy_group_and_rejects_missing_groups() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        assert!(String::from_utf8(request)
            .unwrap()
            .starts_with("CONNECT example.com:80 "));
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .await
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
        }
        assert!(String::from_utf8(request).unwrap().starts_with("GET /sub "));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nnodes")
            .await
            .unwrap();
    });
    let app = App::from_text(Path::new("proxy-download.toml"), format!(
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='http'\ntag='node'\nserver='{address}'\n[proxy_groups.custom]\nmembers=['node']\n"
        )).unwrap();
    assert_eq!(
        download(&app, "http://example.com/sub", "astra-test", "custom")
            .await
            .unwrap(),
        "nodes"
    );
    assert!(
        download(&app, "http://example.com/sub", "astra-test", "missing")
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn stopped_refresh_updates_cache_and_preserves_saved_config_and_last_good_nodes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let directory =
        std::env::temp_dir().join(format!("astra-offline-refresh-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("proxy.toml");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for body in [
            "proxies: [{name: imported, type: http, server: 127.0.0.1, port: 1234}]",
            "invalid yaml: [",
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    });
    let text = format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscriptions.remote]\nurl='http://{address}/sub'\nproxy='DIRECT'\n");
    std::fs::write(&path, &text).unwrap();
    assert!(
        refresh_once(&path, App::from_text(&path, text.clone()).unwrap())
            .await
            .unwrap()
            .contains("Updated 1")
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    let cached = cached_app(&path, App::from_text(&path, text.clone()).unwrap());
    assert_eq!(cached.config.proxy_groups["remote"].members, ["imported"]);
    assert!(
        refresh_once(&path, App::from_text(&path, text.clone()).unwrap())
            .await
            .is_err()
    );
    assert!(cached_app(&path, App::from_text(&path, text).unwrap())
        .outbounds
        .contains_key("imported"));
    server.await.unwrap();
    std::fs::remove_file(super::path(&path).unwrap()).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
#[tokio::test]
async fn protected_download_follows_relative_redirect_and_checks_status() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for (path, response) in [
            (
                "/first",
                "HTTP/1.1 302 Found\r\nLocation: /second\r\nContent-Length: 0\r\n\r\n",
            ),
            (
                "/second",
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nnodes",
            ),
            (
                "/error?token=secret",
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with(&format!("GET {path} ")));
            assert!(request.to_lowercase().contains("user-agent: astra-test"));
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let app = App::from_text(std::path::Path::new("download.toml"),
            "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:1'\n".into()).unwrap();
    assert_eq!(
        download(
            &app,
            &format!("http://{address}/first"),
            "astra-test",
            "DIRECT"
        )
        .await
        .unwrap(),
        "nodes"
    );
    let error = download(
        &app,
        &format!("http://{address}/error?token=secret"),
        "astra-test",
        "DIRECT",
    )
    .await
    .unwrap_err();
    let diagnostic = astra_convert::subscription::diagnostics::error(&error);
    assert!(diagnostic.contains("HTTP 403") && !diagnostic.contains("token=secret"));
    server.await.unwrap();
}
#[test]
fn cache_survives_replacement_and_is_ignored_after_config_changes_or_corruption() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("astra-node-cache-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let config = directory.join("proxy.toml");
    let base = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscription]\nurl='http://127.0.0.1:1/config'\n";
    let settings = Subscription {
        url: "http://127.0.0.1:1/config".into(),
        managed_nodes: vec!["cached-node".into()],
        ..Default::default()
    };
    let effective = format!(
        "{base}\n[[outbounds]]\nkind='socks5'\ntag='cached-node'\nserver='127.0.0.1:1234'\n"
    );
    let cache = Cache {
        base: base.into(),
        effective: effective.clone(),
        ownership: settings,
        subscriptions: Default::default(),
    };
    save(&config, &cache).unwrap();
    save(&config, &cache).unwrap();
    assert_eq!(read(&config, base).unwrap().effective, effective);
    let mut original = App::from_text(&config, base.into()).unwrap();
    original.effective_text = Some(base.into());
    let app = cached_app(&config, original);
    assert!(app.outbounds.contains_key("cached-node"));
    assert_eq!(
        app.standalone_action,
        Some(astra_model::RouteAction::Proxy("PROXY".into()))
    );
    assert_eq!(
        app.config.proxy_groups["subscription"].members,
        ["cached-node"]
    );
    assert_eq!(app.config.proxy_groups["PROXY"].members, ["subscription"]);
    assert_eq!(app.source_text, base);
    // This is the immutable document handed to a worker, even if the
    // shared cache changes or disappears after candidate preparation.
    let worker = App::from_text(&config, app.effective_text.clone().unwrap()).unwrap();
    assert!(worker.outbounds.contains_key("cached-node"));
    assert_eq!(
        worker.config.proxy_groups["subscription"].members,
        ["cached-node"]
    );
    assert!(read(&config, &format!("{base}\n# changed")).is_none());
    std::fs::write(path(&config).unwrap(), b"invalid").unwrap();
    let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
    assert!(app.outbounds.is_empty());
    std::fs::remove_file(path(&config).unwrap()).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
