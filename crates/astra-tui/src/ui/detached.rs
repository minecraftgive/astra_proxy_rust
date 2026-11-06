//! Window navigation stays independent of the originating editor and session.
use super::*;
use crate::windows::{Launch, View};
impl Ui {
    pub(super) fn window_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let explicit = if key.modifiers.contains(KeyModifiers::ALT) {
            if let KeyCode::Char(c @ '1'..='7') = key.code {
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
            if self.client.tui_viewer.is_some() {
                View::TuiLog
            } else if self.client.viewer.is_some() {
                View::Logs
            } else if self.client.diagnostics.is_some() {
                View::Diagnostics
            } else if self.editor.is_some()
                || self.prompt.is_some()
                || self.recovery.is_some()
                || self.client.tasks.showing_details()
                || self.client.explanation.is_some()
                || self.client.help_scroll.is_some()
            {
                self.client.message =
                    t("Use Alt+1–7 to open a page; the current draft stays here").into();
                self.client.tasks.notice_message(&self.client.message);
                return Some(Input::Idle);
            } else {
                View::from_page(self.client.page)
            }
        } else {
            return None;
        };
        // Every page except Profiles follows the active profile (the selected proxy).
        let kind = if view == View::Profiles {
            self.kind
        } else {
            Kind::Proxy
        };
        let instance = if view == View::TuiLog {
            None
        } else if view == View::Logs && self.client.viewer.is_some() {
            self.client.viewer.as_ref().map(|v| v.name.clone())
        } else if view == View::Diagnostics {
            self.client.diagnostics.as_ref().map(|v| v.name.clone())
        } else if view == View::Profiles {
            self.selected().map(|r| r.instance.name.clone())
        } else {
            self.active_proxy().map(|r| r.instance.name.clone())
        };
        Some(Input::OpenWindow(Launch::detached(view, instance, kind)))
    }
    pub(super) fn apply_launch(&mut self, launch: &Launch) -> Option<Action> {
        if let Some(kind) = launch.kind {
            self.kind = kind;
        }
        if let Some(view) = launch.view {
            if let Some(page) = view.page() {
                self.client.page = page;
            }
        }
        if let Some(name) = &launch.instance {
            let Some(row) = self
                .client
                .rows
                .iter()
                .find(|row| &row.instance.name == name)
            else {
                self.client.message = tr!("Instance unavailable: {}", name);
                return None;
            };
            self.kind = row.instance.kind;
            if let Some(index) = self
                .client
                .rows()
                .iter()
                .position(|r| &r.instance.name == name)
            {
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
#[path = "../test/ui_detached_test.rs"]
mod tests;
