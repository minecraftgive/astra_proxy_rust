use crate::{i18n::t, node_transfer, protocol::*, state::*, tr, update};
use anyhow::{Context, Result};
use std::path::PathBuf;

impl Client {
    pub fn submit(&mut self, prompt: Prompt, input: String) -> Result<Option<Action>> {
        Ok(match prompt {
            Prompt::UpdatePackage => Some(update::package_action(&input, &self.working_dir)?),
            Prompt::UpdateConfirm(request) => {
                (input == "yes").then_some(Action::UpdateTransaction { request })
            }
            Prompt::AddName => {
                anyhow::ensure!(
                    !input.is_empty()
                        && input.len() <= 24
                        && input
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                    t("Name: 1–24 letters, digits, - or _")
                );
                Some(Action::Create {
                    name: input,
                    kind: self.kind,
                })
            }
            Prompt::AddPath(name) => Some(Action::Add(Instance {
                name,
                kind: self.kind,
                config: absolute(&input, &self.working_dir)?,
            })),
            Prompt::Config(name) => Some(Action::SetConfig(
                name,
                absolute(&input, &self.working_dir)?,
            )),
            Prompt::Clone(name) => Some(Action::CloneConfig(
                name,
                absolute(&input, &self.working_dir)?,
            )),
            Prompt::ExportConfig { text }
            | Prompt::ExportRule { text }
            | Prompt::ExportTerminal { text, .. } => {
                let path = absolute(&input, &self.working_dir)?;
                crate::export::write_new(&path, &text)?;
                self.message = tr!("Exported to {}", path.display());
                None
            }
            Prompt::ExportNode { text, share } => {
                let path = node_transfer::export_path(&input, &self.working_dir, share)?;
                crate::export::write_new(&path, &text)?;
                self.message = tr!("Exported to {}", path.display());
                None
            }
            Prompt::ExportNodeDestination { .. } | Prompt::ImportNodeSelection { .. } => None,
            Prompt::Remove(name) if input == "yes" => Some(Action::Remove(name)),
            Prompt::ForceStart(name) if input == "yes" => Some(Action::ForceStart(name)),

            Prompt::Shutdown if input == "yes" => Some(Action::Shutdown),
            Prompt::Discard if input == "yes" => {
                self.discard_current_draft();
                None
            }
            Prompt::Find(page) => {
                if !input.is_empty() {
                    self.last_find = input.clone();
                    self.searches.insert(page, input.clone());
                    if !self.find(page, &input) {
                        self.message = tr!("No match for \"{}\"", input);
                    }
                }
                None
            }
            Prompt::CloseAll { name, pid } if input == "yes" => Some(Action::Runtime {
                name,
                command: astra_model::runtime::Command::Disconnect(None),
                expected_pid: pid,
            }),
            Prompt::ElevateTun { name, pid } if input == "yes" => Some(Action::PrepareElevation {
                name,
                expected_pid: pid,
            }),
            Prompt::Explain(name) => Some(Action::ExplainRoute {
                name,
                request: crate::explanation::request(&input)?,
            }),
            Prompt::ImportNode(mode) => {
                self.prepare_node_import(&input, mode)?;
                None
            }
            Prompt::ImportRule(mode) => {
                let editor = self.editor.as_ref().context("Missing editor")?;
                let imported = crate::import::merge_at(
                    &input,
                    &self.working_dir,
                    &editor.text(),
                    Kind::Rule,
                    mode,
                )?;
                self.editor.as_mut().unwrap().import(imported.text)?;
                self.message = "Imported rules to draft · Ctrl-S save".into();
                None
            }
            Prompt::Import => {
                let share = astra_convert::share::is_share(&input);
                if share {
                    self.prepare_node_import(&input, crate::import::MergeMode::Append)?;
                    return Ok(None);
                }
                let editor = self.editor.as_ref().context("Missing editor")?;
                let path = crate::import::path(&input, &self.working_dir)?;
                let imported = crate::import::load(&path, self.kind, &editor.text())?;
                let label = path.display().to_string();
                if editor.text() != editor.original || !imported.warnings.is_empty() {
                    let mut notices = vec![label];
                    if editor.text() != editor.original {
                        notices.push(t("Unsaved draft will be replaced").into());
                    }
                    notices.extend(imported.warnings.iter().map(|s| t(s).to_owned()));
                    self.prompt(Prompt::ImportConfirm {
                        text: imported.text,
                        notices: notices.join("\n\n"),
                    });
                    self.scroll = 0;
                } else {
                    self.editor.as_mut().unwrap().import(imported.text)?;
                    self.message = "Imported to draft · Ctrl-S save".into();
                }
                None
            }
            Prompt::ImportConfirm { text, .. } if input == "yes" => {
                self.editor
                    .as_mut()
                    .context("Missing editor")?
                    .import(text)?;
                self.message = "Imported to draft · Ctrl-S save".into();
                None
            }
            _ => None,
        })
    }
}

