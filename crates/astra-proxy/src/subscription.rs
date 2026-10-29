//! Proxy Core owns the subscription's last validated runtime snapshot.
use crate::App;
use anyhow::{ensure, Result};
use astra_convert::subscription::Subscription;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// HTTP(S) downloads use the same physical egress as DIRECT traffic.
pub(crate) async fn download(app: &App, url: &str, user_agent: &str) -> Result<String> {
    use anyhow::Context;
    use http_body_util::{BodyExt, Empty};
    use hyper_util::rt::TokioIo;
    use std::{sync::Arc, time::Duration};
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
    use std::sync::atomic::Ordering;
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut url = url::Url::parse(url)?;
        for redirect in 0..=3 {
            astra_convert::subscription::validate_url(url.as_str())?;
            let host = url
                .host_str()
                .context("subscription host missing")?
                .trim_matches(['[', ']']);
            let target = astra_ipc::Target {
                host: host.into(),
                port: url
                    .port_or_known_default()
                    .context("subscription port missing")?,
            };
            phase.store(0, Ordering::Relaxed);
            let mut connection = app
                .connect_resolved(&target, &astra_ipc::RouteAction::Direct)
                .await
                .with_context(|| {
                    format!(
                        "connect to subscription server {host}:{} (DNS/TCP/egress)",
                        target.port
                    )
                })?;
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
                        .header("Connection", "close")
                        .body(Empty::<bytes::Bytes>::new())?,
                )
                .await
                .context("send subscription HTTP request/read response headers")?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirect < 3, "too many subscription redirects");
                let next = url.join(
                    response
                        .headers()
                        .get("location")
                        .context("redirect location missing")?
                        .to_str()?,
                )?;
                astra_convert::subscription::validate_redirect(url.as_str(), next.as_str())?;
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
                    .context("read subscription response body")?
                    .into_data()
                {
                    ensure!(
                        data.len() + chunk.len() <= 8 * 1024 * 1024,
                        "subscription exceeds 8 MiB"
                    );
                    data.extend_from_slice(&chunk);
                }
            }
            return String::from_utf8(data).context("subscription must be UTF-8 YAML");
        }
        unreachable!()
    })
    .await
    .with_context(|| {
        format!(
            "subscription download timed out after 20s during {}",
            [
                "DNS/TCP/egress connect",
                "TLS handshake",
                "HTTP response headers",
                "response body"
            ][phase.load(Ordering::Relaxed)]
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
    Ok(astra_ipc::storage::runtime_path(config, "subscriptions")?)
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
    let base = original.source_text.clone();
    let sources = original.config.subscription_sources();
    ensure!(!sources.is_empty(), "No subscriptions configured");
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
        let path = config.to_owned();
        let previous = effective.clone();
        let mut managed = ownership.get(&source).cloned().unwrap_or(settings.clone());
        let name = source.clone();
        let app = std::sync::Arc::new(App::from_text(config, previous.clone())?);
        let runtime = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || -> Result<_> {
            let converted = astra_convert::subscription::fetch_with(
                &settings,
                path.parent().context("config has no parent")?,
                &|url, agent| runtime.block_on(download(&app, url, agent)),
            )
            .context("fetch subscription nodes")?;
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
        .await?;
        match result {
            Ok((text, managed)) => {
                effective = text;
                ownership.insert(source, managed);
                updated += 1;
            }
            Err(error) => failed.push(format!(
                "{source}: {}",
                astra_convert::subscription::diagnostics::error(&error)
            )),
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
    )?;
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
    astra_ipc::storage::prepare_parent(&target)?;
    let temporary = target.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &target)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let text = format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscriptions.remote]\nurl='http://{address}/sub'\n");
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
            download(&app, &format!("http://{address}/first"), "astra-test")
                .await
                .unwrap(),
            "nodes"
        );
        let error = download(
            &app,
            &format!("http://{address}/error?token=secret"),
            "astra-test",
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
            Some(astra_ipc::RouteAction::Proxy("PROXY".into()))
        );
        assert_eq!(
            app.config.proxy_groups["subscription"].members,
            ["cached-node"]
        );
        assert_eq!(app.config.proxy_groups["PROXY"].members, ["subscription"]);
        assert_eq!(app.source_text, base);
        assert!(read(&config, &format!("{base}\n# changed")).is_none());
        std::fs::write(path(&config).unwrap(), b"invalid").unwrap();
        let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
        assert!(app.outbounds.is_empty());
        std::fs::remove_file(path(&config).unwrap()).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
