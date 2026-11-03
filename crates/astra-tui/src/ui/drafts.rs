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
mod tests {
    use super::*;
    #[test]
    fn old_session_migrates_once_and_dismissal_persists_atomically() {
        let fixture = crate::import::tests::Fixture::new();
        let path = fixture.0.join("ui-session.json");
        let mut value = serde_json::to_value(Session::default()).unwrap();
        value.as_object_mut().unwrap().remove("session_version");
        value.as_object_mut().unwrap().remove("keymap_version");
        value.as_object_mut().unwrap().remove("drafts");
        value["editor"] =
            serde_json::to_value(Editor::new("legacy".into(), "standalone=true\n".into())).unwrap();
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let original = std::fs::read(&path).unwrap();
        let session = Session::load(&path).unwrap();
        assert_eq!(
            std::fs::read(path.with_extension("pre-v2.json")).unwrap(),
            original
        );
        assert_eq!(session.session_version, 1);
        assert!(session.keymap_notice_pending);
        assert_eq!(session.drafts.len(), 1);
        let mut ui = Ui::new(session);
        assert!(ui.help_scroll.is_some());
        ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!ui.keymap_notice_pending);
        ui.session.save(&path).unwrap();
        assert!(!Session::load(&path).unwrap().keymap_notice_pending);
        assert_eq!(
            std::fs::read(path.with_extension("pre-v2.json")).unwrap(),
            original
        );
    }
    #[test]
    fn future_or_unknown_session_state_is_rejected_without_overwrite() {
        let fixture = crate::import::tests::Fixture::new();
        let path = fixture.0.join("ui-session.json");
        for (key, value) in [
            ("session_version", serde_json::json!(999)),
            ("unknown_field", serde_json::json!({"draft":"retain"})),
        ] {
            let mut session = serde_json::to_value(Session::default()).unwrap();
            session[key] = value;
            let original = serde_json::to_vec(&session).unwrap();
            std::fs::write(&path, &original).unwrap();
            assert!(Session::load(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }
    #[test]
    fn two_drafts_survive_navigation_and_external_conflicts() {
        let mut ui = Ui::new(Session::default());
        ui.open_draft("a".into(), "standalone=true\n".into());
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n".into())
            .unwrap();
        ui.open_draft("b".into(), "standalone=true\n".into());
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n# b\n".into())
            .unwrap();
        ui.open_draft("a".into(), "standalone=true\n# external\n".into());
        assert_eq!(ui.editor.as_ref().unwrap().text(), "standalone=false\n");
        assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
        assert!(ui.drafts.values().any(|d| d.conflict));
        ui.remember_draft();
        let restored: Session =
            serde_json::from_str(&serde_json::to_string(&ui.session).unwrap()).unwrap();
        assert_eq!(restored.drafts.len(), 2);
    }
}
