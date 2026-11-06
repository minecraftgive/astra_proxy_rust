//! Installation request and status model, independent of frontend rendering.
use anyhow::Result;
use astra_management::v2 as v;
use tokio::{sync::watch, task::JoinHandle};
#[derive(Clone, Debug)]
pub struct Request {
    pub package_id: String,
    pub expected_release_id: String,
    pub scope: v::InstallationScope,
    pub elevate: bool,
}

#[derive(Default)]
pub struct State {
    pub open: bool,
    pub notice: bool,
    pub notice_accept: bool,
    pub notice_key: Option<String>,
    pub refresh: bool,
    pub status: Option<v::InstallationStatus>,
    pub query: Option<JoinHandle<Result<v::InstallationStatus, String>>>,
    pub operation: Option<JoinHandle<Result<v::InstallationResult, String>>>,
    pub progress: Option<watch::Receiver<Option<astra_ipc::progress::Progress>>>,
    pub events: Vec<String>,
    pub result: Option<v::InstallationResult>,
    pub error: Option<String>,
    pub query_error: Option<String>,
    pub selected: usize,
    pub scope: v::InstallationScope,
    pub scroll: u16,
}

impl State {
    pub fn visible(&self) -> bool {
        self.open || self.notice
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
