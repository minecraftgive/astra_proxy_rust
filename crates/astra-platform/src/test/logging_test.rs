use super::*;
use std::sync::{Arc, Mutex};
#[test]
fn stalled_writer_cannot_block_enqueue_or_shutdown() {
    let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
    assert!(enqueue(&sender, Message::Line(vec![1])));
    let started = std::time::Instant::now();
    assert!(!enqueue(&sender, Message::Line(vec![2])));
    flush_writer(&sender, std::time::Duration::from_millis(30));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}
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
fn queued_lines_and_repeated_flushes_complete() {
    use std::io::Write;
    for index in 0..100 {
        let mut line = Line(Vec::new());
        write!(line, "line {index}").unwrap();
    }
    // An empty event queues nothing.
    drop(Line(Vec::new()));
    flush();
    flush();
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
