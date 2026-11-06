//! Configuration-node import and export intent, shared by every frontend.
use crate::{
    protocol::Kind,
    state::{Client, Input, Prompt, Resume},
};
use anyhow::{Context, Result};
use std::path::PathBuf;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferAction {
    Previous,
    Next,
    First,
    Last,
    PagePrevious,
    PageNext,
    Toggle,
    ToggleAll,
    ToggleIndex(usize),
    SetCursor(usize),
    SetDestination(bool),
    SwitchDestination,
    Confirm,
    Cancel,
}
pub fn export_path(input: &str, directory: &std::path::Path, share: bool) -> Result<PathBuf> {
    let mut path = crate::import::path(input, directory)?;
    if path.is_dir()
        || (!path.is_file() && path.extension().is_none())
        || input.trim_end_matches(['\'', '"']).ends_with(['/', '\\'])
    {
        path.push(if share { "nodes.txt" } else { "nodes.toml" });
    }
    Ok(path)
}

impl Client {
    pub fn start_node_export(&mut self, share: bool) -> Input {
        let result = (|| {
            anyhow::ensure!(self.kind == Kind::Proxy, "Select a proxy configuration");
            let editor = self.editor.as_ref().context("Missing editor")?;
            anyhow::ensure!(
                editor.browsing(),
                "Confirm or cancel the field before exporting"
            );
            if share {
                editor.export_selected_node_share()
            } else {
                editor.export_selected_node()
            }
        })();
        match result {
            Ok(text) => self.prompt(Prompt::ExportNodeDestination {
                text,
                share,
                clipboard: false,
            }),
            Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
        }
        Input::Idle
    }

    pub fn prepare_node_import(
        &mut self,
        input: &str,
        mode: crate::import::MergeMode,
    ) -> Result<()> {
        anyhow::ensure!(
            self.kind == Kind::Proxy,
            "Configuration does not match this core"
        );
        let name = self.editor.as_ref().context("Missing editor")?.name.clone();
        let candidates = crate::import::node_candidates_at(input, &self.working_dir)?;
        let selected = (0..candidates.tags.len()).collect();
        let path = self.draft_key(&name).1;
        self.prompt(Prompt::ImportNodeSelection {
            name,
            path,
            candidates,
            selected,
            cursor: 0,
            mode,
        });
        Ok(())
    }

    pub fn copy_node_export_with(
        &mut self,
        text: &str,
        copy: impl FnOnce(&str) -> Result<()>,
    ) -> Result<()> {
        copy(text)?;
        self.message = "Selected nodes copied to clipboard".into();
        Ok(())
    }

    pub fn node_transfer(&mut self, action: TransferAction) -> Option<Input> {
        self.node_transfer_with(action, crate::clipboard::copy)
    }
    pub fn node_transfer_with(
        &mut self,
        action: TransferAction,
        copy: impl FnOnce(&str) -> Result<()>,
    ) -> Option<Input> {
        if action == TransferAction::Confirm {
            let prompt = self.prompt.as_ref()?.clone();
            if !prompt.picker() {
                return None;
            }
            if let Err(error) = self.submit_transfer_with(copy) {
                self.prompt = None;
                self.fail(format!("{error:#}"), Resume::Prompt(prompt, String::new()));
            }
            return Some(Input::Idle);
        }
        match self.prompt.as_mut()? {
            Prompt::ExportNodeDestination { clipboard, .. } => match action {
                TransferAction::SetDestination(value) => *clipboard = value,
                TransferAction::SwitchDestination
                | TransferAction::Previous
                | TransferAction::Next => *clipboard = !*clipboard,
                TransferAction::Cancel => self.prompt = None,
                _ => {}
            },
            Prompt::ImportNodeSelection {
                candidates,
                selected,
                cursor,
                ..
            } => match action {
                TransferAction::Cancel => self.prompt = None,
                TransferAction::SetCursor(index) => {
                    *cursor = index.min(candidates.tags.len().saturating_sub(1))
                }
                TransferAction::Previous => *cursor = cursor.saturating_sub(1),
                TransferAction::Next => {
                    *cursor = cursor
                        .saturating_add(1)
                        .min(candidates.tags.len().saturating_sub(1))
                }
                TransferAction::First => *cursor = 0,
                TransferAction::Last => *cursor = candidates.tags.len().saturating_sub(1),
                TransferAction::PagePrevious => *cursor = cursor.saturating_sub(10),
                TransferAction::PageNext => {
                    *cursor = cursor
                        .saturating_add(10)
                        .min(candidates.tags.len().saturating_sub(1))
                }
                TransferAction::ToggleIndex(index) => {
                    if index < candidates.tags.len() {
                        *cursor = index;
                        if selected.contains(&index) {
                            selected.retain(|i| *i != index);
                        } else {
                            selected.push(index);
                        }
                    }
                }
                TransferAction::Toggle => {
                    if selected.contains(cursor) {
                        selected.retain(|i| i != cursor);
                    } else if *cursor < candidates.tags.len() {
                        selected.push(*cursor);
                    }
                }
                TransferAction::ToggleAll => {
                    if selected.len() == candidates.tags.len() {
                        selected.clear();
                    } else {
                        *selected = (0..candidates.tags.len()).collect();
                    }
                }
                _ => {}
            },
            _ => return None,
        }
        Some(Input::Idle)
    }

