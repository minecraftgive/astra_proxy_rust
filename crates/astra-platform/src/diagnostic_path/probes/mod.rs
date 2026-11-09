//! Native, bounded probes. Callers own path admission and recheck around each send.
use super::*;
use astra_model::network_path::{PathCapability, PathOutcome};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
mod echo_unix;
#[cfg(windows)]
mod echo_windows;
#[cfg(target_os = "linux")]
mod trace_linux;
#[cfg(target_os = "macos")]
mod trace_macos;

#[derive(Debug)]
pub struct Reply {
    pub from: Option<IpAddr>,
    pub rtt_ms: Option<f64>,
    pub ttl: Option<u8>,
    pub outcome: PathOutcome,
    pub icmp_type: Option<u8>,
    pub icmp_code: Option<u8>,
}
impl Reply {
    fn silence() -> Self {
        Self {
            from: None,
            rtt_ms: None,
            ttl: None,
            outcome: PathOutcome::NoResponse,
            icmp_type: None,
            icmp_code: None,
        }
    }
}
#[cfg(unix)]
fn elapsed(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}
fn limited(timeout: Duration) -> io::Result<Duration> {
    if timeout.is_zero() || timeout > Duration::from_secs(30) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Probe timeout must be 1..30000 ms",
        ))
    } else {
        Ok(timeout)
    }
}
fn result_capability(
    method: &str,
    permission: Option<&str>,
    result: io::Result<()>,
) -> PathCapability {
    PathCapability {
        supported: result.is_ok(),
        method: method.into(),
        permission_required: permission.map(str::to_owned),
        reason: result.err().map(|error| error.to_string()),
    }
}
pub fn trace_capability(ipv6: bool) -> PathCapability {
    #[cfg(target_os = "linux")]
    return result_capability(
        "UDP TTL/Hop-Limit + Linux error queue",
        None,
        trace_linux::available(ipv6),
    );
    #[cfg(target_os = "macos")]
    return result_capability(
        "UDP TTL/Hop-Limit + raw ICMP receive",
        Some("root/raw ICMP socket permission"),
        trace_macos::available(ipv6),
    );
    #[cfg(windows)]
    return result_capability(
        "Windows ICMP API + TTL/Hop-Limit and source route checks",
        None,
        echo_windows::available(ipv6),
    );
    #[allow(unreachable_code)]
    result_capability(
        "unavailable",
        None,
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "No traceroute adapter for this platform",
        )),
    )
}
pub fn echo_capability(ipv6: bool) -> PathCapability {
    #[cfg(unix)]
    return result_capability("datagram ICMP echo", None, echo_unix::available(ipv6));
    #[cfg(windows)]
    return result_capability("Windows ICMP API", None, echo_windows::available(ipv6));
}
pub fn trace(
    guard: &PhysicalGuard,
    target: IpAddr,
    hop: u8,
    seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    limited(timeout)?;
    if hop == 0 || hop > 30 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Trace hop must be 1..30",
        ));
    }
    #[cfg(target_os = "linux")]
    return trace_linux::probe(guard, target, hop, seq, timeout);
    #[cfg(target_os = "macos")]
    return trace_macos::probe(guard, target, hop, seq, timeout);
    #[cfg(windows)]
    return echo_windows::probe(Some(guard), target, hop, seq, timeout);
    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "No traceroute adapter for this platform",
    ))
}
pub fn echo(
    guard: Option<&PhysicalGuard>,
    target: IpAddr,
    seq: u16,
    timeout: Duration,
) -> io::Result<Reply> {
    limited(timeout)?;
    #[cfg(unix)]
    return echo_unix::probe(guard, target, seq, timeout);
    #[cfg(windows)]
    return echo_windows::probe(guard, target, 64, seq, timeout);
}

#[cfg(any(unix, test))]
fn outcome(
    ipv4: bool,
    kind: u8,
    code: u8,
    offender: Option<IpAddr>,
    target: IpAddr,
) -> Option<PathOutcome> {
    match (ipv4, kind, code) {
        (true, 11, 0) | (false, 3, 0) => Some(PathOutcome::TimeExceeded),
        (true, 3, 3) | (false, 1, 4) if offender == Some(target) => Some(PathOutcome::Reply),
        (true, 3, _) | (false, 1, _) => Some(PathOutcome::Unreachable),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_matching_destination_errors_complete_a_udp_trace() {
        let target: IpAddr = "198.51.100.1".parse().unwrap();
        let router: IpAddr = "192.0.2.1".parse().unwrap();
        assert_eq!(
            outcome(true, 3, 3, Some(target), target),
            Some(PathOutcome::Reply)
        );
        assert_eq!(
            outcome(true, 3, 3, Some(router), target),
            Some(PathOutcome::Unreachable)
        );
        assert_eq!(
            outcome(true, 11, 0, Some(router), target),
            Some(PathOutcome::TimeExceeded)
        );
        assert_eq!(outcome(true, 0, 0, Some(target), target), None);
        assert_eq!(Reply::silence().outcome, PathOutcome::NoResponse);
        assert_eq!(Reply::silence().rtt_ms, None);
    }
}
