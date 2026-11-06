//! Local operating-system services shared by all Astra applications.
#[cfg(windows)]
pub mod elevation;
pub mod installation;
pub mod lifecycle;
pub mod logging;
pub mod platform;
pub mod program_info;
pub mod program_trust;
pub mod storage;
#[cfg(unix)]
pub mod unix_handoff;
#[cfg(windows)]
pub mod windows_service;

pub mod command;
pub mod monitor;
pub mod recovery;
pub mod runtime;
pub mod supervision;
pub mod tui_log;
