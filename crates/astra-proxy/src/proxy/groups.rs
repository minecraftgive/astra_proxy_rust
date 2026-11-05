use crate::{outbound, App};
use anyhow::{ensure, Context, Result};
use astra_model::groups::Mode;
use astra_model::RouteAction;
use astra_model::Target;
use futures_util::{stream, StreamExt};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
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
    delays: Vec<(String, Option<u64>)>,
}
struct Testing<'a>(&'a Groups, tokio::task::Id);
impl Drop for Testing<'_> {
    fn drop(&mut self) {
        // An aborted test may finish after a replacement started. Only the
        // current task is allowed to clear the visible running flag.
        let slot = self.0.test_slot();
        if slot.as_ref().is_some_and(|task| task.id() == self.1) {
            self.0.testing.store(false, Ordering::Release);
        }
    }
}
pub(crate) struct Groups {
    order: Vec<String>,
    locks: HashMap<String, Mutex<()>>,
    selections: RwLock<HashMap<String, Selection>>,
    pub sessions: Arc<super::group_sessions::Sessions>,
    active: AtomicBool,
    sequence: AtomicU64,
    testing: AtomicBool,
    test_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
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
    fn write_selections(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Selection>> {
        match self.selections.write() {
            Ok(selections) => selections,
            Err(poisoned) => {
                // Selection and cancellation epoch form one transaction. A
                // poisoned transaction cannot be reused: cancel its leases and
                // rebuild automatic selections from fresh probes.
                let mut selections = poisoned.into_inner();
                self.sessions.invalidate();
                selections.clear();
                self.selections.clear_poison();
                tracing::error!("proxy group selection update panicked; cancelled sessions and reset cached selections");
                selections
            }
        }
    }
    fn read_selections(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Selection>> {
        loop {
            match self.selections.read() {
                Ok(selections) => return selections,
                Err(poisoned) => drop(poisoned.into_inner()),
            }
            drop(self.write_selections());
        }
    }
    fn test_slot(&self) -> std::sync::MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
        match self.test_task.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                if let Some(task) = slot.take() {
                    task.abort();
                }
                self.testing.store(false, Ordering::Release);
                self.test_task.clear_poison();
                tracing::error!("proxy group test state panicked; cancelled pending test");
                slot
            }
        }
    }
    pub fn new(config: &crate::config::ProxyConfig) -> Result<Self> {
        let order = astra_model::groups::validate(
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
            sequence: AtomicU64::new(0),
            testing: AtomicBool::new(false),
            test_task: std::sync::Mutex::new(None),
            order,
        })
    }
    fn manual_target(
        app: &App,
        name: &str,
        group: &astra_model::groups::Group,
        path: &mut Vec<String>,
    ) -> Result<String> {
        let override_target = app
            .overrides
            .load()
            .selected
            .get(name)
            .cloned()
            .filter(|member| {
                astra_model::groups::member_path(&app.config.proxy_groups, name, member).is_some()
            });
        let target = override_target
            .as_ref()
            .or(group.selected.as_ref())
            .or_else(|| group.members.first())
            .context("proxy group has no members")?;
        let members = astra_model::groups::member_path(&app.config.proxy_groups, name, target)
            .with_context(|| format!("proxy group {name}: selected node is not available"))?;
        ensure!(
            path.len() + members.len() <= 32,
            "proxy group nesting exceeds limit"
        );
        path.extend(members[..members.len() - 1].iter().cloned());
        Ok(target.clone())
    }
    /// The group a route action starts in; direct and block go through their built-in groups.
    fn entry(app: &App, action: &RouteAction) -> Option<String> {
        let name = match action {
            RouteAction::Proxy(name) => name.as_str(),
            RouteAction::Direct => "DIRECT",
            RouteAction::Block => "REJECT",
        };
        app.config
            .proxy_groups
            .contains_key(name)
            .then(|| name.to_owned())
    }
    /// DIRECT/REJECT end the walk when named by their own group or when no such group exists.
    fn terminal(app: &App, name: &str, parent: Option<&String>) -> Option<RouteAction> {
        let action = match name {
            "DIRECT" => RouteAction::Direct,
            "REJECT" => RouteAction::Block,
            _ => return None,
        };
        (parent.is_some_and(|p| p == name) || !app.config.proxy_groups.contains_key(name))
            .then_some(action)
    }
    fn cached(&self, app: &App, name: &str) -> Result<RouteAction> {
        self.cached_path(app, name, Vec::new()).map(|v| v.0)
    }
    /// `path` holds the groups already entered, so a group's own DIRECT/REJECT stays terminal.
    fn cached_path(
        &self,
        app: &App,
        name: &str,
        mut path: Vec<String>,
    ) -> Result<(RouteAction, Vec<String>)> {
        let mut name = name.to_owned();
        let selections = self.read_selections();
        for _ in 0..=32 {
            if let Some(action) = Self::terminal(app, &name, path.last()) {
                return Ok((action, path));
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
        let Some(name) = Self::entry(app, action) else {
            return Ok((action.clone(), self.sessions.lease(&[])));
        };
        // A switch cannot race subscription: refresh holds the selection write lock
        // while it advances the cancellation epoch.
        let guard = self.read_selections();
        let mut current = name;
        let mut path: Vec<String> = Vec::new();
        let resolved = loop {
            if let Some(action) = Self::terminal(app, &current, path.last()) {
                break action;
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
                    .and_then(|s| self.member(group.mode, s))
                    .context("proxy group has no healthy members")?
            };
        };
        let mut lease = self.sessions.lease(&path);
        lease.1 = Self::interface_for_path(app, &path);
        drop(guard);
        Ok((resolved, lease))
    }
    fn interface_for_path(app: &App, path: &[String]) -> Option<String> {
        path.iter()
            .find_map(|name| app.config.proxy_groups[name].interface.clone())
    }
    pub fn inherit(&mut self, old: &App, config: &crate::config::ProxyConfig) {
        self.sessions = old.groups.sessions.clone();
        let previous = old.groups.read_selections();
        let mut next = self.write_selections();
        for (name, group) in &config.proxy_groups {
            if old
                .config
                .proxy_groups
                .get(name)
                .is_some_and(|g| g.mode != Mode::Select)
                && group.mode != Mode::Select
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
        let _guard = old.groups.write_selections();
        old.groups.active.store(false, Ordering::Release);
        old.groups.stop_test();
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
            let structural = next.is_none_or(|g| {
                g.mode != previous.mode
                    || g.members != previous.members
                    || g.interface != previous.interface
            });
            if (manual && policy.disconnect_on_manual)
                || (structural && policy.disconnect_on_config_change)
            {
                self.sessions
                    .disconnect(name, if manual { "manual" } else { "config_change" });
            }
        }
    }
    pub async fn resolve(&self, app: &App, action: &RouteAction) -> Result<RouteAction> {
        let Some(name) = Self::entry(app, action) else {
            return Ok(action.clone());
        };
        let name = name.as_str();
        let mut reachable = HashSet::new();
        let mut pending = vec![name];
        while let Some(name) = pending.pop() {
            if !reachable.insert(name) {
                continue;
            }
            if let Some(group) = app.config.proxy_groups.get(name) {
                if group.mode == Mode::Select {
                    // Refresh all nested automatic groups; manual overrides can name descendants.
                    pending.extend(group.members.iter().map(String::as_str));
                } else {
                    pending.extend(group.members.iter().map(String::as_str));
                }
            }
        }
        for name in &self.order {
            if reachable.contains(name.as_str()) {
                let usable = self
                    .read_selections()
                    .get(name)
                    .is_some_and(|s| s.member.is_some());
                // Keep forwarding through the last healthy selection during a background scan.
                if usable && self.locks[name].try_lock().is_err() {
                    continue;
                }
                self.refresh(app, name, false).await;
            }
        }
        let resolved = self.cached(app, name)?;
        tracing::debug!(target: "astra_route", phase="group", group=%name, action=?resolved, "proxy group resolved");
        Ok(resolved)
    }
    async fn refresh(&self, app: &App, name: &str, force: bool) {
        let group = &app.config.proxy_groups[name];
        if (group.mode == Mode::Select && !force) || !self.active.load(Ordering::Acquire) {
            return;
        }
        let _guard = self.locks[name].lock().await;
        if !force
            && self
                .read_selections()
                .get(name)
                .and_then(|s| s.checked)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(group.interval))
        {
            return;
        }
        let results: Vec<_> = stream::iter(group.members.iter().cloned())
            .map(|member| async move {
                let result = async {
                    let (action, path) = self.cached_path(app, &member, vec![name.to_owned()])?;
                    let interface = Self::interface_for_path(app, &path);
                    let start = Instant::now();
                    tokio::time::timeout(
                        Duration::from_millis(group.timeout_ms),
                        probe(app, &action, &group.url, interface.as_deref()),
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
        let mut selections = self.write_selections();
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        let selection = selections.entry(name.into()).or_default();
        selection.delays = results.clone();
        if group.mode == Mode::Select {
            selection.checked = Some(Instant::now());
            return;
        }
        let best = results
            .iter()
            .filter_map(|(name, time)| time.map(|t| (name, t)))
            .min_by_key(|(member, t)| {
                if group.mode == Mode::Auto {
                    *t
                } else {
                    group
                        .members
                        .iter()
                        .position(|v| v == *member)
                        .unwrap_or(usize::MAX) as u64
                }
            });
        let next = best.map(|(best, delay)| {
            if let Some((current, Some(current_delay))) = results
                .iter()
                .find(|(member, _)| Some(member) == selection.member.as_ref())
            {
                if group.mode == Mode::Auto
                    && *current_delay <= delay.saturating_add(group.tolerance_ms)
                {
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
    fn member(&self, mode: Mode, selection: &Selection) -> Option<String> {
        if mode == Mode::LoadBalance {
            let healthy: Vec<_> = selection
                .delays
                .iter()
                .filter(|(_, delay)| delay.is_some())
                .map(|(name, _)| name)
                .collect();
            if healthy.is_empty() {
                return None;
            }
            return Some(
                healthy[self.sequence.fetch_add(1, Ordering::Relaxed) as usize % healthy.len()]
                    .clone(),
            );
        }
        selection.member.clone()
    }
    pub fn commit_overrides(
        &self,
        app: &App,
        next: astra_model::runtime::Overrides,
        disconnect: Option<&str>,
    ) {
        // The selected path and its cancellation epoch change as one operation.
        let _guard = self.write_selections();
        app.overrides.store(Arc::new(next));
        if let Some(group) = disconnect {
            self.sessions.disconnect(group, "manual");
        }
    }
    pub fn stop_test(&self) {
        let mut slot = self.test_slot();
        if let Some(task) = slot.take() {
            task.abort();
        }
        self.testing.store(false, Ordering::Release);
    }
    pub fn request_test(app: std::sync::Arc<App>, name: String) -> Result<()> {
        ensure!(
            app.config.proxy_groups.contains_key(&name),
            "unknown proxy group"
        );
        let owner = app.clone();
        let mut slot = owner.groups.test_slot();
        ensure!(
            app.groups
                .testing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a group test is already running"
        );
        let task = tokio::spawn(async move {
            let _testing = Testing(&app.groups, tokio::task::id());
            // Refresh dependencies first so nested automatic groups can be tested.
            for dependency in &app.groups.order {
                if dependency == &name {
                    break;
                }
                app.groups.refresh(&app, dependency, false).await;
            }
            app.groups.refresh(&app, &name, true).await;
        });
        *slot = Some(task);
        Ok(())
    }
    pub fn snapshot(&self, app: &App) -> Vec<astra_model::runtime::Group> {
        let selections = self.read_selections();
        let mut budget = 2048usize;
        app.config
            .proxy_groups
            .iter()
            .take(64)
            .map(|(name, group)| {
                let selection = selections.get(name);
                let selected = if group.mode == Mode::Select {
                    Self::manual_target(app, name, group, &mut Vec::new()).ok()
                } else {
                    selection.and_then(|s| s.member.clone())
                };
                let count = group.members.len().min(budget);
                budget -= count;
                astra_model::runtime::Group {
                    name: name.clone(),
                    mode: match group.mode {
                        Mode::Select => "select",
                        Mode::Auto => "url-test",
                        Mode::Fallback => "fallback",
                        Mode::LoadBalance => "load-balance",
                    }
                    .into(),
                    selected,
                    testing: self.testing.load(Ordering::Acquire),
                    members: group
                        .members
                        .iter()
                        .take(count)
                        .map(|member| {
                            let measured = selection
                                .and_then(|s| s.delays.iter().find(|(name, _)| name == member));
                            astra_model::runtime::Member {
                                name: member.clone(),
                                delay_ms: measured.and_then(|(_, delay)| *delay),
                                alive: measured.map(|(_, delay)| delay.is_some()),
                            }
                        })
                        .collect(),
                }
            })
            .collect()
    }
    pub async fn refresh_all(&self, app: &App) {
        for name in &self.order {
            self.refresh(app, name, false).await;
        }
    }
}
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn probe(app: &App, action: &RouteAction, url: &str, interface: Option<&str>) -> Result<()> {
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
    let mut connection: outbound::Stream =
        app.connect_resolved_on(&target, action, interface).await?;
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
#[path = "../test/proxy_groups_test.rs"]
mod tests;
