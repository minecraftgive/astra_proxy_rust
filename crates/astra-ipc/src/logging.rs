//! Runtime log filtering shared by the two cores.
use serde::{Deserialize, Serialize};
use std::{
    io::{self, IsTerminal},
    sync::OnceLock,
};
use tracing_subscriber::{
    layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter, Registry,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}
impl LogLevel {
    pub const ALL: [Self; 5] = [
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
    pub fn severity(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
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
type FilterHandle = reload::Handle<EnvFilter, Registry>;
static FILTER: OnceLock<FilterHandle> = OnceLock::new();

pub fn init() -> io::Result<()> {
    let filter = match std::env::var("ASTRA_LOG_LEVEL") {
        Ok(level) => EnvFilter::try_new(level).map_err(io::Error::other)?,
        Err(_) => EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
    };
    let (filter, handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_ansi(io::stdout().is_terminal()))
        .try_init()
        .map_err(io::Error::other)?;
    FILTER
        .set(handle)
        .map_err(|_| io::Error::other("log filter already initialized"))
}

pub fn set_level(level: LogLevel) -> io::Result<()> {
    FILTER
        .get()
        .ok_or_else(|| io::Error::other("runtime log filter is unavailable"))?
        .reload(EnvFilter::new(level.as_str()))
        .map_err(io::Error::other)
}
