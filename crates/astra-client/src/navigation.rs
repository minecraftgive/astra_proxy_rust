use crate::windows::{Launch, View};
use crate::{navigation as shell, state::*};
use crate::{
    protocol::{Action, Kind},
    tr,
};
pub const OVERVIEW: usize = 0;
pub const PROXIES: usize = 1;
pub const CONNECTIONS: usize = 2;
pub const RULES: usize = 3;
pub const LOGS: usize = 4;
pub const PROFILES: usize = 5;
pub const SETTINGS: usize = 6;
/// Additional terminal page; the native GUI retains its existing page catalog.
pub const DNS: usize = 7;
pub const PAGES: [&str; 7] = [
    "Overview",
    "Proxies",
    "Connections",
    "Rules",
    "Logs",
    "Profiles",
    "Settings",
];

impl Client {
    pub fn remember_page(&mut self, page: usize) {
        if self.page != page {
            self.navigation_stack.push(self.page);
            self.page = page;
        }
    }
    pub fn navigate_back(&mut self) -> Input {
        let Some(page) = self.navigation_stack.pop() else {
            return Input::Idle;
        };
        // navigate performs normal page setup; returning must not push history.
        let history_len = self.navigation_stack.len();
        let input = self.navigate(page);
        self.navigation_stack.truncate(history_len);
        input
    }
    pub fn navigate(&mut self, page: usize) -> Input {
        self.suspend_editor();
        let page = page.min(DNS);
        if self.page == LOGS && page != LOGS {
            self.close_viewer();
        }
        self.remember_page(page);
        if page == shell::SETTINGS {
            self.program_info.refresh = true;
        }
        if page == CONNECTIONS {
            self.reveal_selected_connection();
        }
        self.connection_filtering = false;
        self.dns_state.filtering = false;
        self.dns_state.query_open = false;
        self.dns_state.status_open = false;
        self.dns_state.detail = false;
        self.runtime_picker = None;
        if page == LOGS && self.viewer.is_none() {
            if let Some(name) = self.log_source(true) {
                return self.open_logs(name);
            }
        }
        Input::Idle
    }
}

impl Client {
    pub fn apply_launch(&mut self, launch: &Launch) -> Option<Action> {
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
    pub fn detached_launch(&self, view: View) -> Launch {
        let kind = if view == View::Profiles {
            self.kind
        } else {
            Kind::Proxy
        };
        let instance = match view {
            View::TuiLog => None,
            View::Logs if self.viewer.is_some() => self.viewer.as_ref().map(|v| v.name.clone()),
            View::Diagnostics => self.diagnostics.as_ref().map(|v| v.name.clone()),
            View::Profiles => self.selected().map(|r| r.instance.name.clone()),
            _ => self.active_proxy().map(|r| r.instance.name.clone()),
        };
        Launch::detached(view, instance, kind)
    }
    pub fn detach_current(&mut self) -> Input {
        let view = if self.tui_viewer.is_some() {
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
            self.message =
                crate::i18n::t("Use Alt+1–7 to open a page; the current draft stays here").into();
            self.tasks.notice_message(&self.message);
            return Input::Idle;
        } else {
            View::from_page(self.page)
        };
        Input::OpenWindow(self.detached_launch(view))
    }
}

#[cfg(test)]
#[path = "test/navigation_test.rs"]
mod tests;
