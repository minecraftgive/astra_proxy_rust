//! Shared log severity and serialized values.
use serde::{Deserialize, Serialize};
use std::io;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
    // Append on the wire: existing bincode values for Info/Warn/Error stay stable.
    Route,
}
impl LogLevel {
    pub const ALL: [Self; 6] = [
        Self::Trace,
        Self::Debug,
        Self::Route,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Route => "route",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
    pub fn severity(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Route => 2,
            Self::Info => 3,
            Self::Warn => 4,
            Self::Error => 5,
        }
    }
}
impl std::str::FromStr for LogLevel {
    type Err = io::Error;
    fn from_str(text: &str) -> io::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str().eq_ignore_ascii_case(text))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsupported log level"))
    }
}
