//! Installation request and status model, independent of frontend rendering.
use crate::{
    i18n::t,
    state::{Client, Input},
};
use anyhow::Result;
use astra_management::v2 as v;
use futures_util::FutureExt;
use std::{path::PathBuf, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
#[derive(Clone, Debug)]
pub struct Request {
    pub package_id: String,
    pub expected_release_id: String,
    pub scope: v::InstallationScope,
    pub elevate: bool,
}

#[derive(Default)]
pub struct State {
    pub open: bool,
    pub notice: bool,
    pub notice_accept: bool,
    pub notice_key: Option<String>,
    pub refresh: bool,
    pub status: Option<v::InstallationStatus>,
    pub query: Option<JoinHandle<Result<v::InstallationStatus, String>>>,
    pub operation: Option<JoinHandle<Result<v::InstallationResult, String>>>,
    pub progress: Option<watch::Receiver<Option<astra_ipc::progress::Progress>>>,
    pub events: Vec<String>,
    pub result: Option<v::InstallationResult>,
    pub error: Option<String>,
    pub query_error: Option<String>,
    pub selected: usize,
    pub scope: v::InstallationScope,
    pub scroll: u16,
}

impl State {
    pub fn visible(&self) -> bool {
        self.open || self.notice
    }
    pub fn busy(&self) -> bool {
        self.operation.is_some()
    }
    pub fn selected(&self) -> Option<&v::InstallationPackage> {
        self.status.as_ref()?.packages.get(self.selected)
    }
    pub fn destination(&self) -> Option<&v::InstallationDestination> {
        self.selected().map(|package| match self.scope {
            v::InstallationScope::User => &package.user,
            v::InstallationScope::System => &package.system,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallationAction {
    Open,
    Dismiss,
    Select(usize),
    ToggleChoice,
    Previous,
    Next,
    PageUp,
    PageDown,
    Refresh,
    Confirm,
}
pub fn notice_key(executable: &std::path::Path, socket: &std::path::Path) -> String {
    use sha2::Digest;
    let kind = if executable
        .file_stem()
        .is_some_and(|name| name == "astra-server-tui" || name == "astra-server-gui")
    {
        "server"
    } else {
        "client"
    };
    let (endpoint, agent) = crate::protocol::target().unwrap_or_else(|| (socket.into(), None));
    format!(
        "{kind}:{:x}",
        sha2::Sha256::digest(format!(
            "{}\0{}",
            endpoint.display(),
            agent.unwrap_or_default()
        ))
    )
}

async fn read_status(socket: PathBuf) -> Result<v::InstallationStatus, String> {
    let result = async {
        let hello =
            crate::protocol::request(&socket, v::RequestBody::Query(v::Query::Hello)).await?;
        anyhow::ensure!(
            matches!(hello, v::ResponseBody::Hello(info) if info.capabilities.iter().any(|c| c == "installation")),
            "{}",
            t("This target does not support installation yet.")
        );
        match crate::protocol::request(&socket, v::RequestBody::Query(v::Query::InstallationStatus))
            .await?
        {
            v::ResponseBody::InstallationStatus(status) => Ok(status),
            _ => anyhow::bail!("{}", t("Unexpected installation response")),
        }
    };
    tokio::time::timeout(Duration::from_secs(60), result)
        .await
        .map_err(|_| t("Installation status timed out. Press r to retry.").to_owned())?
        .map_err(|error: anyhow::Error| format!("{error:#}"))
}

impl Client {
    pub fn installation_failed(&mut self, error: String) {
        self.installation.error = Some(error.clone());
        self.message = error;
        self.tasks.notice_result(&self.message, true);
    }
    pub fn initialize_installation(
        &mut self,
        context: &crate::windows::Context,
        socket: &std::path::Path,
        launch: &crate::windows::Launch,
        resumed: bool,
    ) {
        if launch.window_id.is_some() || resumed {
            return;
        }
        let key = notice_key(&context.executable, socket);
        if !self.preferences.installation_notices.contains(&key) {
            self.installation.notice_key = Some(key);
            self.installation.refresh = true;
        }
    }

    pub fn open_installation(&mut self) {
        if let Some(key) = self.installation.notice_key.take() {
            self.change_preferences(|preferences| {
                preferences.installation_notices.insert(key);
            });
        }
        self.installation.notice = false;
        self.installation.open = true;
        self.installation.refresh = true;
        self.installation.scroll = 0;
    }

    pub async fn poll_installation(
        &mut self,
        socket: &std::path::Path,
        overlay_clear: bool,
    ) -> bool {
        self.poll_installation_ready(socket, overlay_clear)
    }

    pub fn poll_installation_ready(
        &mut self,
        socket: &std::path::Path,
        overlay_clear: bool,
    ) -> bool {
        let mut changed = false;
        if let Some(progress) = &mut self.installation.progress {
            if progress.has_changed().unwrap_or(false) {
                if let Some(event) = progress.borrow_and_update().clone() {
                    self.installation.events.push(format!(
                        "{} · {}",
                        t(&event.phase),
                        event.detail
                    ));
                    if self.installation.events.len() > 64 {
                        self.installation.events.remove(0);
                    }
                    changed = true;
                }
            }
        }
        if self
            .installation
            .operation
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let result = self
                .installation
                .operation
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished installation task")
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            self.installation.progress = None;
            match result {
                Ok(result) => {
                    self.message = result.message.clone();
                    self.installation.result = Some(result);
                    self.installation.error = None;
                    self.tasks.notice_result(&self.message, false);
                }
                Err(error) => {
                    self.message = error.clone();
                    self.installation.error = Some(error);
                    self.tasks.notice_result(&self.message, true);
                }
            }
            self.installation.refresh = true;
            changed = true;
        }
        if self
            .installation
            .query
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            match self
                .installation
                .query
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished installation task")
                .map_err(|error| error.to_string())
                .and_then(|result| result)
            {
                Ok(mut status) => {
                    self.installation.query_error = None;
                    status
                        .packages
                        .sort_by(|a, b| (&a.role, &a.source).cmp(&(&b.role, &b.source)));
                    self.installation.selected = self
                        .installation
                        .selected
                        .min(status.packages.len().saturating_sub(1));
                    self.installation.status = Some(status);
                }
                Err(error) => self.installation.query_error = Some(error),
            }
            changed = true;
        }
        if self.installation.refresh
            && self.installation.query.is_none()
            && !self.installation.busy()
        {
            self.installation.refresh = false;
            let socket = socket.to_owned();
            let waker = self.waker.clone();
            self.installation.query = Some(tokio::spawn(async move {
                let result = read_status(socket).await;
                waker.wake();
                result
            }));
            changed = true;
        }
        if self.installation.notice_key.is_some()
            && self.installation.status.is_some()
            && self.installation.query_error.is_none()
            && !self.installation.visible()
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.editor.is_none()
            && self.help_scroll.is_none()
            && overlay_clear
        {
            self.installation.notice = true;
            self.installation.notice_accept = true;
            changed = true;
        }
        changed
    }

    pub fn dismiss_installation_notice(&mut self, open: bool) {
        if let Some(key) = self.installation.notice_key.take() {
            self.change_preferences(|preferences| {
                preferences.installation_notices.insert(key);
            });
        }
        self.installation.notice = false;
        if open {
            self.page = crate::navigation::SETTINGS;
            self.open_installation();
        }
    }

    pub fn installation_action(&mut self, action: InstallationAction) -> Input {
        match action {
            InstallationAction::Open => {
                self.dismiss_installation_notice(true);
                return Input::Idle;
            }
            InstallationAction::Select(index) => {
                let count = self
                    .installation
                    .status
                    .as_ref()
                    .map_or(0, |s| s.packages.len());
                self.installation.selected = index.min(count.saturating_sub(1));
                self.installation.scroll = 0;
                return Input::Idle;
            }
            _ => {}
        }
        if self.installation.notice {
            match action {
                InstallationAction::ToggleChoice => {
                    self.installation.notice_accept = !self.installation.notice_accept
                }
                InstallationAction::Confirm => {
                    self.dismiss_installation_notice(self.installation.notice_accept)
                }
                InstallationAction::Dismiss => self.dismiss_installation_notice(false),
                _ => {}
            }
            return Input::Idle;
        }
        match action {
            InstallationAction::Dismiss => self.installation.open = false,
            InstallationAction::Previous => {
                self.installation.selected = self.installation.selected.saturating_sub(1);
                self.installation.scroll = 0;
            }
            InstallationAction::Next => {
                let count = self
                    .installation
                    .status
                    .as_ref()
                    .map_or(0, |s| s.packages.len());
                self.installation.selected =
                    (self.installation.selected + 1).min(count.saturating_sub(1));
                self.installation.scroll = 0;
            }
            InstallationAction::ToggleChoice if !self.installation.busy() => {
                self.installation.scope = match self.installation.scope {
                    v::InstallationScope::User => v::InstallationScope::System,
                    v::InstallationScope::System => v::InstallationScope::User,
                };
                self.installation.scroll = 0;
            }
            InstallationAction::Refresh if !self.installation.busy() => {
                self.installation.query_error = None;
                self.installation.refresh = true;
            }
            InstallationAction::PageDown => {
                self.installation.scroll = self.installation.scroll.saturating_add(5)
            }
            InstallationAction::PageUp => {
                self.installation.scroll = self.installation.scroll.saturating_sub(5)
            }
            InstallationAction::Confirm
                if !self.installation.busy()
                    && !self.installation.refresh
                    && self.installation.query.is_none()
                    && self.installation.query_error.is_none() =>
            {
                return self.installation_request()
            }
            _ => {}
        }
        Input::Idle
    }

    pub fn installation_request(&mut self) -> Input {
        if self.installation.busy()
            || self.installation.refresh
            || self.installation.query.is_some()
            || self.installation.query_error.is_some()
        {
            return Input::Idle;
        }
        let Some(destination) = self.installation.destination() else {
            return Input::Idle;
        };
        if let Some(reason) = &destination.unavailable_reason {
            self.installation.error = Some(reason.clone());
            return Input::Idle;
        }
        if destination.requires_elevation
            && crate::protocol::target().is_some_and(|(_, agent)| agent.is_some())
        {
            self.installation.error = Some(t("Administrator permission is required. Run installation on the target machine; remote requests cannot open a permission prompt.").into());
            return Input::Idle;
        }
        let package = self.installation.selected().unwrap();
        Input::Install(Request {
            package_id: package.id.clone(),
            expected_release_id: package.release_id.clone(),
            scope: self.installation.scope,
            elevate: destination.requires_elevation,
        })
    }

    pub fn start_installation(&mut self, socket: &std::path::Path, request: Request) {
        if self.installation.busy() {
            return;
        }
        self.installation.error = None;
        self.installation.result = None;
        self.installation.events.clear();
        let socket = socket.to_owned();
        let (progress, receiver) = watch::channel(None);
        self.installation.progress = Some(receiver);
        let waker = self.waker.clone();
        let mut progress_changed = progress.subscribe();
        self.installation.operation = Some(tokio::spawn(async move {
            let progress_waker = waker.clone();
            let wake = tokio::spawn(async move {
                while progress_changed.changed().await.is_ok() {
                    progress_waker.wake();
                }
            });
            let result = astra_ipc::progress::scope(
                progress,
                crate::protocol::execute(
                    &socket,
                    v::Command::InstallCurrentPackage {
                        package_id: request.package_id,
                        expected_release_id: request.expected_release_id,
                        scope: request.scope,
                    },
                ),
            )
            .await;
            let result = result
                .and_then(|response| match response {
                    v::ResponseBody::InstallationResult(result) => Ok(result),
                    _ => anyhow::bail!("{}", t("Unexpected installation response")),
                })
                .map_err(|error| format!("{error:#}"));
            wake.abort();
            waker.wake();
            result
        }));
    }
}

#[cfg(test)]
#[path = "test/installation_test.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailKind {
    Package { index: usize, selected: bool },
    Plain,
    Success,
    Error,
}
pub struct Detail {
    pub text: String,
    pub kind: DetailKind,
}
impl Detail {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: DetailKind::Plain,
        }
    }
}
impl State {
    pub fn scope_label(&self) -> &'static str {
        t(match self.scope {
            v::InstallationScope::User => "Current user",
            v::InstallationScope::System => "All users",
        })
    }
    pub fn target_label(&self) -> String {
        self.status
            .as_ref()
            .map(|s| format!("{} · {}", s.target, s.account))
            .unwrap_or_else(|| t("Loading installation status…").into())
    }
    pub fn notice_text(&self) -> String {
        let target = self
            .status
            .as_ref()
            .map(|s| s.target.as_str())
            .unwrap_or("");
        format!("{}\n\n{}: {target}\n\n{}", t("Install the current package to a stable location and create a launch entry."), t("Management target"), t("You can do this later from Settings → Installation. Existing configurations and running instances are retained."))
    }
    pub fn details(&self) -> Vec<Detail> {
        let mut lines = Vec::new();
        if let Some(status) = &self.status {
            for (index, package) in status.packages.iter().enumerate() {
                lines.push(Detail {
                    text: format!("{} · {}", t(&package.role), package.release_id),
                    kind: DetailKind::Package {
                        index,
                        selected: index == self.selected,
                    },
                });
            }
            for role in ["client", "server", "controller", "web"] {
                if !status.packages.iter().any(|package| package.role == role) {
                    let reason = status
                        .unavailable_roles
                        .get(role)
                        .map(String::as_str)
                        .unwrap_or("No complete package was found on this target.");
                    lines.push(Detail::plain(format!(
                        "  {} · {}: {}",
                        t(role),
                        t("Unavailable"),
                        t(reason)
                    )));
                }
            }
            if let Some(package) = self.selected() {
                let destination = self.destination().unwrap();
                lines.extend([
                    Detail::plain(""),
                    Detail::plain(format!(
                        "{}: {}",
                        t("Package source"),
                        package.source.display()
                    )),
                    Detail::plain(format!(
                        "{}: {}",
                        t("Install location"),
                        destination
                            .directory
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "—".into())
                    )),
                    Detail::plain(format!(
                        "{}: {}",
                        t("Launch entry"),
                        destination
                            .entry
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "—".into())
                    )),
                ]);
                if destination.installed {
                    lines.push(Detail::plain(t("This package is already installed.")));
                }
                if destination.requires_elevation {
                    lines.push(Detail::plain(t(
                        "Administrator permission is required for this location.",
                    )));
                }
                if let Some(reason) = &destination.unavailable_reason {
                    lines.push(Detail::plain(reason.clone()));
                }
            }
            for warning in &status.warnings {
                lines.push(Detail::plain(warning.clone()));
            }
        }
        if self.query.is_some() {
            lines.push(Detail::plain(t("Checking the current package…")));
        }
        if self.busy() {
            lines.push(Detail::plain(t(
                "Installing… You may close this panel while the operation continues.",
            )));
        }
        for event in &self.events {
            lines.push(Detail::plain(event.clone()));
        }
        if let Some(result) = &self.result {
            lines.extend([Detail::plain(""), Detail { text: result.message.clone(), kind: DetailKind::Success }, Detail::plain(format!("{}: {}", t("Launch command"), result.launch_command)), Detail::plain(t("Running programs keep their current version. Use the launch command after a maintenance restart."))]);
        }
        if let Some(error) = &self.error {
            lines.push(Detail {
                text: error.clone(),
                kind: DetailKind::Error,
            });
        }
        if let Some(error) = &self.query_error {
            lines.push(Detail {
                text: error.clone(),
                kind: DetailKind::Error,
            });
        }
        lines
    }
}
