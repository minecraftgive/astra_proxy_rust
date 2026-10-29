//! Window navigation stays independent of the originating editor and session.
use super::*;
use crate::windows::{Launch, View};
impl Ui {
    pub(super) fn window_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let explicit = if key.modifiers.contains(KeyModifiers::ALT) {
            if let KeyCode::Char(c @ '1'..='5') = key.code {
                Some(View::from_page(c as usize - '1' as usize))
            } else {
                None
            }
        } else {
            None
        };
        let view = if let Some(view) = explicit {
            view
        } else if key.code == KeyCode::F(8) {
            if self.tui_viewer.is_some() {
                View::TuiLog
            } else if self.viewer.is_some() {
                View::Logs
            } else if self.diagnostics.is_some() {
                View::Diagnostics
            } else if self.editor.is_some()
                || self.prompt.is_some()
                || self.recovery.is_some()
                || self.tasks.showing_details()
                || self.explanation.is_some()
                || self.help_scroll.is_some()
            {
                self.message = t("Use Alt+1–5 to open a page; the current draft stays here").into();
                self.tasks.notice_message(&self.message);
                return Some(Input::Idle);
            } else {
                View::from_page(self.page)
            }
        } else {
            return None;
        };
        let kind = if matches!(view, View::Proxies | View::Connections) {
            Kind::Proxy
        } else {
            self.kind
        };
        let instance = if view == View::TuiLog {
            None
        } else if view == View::Logs {
            self.viewer.as_ref().map(|v| v.name.clone())
        } else if view == View::Diagnostics {
            self.diagnostics.as_ref().map(|v| v.name.clone())
        } else {
            self.selected()
                .filter(|r| r.instance.kind == kind)
                .map(|r| r.instance.name.clone())
        };
        Some(Input::OpenWindow(Launch::detached(view, instance, kind)))
    }
    pub(super) fn apply_launch(&mut self, launch: &Launch) -> Option<Action> {
        if let Some(kind) = launch.kind {
            self.kind = kind;
        }
        if let Some(view) = launch.view {
            if let Some(page) = view.page() {
                self.page = page;
            }
        }
        if let Some(name) = &launch.instance {
            let Some(row) = self.rows.iter().find(|row| &row.instance.name == name) else {
                self.message = tr!("Instance unavailable: {}", name);
                return None;
            };
            self.kind = row.instance.kind;
            if let Some(index) = self.rows().iter().position(|r| &r.instance.name == name) {
                let kind = self.index();
                self.selections[kind] = index;
            }
        }
        match launch.view {
            Some(View::TuiLog) => Some(Action::Logs("@tui".into())),
            Some(View::Logs) => self
                .selected()
                .map(|r| Action::Logs(r.instance.name.clone())),
            Some(View::Diagnostics) => self
                .selected()
                .filter(|r| r.instance.kind == Kind::Proxy)
                .map(|r| Action::Diagnostics(r.instance.name.clone())),
            _ => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> Ui {
        let mut ui = Ui::new(Session::default());
        ui.rows = vec![
            Row {
                instance: Instance {
                    name: "rule".into(),
                    kind: Kind::Rule,
                    config: "rule.toml".into(),
                },
                pid: Some(1),
                state: "Running".into(),
                uptime: 0,
                changed: false,
            },
            Row {
                instance: Instance {
                    name: "edge".into(),
                    kind: Kind::Proxy,
                    config: "edge.toml".into(),
                },
                pid: Some(2),
                state: "Running".into(),
                uptime: 0,
                changed: false,
            },
        ];
        ui
    }
    #[test]
    fn shortcuts_open_pages_and_selected_logs_without_editing_or_navigation() {
        let mut ui = setup();
        ui.page = 3;
        let Input::OpenWindow(launch) =
            ui.key(event::KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE))
        else {
            panic!("no new window")
        };
        assert_eq!(launch.view, Some(View::Connections));
        assert_eq!(launch.instance.as_deref(), Some("edge"));
        ui.editor =
            Some(Editor::new("edge".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
        let draft = ui.editor.as_ref().unwrap().text();
        assert!(matches!(
            ui.key(event::KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            Input::Idle
        ));
        for (c, view) in [
            ('1', View::Dashboard),
            ('2', View::Proxies),
            ('3', View::Profiles),
            ('4', View::Connections),
            ('5', View::Tools),
        ] {
            assert!(
                matches!(ui.key(event::KeyEvent::new(KeyCode::Char(c),KeyModifiers::ALT)),Input::OpenWindow(launch) if launch.view==Some(view))
            );
        }
        assert_eq!(ui.page, 3);
        assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
        ui.editor = None;
        ui.viewer = Some(logs::Viewer::new("edge".into(), Default::default()));
        assert!(
            matches!(ui.key(event::KeyEvent::new(KeyCode::F(8),KeyModifiers::NONE)),Input::OpenWindow(launch)if launch.view==Some(View::Logs)&&launch.instance.as_deref()==Some("edge"))
        );
        ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
        assert!(
            matches!(ui.key(event::KeyEvent::new(KeyCode::F(8),KeyModifiers::NONE)),Input::OpenWindow(launch)if launch.view==Some(View::TuiLog))
        );
    }
    #[test]
    fn startup_selects_instance_after_state_arrives_and_does_not_act_on_a_replacement() {
        let mut ui = setup();
        let launch = Launch::detached(View::Logs, Some("rule".into()), Kind::Proxy);
        assert!(matches!(ui.apply_launch(&launch),Some(Action::Logs(name))if name=="rule"));
        assert_eq!(ui.kind, Kind::Rule);
        assert_eq!(ui.selected().unwrap().instance.name, "rule");
        let missing = Launch::detached(View::Diagnostics, Some("removed".into()), Kind::Proxy);
        assert!(ui.apply_launch(&missing).is_none());
        assert!(ui.message.contains("removed"));
    }
}
