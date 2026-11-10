//! Installation request and status model, independent of frontend rendering.
mod local;
use crate::package_choice::Package;
use crate::{
    i18n::t,
    state::{Client, Input},
};
use anyhow::Result;
use astra_management::v2 as v;
use futures_util::FutureExt;
pub use local::prepare_elevation;
use std::{path::PathBuf, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
#[derive(Clone, Debug)]
pub enum Origin {
    FrontendPackage(Package),
    ManagerPackage,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub origin: Origin,
    pub package: Box<v::InstallationPackage>,
    pub package_id: String,
    pub expected_release_id: String,
    pub scope: v::InstallationScope,
    pub elevate: bool,
}

#[derive(Clone, Debug)]
pub struct Completed {
    pub manifest_sha256: Option<String>,
    pub source: PathBuf,
    pub release_id: String,
    pub scope: v::InstallationScope,
    pub directory: PathBuf,
}

#[derive(Debug)]
pub struct Catalog {
    status: v::InstallationStatus,
    frontend: Option<Package>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activation {
    Waiting,
    Running,
}
impl Activation {
    fn label(self) -> &'static str {
        t(match self {
            Self::Waiting => "Package installed; waiting to switch the running version.",
            Self::Running => "Running the installed version.",
        })
    }
}

#[derive(Default)]
pub struct State {
    context: Option<crate::windows::Context>,
    frontend_directory: Option<PathBuf>,
    frontend: Option<Package>,
    pending_digest: Option<String>,
    pub completed: Option<Completed>,
    pub activation: Option<Activation>,
    pending_package: Option<(v::InstallationPackage, v::InstallationScope)>,
    pub open: bool,
    pub notice: bool,
    pub notice_accept: bool,
    pub notice_key: Option<String>,
    pub refresh: bool,
    pub status: Option<v::InstallationStatus>,
    pub query: Option<JoinHandle<Result<Catalog, String>>>,
    pub operation: Option<JoinHandle<Result<v::InstallationResult, String>>>,
    pub progress: Option<watch::Receiver<Option<astra_ipc::progress::Progress>>>,
    /// Latest structured step; steps only advance, so a coalesced update is harmless.
    pub latest: Option<ProgressView>,
    pub result: Option<v::InstallationResult>,
    pub error: Option<String>,
    pub query_error: Option<String>,
    pub selected: usize,
    pub scope: v::InstallationScope,
    pub scroll: u16,
}

impl State {
    fn accept_catalog(&mut self, catalog: Catalog) {
        let previous = self.selected().map(|p| p.id.clone());
        let first = self.status.is_none();
        self.query_error = None;
        self.frontend = catalog.frontend;
        self.selected = match previous {
            Some(id) => catalog
                .status
                .packages
                .iter()
                .position(|p| p.id == id)
                .unwrap_or(catalog.status.packages.len()),
            None if first => 0,
            None => catalog.status.packages.len(),
        };
        self.status = Some(catalog.status);
    }
    pub fn visible(&self) -> bool {
        self.open || self.notice
    }
    pub fn can_install(&self) -> bool {
        !self.busy()
            && !self.refresh
            && self.query.is_none()
            && self.query_error.is_none()
            && self
                .destination()
                .is_some_and(|destination| destination.unavailable_reason.is_none())
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
    RetryMaintenance,
    RestorePrevious,
    MaintenanceLog,
    CancelMaintenance,
    LaunchNow,
    ToggleAutoLaunch,
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

async fn read_catalog(
    socket: PathBuf,
    directory: Option<PathBuf>,
    pinned: Option<Package>,
) -> Result<Catalog, String> {
    let local = if let Some(directory) = directory {
        Some(
            tokio::task::spawn_blocking(move || -> Result<_> {
                let package = match pinned {
                    Some(package) => {
                        package.verify()?;
                        package
                    }
                    None => Package::inspect(&directory)?,
                };
                let candidate = local::candidate(&package)?;
                Ok((package, candidate))
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("{e:#}"))?,
        )
    } else {
        None
    };
    let mut status = match read_status(socket).await {
        Ok(status) => status,
        Err(error) if local.is_some() => v::InstallationStatus {
            target: t("Local computer").into(),
            account: astra_platform::installation::account_name(),
            packages: Vec::new(),
            unavailable_roles: Default::default(),
            warnings: vec![error],
        },
        Err(error) => return Err(error),
    };
    status
        .packages
        .sort_by(|a, b| (&a.role, &a.source).cmp(&(&b.role, &b.source)));
    let frontend = local.map(|(package, candidate)| {
        status.packages.retain(|p| p.source != candidate.source);
        status.unavailable_roles.remove(&candidate.role);
        status.packages.insert(0, candidate);
        package
    });
    Ok(Catalog { status, frontend })
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
        if crate::protocol::target().is_none() {
            self.installation.context = Some(context.clone());
            self.installation.frontend_directory = context
                .executable
                .parent()
                .filter(|p| p.join(astra_update::MANIFEST_FILE).is_file())
                .map(PathBuf::from);
        }
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
                if let Some(view) = progress
                    .borrow_and_update()
                    .as_ref()
                    .and_then(ProgressView::from_event)
                {
                    if self
                        .installation
                        .latest
                        .is_none_or(|latest| latest.step_index <= view.step_index)
                    {
                        self.installation.latest = Some(view);
                        changed = true;
                    }
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
                    if crate::protocol::target().is_none() {
                        if let Some((package, scope)) = self.installation.pending_package.take() {
                            if matches!(package.role.as_str(), "client" | "server") {
                                self.installation.completed = Some(Completed {
                                    manifest_sha256: self.installation.pending_digest.take(),
                                    source: package.source,
                                    release_id: package.release_id,
                                    scope,
                                    directory: result.directory.clone(),
                                });
                            }
                        }
                    }
                    self.message = t(&result.message).to_owned();
                    if self.installation.completed.is_some() {
                        self.installation.activation = Some(Activation::Waiting);
                    }
                    self.installation.latest = Some(ProgressView::complete());
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
                Ok(catalog) => {
                    self.installation.accept_catalog(catalog);
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
            let directory = self.installation.frontend_directory.clone();
            let pinned = self.installation.frontend.clone();
            self.installation.query = Some(tokio::spawn(async move {
                let result = read_catalog(socket, directory, pinned).await;
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
            InstallationAction::RetryMaintenance => {
                return crate::update::Command::RetryMaintenance.run(self)
            }
            InstallationAction::RestorePrevious => {
                return crate::update::Command::RestorePrevious.run(self)
            }
            InstallationAction::MaintenanceLog => {
                return crate::update::Command::MaintenanceLog.run(self)
            }
            InstallationAction::CancelMaintenance => {
                return crate::update::Command::CancelMaintenance.run(self)
            }
            InstallationAction::LaunchNow => return crate::update::Command::LaunchNow.run(self),
            InstallationAction::ToggleAutoLaunch => {
                return crate::update::Command::ToggleAutoLaunch.run(self)
            }
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
        let origin = match &self.installation.frontend {
            Some(local) if local.id() == package.id => Origin::FrontendPackage(local.clone()),
            _ => Origin::ManagerPackage,
        };
        Input::Install(Request {
            package: Box::new(package.clone()),
            origin,
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
        self.installation.completed = None;
        self.update_launch.clear();
        let package = request.package.as_ref().clone();
        self.installation.activation = None;
        self.installation.pending_digest = match &request.origin {
            Origin::FrontendPackage(local) => Some(local.manifest_sha256.clone()),
            Origin::ManagerPackage => None,
        };
        self.installation.pending_package = Some((package, request.scope));
        self.installation.result = None;
        self.installation.latest = None;
        let socket = socket.to_owned();
        let (progress, receiver) = watch::channel(None);
        self.installation.progress = Some(receiver);
        let waker = self.waker.clone();
        let mut progress_changed = progress.subscribe();
        let context = self.installation.context.clone();
        self.installation.operation = Some(tokio::spawn(async move {
            let progress_waker = waker.clone();
            let wake = tokio::spawn(async move {
                while progress_changed.changed().await.is_ok() {
                    progress_waker.wake();
                }
            });
            let result = match &request.origin {
                Origin::FrontendPackage(package) => {
                    let package = package.clone();
                    match context {
                        Some(context) => tokio::task::spawn_blocking(move || {
                            local::install(&package, &request, &context, progress)
                        })
                        .await
                        .map_err(anyhow::Error::from)
                        .and_then(|r| r),
                        None => Err(anyhow::anyhow!("Local installation context is missing")),
                    }
                }
                Origin::ManagerPackage => astra_ipc::progress::scope(
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
                .await
                .and_then(|response| match response {
                    v::ResponseBody::InstallationResult(result) => Ok(result),
                    _ => anyhow::bail!("Unexpected installation response"),
                }),
            }
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
                    text: format!(
                        "{} · {}{}",
                        t(&package.role),
                        package.release_id,
                        if self
                            .frontend
                            .as_ref()
                            .is_some_and(|local| local.id() == package.id)
                        {
                            format!(" · {}", t("Current interface package"))
                        } else {
                            String::new()
                        }
                    ),
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
        if let Some(result) = &self.result {
            lines.extend([Detail::plain(""), Detail { text: t(&result.message).to_owned(), kind: DetailKind::Success }, Detail::plain(format!("{}: {}", t("Launch command"), result.launch_command)), Detail::plain(t("The complete package is installed in a new directory, including running components. The local interface can launch now or automatically after 30 seconds. Hosts and services use the new files after a maintenance restart."))]);
        }
        if let Some(activation) = self.activation {
            lines.push(Detail::plain(activation.label()));
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

/// One structured installation step observed from the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgressView {
    pub step: v::InstallationStep,
    pub step_index: u8,
    pub step_count: u8,
    pub current: u64,
    pub total: u64,
}
impl ProgressView {
    pub fn from_payload(payload: &v::InstallationProgress) -> Self {
        Self {
            step: payload.step,
            step_index: payload.step_index,
            step_count: payload.step_count.max(1),
            current: payload.current.parse().unwrap_or(0),
            total: payload.total.parse().unwrap_or(0),
        }
    }
    fn from_event(event: &astra_ipc::progress::Progress) -> Option<Self> {
        (event.phase == "installation_event")
            .then(|| serde_json::from_str::<v::InstallationProgress>(&event.detail).ok())
            .flatten()
            .map(|payload| Self::from_payload(&payload))
    }
    fn complete() -> Self {
        Self {
            step: v::InstallationStep::Complete,
            step_index: STEPS.len() as u8 - 1,
            step_count: STEPS.len() as u8,
            current: 0,
            total: 0,
        }
    }
    /// Overall percentage: copying dominates, the short steps get fixed slices.
    pub fn percent(&self) -> u8 {
        match self.step {
            v::InstallationStep::Preparing => 2,
            v::InstallationStep::Copying if self.total > 0 => {
                5 + (80 * self.current.min(self.total) / self.total) as u8
            }
            v::InstallationStep::Copying => 5,
            v::InstallationStep::Verifying => 88,
            v::InstallationStep::Publishing => 96,
            v::InstallationStep::Complete => 100,
        }
    }
}

pub const STEPS: [(v::InstallationStep, &str); 5] = [
    (v::InstallationStep::Preparing, "Prepare installation"),
    (v::InstallationStep::Copying, "Copy program package"),
    (v::InstallationStep::Verifying, "Verify installed files"),
    (v::InstallationStep::Publishing, "Publish launch entry"),
    (v::InstallationStep::Complete, "Installation complete"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepState {
    Done,
    Active,
    Failed,
    Pending,
}
pub struct StepRow {
    pub label: &'static str,
    pub state: StepState,
}

impl State {
    /// Whether a progress section belongs on screen.
    pub fn progress_visible(&self) -> bool {
        self.busy() || self.latest.is_some()
    }
    /// `None` while waiting for the first step from the target (indeterminate).
    pub fn percent(&self) -> Option<u8> {
        self.latest.map(|latest| latest.percent())
    }
    pub fn progress_label(&self) -> String {
        let Some(latest) = self.latest else {
            return t("Connecting to the target…").into();
        };
        let step = STEPS
            .iter()
            .find(|(step, _)| *step == latest.step)
            .map_or("", |(_, label)| t(label));
        if latest.step == v::InstallationStep::Copying && latest.total > 0 {
            format!(
                "{step} · {} / {}",
                format_bytes(latest.current),
                format_bytes(latest.total)
            )
        } else {
            step.into()
        }
    }
    pub fn step_rows(&self) -> Vec<StepRow> {
        let active = self.latest.map(|latest| latest.step_index as usize);
        let failed = !self.busy() && self.error.is_some();
        STEPS
            .iter()
            .enumerate()
            .map(|(index, (step, label))| StepRow {
                label: t(label),
                state: match active {
                    Some(_) if *step == v::InstallationStep::Complete && self.result.is_some() => {
                        StepState::Done
                    }
                    Some(active) if index < active => StepState::Done,
                    Some(active) if index == active && failed => StepState::Failed,
                    Some(active) if index == active && *step == v::InstallationStep::Complete => {
                        StepState::Done
                    }
                    Some(active) if index == active => StepState::Active,
                    None if index == 0 && self.busy() => StepState::Active,
                    None if index == 0 && failed => StepState::Failed,
                    _ => StepState::Pending,
                },
            })
            .collect()
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
