//! Bounded reconnecting subscriptions with injected frontend wake-ups.
use crate::protocol::*;
use anyhow::Result;
use std::{path::PathBuf, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
pub struct Feed {
    receiver: watch::Receiver<Option<Result<Reply, String>>>,
    handle: JoinHandle<()>,
}
pub fn feed_signature(reply: &Reply, action: &Action) -> Result<String> {
    if matches!(action, Action::WatchUpdates) {
        let mut value: serde_json::Value = serde_json::from_str(&reply.text)?;
        // Observing unchanged hosts is a heartbeat, not a visual change.
        if let Some(object) = value.as_object_mut() {
            object.remove("observed_at_ms");
        }
        return Ok(serde_json::to_string(&value)?);
    }
    Ok(serde_json::to_string(reply)?)
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Feed {
    pub fn start(socket: PathBuf, action: Action, waker: crate::waker::Waker) -> Self {
        let (sender, receiver) = watch::channel(None);
        let handle = tokio::spawn(async move {
            let mut delay = 250;
            let mut previous = String::new();
            let log = socket
                .parent()
                .and_then(|p| crate::tui_log::Log::frontend(p).ok());
            let label = if matches!(&action, Action::WatchList) {
                "instance state"
            } else {
                "core runtime"
            };
            let mut online = false;
            if let Some(log) = &log {
                log.write("connection", &format!("connecting {label}"));
            }
            loop {
                let result: Result<()> = async {
                    let mut stream = tokio::time::timeout(
                        Duration::from_secs(3),
                        crate::protocol::subscribe(&socket, action.clone()),
                    )
                    .await??;
                    loop {
                        let reply: Reply = tokio::time::timeout(
                            Duration::from_secs(7),
                            astra_ipc::receive_management(&mut stream),
                        )
                        .await??;
                        anyhow::ensure!(
                            reply.version == astra_ipc::API_VERSION,
                            "通信版本不匹配，请重启管理器和核心"
                        );
                        if let Some(error) = &reply.error {
                            anyhow::bail!(error.clone());
                        }
                        if !online {
                            if let Some(log) = &log {
                                log.write("connection", &format!("connected {label}"));
                            }
                            online = true;
                        }
                        let encoded = feed_signature(&reply, &action)?;
                        if encoded != previous {
                            previous = encoded;
                            sender.send_replace(Some(Ok(reply)));
                            waker.wake();
                        }
                        delay = 250;
                    }
                }
                .await;
                if let Err(error) = result {
                    previous.clear();
                    if online {
                        if let Some(log) = &log {
                            log.write("connection", &format!("disconnected {label}; reconnecting"));
                        }
                        online = false;
                    }
                    sender.send_replace(Some(Err(format!("连接中断，自动重连：{error:#}"))));
                    waker.wake();
                }
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(5000);
            }
        });
        Self { receiver, handle }
    }
    pub fn take(&mut self) -> Option<Result<Reply, String>> {
        if self.receiver.has_changed().unwrap_or(false) {
            self.receiver.borrow_and_update().clone()
        } else {
            None
        }
    }
}

use crate::{i18n::t, operations};
use std::time::Instant;

/// Frontends retain presentation-specific runtime invalidation; subscription
/// scheduling, metadata and reconnect policy are shared.
pub trait Host: std::ops::DerefMut<Target = crate::state::Client> {
    fn refresh_program_info(&mut self);
    fn notice_host_restart(&mut self);
    fn reset_live_identity(&mut self);
    fn accept_runtime_view(&mut self, name: String, view: astra_model::runtime::RuntimeView);
    fn runtime_disconnected(&mut self, error: String);
}
pub struct Feeds {
    pub ready: bool,
    socket: PathBuf,
    list: Feed,
    runtime: Option<((String, u32), Feed)>,
    metadata: Option<(String, JoinHandle<operations::Metadata>)>,
    metadata_time: Instant,
    updates: Option<Feed>,
    update_probe: Option<JoinHandle<Option<crate::protocol::updates::UpdateStatus>>>,
    program_info: Option<JoinHandle<Result<astra_management::v2::ProgramInfo, String>>>,
    local_info: Option<JoinHandle<Result<astra_management::v2::ProgramInfo, String>>>,
    list_online: bool,
    waker: crate::waker::Waker,
    local_build: astra_platform::program_info::BuildInfo,
}
impl Drop for Feeds {
    fn drop(&mut self) {
        if let Some((_, task)) = &self.metadata {
            task.abort();
        }
        if let Some(task) = &self.update_probe {
            task.abort();
        }
        if let Some(task) = &self.program_info {
            task.abort();
        }
        if let Some(task) = &self.local_info {
            task.abort();
        }
    }
}
impl Feeds {
    pub fn new(socket: &std::path::Path, waker: crate::waker::Waker) -> Self {
        let notify = waker.clone();
        let probe_socket = socket.to_owned();
        let update_probe = tokio::spawn(async move {
            let status = async {
                let reply = tokio::time::timeout(
                    Duration::from_secs(3),
                    call(&probe_socket, Action::UpdateStatus),
                )
                .await
                .ok()?
                .ok()?;
                let value: serde_json::Value = serde_json::from_str(&reply.text).ok()?;
                if value.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
                    return None;
                }
                serde_json::from_value(value).ok()
            }
            .await;
            notify.wake();
            status
        });
        Self {
            socket: socket.into(),
            list: Feed::start(socket.into(), Action::WatchList, waker.clone()),
            waker,
            local_build: astra_platform::build_info!(),
            ready: false,
            runtime: None,
            metadata: None,
            metadata_time: Instant::now() - Duration::from_secs(3),
            updates: None,
            update_probe: Some(update_probe),
            program_info: None,
            local_info: None,
            list_online: false,
        }
    }
    pub fn with_local_build_info(mut self, build: astra_platform::program_info::BuildInfo) -> Self {
        self.local_build = build;
        self
    }
    pub async fn poll(&mut self, ui: &mut impl Host) -> bool {
        let mut changed = false;
        if self
            .update_probe
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            if let Ok(Some(status)) = self.update_probe.take().unwrap().await {
                ui.update_status = Some(status);
                self.updates = Some(Feed::start(
                    self.socket.clone(),
                    Action::WatchUpdates,
                    self.waker.clone(),
                ));
                changed = true;
            }
        }
        if let Some(feed) = &mut self.updates {
            if let Some(result) = feed.take() {
                changed = true;
                match result
                    .and_then(|reply| serde_json::from_str(&reply.text).map_err(|e| e.to_string()))
                {
                    Ok(status) => {
                        ui.update_status = Some(status);
                        ui.tasks.update_error = None;
                    }
                    Err(error) => ui.tasks.update_error = Some(error),
                }
            }
        }
        if let Some(result) = self.list.take() {
            changed = true;
            match result {
                Ok(reply) => {
                    self.ready = true;
                    if !self.list_online {
                        ui.refresh_program_info();
                    }
                    self.list_online = true;
                    ui.update_rows(reply)
                }
                Err(error) => {
                    self.list_online = false;
                    // Cached platform information must not imply the manager
                    // is still reachable after its state subscription fails.
                    if ui.program_info.data.is_some() {
                        ui.program_info.error = Some(error.clone());
                    }
                    ui.runtime_error = error;
                }
            }
        }
        changed |= self.poll_program_info(ui).await;
        // Runtime data always follows the active profile, whatever Profiles shows.
        let key = ui
            .active_proxy()
            .and_then(|r| r.pid.map(|pid| (r.instance.name.clone(), pid)));
        if key.as_ref() != self.runtime.as_ref().map(|(key, _)| key) {
            if self.runtime.as_ref().zip(key.as_ref()).is_some_and(
                |(((old_name, old_pid), _), (new_name, new_pid))| {
                    old_name == new_name && old_pid != new_pid
                },
            ) {
                ui.notice_host_restart();
            }
            self.runtime = key.map(|(name, pid)| {
                let feed = Feed::start(
                    self.socket.clone(),
                    Action::WatchRuntime {
                        name: name.clone(),
                        expected_pid: pid,
                        interval_ms: crate::bootstrap::UI_REFRESH_INTERVAL.as_millis() as u64,
                    },
                    self.waker.clone(),
                );
                ((name, pid), feed)
            });
            // This is a profile/host PID transition, not a reconnect of the same Feed.
            // Clear identity before the old Snapshot is lost, including legacy peers.
            ui.reset_live_identity();
        }
        if let Some(((name, pid), feed)) = &mut self.runtime {
            if let Some(result) = feed.take() {
                changed = true;
                match result.and_then(|reply| {
                    serde_json::from_str::<astra_model::runtime::RuntimeView>(&reply.text)
                        .map_err(|e| e.to_string())
                }) {
                    Ok(view) if view.snapshot.pid == *pid => {
                        ui.accept_runtime_view(name.clone(), view)
                    }
                    Ok(_) => ui.runtime_disconnected(t("Runtime identity changed").into()),
                    Err(error) => {
                        ui.runtime_disconnected(error);
                    }
                }
            }
        }
        if self
            .metadata
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            let (name, task) = self.metadata.take().unwrap();
            changed = true;
            if let Ok(metadata) = task.await {
                if ui.editor.as_ref().is_some_and(|e| e.name == name) {
                    metadata.apply(ui);
                }
            }
        }
        if self.metadata.is_none() && self.metadata_time.elapsed() > Duration::from_secs(2) {
            if let Some(target) = operations::metadata_target(ui) {
                let name = ui.editor.as_ref().unwrap().name.clone();
                let socket = self.socket.clone();
                let notify = self.waker.clone();
                self.metadata = Some((
                    name,
                    tokio::task::spawn_local(async move {
                        let result = operations::metadata(&socket, target).await;
                        notify.wake();
                        result
                    }),
                ));
                self.metadata_time = Instant::now();
            }
        }
        changed
    }

    async fn poll_program_info(&mut self, ui: &mut impl Host) -> bool {
        let mut changed = false;
        if self
            .local_info
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let result = self.local_info.take().unwrap().await;
            match result {
                Ok(Ok(info)) => {
                    ui.program_info.local = Some(info);
                    ui.program_info.local_error = None;
                }
                Ok(Err(error)) => ui.program_info.local_error = Some(error),
                Err(error) => ui.program_info.local_error = Some(error.to_string()),
            }
            changed = true;
        }
        if self
            .program_info
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let result = self.program_info.take().unwrap().await;
            ui.program_info.loading = false;
            match result {
                Ok(Ok(info)) => {
                    ui.program_info.data = Some(info);
                    ui.program_info.error = None;
                }
                Ok(Err(error)) => ui.program_info.error = Some(error),
                Err(error) => ui.program_info.error = Some(error.to_string()),
            }
            changed = true;
        }
        if ui.page != 6 {
            return changed;
        }
        if self.local_info.is_none()
            && ui.program_info.local.is_none()
            && (ui.program_info.local_error.is_none() || ui.program_info.refresh)
        {
            ui.program_info.local_error = None;
            let notify = self.waker.clone();
            let build = self.local_build.clone();
            self.local_info = Some(tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    astra_management::v2::ProgramInfo::collect(build, vec![])
                })
                .await
                .map_err(|error| error.to_string());
                notify.wake();
                result
            }));
            changed = true;
        }
        if ui.program_info.refresh {
            ui.program_info.refresh = false;
            ui.program_info.loading = true;
            ui.program_info.error = None;
            if let Some(task) = self.program_info.take() {
                task.abort();
            }
            let socket = self.socket.clone();
            let notify = self.waker.clone();
            self.program_info = Some(tokio::spawn(async move {
                use astra_management::v2::{Query, RequestBody, ResponseBody};
                let result = async {
                    let response = tokio::time::timeout(
                        Duration::from_secs(3),
                        crate::protocol::request(&socket, RequestBody::Query(Query::ProgramInfo)),
                    )
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| format!("{error:#}"))?;
                    match response {
                        ResponseBody::ProgramInfo(info) => Ok(info),
                        _ => Err("Manager did not return program information".to_owned()),
                    }
                }
                .await;
                notify.wake();
                result
            }));
            changed = true;
        }
        changed
    }
}
