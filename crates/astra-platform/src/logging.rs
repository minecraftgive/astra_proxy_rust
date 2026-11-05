//! Runtime log filtering shared by the two cores.
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

use astra_model::logging::LogLevel;
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

/// Formatted events go to one writer thread instead of being written to
/// stdout (usually a redirected log file) by the async worker that logged
/// them. The thread batches whatever is queued into one buffered write.
enum Message {
    Line(Vec<u8>),
    Flush(std::sync::mpsc::SyncSender<()>),
}
static WRITER: OnceLock<std::sync::mpsc::SyncSender<Message>> = OnceLock::new();
static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Lines discarded because the output writer could not keep up.
pub fn dropped_lines() -> u64 {
    DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}
fn enqueue(sender: &std::sync::mpsc::SyncSender<Message>, message: Message) -> bool {
    sender.try_send(message).is_ok()
}
fn writer() -> &'static std::sync::mpsc::SyncSender<Message> {
    WRITER.get_or_init(|| {
        // A stalled disk must not block async workers or their Drop cleanup.
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Message>(16 * 1024);
        let _ = std::thread::Builder::new()
            .name("astra-log".into())
            .spawn(move || {
                use std::io::Write;
                let mut out = io::BufWriter::with_capacity(64 * 1024, io::stdout());
                let handle = |message, out: &mut io::BufWriter<io::Stdout>| match message {
                    Message::Line(bytes) => {
                        let _ = out.write_all(&bytes);
                    }
                    Message::Flush(done) => {
                        let _ = out.flush();
                        let _ = done.send(());
                    }
                };
                let mut reported = 0;
                while let Ok(message) = receiver.recv() {
                    handle(message, &mut out);
                    let total = dropped_lines();
                    let dropped = total.saturating_sub(reported);
                    if dropped != 0 {
                        let _ = writeln!(out, "WARN astra_log: dropped {dropped} log lines while output was unavailable");
                        reported = total;
                    }
                    while let Ok(message) = receiver.try_recv() {
                        handle(message, &mut out);
                    }
                    let _ = out.flush();
                }
            });
        sender
    })
}
/// One formatted event; queued as a whole when tracing drops the writer.
struct Line(Vec<u8>);
impl io::Write for Line {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for Line {
    fn drop(&mut self) {
        if !self.0.is_empty() && !enqueue(writer(), Message::Line(std::mem::take(&mut self.0))) {
            DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
/// Wait until queued log lines reach stdout.
pub fn flush() {
    // Do not initialize a new writer while unwinding an initialization failure.
    let Some(writer) = WRITER.get() else { return };
    flush_writer(writer, std::time::Duration::from_secs(2));
}
fn flush_writer(writer: &std::sync::mpsc::SyncSender<Message>, budget: std::time::Duration) {
    let deadline = std::time::Instant::now() + budget;
    let (done, wait) = std::sync::mpsc::sync_channel(1);
    let mut message = Message::Flush(done);
    loop {
        match writer.try_send(message) {
            Ok(()) => {
                let _ = wait
                    .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
                break;
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
            Err(std::sync::mpsc::TrySendError::Full(pending)) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                message = pending;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }
}
/// Flushes queued log lines when the core's `main` returns.
#[must_use = "dropping the guard immediately flushes too early"]
pub struct Guard(());
impl Drop for Guard {
    fn drop(&mut self) {
        flush();
    }
}

pub fn init() -> io::Result<Guard> {
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
                .with_ansi(io::stdout().is_terminal())
                .with_writer(|| Line(Vec::with_capacity(256))),
        )
        .try_init()
        .map_err(io::Error::other)?;
    FILTER
        .set(handle)
        .map_err(|_| io::Error::other("log filter already initialized"))?;
    Ok(Guard(()))
}

pub fn set_level(level: LogLevel) -> io::Result<()> {
    FILTER
        .get()
        .ok_or_else(|| io::Error::other("runtime log filter is unavailable"))?
        .reload(filter(level.as_str())?)
        .map_err(io::Error::other)
}

#[cfg(test)]
#[path = "test/logging_test.rs"]
mod tests;
