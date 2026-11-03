//! Local operating-system services shared by all Astra applications.
#[cfg(windows)]
pub mod elevation;
pub mod logging;
pub mod platform;
pub mod storage;
pub mod lifecycle;
#[cfg(unix)]
pub mod unix_handoff;

pub mod runtime;
pub mod command;
pub mod supervision;
pub mod recovery;
pub mod monitor;
pub mod tui_log;
