//! Runtime log filtering shared by the two cores.
use serde::{Deserialize, Serialize};
use std::{
    io::{self, IsTerminal},
    sync::OnceLock,
};
use tracing_subscriber::{
    fmt::{
        format::{FormatEvent, FormatFields, Writer},
        time::FormatTime,
        FmtContext,
    },
    layer::SubscriberExt,
    registry::LookupSpan,
    reload,
    util::SubscriberInitExt,
    EnvFilter, Registry,
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
type FilterHandle = reload::Handle<EnvFilter, Registry>;
static FILTER: OnceLock<FilterHandle> = OnceLock::new();

/// tracing has five native levels. Route uses a dedicated DEBUG target, enabled
/// at the Route threshold and rendered with its own ROUTE label.
pub const ROUTE_TARGET: &str = "astra_route";
fn filter(input: &str) -> io::Result<EnvFilter> {
    let route = input
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case("route"));
    let input = if route {
        format!(
            "{ROUTE_TARGET}=debug,{}",
            input
                .split(',')
                .map(|part| if part.trim().eq_ignore_ascii_case("route") {
                    "info"
                } else {
                    part
                })
                .collect::<Vec<_>>()
                .join(",")
        )
    } else {
        input.to_owned()
    };
    EnvFilter::try_new(input).map_err(io::Error::other)
}
struct EventFormatter;
impl<S, N> FormatEvent<S, N> for EventFormatter
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        if event.metadata().target() != ROUTE_TARGET {
            return tracing_subscriber::fmt::format().format_event(ctx, writer, event);
        }
        tracing_subscriber::fmt::time::SystemTime.format_time(&mut writer)?;
        write!(writer, " ROUTE {ROUTE_TARGET}: ")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

pub fn init() -> io::Result<()> {
    let filter = match std::env::var("ASTRA_LOG_LEVEL") {
        Ok(level) => filter(&level)?,
        Err(_) => std::env::var("RUST_LOG")
            .ok()
            .and_then(|value| filter(&value).ok())
            .unwrap_or_else(|| "info".into()),
    };
    let (filter, handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .event_format(EventFormatter)
                .with_ansi(io::stdout().is_terminal()),
        )
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
        .reload(filter(level.as_str())?)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn emit() {
        tracing::trace!("trace-message");
        tracing::debug!("debug-message");
        tracing::debug!(target: "astra_route", "route-message");
        tracing::info!("info-message");
        tracing::warn!("warn-message");
        tracing::error!("error-message");
    }
    #[test]
    fn route_thresholds_and_formatter() {
        for level in LogLevel::ALL {
            let output = Output::default();
            let sink = output.clone();
            let subscriber = tracing_subscriber::registry()
                .with(filter(level.as_str()).unwrap())
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(EventFormatter)
                        .with_ansi(false)
                        .with_writer(move || sink.clone()),
                );
            tracing::subscriber::with_default(subscriber, emit);
            let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
            for (candidate, message) in [
                (LogLevel::Trace, "trace-message"),
                (LogLevel::Debug, "debug-message"),
                (LogLevel::Route, "route-message"),
                (LogLevel::Info, "info-message"),
                (LogLevel::Warn, "warn-message"),
                (LogLevel::Error, "error-message"),
            ] {
                assert_eq!(
                    text.contains(message),
                    candidate.severity() >= level.severity(),
                    "{}: {text}",
                    level.as_str()
                );
            }
            if level.severity() <= LogLevel::Route.severity() {
                assert!(text.contains(" ROUTE astra_route:"));
            }
        }
    }
    #[test]
    fn reload_enables_route_without_debug_and_then_disables_it() {
        let output = Output::default();
        let sink = output.clone();
        let (layer, handle) = reload::Layer::new(filter("info").unwrap());
        let subscriber = tracing_subscriber::registry().with(layer).with(
            tracing_subscriber::fmt::layer()
                .event_format(EventFormatter)
                .with_ansi(false)
                .with_writer(move || sink.clone()),
        );
        tracing::subscriber::with_default(subscriber, || {
            emit();
            assert!(!String::from_utf8_lossy(&output.0.lock().unwrap()).contains("route-message"));
            output.0.lock().unwrap().clear();
            handle.reload(filter("route").unwrap()).unwrap();
            emit();
            let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
            assert!(text.contains("route-message") && text.contains("info-message"));
            assert!(!text.contains("debug-message"));
            output.0.lock().unwrap().clear();
            handle.reload(filter("info").unwrap()).unwrap();
            emit();
            assert!(!String::from_utf8_lossy(&output.0.lock().unwrap()).contains("route-message"));
        });
    }
    #[test]
    fn route_serialization_keeps_existing_wire_values() {
        assert_eq!("Route".parse::<LogLevel>().unwrap(), LogLevel::Route);
        assert_eq!(
            bincode::serialize(&LogLevel::Info).unwrap(),
            2u32.to_le_bytes()
        );
        assert_eq!(
            bincode::serialize(&LogLevel::Error).unwrap(),
            4u32.to_le_bytes()
        );
        assert_eq!(
            bincode::serialize(&LogLevel::Route).unwrap(),
            5u32.to_le_bytes()
        );
        assert_eq!(LogLevel::ALL[2], LogLevel::Route);
        assert!(filter("route,astra_route=off").is_ok());
    }
}
