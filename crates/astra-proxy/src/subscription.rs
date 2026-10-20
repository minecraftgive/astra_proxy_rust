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
            let mut connection = app
                .connect_resolved(&target, &astra_ipc::RouteAction::Direct)
                .await?;
            if url.scheme() == "https" {
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
                        .await?,
                );
            }
            let uri: http::Uri = url.as_str().parse()?;
            let (mut sender, driver) = hyper::client::conn::http1::Builder::new()
                .max_buf_size(16 * 1024)
                .handshake(TokioIo::new(connection))
                .await?;
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
                .await?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                ensure!(redirect < 3, "too many subscription redirects");
                url = url.join(
                    response
                        .headers()
                        .get("location")
                        .context("redirect location missing")?
                        .to_str()?,
                )?;
                continue;
            }
            ensure!(
                response.status().is_success(),
                "subscription HTTP {}",
                response.status()
            );
            let mut body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Ok(chunk) = frame?.into_data() {
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
    .context("subscription download timed out")?
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    pub base: String,
    pub effective: String,
    pub ownership: Subscription,
    #[serde(default)]
    pub subscriptions: std::collections::BTreeMap<String, Subscription>,
}
fn path(config: &Path) -> PathBuf {
    config.with_extension("subscription-cache.json")
}
pub(crate) fn read(config: &Path, base: &str) -> Option<Cache> {
    let load = || -> Result<Cache> {
        let mut bytes = Vec::new();
        std::fs::File::open(path(config))?
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
pub(crate) fn cached_app(config: &Path, original: App) -> App {
    if original.config.subscription_sources().is_empty() {
        return original;
    }
    if let Some(cache) = read(config, &original.source_text) {
        if let Ok(mut app) = App::from_text(config, cache.effective) {
            app.source_text = original.source_text;
            tracing::info!("validated subscription cache loaded");
            return app;
        }
    }
    original
}
pub(crate) fn save(config: &Path, cache: &Cache) -> Result<()> {
    let data = serde_json::to_vec(cache)?;
    ensure!(
        data.len() <= 16 * 1024 * 1024,
        "subscription cache too large"
    );
    let target = path(config);
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
                    "/error",
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
        assert!(
            download(&app, &format!("http://{address}/error"), "astra-test")
                .await
                .is_err()
        );
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
        let base = "standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[subscription]\nurl='http://127.0.0.1:1/config'\n";
        let settings = Subscription {
            url: "http://127.0.0.1:1/config".into(),
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
        assert_eq!(app.source_text, base);
        assert!(read(&config, &format!("{base}\n# changed")).is_none());
        std::fs::write(path(&config), b"invalid").unwrap();
        let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
        assert!(app.outbounds.is_empty());
        std::fs::remove_file(path(&config)).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
