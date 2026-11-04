//! Proxy Core owns the subscription's last validated runtime snapshot.
use crate::App;
use anyhow::{ensure, Result};
use astra_convert::subscription::Subscription;
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

/// HTTP(S) downloads follow the configured group or outbound, including redirects.
pub(crate) async fn download(
    app: &App,
    url: &str,
    user_agent: &str,
    proxy: &str,
) -> Result<String> {
    use anyhow::Context;
    use astra_convert::subscription::{diagnostics, http as subscription_http};
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::TokioIo;
    use std::sync::Arc;
    use tokio_rustls::{
        rustls::{self, pki_types::ServerName, RootCertStore},
        TlsConnector,
    };
    struct Driver(tokio::task::JoinHandle<()>);
    impl Drop for Driver {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let phase = std::sync::atomic::AtomicUsize::new(0);
    let received = std::sync::atomic::AtomicUsize::new(0);
    use std::sync::atomic::Ordering;
    let started = std::time::Instant::now();
    tokio::time::timeout(subscription_http::TIMEOUT, async {
        let mut url = url::Url::parse(url)?;
        for redirect in 0..=subscription_http::MAX_REDIRECTS {
            astra_convert::subscription::validate_url(url.as_str())?;
            let endpoint = diagnostics::endpoint(url.as_str());
            tracing::info!(%endpoint, proxy, user_agent, redirect,
                timeout_secs = subscription_http::TIMEOUT.as_secs(),
                "subscription HTTP request started");
            let host = url
                .host_str()
                .context("subscription host missing")?
                .trim_matches(['[', ']']);
            let target = astra_model::Target {
                host: host.into(),
                port: url
                    .port_or_known_default()
                    .context("subscription port missing")?,
            };
            phase.store(0, Ordering::Relaxed);
            received.store(0, Ordering::Relaxed);
            let mut connection = app
                .connect(&target, &astra_model::RouteAction::Proxy(proxy.into()))
                .await
                .with_context(|| {
                    format!(
                        "connect to subscription server {host}:{} (DNS/TCP/egress)",
                        target.port
                    )
                })?;
            tracing::info!(%endpoint, proxy, elapsed_ms = started.elapsed().as_millis() as u64,
                "subscription server connected (DNS/TCP/egress)");
            if url.scheme() == "https" {
                phase.store(1, Ordering::Relaxed);
                let mut roots = RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::aws_lc_rs::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
                connection = Box::new(
                    TlsConnector::from(Arc::new(config))
                        .connect(ServerName::try_from(host.to_owned())?, connection)
                        .await
                        .context("subscription TLS handshake/certificate validation")?,
                );
                tracing::info!(%endpoint, elapsed_ms = started.elapsed().as_millis() as u64,
                    "subscription TLS handshake completed");
            }
            phase.store(2, Ordering::Relaxed);
            let uri: http::Uri = url.as_str().parse()?;
            let (mut sender, driver) = hyper::client::conn::http1::Builder::new()
                .max_buf_size(16 * 1024)
                .handshake(TokioIo::new(connection))
                .await
                .context("subscription HTTP handshake")?;
            let _driver = Driver(tokio::spawn(async move {
                let _ = driver.await;
            }));
            let response = sender
                .send_request(
                    hyper::Request::builder()
                        .uri(uri.path_and_query().map_or("/", |part| part.as_str()))
                        .header(
                            "Host",
                            uri.authority()
                                .context("subscription authority missing")?
                                .as_str(),
                        )
                        .header("User-Agent", user_agent)
                        .header("Accept", "*/*")
                        .header("Accept-Encoding", subscription_http::ACCEPT_ENCODING)
                        .header("Connection", "close")
                        .body(Empty::<bytes::Bytes>::new())?,
                )
                .await
                .context("send subscription HTTP request/read response headers")?;
            let encoding = response.headers().get("Content-Encoding")
                .map(|value| value.to_str()).transpose().context("invalid Content-Encoding header")?
                .unwrap_or("identity").to_owned();
            let content_type = response.headers().get("Content-Type")
                .and_then(|value| value.to_str().ok()).unwrap_or("unknown");
            tracing::info!(%endpoint, status = response.status().as_u16(),
                content_type = %diagnostics::error(&anyhow::anyhow!(content_type.to_owned())),
                content_encoding = %diagnostics::error(&anyhow::anyhow!(encoding.clone())),
                content_length = ?response.headers().get("Content-Length").and_then(|value| value.to_str().ok()).and_then(|value| value.parse::<u64>().ok()),
                elapsed_ms = started.elapsed().as_millis() as u64, "subscription HTTP response received");
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirect < subscription_http::MAX_REDIRECTS, "too many subscription redirects (limit 10)");
                let next = url.join(
                    response
                        .headers()
                        .get("location")
                        .context("redirect location missing")?
                        .to_str()?,
                )?;
                astra_convert::subscription::validate_redirect(url.as_str(), next.as_str())?;
                tracing::info!(%endpoint, next_endpoint = %diagnostics::endpoint(next.as_str()), redirect = redirect + 1,
                    "subscription HTTP redirect");
                url = next;
                continue;
            }
            ensure!(
                response.status().is_success(),
                "subscription HTTP {}",
                response.status()
            );
            phase.store(3, Ordering::Relaxed);
            let mut body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Ok(chunk) = frame
                    .with_context(|| format!("read subscription response body (received {} bytes)", data.len()))?
                    .into_data()
                {
                    ensure!(
                        data.len() + chunk.len() <= 8 * 1024 * 1024,
                        "subscription exceeds 8 MiB"
                    );
                    data.extend_from_slice(&chunk);
                    received.store(data.len(), Ordering::Relaxed);
                }
            }
            let wire_bytes = data.len();
            let text = subscription_http::decode(data.as_slice(), &encoding)?;
            tracing::info!(%endpoint, wire_bytes, bytes = text.len(), redirects = redirect,
                elapsed_ms = started.elapsed().as_millis() as u64, "subscription download completed");
            return Ok(text);
        }
        unreachable!()
    })
    .await
    .with_context(|| {
        format!(
            "subscription download timed out after 30s during {} (received {} bytes)",
            [
                "DNS/TCP/egress connect",
                "TLS handshake",
                "HTTP response headers",
                "response body"
            ][phase.load(Ordering::Relaxed)],
            received.load(Ordering::Relaxed)
        )
    })?
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    pub base: String,
    pub effective: String,
    pub ownership: Subscription,
    #[serde(default)]
    pub subscriptions: std::collections::BTreeMap<String, Subscription>,
}
fn path(config: &Path) -> Result<PathBuf> {
    Ok(astra_platform::storage::runtime_path(
        config,
        "subscriptions",
    )?)
}
pub(crate) fn read(config: &Path, base: &str) -> Option<Cache> {
    let load = || -> Result<Cache> {
        let mut bytes = Vec::new();
        std::fs::File::open(if path(config)?.exists() {
            path(config)?
        } else {
            // Read old sidecar caches for compatibility, but never update them.
            config.with_extension("subscription-cache.json")
        })?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "subscription cache too large"
        );
        let cache: Cache = serde_json::from_slice(&bytes)?;
        ensure!(
            cache.base == base,
            "subscription cache belongs to a different configuration"
        );
        Ok(cache)
    };
    load().ok()
}
pub(crate) fn effective_text(cache: &Cache) -> Result<String> {
    let mut doc: toml::Table = toml::from_str(&cache.effective)?;
    for (name, source) in cache
        .subscriptions
        .iter()
        .map(|(n, s)| (n.as_str(), s))
        .chain(std::iter::once(("__legacy__", &cache.ownership)))
    {
        let settings = if name == "__legacy__" {
            doc.get_mut("subscription")
        } else {
            doc.get_mut("subscriptions")
                .and_then(|v| v.as_table_mut())
                .and_then(|s| s.get_mut(name))
        };
        if let Some(settings) = settings
            .and_then(|s| s.as_table_mut())
            .filter(|s| s.get("url").and_then(|v| v.as_str()) == Some(source.url.as_str()))
        {
            settings.insert(
                "managed_nodes".into(),
                source
                    .managed_nodes
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect::<Vec<_>>()
                    .into(),
            );
        }
    }
    Ok(toml::to_string(&doc)?)
}
pub(crate) fn cached_app(config: &Path, original: App) -> App {
    if original.config.subscription_sources().is_empty() {
        return original;
    }
    if let Some(cache) = read(config, &original.source_text) {
        // Older caches kept ownership separately from the effective document.
        // Restore it before generating per-subscription groups.
        let effective = effective_text(&cache).unwrap_or(cache.effective);
        if let Ok(mut app) = App::from_text(config, effective) {
            app.source_text = original.source_text;
            tracing::info!("validated subscription cache loaded");
            return app;
        }
    }
    original
}
/// Refresh a stopped instance without starting listeners or changing OS settings.
pub(crate) async fn refresh_once(config: &Path, original: App) -> Result<String> {
    use anyhow::Context;
    let started = std::time::Instant::now();
    let base = original.source_text.clone();
    let sources = original.config.subscription_sources();
    ensure!(!sources.is_empty(), "No subscriptions configured");
    tracing::info!(
        subscriptions = sources.len(),
        mode = "stopped",
        "subscription refresh started"
    );
    let mut ownership = sources.clone();
    let mut effective = base.clone();
    if let Some(cache) = read(config, &base) {
        let text = effective_text(&cache)?;
        if App::from_text(config, text.clone()).is_ok() {
            effective = text;
            ownership.extend(cache.subscriptions);
            if !cache.ownership.url.is_empty() {
                ownership.insert("__legacy__".into(), cache.ownership);
            }
        }
    }
    let mut updated = 0;
    let mut failed = Vec::new();
    for (source, settings) in sources {
        let source_started = std::time::Instant::now();
        let endpoint = astra_convert::subscription::diagnostics::endpoint(&settings.url);
        let path = config.to_owned();
        let previous = effective.clone();
        let mut managed = ownership.get(&source).cloned().unwrap_or(settings.clone());
        let previous_nodes = managed.managed_nodes.len();
        tracing::info!(subscription = %source, %endpoint,
            proxy = %settings.proxy, import_groups = settings.import_groups,
            previous_nodes, "subscription update started");
        let name = source.clone();
        let app = std::sync::Arc::new(App::from_text(config, previous.clone())?);
        let runtime = tokio::runtime::Handle::current();
        let span = tracing::info_span!("subscription_update", subscription = %source);
        let result = tokio::task::spawn_blocking(move || -> Result<_> {
            let _entered = span.enter();
            let converted = astra_convert::subscription::fetch_with(
                &settings,
                path.parent().context("config has no parent")?,
                &|url, agent| runtime.block_on(download(&app, url, agent, &settings.proxy)),
            )
            .context("fetch subscription nodes")?;
            tracing::info!(subscription = %name, nodes = converted.nodes,
                warnings = converted.warnings.len(), "subscription converted");
            for warning in converted.warnings.iter().take(50) {
                tracing::warn!(subscription = %name,
                    detail = %astra_convert::subscription::diagnostics::error(&anyhow::anyhow!(warning.clone())),
                    "subscription conversion warning");
            }
            if converted.warnings.len() > 50 {
                tracing::warn!(subscription = %name,
                    omitted = converted.warnings.len() - 50,
                    "additional subscription conversion warnings omitted");
            }
            let text = astra_convert::subscription::merge_named(
                &previous,
                &converted.proxy,
                &mut managed,
                &name,
            )
            .context("merge subscription with local configuration")?;
            App::from_text(&path, text.clone())
                .context("validate merged subscription configuration")?;
            Ok((text, managed))
        })
        .await
        .context("subscription refresh worker failed")
        .and_then(|result| result);
        match result {
            Ok((text, managed)) => {
                tracing::info!(subscription = %source, %endpoint, previous_nodes,
                    nodes = managed.managed_nodes.len(), groups = managed.managed_groups.len(),
                    elapsed_ms = source_started.elapsed().as_millis() as u64,
                    "subscription validated; awaiting cache save");
                effective = text;
                ownership.insert(source, managed);
                updated += 1;
            }
            Err(error) => {
                let detail = astra_convert::subscription::diagnostics::error(&error);
                tracing::warn!(subscription = %source, %endpoint, previous_nodes,
                    elapsed_ms = source_started.elapsed().as_millis() as u64,
                    error = %detail,
                    "subscription download or validation failed; previous nodes retained");
                failed.push(format!("{source}: {detail}"));
            }
        }
    }
    ensure!(
        updated > 0,
        "Subscription update failed; previous nodes retained: {}",
        failed.join("; ")
    );
    save(
        config,
        &Cache {
            base,
            effective,
            ownership: ownership.get("__legacy__").cloned().unwrap_or_default(),
            subscriptions: ownership,
        },
    )
    .inspect_err(|error| {
        tracing::warn!(updated, error = %astra_convert::subscription::diagnostics::error(error),
            "subscription cache save failed; previous cached nodes retained");
    })?;
    tracing::info!(
        updated,
        failed = failed.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "subscription refresh finished; node cache saved"
    );
    if failed.is_empty() {
        Ok(format!("Updated {updated} subscriptions"))
    } else {
        Ok(format!(
            "Updated {updated} subscriptions; failed (previous nodes retained): {}",
            failed.join(", ")
        ))
    }
}
pub(crate) fn save(config: &Path, cache: &Cache) -> Result<()> {
    let data = serde_json::to_vec(cache)?;
    ensure!(
        data.len() <= 16 * 1024 * 1024,
        "subscription cache too large"
    );
    let target = path(config)?;
    astra_platform::storage::atomic_write(
        &target,
        &data,
        astra_platform::storage::WritePolicy::runtime(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
                        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 203, 203, 79, 73, 45, 6, 0, 252, 5, 61,
                        29, 5, 0, 0, 0,
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
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nnodes",
                )
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
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
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
        let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
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
        let worker = App::from_text(&config, app.effective_text.clone()).unwrap();
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
}
