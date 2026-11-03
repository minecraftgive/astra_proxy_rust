//! Global update actions use the existing background request/progress/retry UI.
use super::{Action, Input, Prompt, Ui};
use crate::protocol::updates::{UpdateIntent, UpdateRequest, UpdateStatus};
use anyhow::Result;
use ratatui::text::Line;
use std::path::Path;

#[derive(Clone, Copy)]
pub(super) enum Command {
    Import,
    Status,
    Rollback,
    Activate,
    Recover,
}
impl Command {
    pub(super) const ALL: [Self; 5] = [
        Self::Import,
        Self::Status,
        Self::Activate,
        Self::Rollback,
        Self::Recover,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Import => "Import local update package",
            Self::Status => "Update status",
            Self::Rollback => "Roll back release",
            Self::Activate => "Activate staged release",
            Self::Recover => "Recover update operation",
        }
    }

    pub(super) fn run(self, ui: &mut Ui) -> Input {
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
    pub(super) fn unavailable(self, status: Option<&UpdateStatus>) -> Option<&'static str> {
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
    pub(super) fn request(self, status: &UpdateStatus) -> Result<UpdateRequest> {
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

pub(super) fn package_action(input: &str, directory: &Path) -> Result<Action> {
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

pub(super) struct View {
    pub(super) status: UpdateStatus,
}
impl View {
    pub(super) fn new(status: UpdateStatus) -> Self {
        Self { status }
    }
    pub(super) fn decode(text: &str) -> Result<Self> {
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
    pub(super) fn lines(&self) -> Vec<Line<'static>> {
        use crate::i18n::t;
        let status = &self.status;
        let clean = |value: String| {
            value
                .chars()
                .filter(|c| !c.is_control())
                .take(2048)
                .collect::<String>()
        };
        let mut lines: Vec<Line<'static>> = vec![
            t("Update status · staged, running and next-launch versions are separate")
                .to_owned()
                .into(),
            crate::tr!(
                "Manager host {} · experimental activation {}",
                status.host_version,
                t(if status.capabilities.experimental_enabled {
                    "Enabled"
                } else {
                    "Disabled"
                })
            )
            .into(),
            clean(crate::tr!(
                "Manager binary: {}",
                status
                    .manager_binary
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| t("Unknown").into())
            ))
            .into(),
            clean(crate::tr!(
                "Future core launch directory: {}",
                status.runtime_directory.display()
            ))
            .into(),
            crate::tr!(
                "Next launch: {}",
                status
                    .next_launch
                    .as_ref()
                    .map(|r| r.release_id.as_str())
                    .unwrap_or(t("Bundled release"))
            )
            .into(),
            crate::tr!(
                "Staged release: {} · {}",
                status.staged_release_id.as_deref().unwrap_or(t("None")),
                t(match status.staged_compatible {
                    Some(true) => "Host compatible",
                    Some(false) => "Staged only; host maintenance required",
                    None => "Not checked",
                })
            )
            .into(),
        ];
        if let Some(reason) = &status.capabilities.activation_blocked_reason {
            lines.push(clean(crate::tr!("Activation condition: {}", t(reason))).into());
        }
        if let Some(operation) = &status.operation {
            lines.push(
                crate::tr!(
                    "Operation {} · {}",
                    operation.request.operation_id,
                    t(operation.phase.label())
                )
                .into(),
            );
            if let Some(target) = &operation.target_directory {
                lines.push(clean(crate::tr!("Fixed target: {}", target.display())).into());
            }
            lines.push(
                crate::tr!(
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
                )
                .into(),
            );
            if let Some(error) = &operation.error {
                lines.push(clean(crate::tr!("Attention: {}", error)).into());
            }
        }
        if let Some(error) = &status.rule_sync_error {
            lines.push(clean(crate::tr!("Rule synchronization deferred: {}", error)).into());
        }
        for (name, source) in &status.source_rule_revisions {
            let effective = status.effective_rule_revisions.get(name);
            lines.push(
                clean(crate::tr!(
                    "Rule {} · source revision {} · new flows use {} · {}",
                    name,
                    source,
                    effective
                        .map(u64::to_string)
                        .unwrap_or_else(|| t("Unknown").into()),
                    t(if effective == Some(source) {
                        "Synchronized"
                    } else {
                        "Waiting for synchronization"
                    })
                ))
                .into(),
            );
        }
        for (name, instance) in &status.instances {
            lines.push(clean(crate::tr!("Instance {}", name)).into());
            if let Some(error) = &instance.error {
                lines.push(clean(format!("  {}", t(error))).into());
            }
            if let Some(value) = &instance.status {
                lines.push(
                    clean(crate::tr!(
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
                    ))
                    .into(),
                );
                if value.get("paused").and_then(|v| v.as_bool()) == Some(true) {
                    lines.push(t("  New-flow admission paused; existing connections retain their generation").to_owned().into());
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
                        lines.push(
                            clean(crate::tr!(
                                "  Generation {} · {} · {} sessions · {}",
                                generation["generation"],
                                t(state),
                                generation["flows"],
                                generation["directory"].as_str().unwrap_or("—")
                            ))
                            .into(),
                        );
                    }
                }
            }
        }
        lines.push(t("Closing this interface does not update hosts. Maintenance restarts end existing sessions. Draining has no completion estimate.").to_owned().into());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{with_language, Language};
    use crate::ui::{shell::Palette, Session};
    use std::path::PathBuf;

    #[test]
    fn update_palette_actions_work_without_an_instance() {
        with_language(Language::English, || {
            let mut ui = Ui::new(Session::default());
            ui.update_status = Some(UpdateStatus::default());
            for (query, expected) in [("Update status", 0), ("Import local update package", 2)] {
                let palette = Palette {
                    query: query.into(),
                    ..Default::default()
                };
                ui.palette = Some(palette);
                match (expected, ui.palette_run(0)) {
                    (0, Input::Run(Action::UpdateStatus, None)) => {}
                    (2, Input::Idle) => assert!(matches!(ui.prompt, Some(Prompt::UpdatePackage))),
                    _ => panic!("Unexpected update command"),
                }
            }
        });
    }

    #[test]
    fn package_prompt_keeps_quoted_paths_and_rejects_multiple_lines() {
        let directory = std::env::current_dir().unwrap();
        let action = package_action("& 'release files/local update.zip'", &directory).unwrap();
        assert!(
            matches!(action, Action::UpdateTransaction {request:UpdateRequest {intent:UpdateIntent::Stage {package}, ..}} if package == directory.join("release files/local update.zip"))
        );
        for input in ["", "  ", "''", "one.zip\ntwo.zip", "one.zip\0"] {
            assert!(package_action(input, &directory).is_err());
        }
        let mut ui = Ui::new(Session::default());
        ui.working_dir = PathBuf::from(&directory);
        assert!(
            matches!(ui.submit(Prompt::UpdatePackage, "release.tar.gz".into()).unwrap(), Some(Action::UpdateTransaction {request:UpdateRequest {intent:UpdateIntent::Stage {package}, ..}}) if package == directory.join("release.tar.gz"))
        );
    }

    #[test]
    fn transactions_require_capabilities_and_freeze_confirmed_target() {
        assert!(Command::Import.unavailable(None).is_some());
        let mut status = UpdateStatus::default();
        assert!(Command::Activate.request(&status).is_err());
        status.capabilities.activation_allowed = true;
        status.staged_release_id = Some("first".into());
        status.staged_compatible = Some(true);
        let request = Command::Activate.request(&status).unwrap();
        status.staged_release_id = Some("second".into());
        assert_eq!(
            request.intent,
            UpdateIntent::Activate {
                release_id: "first".into()
            }
        );
        assert!(Command::Rollback.request(&status).is_err());
        assert!(View::decode("{\"host_version\":\"legacy\"}").is_err());
        assert!(View::decode("{\"schema_version\":2}").is_err());
        assert!(View::decode(&serde_json::to_string(&status).unwrap()).is_ok());
    }

    #[test]
    fn update_status_uses_the_selected_ui_language() {
        let status = UpdateStatus {
            manager_binary: None,
            runtime_directory: PathBuf::from("bundle"),
            ..Default::default()
        };
        for (language, expected) in [
            (Language::English, "Manager binary:"),
            (Language::Simplified, "管理器程序："),
            (Language::Traditional, "管理器程式："),
        ] {
            with_language(language, || {
                let rendered = View::new(status.clone())
                    .lines()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(rendered.contains(expected), "{rendered}");
                if language == Language::English {
                    assert!(!rendered
                        .chars()
                        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)));
                }
            });
        }
    }
}