fn absolute(text: &str, directory: &std::path::Path) -> Result<PathBuf> {
    anyhow::ensure!(!text.is_empty(), t("Enter a configuration file path"));
    let path = PathBuf::from(text);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

#[derive(Clone, Copy)]
pub enum RecoveryAction {
    Back,
    Continue,
}
/// Validation failure retains exactly the submitted prompt and text for either
/// inline correction or the terminal recovery page.
pub struct PromptError {
    pub prompt: Prompt,
    pub input: String,
    pub error: anyhow::Error,
}
impl Client {
    pub fn submit_prompt(&mut self, input: String) -> std::result::Result<Input, Box<PromptError>> {
        if let Some(prompt) = self
            .prompt
            .as_ref()
            .filter(|prompt| prompt.picker())
            .cloned()
        {
            return self.submit_transfer().map_err(|error| {
                Box::new(PromptError {
                    prompt,
                    input,
                    error,
                })
            });
        }
        let Some(prompt) = self.prompt.take() else {
            return Ok(Input::Idle);
        };
        let input = input.trim().to_owned();
        match self.submit(prompt.clone(), input.clone()) {
            Ok(Some(action)) => Ok(Input::Run(action, Some(Box::new((prompt, input))))),
            Ok(None) => Ok(Input::Idle),
            Err(error) => {
                self.prompt = Some(prompt.clone());
                self.input = input.clone();
                self.input_cursor = self.input.chars().count();
                Err(Box::new(PromptError {
                    prompt,
                    input,
                    error,
                }))
            }
        }
    }
    pub fn cancel_prompt(&mut self) {
        if let Some(Prompt::AddPath(name)) = self.prompt.take() {
            self.prompt = Some(Prompt::AddName);
            self.input = name;
            self.input_cursor = self.input.chars().count();
        }
    }
    pub fn recover(&mut self, action: RecoveryAction) -> Input {
        match action {
            RecoveryAction::Back => {
                let Some(recovery) = self.recovery.take() else {
                    return Input::Idle;
                };
                if matches!(recovery.resume, Resume::Session) {
                    self.persistence_paused = true;
                }
                if matches!(recovery.resume, Resume::Request(Action::List)) {
                    self.refresh_paused = true;
                }
                if matches!(recovery.resume, Resume::Editor) {
                    if let Some(editor) = &mut self.editor {
                        editor.cancel_input();
                    }
                }
                if matches!(recovery.resume, Resume::Request(Action::Logs(_))) {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.stop();
                    }
                }
                if matches!(recovery.resume, Resume::Request(Action::SetLogLevel { .. })) {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.cancel_input();
                    }
                }
                self.prompt = None;
                self.message = if self.persistence_paused {
                    "Operation ended; draft retained in memory; session saving disabled until next launch"
                } else { "Operation ended; editor draft retained" }.into();
            }
            RecoveryAction::Continue => {
                let Some(recovery) = self.recovery.take() else {
                    return Input::Idle;
                };
                self.scroll = 0;
                match recovery.resume {
                    Resume::Prompt(prompt, input) => {
                        self.prompt = Some(prompt);
                        self.input = input;
                        self.input_cursor = self.input.chars().count();
                        self.confirmation_accept = false;
                    }
                    Resume::Request(Action::Save { .. }) if self.editor.is_some() => {
                        self.message =
                            "Draft retained; correct the file or permissions, then Ctrl-S to retry"
                                .into();
                    }
                    Resume::Request(Action::BindStart { name, targets }) => {
                        self.open_bindings(name, Some(targets));
                    }
                    Resume::Request(action) => return Input::Run(action, None),
                    Resume::Editor => {
                        self.message = "Edit the value · Enter confirm".into();
                    }
                    Resume::Session => {
                        self.message = "Retrying…".into();
                    }
                }
            }
        }
        Input::Idle
    }
}

#[cfg(test)]
#[path = "test/prompts_test.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptPathKind {
    OpenFile,
    OpenFileOrDirectory,
    SaveFile,
}
impl Prompt {
    pub fn path_kind(&self) -> Option<PromptPathKind> {
        use PromptPathKind::*;
        match self {
            Self::AddPath(_)
            | Self::Config(_)
            | Self::Import
            | Self::ImportNode(_)
            | Self::ImportRule(_) => Some(OpenFile),
            Self::UpdatePackage => Some(OpenFileOrDirectory),
            Self::Clone(_)
            | Self::ExportConfig { .. }
            | Self::ExportNode { .. }
            | Self::ExportRule { .. }
            | Self::ExportTerminal { .. } => Some(SaveFile),
            _ => None,
        }
    }
    pub fn body(&self) -> Option<String> {
        match self {
            Self::ImportConfirm { notices, .. } => Some(notices.clone()),
            Self::ForceStart(name) => Some(format!("{}\n\n{}\n\n{}", name,
                t("Replace previous session records for this instance and its stopped startup bindings. An unreachable previous process may still be running."),
                t("Active or inaccessible control endpoints still prevent replacement."))),
            Self::UpdateConfirm(request) => {
                let target = match &request.intent {
                    crate::protocol::updates::UpdateIntent::Stage { package } => package.display().to_string(),
                    crate::protocol::updates::UpdateIntent::Activate { release_id } => release_id.clone(),
                    crate::protocol::updates::UpdateIntent::Rollback { target_directory } => target_directory.display().to_string(),
                };
                Some(format!("{}\n\n{}\n\n{}: {}", t("Apply this exact update target?"), target, t("Operation"), request.operation_id))
            }
            _ => None,
        }
    }
}
