use crate::{outbound, App};
use anyhow::{ensure, Context, Result};
use astra_ipc::{groups::Mode, RouteAction, Target};
use futures_util::{stream, StreamExt};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Default, Clone)]
struct Selection {
    checked: Option<Instant>,
    member: Option<String>,
    initialized: bool,
}
pub(crate) struct Groups {
    order: Vec<String>,
    locks: HashMap<String, Mutex<()>>,
    selections: RwLock<HashMap<String, Selection>>,
    pub sessions: Arc<super::group_sessions::Sessions>,
    active: AtomicBool,
}
pub(crate) fn test_uri(url: &str) -> Result<http::Uri> {
    let uri: http::Uri = url.parse().context("invalid group test URL")?;
    ensure!(
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some()
            && !url.contains(['#', '@'])
            && uri.authority().is_some(),
        "group test URL must be an absolute HTTP(S) URL without credentials or fragment"
    );
    let authority = uri.authority().unwrap().as_str();
    let port = if authority.starts_with('[') {
        authority
            .split_once(']')
            .and_then(|(_, rest)| rest.strip_prefix(':'))
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    ensure!(
        port.is_none_or(|port| port.parse::<u16>().is_ok_and(|p| p > 0)),
        "invalid group test URL port"
    );
    Ok(uri)
}
impl Groups {
    pub fn new(config: &crate::config::ProxyConfig) -> Result<Self> {
        let order = astra_ipc::groups::validate(
            &config.proxy_groups,
            &config
                .outbounds
                .iter()
                .map(|node| node.tag().to_owned())
                .collect(),
        )
        .map_err(anyhow::Error::msg)?;
        Ok(Self {
            locks: order
                .iter()
                .map(|name| (name.clone(), Mutex::new(())))
                .collect(),
            selections: RwLock::new(HashMap::new()),
            sessions: Arc::default(),
            active: AtomicBool::new(true),
            order,
        })
    }
    fn manual_target(
        app: &App,
        name: &str,
        group: &astra_ipc::groups::Group,
        path: &mut Vec<String>,
    ) -> Result<String> {
        let target = group
            .selected
            .as_ref()
            .or_else(|| group.members.first())
            .context("proxy group has no members")?;
        let members = astra_ipc::groups::member_path(&app.config.proxy_groups, name, target)
            .with_context(|| format!("proxy group {name}: selected node is not available"))?;
        ensure!(
            path.len() + members.len() <= 32,
            "proxy group nesting exceeds limit"
        );
        path.extend(members[..members.len() - 1].iter().cloned());
        Ok(target.clone())
    }
    fn cached(&self, app: &App, name: &str) -> Result<RouteAction> {
        self.cached_path(app, name).map(|v| v.0)
    }
    fn cached_path(&self, app: &App, name: &str) -> Result<(RouteAction, Vec<String>)> {
        let mut name = name.to_owned();
        let mut path = Vec::new();
        let selections = self.selections.read().unwrap();
        for _ in 0..=32 {
            match name.as_str() {
                "DIRECT" => return Ok((RouteAction::Direct, path)),
                "REJECT" => return Ok((RouteAction::Block, path)),
                _ => {}
            }
            let Some(group) = app.config.proxy_groups.get(&name) else {
                ensure!(
                    app.outbounds.contains_key(&name),
                    "undefined outbound tag {name}"
                );
                return Ok((RouteAction::Proxy(name), path));
            };
            path.push(name.clone());
            name = if group.mode == Mode::Select {
                Self::manual_target(app, &name, group, &mut path)?
            } else {
                selections
                    .get(&name)
                    .and_then(|s| s.member.clone())
                    .with_context(|| format!("proxy group {name}: no healthy members"))?
            };
        }
        anyhow::bail!("proxy group nesting exceeds limit")
    }
    pub async fn resolve_session(
        &self,
        app: &App,
        action: &RouteAction,
    ) -> Result<(RouteAction, super::group_sessions::Lease)> {
        self.resolve(app, action).await?;
        let RouteAction::Proxy(name) = action else {
            return Ok((action.clone(), self.sessions.lease(&[])));
        };
        // A switch cannot race subscription: refresh holds the selection write lock
        // while it advances the cancellation epoch.
        let guard = self.selections.read().unwrap();
        let mut current = name.clone();
        let mut path = Vec::new();
        let resolved = loop {
            if current == "DIRECT" {
                break RouteAction::Direct;
            }
            if current == "REJECT" {
                break RouteAction::Block;
            }
            let Some(group) = app.config.proxy_groups.get(&current) else {
                break RouteAction::Proxy(current);
            };
            ensure!(path.len() < 32, "proxy group nesting exceeds limit");
            path.push(current.clone());
            current = if group.mode == Mode::Select {
                Self::manual_target(app, &current, group, &mut path)?
            } else {
                guard
                    .get(&current)
                    .and_then(|s| s.member.clone())
                    .context("proxy group has no healthy members")?
            };
        };
        let lease = self.sessions.lease(&path);
        drop(guard);
        Ok((resolved, lease))
    }
    pub fn inherit(&mut self, old: &App, config: &crate::config::ProxyConfig) {
        self.sessions = old.groups.sessions.clone();
        let previous = old.groups.selections.read().unwrap();
        let mut next = self.selections.write().unwrap();
        for (name, group) in &config.proxy_groups {
            if old
                .config
                .proxy_groups
                .get(name)
                .is_some_and(|g| g.mode == Mode::Auto)
                && group.mode == Mode::Auto
            {
                if let Some(selection) = previous.get(name) {
                    if selection
                        .member
                        .as_ref()
                        .is_none_or(|m| group.members.contains(m))
                    {
                        let mut selection = selection.clone();
                        if old.config.proxy_groups.get(name) != Some(group) {
                            selection.checked = None;
                        }
                        next.insert(name.clone(), selection);
                    }
                }
            }
        }
    }
    pub fn committed(&self, old: &App, config: &crate::config::ProxyConfig) {
        let _guard = old.groups.selections.write().unwrap();
        old.groups.active.store(false, Ordering::Release);
        for (name, previous) in &old.config.proxy_groups {
            let next = config.proxy_groups.get(name);
            let policy = next.unwrap_or(previous);
            let manual = next.is_some_and(|g| {
                previous.mode == Mode::Select
                    && g.mode == Mode::Select
                    && previous
                        .selected
                        .as_ref()
                        .or_else(|| previous.members.first())
                        != g.selected.as_ref().or_else(|| g.members.first())
            });
            let structural =
                next.is_none_or(|g| g.mode != previous.mode || g.members != previous.members);
            if (manual && policy.disconnect_on_manual)
                || (structural && policy.disconnect_on_config_change)
            {
                self.sessions
                    .disconnect(name, if manual { "manual" } else { "config_change" });
            }
        }
    }
    pub async fn resolve(&self, app: &App, action: &RouteAction) -> Result<RouteAction> {
        let RouteAction::Proxy(name) = action else {
            return Ok(action.clone());
        };
        if !app.config.proxy_groups.contains_key(name) {
            return Ok(action.clone());
        }
        let mut reachable = HashSet::new();
        let mut pending = vec![name.as_str()];
        while let Some(name) = pending.pop() {
            if !reachable.insert(name) {
                continue;
            }
            if let Some(group) = app.config.proxy_groups.get(name) {
                if group.mode == Mode::Select {
                    if let Some(member) = group
                        .selected
                        .as_deref()
                        .or_else(|| group.members.first().map(String::as_str))
                    {
                        pending.push(member);
                    }
                } else {
                    pending.extend(group.members.iter().map(String::as_str));
                }
            }
        }
        for name in &self.order {
            if reachable.contains(name.as_str()) {
                let usable = self
                    .selections
                    .read()
                    .unwrap()
                    .get(name)
                    .is_some_and(|s| s.member.is_some());
                // Keep forwarding through the last healthy selection during a background scan.
                if usable && self.locks[name].try_lock().is_err() {
                    continue;
                }
                self.refresh(app, name).await;
            }
        }
        let resolved = self.cached(app, name)?;
        tracing::debug!(target: "astra_route", phase="group", group=%name, action=?resolved, "proxy group resolved");
        Ok(resolved)
    }
    async fn refresh(&self, app: &App, name: &str) {
        let group = &app.config.proxy_groups[name];
        if group.mode != Mode::Auto || !self.active.load(Ordering::Acquire) {
            return;
        }
        let _guard = self.locks[name].lock().await;
        if self
            .selections
            .read()
            .unwrap()
            .get(name)
            .and_then(|s| s.checked)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(group.interval))
        {
            return;
        }
        let results: Vec<_> = stream::iter(group.members.iter().cloned())
            .map(|member| async move {
                let result = async {
                    let action = self.cached(app, &member)?;
                    let start = Instant::now();
                    tokio::time::timeout(
                        Duration::from_millis(group.timeout_ms),
                        probe(app, &action, &group.url),
                    )
                    .await??;
                    Ok::<_, anyhow::Error>(start.elapsed().as_millis() as u64)
                }
                .await;
                if let Err(error) = &result {
                    tracing::debug!(group=%name, %member, %error, "proxy group probe failed");
                }
                (member, result.ok())
            })
            .buffered(4)
            .collect()
            .await;
        let mut selections = self.selections.write().unwrap();
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let selection = selections.entry(name.into()).or_default();
        let best = results
            .iter()
            .filter_map(|(name, time)| time.map(|t| (name, t)))
            .min_by_key(|(_, t)| *t);
        let next = best.map(|(best, delay)| {
            if let Some((current, Some(current_delay))) = results
                .iter()
                .find(|(member, _)| Some(member) == selection.member.as_ref())
            {
                if *current_delay <= delay.saturating_add(group.tolerance_ms) {
                    return current.clone();
                }
            }
            best.clone()
        });
        if selection.member != next {
            let reason = if selection.member.is_none() {
                "recovery"
            } else if results
                .iter()
                .any(|(member, delay)| Some(member) == selection.member.as_ref() && delay.is_some())
            {
                "auto"
            } else {
                "failure"
            };
            let disconnect = match reason {
                "auto" => group.disconnect_on_auto,
                "failure" => group.disconnect_on_failure,
                _ => selection.initialized && group.disconnect_on_recovery,
            };
            if disconnect {
                self.sessions.disconnect(name, reason);
            }
            tracing::info!(group=%name, previous=?selection.member, selected=?next, %reason, disconnect, "proxy group selection changed");
        }
        selection.member = next;
        selection.checked = Some(Instant::now());
        selection.initialized = true;
    }
    pub async fn refresh_all(&self, app: &App) {
        for name in &self.order {
            self.refresh(app, name).await;
        }
    }
}
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn probe(app: &App, action: &RouteAction, url: &str) -> Result<()> {
    let uri = test_uri(url)?;
    let host = uri.host().unwrap().trim_matches(['[', ']']);
    let target = Target {
        host: host.into(),
        port: uri
            .port_u16()
            .unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            }),
    };
    let mut connection: outbound::Stream = app.connect_resolved(&target, action).await?;
    if uri.scheme_str() == Some("https") {
        use tokio_rustls::{
            rustls::{self, pki_types::ServerName, RootCertStore},
            TlsConnector,
        };
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
                .uri(uri.path_and_query().map(|v| v.as_str()).unwrap_or("/"))
                .header("Host", uri.authority().unwrap().as_str())
                .header("Connection", "close")
                .header("User-Agent", "astra-dual-core/0.1.0")
                .body(Empty::<bytes::Bytes>::new())?,
        )
        .await?;
    ensure!(
        response.status().is_success() || response.status().is_redirection(),
        "test returned HTTP {}",
        response.status()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[test]
    fn test_urls_reject_invalid_targets_and_allow_ipv6() {
        for url in [
            "ftp://example.test/",
            "relative/path",
            "http://user:pass@example.test/",
            "http://example.test/#fragment",
            "http://example.test:0/",
            "http://example.test:65536/",
        ] {
            assert!(test_uri(url).is_err(), "{url}");
        }
        for url in [
            "http://example.test/204",
            "https://example.test/",
            "http://[::1]:8080/204",
        ] {
            assert!(test_uri(url).is_ok(), "{url}");
        }
    }
    struct Stub {
        address: std::net::SocketAddr,
        delay: Arc<AtomicU64>,
        healthy: Arc<AtomicBool>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Stub {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn headers(stream: &mut tokio::net::TcpStream) -> Result<String> {
        let mut text = Vec::new();
        while !text.ends_with(b"\r\n\r\n") {
            ensure!(text.len() < 8192, "headers too large");
            text.push(stream.read_u8().await?);
        }
        Ok(String::from_utf8(text)?)
    }
    async fn stub(delay: u64, marker: &'static str) -> Stub {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let delay = Arc::new(AtomicU64::new(delay));
        let healthy = Arc::new(AtomicBool::new(true));
        let (d, h) = (delay.clone(), healthy.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let (d, h) = (d.clone(), h.clone());
                tokio::spawn(async move {
                    let request = headers(&mut socket).await?;
                    ensure!(
                        request.starts_with("CONNECT example.test:80 "),
                        "wrong test target"
                    );
                    socket
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await?;
                    let request = headers(&mut socket).await?;
                    ensure!(request.starts_with("GET /204 "), "wrong test path");
                    tokio::time::sleep(Duration::from_millis(d.load(Ordering::SeqCst))).await;
                    let status = if h.load(Ordering::SeqCst) { 200 } else { 503 };
                    socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",marker.len()).as_bytes()).await?;
                    Ok::<_, anyhow::Error>(())
                });
            }
        });
        Stub {
            address,
            delay,
            healthy,
            task,
        }
    }
    fn expire(app: &App) {
        for selection in app.groups.selections.write().unwrap().values_mut() {
            selection.checked = None;
        }
    }
    fn app(a: &Stub, b: &Stub, mode: &str, selected: &str, tolerance: u64) -> App {
        App::from_text(std::path::Path::new("groups.toml"), format!("standalone=true\nstandalone_outbound='main'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='http'\ntag='a'\nserver='{}'\n[[outbounds]]\nkind='http'\ntag='b'\nserver='{}'\n[proxy_groups.speed]\nmode='{mode}'\nmembers=['a','b']\nselected='{selected}'\nurl='http://example.test/204'\ninterval=1\ntimeout_ms=1000\ntolerance_ms={tolerance}\n[proxy_groups.main]\nmode='select'\nmembers=['speed','DIRECT']\nselected='speed'\n",a.address,b.address)).unwrap()
    }
    #[tokio::test]
    async fn parent_pins_descendant_without_changing_shared_child_selection() {
        let a = stub(1, "a").await;
        let b = stub(1, "b").await;
        let original = app(&a, &b, "select", "b", 0);
        let text = original
            .source_text
            .replace("selected='speed'", "selected='a'");
        let app = App::from_text(std::path::Path::new("groups.toml"), text).unwrap();
        let parent = RouteAction::Proxy("main".into());
        let child = RouteAction::Proxy("speed".into());
        assert_eq!(
            app.groups.resolve(&app, &parent).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
        assert_eq!(
            app.groups.resolve(&app, &child).await.unwrap(),
            RouteAction::Proxy("b".into())
        );
        let (resolved, path) = app.groups.cached_path(&app, "main").unwrap();
        assert_eq!(resolved, RouteAction::Proxy("a".into()));
        assert_eq!(path, ["main", "speed"]);
        assert_eq!(
            app.groups.resolve_session(&app, &parent).await.unwrap().0,
            resolved
        );
        let invalid = app
            .source_text
            .replace("selected='a'", "selected='missing'");
        assert!(App::from_text(std::path::Path::new("groups.toml"), invalid).is_err());
    }
    #[tokio::test]
    async fn automatic_groups_measure_switch_retain_tolerance_and_fail_closed() {
        let a = stub(20, "a").await;
        let b = stub(160, "b").await;
        let app = app(&a, &b, "auto", "a", 0);
        let action = RouteAction::Proxy("main".into());
        assert_eq!(
            app.groups.resolve(&app, &action).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
        {
            let _checking = app.groups.locks["speed"].lock().await;
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(50), app.resolve_action(&action))
                    .await
                    .unwrap()
                    .unwrap(),
                RouteAction::Proxy("a".into())
            );
        }
        a.healthy.store(false, Ordering::SeqCst);
        expire(&app);
        assert_eq!(
            app.groups.resolve(&app, &action).await.unwrap(),
            RouteAction::Proxy("b".into())
        );
        b.healthy.store(false, Ordering::SeqCst);
        expire(&app);
        assert!(app.groups.resolve(&app, &action).await.is_err());
        a.healthy.store(true, Ordering::SeqCst);
        expire(&app);
        assert_eq!(
            app.groups.resolve(&app, &action).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
        // Small changes retain a measured healthy member, even if a different one is faster.
        b.healthy.store(true, Ordering::SeqCst);
        let app = self::app(&a, &b, "auto", "a", 500);
        app.groups.resolve(&app, &action).await.unwrap();
        a.delay.store(180, Ordering::SeqCst);
        b.delay.store(10, Ordering::SeqCst);
        expire(&app);
        assert_eq!(
            app.groups.resolve(&app, &action).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
    }
    #[tokio::test]
    async fn switch_reasons_obey_independent_disconnect_policies() {
        let a = stub(10, "a").await;
        let b = stub(100, "b").await;
        let action = RouteAction::Proxy("main".into());
        let mut app = self::app(&a, &b, "auto", "a", 0);
        app.config
            .proxy_groups
            .get_mut("speed")
            .unwrap()
            .disconnect_on_failure = true;
        let (_, lease) = app.resolve_session(&action).await.unwrap();
        // A healthy improvement keeps the old lease because its policy is false.
        a.delay.store(100, Ordering::SeqCst);
        b.delay.store(10, Ordering::SeqCst);
        expire(&app);
        app.resolve_action(&action).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), lease.cancelled())
                .await
                .is_err()
        );
        // Failure closes every older connection through this group, even those kept at earlier switches.
        b.healthy.store(false, Ordering::SeqCst);
        expire(&app);
        app.resolve_action(&action).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
        let (_, lease) = app.resolve_session(&action).await.unwrap();
        a.healthy.store(false, Ordering::SeqCst);
        expire(&app);
        assert!(app.resolve_action(&action).await.is_err());
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
        // Opt in only to recovery: preserve a lease during outage, close on recovery.
        a.healthy.store(true, Ordering::SeqCst);
        let mut app = self::app(&a, &b, "auto", "a", 0);
        app.config
            .proxy_groups
            .get_mut("speed")
            .unwrap()
            .disconnect_on_recovery = true;
        let (_, lease) = app.resolve_session(&action).await.unwrap();
        a.healthy.store(false, Ordering::SeqCst);
        expire(&app);
        assert!(app.resolve_action(&action).await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), lease.cancelled())
                .await
                .is_err()
        );
        a.healthy.store(true, Ordering::SeqCst);
        expire(&app);
        app.resolve_action(&action).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
        // Independently opt in to healthy automatic improvements.
        b.healthy.store(true, Ordering::SeqCst);
        a.delay.store(10, Ordering::SeqCst);
        b.delay.store(100, Ordering::SeqCst);
        let mut app = self::app(&a, &b, "auto", "a", 0);
        app.config
            .proxy_groups
            .get_mut("speed")
            .unwrap()
            .disconnect_on_auto = true;
        let (_, lease) = app.resolve_session(&action).await.unwrap();
        a.delay.store(100, Ordering::SeqCst);
        b.delay.store(10, Ordering::SeqCst);
        expire(&app);
        app.resolve_action(&action).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn reload_cancels_only_configured_reasons_and_preserves_unrelated_connections() {
        let a = stub(0, "a").await;
        let b = stub(0, "b").await;
        let old = self::app(&a, &b, "select", "a", 50);
        let action = RouteAction::Proxy("main".into());
        let (_, lease) = old.resolve_session(&action).await.unwrap();
        let (_, unrelated) = old
            .resolve_session(&RouteAction::Proxy("b".into()))
            .await
            .unwrap();
        let mut next = self::app(&a, &b, "select", "b", 50);
        next.config
            .proxy_groups
            .get_mut("speed")
            .unwrap()
            .disconnect_on_manual = true;
        next.groups.inherit(&old, &next.config);
        next.groups.committed(&old, &next.config);
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), unrelated.cancelled())
                .await
                .is_err()
        );
        let (_, lease) = next.resolve_session(&action).await.unwrap();
        let mut last = self::app(&a, &b, "select", "b", 50);
        last.config
            .proxy_groups
            .get_mut("speed")
            .unwrap()
            .disconnect_on_config_change = true;
        last.config.proxy_groups.get_mut("speed").unwrap().members = vec!["b".into()];
        last.groups.inherit(&next, &last.config);
        last.groups.committed(&next, &last.config);
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn default_dynamic_group_reloads_members_and_never_falls_back_to_direct_when_empty() {
        let settings = |tag: &str| {
            format!("standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='{tag}'\nserver='127.0.0.1:1081'")
        };
        let old = App::from_text(std::path::Path::new("dynamic.toml"), settings("a")).unwrap();
        let action = RouteAction::Proxy("PROXY".into());
        assert_eq!(
            old.resolve_action(&action).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
        assert!(!old.source_text.contains("proxy_groups"));
        let mut next = App::from_text(std::path::Path::new("dynamic.toml"), settings("b")).unwrap();
        let (_, lease) = old.resolve_session(&action).await.unwrap();
        next.config
            .proxy_groups
            .get_mut("PROXY")
            .unwrap()
            .disconnect_on_config_change = true;
        next.groups.inherit(&old, &next.config);
        next.groups.committed(&old, &next.config);
        assert_eq!(
            next.resolve_action(&action).await.unwrap(),
            RouteAction::Proxy("b".into())
        );
        tokio::time::timeout(Duration::from_secs(1), lease.cancelled())
            .await
            .unwrap();
        let empty = App::from_text(
            std::path::Path::new("dynamic.toml"),
            "standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'".into(),
        )
        .unwrap();
        assert!(empty.resolve_action(&action).await.is_err());
    }
    #[tokio::test]
    async fn manual_group_connects_selected_member_and_existing_stream_survives_new_choice() {
        let a = stub(0, "a").await;
        let b = stub(0, "b").await;
        let app = app(&a, &b, "select", "b", 50);
        let action = RouteAction::Proxy("main".into());
        let target = Target {
            host: "example.test".into(),
            port: 80,
        };
        let mut connection = app.connect(&target, &action).await.unwrap();
        assert!(
            app.groups.selections.read().unwrap().is_empty(),
            "manual selection must not start probes"
        );
        let next = self::app(&a, &b, "select", "a", 50);
        assert_eq!(
            next.groups.resolve(&next, &action).await.unwrap(),
            RouteAction::Proxy("a".into())
        );
        connection
            .write_all(b"GET /204 HTTP/1.1\r\nHost: example.test\r\n\r\n")
            .await
            .unwrap();
        let mut bytes = vec![];
        connection.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.ends_with(b"b"));
    }
}
