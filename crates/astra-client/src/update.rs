use crate::protocol::updates::{UpdateIntent, UpdateRequest, UpdateStatus};
use crate::{
    protocol::Action,
    state::{Client, Input, Prompt},
};
use anyhow::Result;
use futures_util::FutureExt;
use std::path::Path;
pub struct View {
    pub status: UpdateStatus,
}
impl View {
    pub fn new(status: UpdateStatus) -> Self {
        Self { status }
    }
    pub fn decode(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        anyhow::ensure!(
            value
                .get("schema_version")
                .and_then(|version| version.as_u64())
                == Some(1),
            "This manager does not support the update transaction view"
        );
        Ok(Self::new(serde_json::from_value(value)?))
    }
}

/// Observe only this local host's new transactions. Historical completions must
/// not override an intentional launch with --bin-dir, nor cause restart loops.
#[derive(Default)]
pub struct Restart {
    waker: crate::waker::Waker,
    observed: bool,
    attempted: Option<(String, String)>,
    checking: Option<tokio::task::JoinHandle<Result<Option<crate::windows::Context>>>>,
    ready: Option<crate::windows::Context>,
}
impl Restart {
    pub fn with_waker(mut self, waker: crate::waker::Waker) -> Self {
        self.waker = waker;
        self
    }
    pub fn new(resumed: bool) -> Self {
        // A replacement may first observe a newer update completed while it
        // was starting. Resolving its own release is harmless (returns None).
        Self {
            observed: resumed,
            ..Default::default()
        }
    }

    pub fn defer(&mut self, target: crate::windows::Context) {
        self.ready = Some(target);
    }
    pub fn candidate(&mut self, status: &UpdateStatus) -> Option<(String, String)> {
        let key = crate::self_update::completed_release(status).map(|release| {
            (
                status
                    .operation
                    .as_ref()
                    .unwrap()
                    .request
                    .operation_id
                    .clone(),
                release.to_owned(),
            )
        });
        if !self.observed {
            self.observed = true;
            self.attempted = key.clone();
        }
        key
    }

    pub async fn poll(
        &mut self,
        status: Option<&UpdateStatus>,
        context: &crate::windows::Context,
        idle: bool,
    ) -> Result<Option<crate::windows::Context>> {
        self.poll_ready(status, context, idle)
    }

    pub fn poll_ready(
        &mut self,
        status: Option<&UpdateStatus>,
        context: &crate::windows::Context,
        idle: bool,
    ) -> Result<Option<crate::windows::Context>> {
        // An Agent's release paths belong to the target machine. They cannot
        // select code to execute in the controller's local frontend.
        if crate::protocol::target().is_some() {
            return Ok(None);
        }
        let Some(status) = status else {
            return Ok(None);
        };
        let key = self.candidate(status);
        if self
            .checking
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            self.ready = self
                .checking
                .take()
                .unwrap()
                .now_or_never()
                .expect("finished update check")??;
        }
        if key != self.attempted {
            self.ready = None;
            if self.checking.is_none() {
                if let Some((_, release)) = &key {
                    let context = context.clone();
                    let release = release.clone();
                    self.attempted = key.clone();
                    let waker = self.waker.clone();
                    self.checking = Some(tokio::task::spawn_blocking(move || {
                        let result = crate::self_update::resolve(&context, &release);
                        waker.wake();
                        result
                    }));
                }
            }
        }
        Ok(if idle && key.is_some() && key == self.attempted {
            self.ready.take()
        } else {
            None
        })
    }
}

