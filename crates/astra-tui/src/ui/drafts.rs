//! Per-configuration drafts. Navigation never writes the source configuration.
use super::*;

pub(super) const KEYMAP_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Draft {
    pub(super) path: PathBuf,
    pub(super) kind: Kind,
    pub(super) editor: Editor,
    #[serde(default)]
    pub(super) conflict: bool,
    /// Disk contents observed on restore, kept only when an external change conflicts.
    #[serde(default)]
    pub(super) observed_disk: Option<String>,
}

impl Ui {
    pub(super) fn draft_key(&self, name: &str) -> (String, PathBuf) {
        let path = self
            .rows
            .iter()
            .find(|r| r.instance.name == name)
            .map(|r| r.instance.config.clone())
            .unwrap_or_default();
        let key = format!("{}\n{}", name, path.to_string_lossy());
        (key, path)
    }

    /// Snapshot without closing, including a partially entered field.
    pub(super) fn remember_draft(&mut self) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let (key, path) = self.draft_key(&editor.name);
        let conflict = self.drafts.get(&key).is_some_and(|draft| draft.conflict);
        let observed_disk = self
            .drafts
            .get(&key)
            .and_then(|draft| draft.observed_disk.clone());
        let kind = self
            .rows
            .iter()
            .find(|r| r.instance.name == editor.name)
            .map_or(self.kind, |r| r.instance.kind);
        self.drafts.insert(
            key,
            Draft {
                path,
                kind,
                editor,
                conflict,
                observed_disk,
            },
        );
    }

    pub(super) fn suspend_editor(&mut self) {
        if self.editor.is_some() {
            self.remember_draft();
            self.editor = None;
            self.message = t("Draft retained · reopen the configuration to continue").into();
        }
    }

    #[cfg(test)]
    pub(super) fn open_draft(&mut self, name: String, disk: String) {
        self.suspend_editor();
        let prepared = operations::OperationInput::capture(self).prepare(name.clone(), disk);
        self.apply_operation(
            operations::JobResult::Document {
                prepared,
                binding: None,
            },
            &Action::Read(name),
            operations::ApplyContext {
                same: true,
                log_target_current: true,
                background: false,
            },
        );
    }

    pub(super) fn discard_current_draft(&mut self) {
        if let Some(editor) = self.editor.take() {
            let (key, _) = self.draft_key(&editor.name);
            self.drafts.remove(&key);
            self.ready_editors.remove(&editor.name);
        }
    }
    pub(super) fn compare_draft(&mut self) {
        self.remember_draft();
        let Some(editor) = &self.editor else {
            return;
        };
        let (key, _) = self.draft_key(&editor.name);
        let Some(disk) = self
            .drafts
            .get(&key)
            .and_then(|draft| draft.observed_disk.as_ref())
        else {
            self.message = t("No external conflict was observed when this draft was opened").into();
            return;
        };
        let draft = editor.text();
        let disk_lines: Vec<_> = disk.lines().collect();
        let draft_lines: Vec<_> = draft.lines().collect();
        let mut text=format!("{} · {}\n{}\n\n",t("Draft conflict"),editor.name,t("Disk snapshot from the latest conflict check. - disk / + draft; no source file is changed."));
        let count = disk_lines.len().max(draft_lines.len());
        for index in 0..count.min(2000) {
            match (disk_lines.get(index), draft_lines.get(index)) {
                (Some(a), Some(b)) if a == b => text.push_str(&format!("  {a}\n")),
                (a, b) => {
                    if let Some(a) = a {
                        text.push_str(&format!("- {a}\n"));
                    }
                    if let Some(b) = b {
                        text.push_str(&format!("+ {b}\n"));
                    }
                }
            }
        }
        if count > 2000 {
            text.push_str(
                "\nPreview limited to 2000 lines; use the external editor for the complete file.\n",
            );
        }
        self.help_override = Some(
            text.chars()
                .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
                .collect(),
        );
        self.help_scroll = Some(0);
    }

    pub(super) fn activate_profile(&mut self, name: String) {
        self.suspend_editor();
        self.active_profile = Some(name);
        self.runtime_picker = None;
        self.focus = Default::default();
        if self.page == shell::LOGS {
            self.close_viewer();
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_drafts_test.rs"]
mod tests;
