//! Local operating-system services shared by all Astra applications.
#[cfg(windows)]
pub mod elevation;
pub mod lifecycle;
pub mod logging;
pub mod platform;
pub mod storage;
#[cfg(unix)]
pub mod unix_handoff;

pub mod command;
pub mod monitor;
pub mod recovery;
pub mod runtime;
pub mod supervision;
pub mod tui_log;