#[derive(Clone, Copy)]
pub enum Command {
    Import,
    Status,
    Rollback,
    Activate,
    Recover,
}
impl Command {
    pub const ALL: [Self; 5] = [
        Self::Import,
        Self::Status,
        Self::Activate,
        Self::Rollback,
        Self::Recover,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Import => "Import local update package",
            Self::Status => "Update status",
            Self::Rollback => "Roll back release",
            Self::Activate => "Activate staged release",
            Self::Recover => "Recover update operation",
        }
    }

    pub fn run(self, ui: &mut Client) -> Input {
        if let Some(reason) = self.unavailable(ui.update_status.as_ref()) {
            ui.message = reason.into();
            return Input::Idle;
        }
        match self {
            Self::Import => {
                ui.prompt(Prompt::UpdatePackage);
                Input::Idle
            }
            Self::Status => Input::Run(Action::UpdateStatus, None),
            Self::Activate | Self::Rollback => {
                let request = ui
                    .update_status
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Read update status first"))
                    .and_then(|status| self.request(status));
                match request {
                    Ok(request) => ui.prompt(Prompt::UpdateConfirm(request)),
                    Err(error) => ui.message = error.to_string(),
                }
                Input::Idle
            }
            Self::Recover => match ui
                .update_status
                .as_ref()
                .and_then(|status| status.capabilities.recovery_operation_id.clone())
            {
                Some(operation_id) => Input::Run(Action::RecoverUpdate { operation_id }, None),
                None => {
                    ui.message = "No update operation requires recovery".into();
                    Input::Idle
                }
            },
        }
    }
    pub fn unavailable(self, status: Option<&UpdateStatus>) -> Option<&'static str> {
        match self {
            Self::Status => None,
            _ if status.is_none() => Some("Read update status first"),
            _ if status.is_some_and(|s| s.capabilities.operation_running) => {
                Some("An update operation is still running")
            }
            Self::Import
                if status.is_some_and(|s| s.capabilities.recovery_operation_id.is_some()) =>
            {
                Some("Recover the unfinished update first")
            }
            Self::Activate if status.is_some_and(|s| !s.capabilities.activation_allowed) => {
                Some("Online activation is unavailable; inspect update status")
            }
            Self::Activate
                if status.is_some_and(|s| {
                    s.staged_release_id.is_none() || s.staged_compatible != Some(true)
                }) =>
            {
                Some("No compatible staged release")
            }
            Self::Rollback if status.is_some_and(|s| s.capabilities.rollback_target.is_none()) => {
                Some("No rollback target in this host session")
            }
            Self::Rollback if status.is_some_and(|s| !s.capabilities.activation_allowed) => {
                Some("Online activation is unavailable; inspect update status")
            }
            Self::Recover
                if status.is_some_and(|s| s.capabilities.recovery_operation_id.is_none()) =>
            {
                Some("No update operation requires recovery")
            }
            _ => None,
        }
    }
    pub fn request(self, status: &UpdateStatus) -> Result<UpdateRequest> {
        if let Some(reason) = self.unavailable(Some(status)) {
            anyhow::bail!("{reason}");
        }
        let intent = match self {
            Self::Activate => UpdateIntent::Activate {
                release_id: status.staged_release_id.clone().unwrap(),
            },
            Self::Rollback => UpdateIntent::Rollback {
                target_directory: status.capabilities.rollback_target.clone().unwrap(),
            },
            _ => anyhow::bail!("This action does not activate a release"),
        };
        Ok(UpdateRequest::new(intent))
    }
}

pub fn package_action(input: &str, directory: &Path) -> Result<Action> {
    anyhow::ensure!(
        !input.trim().is_empty() && !input.contains(['\n', '\r', '\0']),
        crate::i18n::t("Choose one local update package or directory")
    );
    let path = crate::import::path(input, directory).map_err(|_| {
        anyhow::anyhow!(crate::i18n::t(
            "Choose one local update package or directory"
        ))
    })?;
    Ok(Action::UpdateTransaction {
        request: UpdateRequest::new(UpdateIntent::Stage { package: path }),
    })
}

#[cfg(test)]
#[path = "test/update_test.rs"]
mod tests;

