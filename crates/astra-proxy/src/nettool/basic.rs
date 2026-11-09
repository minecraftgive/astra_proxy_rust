//! Multi-port TCP and IPv6 transport reachability share the constrained dialer.
use super::{detailed::path::Route, millis};
use anyhow::{bail, ensure, Result};
use astra_model::nettool::{
    AddressFamily, Attempt, ConnectionOutcome, ConnectionResult, DetailedRequest, Event, Probe,
    Summary,
};
use std::{
    io::ErrorKind,
    time::{Duration, Instant},
};

pub(super) async fn run(
    request: &DetailedRequest,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    let mut request = request.clone();
    let probe = request.probe.clone();
    let (host, ports) = match &probe {
        Probe::Tcp { host, port } => (host.clone(), vec![*port]),
        Probe::TcpPorts { host, ports } => (host.clone(), ports.clone()),
        Probe::Ipv6 { host, port } => {
            request.family = Some(AddressFamily::Ipv6);
            (host.clone(), vec![*port])
        }
        _ => bail!("Invalid connection probe"),
    };
    ensure!(
        !host.is_empty() && host.len() <= 1024 && !host.chars().any(char::is_control),
        "Invalid connection host"
    );
    ensure!(
        !ports.is_empty()
            && ports.len() <= 16
            && ports.iter().all(|port| *port != 0)
            && ports.iter().collect::<std::collections::HashSet<_>>().len() == ports.len(),
        "Select 1–16 unique nonzero TCP ports"
    );
    let route = Route::new(&request)?;
    emit(Event::Started {
        egress: request.egress.to_string(),
        probe: request.probe.to_string(),
        note: format!(
            "{}; duration includes resolution, connection and proxy negotiation, not target RTT",
            route.note()
        ),
        running_config: false,
    })?;
    let mut attempts = vec![];
    let mut sequence = 0;
    for round in 0..request.count {
        if round > 0 {
            tokio::time::sleep(Duration::from_millis(request.interval_ms)).await;
        }
        for port in &ports {
            sequence += 1;
            let started = Instant::now();
            let result = tokio::time::timeout(
                Duration::from_millis(request.timeout_ms),
                route.connect(sequence, "business_target", &host, *port, None, &emit),
            )
            .await;
            let (outcome, duration, error) = match result {
                Ok(Ok(_stream)) => (
                    ConnectionOutcome::Connected,
                    Some(millis(started.elapsed())),
                    None,
                ),
                Err(_) => (
                    ConnectionOutcome::TimedOut,
                    None,
                    Some("Connection timed out".to_owned()),
                ),
                Ok(Err(error)) => {
                    let kind = error.chain().find_map(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .map(std::io::Error::kind)
                    });
                    let outcome = match kind {
                        Some(ErrorKind::ConnectionRefused) => ConnectionOutcome::Refused,
                        Some(ErrorKind::TimedOut) => ConnectionOutcome::TimedOut,
                        Some(
                            ErrorKind::NetworkUnreachable
                            | ErrorKind::HostUnreachable
                            | ErrorKind::AddrNotAvailable,
                        ) => ConnectionOutcome::Unreachable,
                        _ => ConnectionOutcome::Failed,
                    };
                    (
                        outcome,
                        None,
                        Some(super::clean(&format!("{error:#}"), 1024)),
                    )
                }
            };
            route.tls.flush(
                sequence,
                error.as_deref().unwrap_or("Connection interrupted"),
                &emit,
            )?;
            emit(Event::Tcp {
                seq: sequence,
                result: ConnectionResult {
                    host: host.clone(),
                    port: *port,
                    family: request.family,
                    outcome,
                    connect_ms: duration,
                    message: error.clone().unwrap_or_else(|| {
                        "Connected; connection/handshake duration, not RTT".into()
                    }),
                },
            })?;
            let attempt = Attempt {
                seq: sequence,
                rtt_ms: duration,
                detail: format!("{host}:{port} · {outcome:?} · connection/handshake duration"),
                error,
            };
            emit(Event::Attempt(attempt.clone()))?;
            attempts.push(attempt);
        }
    }
    let summary = Summary::from_attempts(&attempts);
    emit(Event::Summary(summary.clone()))?;
    emit(Event::Done)?;
    Ok(summary)
}

#[cfg(test)]
#[path = "../test/nettool_basic_test.rs"]
mod tests;