    /// Submit the current picker, retaining it unchanged when validation or copying fails.
    pub fn submit_transfer(&mut self) -> Result<Input> {
        self.submit_transfer_with(crate::clipboard::copy)
    }
    pub fn submit_transfer_with(&mut self, copy: impl FnOnce(&str) -> Result<()>) -> Result<Input> {
        let prompt = self
            .prompt
            .clone()
            .context("Missing node transfer prompt")?;
        match &prompt {
            Prompt::ExportNodeDestination {
                text,
                share,
                clipboard,
            } => {
                if *clipboard {
                    self.copy_node_export_with(text, copy)?;
                    self.prompt = None;
                } else {
                    self.prompt(Prompt::ExportNode {
                        text: text.clone(),
                        share: *share,
                    });
                }
            }
            Prompt::ImportNodeSelection { .. } => {
                self.apply_node_import(&prompt)?;
                self.prompt = None;
            }
            _ => anyhow::bail!("Missing node transfer prompt"),
        }
        Ok(Input::Idle)
    }

    pub fn apply_node_import(&mut self, prompt: &Prompt) -> Result<()> {
        let Prompt::ImportNodeSelection {
            name,
            path,
            candidates,
            selected,
            mode,
            ..
        } = prompt
        else {
            return Ok(());
        };
        anyhow::ensure!(
            self.kind == Kind::Proxy && self.draft_key(name).1 == *path,
            "Import target changed; reopen node import"
        );
        let editor = self.editor.as_mut().context("Missing editor")?;
        anyhow::ensure!(
            editor.name == *name,
            "Import target changed; reopen node import"
        );
        let imported =
            crate::import::merge_selected_nodes(&editor.text(), candidates, selected, *mode)?;
        editor.import(imported.text)?;
        self.message = "Imported nodes to draft · Ctrl-S save".into();
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/node_transfer_test.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Export,
    Import,
}
pub struct Picker {
    pub items: Vec<String>,
    pub cursor: usize,
    pub title: String,
    pub warnings: String,
    pub kind: PickerKind,
}
pub fn picker(prompt: &Prompt) -> Option<Picker> {
    use crate::i18n::t;
    Some(match prompt {
        Prompt::ExportNodeDestination {
            clipboard, share, ..
        } => Picker {
            items: vec![
                format!(
                    "{} · {}",
                    t("File or folder"),
                    if *share { "nodes.txt" } else { "nodes.toml" }
                ),
                t("Clipboard").to_owned(),
            ],
            cursor: usize::from(*clipboard),
            title: t("Export selected nodes").to_owned(),
            warnings: String::new(),
            kind: PickerKind::Export,
        },
        Prompt::ImportNodeSelection {
            candidates,
            selected,
            cursor,
            ..
        } => Picker {
            items: candidates
                .tags
                .iter()
                .enumerate()
                .map(|(index, tag)| {
                    format!(
                        "[{}] {}",
                        if selected.contains(&index) { "x" } else { " " },
                        tag.replace(['\r', '\n', '\t'], " ")
                    )
                })
                .collect(),
            cursor: *cursor,
            title: format!(
                "{} · {}/{}",
                t("Select nodes to import"),
                selected.len(),
                candidates.tags.len()
            ),
            warnings: candidates.warnings.join("\n"),
            kind: PickerKind::Import,
        },
        _ => return None,
    })
}