impl View {
    pub fn summary(&self) -> Vec<Vec<crate::highlight::Token>> {
        use crate::i18n::t;
        let status = &self.status;
        let clean = |value: String| {
            value
                .chars()
                .filter(|c| !c.is_control())
                .take(2048)
                .collect::<String>()
        };
        let mut lines: Vec<String> = vec![
            t("Update status · staged, running and next-launch versions are separate").to_owned(),
            crate::tr!(
                "Manager host {} · experimental activation {}",
                status.host_version,
                t(if status.capabilities.experimental_enabled {
                    "Enabled"
                } else {
                    "Disabled"
                })
            ),
            clean(crate::tr!(
                "Manager binary: {}",
                status
                    .manager_binary
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| t("Unknown").to_owned())
            )),
            clean(crate::tr!(
                "Future core launch directory: {}",
                status.runtime_directory.display()
            )),
            crate::tr!(
                "Next launch: {}",
                status
                    .next_launch
                    .as_ref()
                    .map(|r| r.release_id.as_str())
                    .unwrap_or(t("Bundled release"))
            ),
            crate::tr!(
                "Staged release: {} · {}",
                status.staged_release_id.as_deref().unwrap_or(t("None")),
                t(match status.staged_compatible {
                    Some(true) => "Host compatible",
                    Some(false) => "Staged only; host maintenance required",
                    None => "Not checked",
                })
            ),
        ];
        if let Some(reason) = &status.capabilities.activation_blocked_reason {
            lines.push(clean(crate::tr!("Activation condition: {}", t(reason))));
        }
        if let Some(operation) = &status.operation {
            lines.push(crate::tr!(
                "Operation {} · {}",
                operation.request.operation_id,
                t(operation.phase.label())
            ));
            if let Some(target) = &operation.target_directory {
                lines.push(clean(crate::tr!("Fixed target: {}", target.display())));
            }
            lines.push(crate::tr!(
                "Runtime transition: {} · next-launch selection: {}",
                t(if operation.runtime_committed {
                    "Committed"
                } else {
                    "Not committed"
                }),
                t(if operation.next_launch_saved {
                    "Saved"
                } else {
                    "Not saved"
                })
            ));
            if let Some(error) = &operation.error {
                lines.push(clean(crate::tr!("Attention: {}", error)));
            }
        }
        if let Some(error) = &status.rule_sync_error {
            lines.push(clean(crate::tr!(
                "Rule synchronization deferred: {}",
                error
            )));
        }
        for (name, source) in &status.source_rule_revisions {
            let effective = status.effective_rule_revisions.get(name);
            lines.push(clean(crate::tr!(
                "Rule {} · source revision {} · new flows use {} · {}",
                name,
                source,
                effective
                    .map(u64::to_string)
                    .unwrap_or_else(|| t("Unknown").to_owned()),
                t(if effective == Some(source) {
                    "Synchronized"
                } else {
                    "Waiting for synchronization"
                })
            )));
        }
        for (name, instance) in &status.instances {
            lines.push(clean(crate::tr!("Instance {}", name)));
            if let Some(error) = &instance.error {
                lines.push(clean(format!("  {}", t(error))));
            }
            if let Some(value) = &instance.status {
                lines.push(clean(crate::tr!(
                    "  Network host {} · {}",
                    value
                        .get("version")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .unwrap_or(t("Version unknown")),
                    value
                        .get("binary")
                        .and_then(|v| v.as_str())
                        .unwrap_or(t("Binary path unknown"))
                )));
                if value.get("paused").and_then(|v| v.as_bool()) == Some(true) {
                    lines.push(t("  New-flow admission paused; existing connections retain their generation").to_owned());
                }
                if let Some(generations) = value.get("generations").and_then(|v| v.as_array()) {
                    for generation in generations {
                        let state = if generation.get("failed").and_then(|v| v.as_bool())
                            == Some(true)
                        {
                            "Failed"
                        } else if generation.get("prepared").and_then(|v| v.as_bool()) == Some(true)
                        {
                            "Prepared"
                        } else if generation.get("active").and_then(|v| v.as_bool()) == Some(true) {
                            "Accepting new flows"
                        } else {
                            "Draining naturally"
                        };
                        lines.push(clean(crate::tr!(
                            "  Generation {} · {} · {} sessions · {}",
                            generation["generation"],
                            t(state),
                            generation["flows"],
                            generation["directory"].as_str().unwrap_or("—")
                        )));
                    }
                }
            }
        }
        lines.push(t("After a successful local activation, this interface saves its session and restarts into the updated version.").to_owned());
        lines.push(t("Closing this interface does not update hosts. Maintenance restarts end existing sessions. Draining has no completion estimate.").to_owned());
        lines
            .into_iter()
            .map(|line| vec![crate::highlight::Token::raw(line)])
            .collect()
    }
}
