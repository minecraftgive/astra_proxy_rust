//! Local operating-system services shared by all Astra applications.
#[cfg(windows)]
pub mod elevation;
pub mod logging;
pub mod platform;
pub mod storage;
#[cfg(unix)]
pub mod unix_handoff;

pub mod runtime;
