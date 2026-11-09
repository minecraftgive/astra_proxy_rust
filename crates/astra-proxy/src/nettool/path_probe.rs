//! User-selected physical traces and TUN-aware echo observations.
use super::detailed::path::Route;
use anyhow::{ensure, Context, Result};
use astra_model::{
    nettool::{AddressFamily, Attempt, DetailedRequest, Egress, Event, Probe, Summary},
    network_path::{EchoObservation, PathCapability, PathOutcome, TraceSample, TraceSummary},
};
use astra_platform::diagnostic_path::{probes, PhysicalGuard};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

fn family(request: &DetailedRequest, host: &str) -> AddressFamily {
    request.family.unwrap_or_else(|| {
        if host.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
            AddressFamily::Ipv6
        } else {
            AddressFamily::Ipv4
        }
    })
}
fn family_name(family: AddressFamily) -> String {
    match family {
        AddressFamily::Ipv4 => "ipv4",
        AddressFamily::Ipv6 => "ipv6",
    }
    .into()
}
fn physical_interface(request: &DetailedRequest) -> Result<&str> {
    match &request.egress {
        Egress::Interface(name) => Ok(name),
        Egress::Outbound(_) => {
            let pin = request.pinned.as_ref().context("A confirmed bound DIRECT selection is required")?;
            ensure!(pin.version == 1 && pin.leaf_tag == "DIRECT" && pin.node.is_none(), "Trace/physical ICMP cannot inspect a proxy tunnel; select a physical interface or bound DIRECT");
            pin.effective_interface.as_deref().context("DIRECT has no confirmed physical interface binding")
        }
        Egress::System => anyhow::bail!("System routing cannot exclude TUN processing; explicitly choose a physical interface or bound DIRECT"),
    }
}
fn error_outcome(error: &std::io::Error) -> PathOutcome {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        PathOutcome::PermissionDenied
    } else {
        PathOutcome::Unavailable
    }
}
fn denied_trace(
    host: &str,
    family: AddressFamily,
    max_hops: u8,
    capability: PathCapability,
    message: String,
    emit: &impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    emit(Event::TraceSummary(TraceSummary {
        target: host.into(),
        family: family_name(family),
        capability,
        physical: None,
        max_hops,
        probes_sent: 0,
        reached: false,
        completed: false,
        message: message.clone(),
    }))?;
    anyhow::bail!("{message}")
}

pub(super) async fn run_trace(
    request: &DetailedRequest,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    let Probe::Trace {
        host,
        max_hops,
        probes_per_hop,
    } = &request.probe
    else {
        anyhow::bail!("Expected trace request");
    };
    ensure!(
        !host.is_empty() && host.len() <= 1024 && !host.chars().any(char::is_control),
        "Invalid trace target"
    );
    ensure!(
        (1..=30).contains(max_hops)
            && (1..=3).contains(probes_per_hop)
            && (1..=1000).contains(&request.timeout_ms),
        "Trace is bounded to 30 hops, 3 probes per hop, 1 second per probe"
    );
    let family = family(request, host);
    let ipv6 = family == AddressFamily::Ipv6;
    let mut capability = probes::trace_capability(ipv6);
    if !capability.supported {
        return denied_trace(
            host,
            family,
            *max_hops,
            capability.clone(),
            capability
                .reason
                .clone()
                .unwrap_or_else(|| "Trace permission unavailable; no elevation attempted".into()),
            &emit,
        );
    }
    let interface = match physical_interface(request) {
        Ok(interface) => interface,
        Err(error) => {
            capability.supported = false;
            capability.reason = Some(error.to_string());
            return denied_trace(
                host,
                family,
                *max_hops,
                capability,
                error.to_string(),
                &emit,
            );
        }
    };
    let guard = match PhysicalGuard::new(interface, ipv6).await {
        Ok(guard) => guard,
        Err(error) => {
            capability.supported = false;
            capability.reason = Some(error.to_string());
            return denied_trace(
                host,
                family,
                *max_hops,
                capability,
                error.to_string(),
                &emit,
            );
        }
    };
    let mut fixed = request.clone();
    fixed.family = Some(family);
    let route = Route::new(&fixed)?.with_physical_guard(&guard);
    let target = tokio::time::timeout(
        Duration::from_secs(10),
        route.address(0, "trace_target", host, 33434, None, true, &emit),
    )
    .await
    .context("Trace target resolution timed out")??;
    emit(Event::Started {
        egress: request.egress.to_string(),
        probe: format!("Trace {}", target.ip()),
        note: format!(
            "Physical {}; {}. No response means only no response.",
            guard.name(),
            capability.method
        ),
        running_config: false,
    })?;
    let mut count = 0u16;
    let mut reached = false;
    let mut interrupted = None;
    'hops: for hop in 1..=*max_hops {
        for probe in 1..=*probes_per_hop {
            let seq = count + 1;
            let destination = SocketAddr::new(target.ip(), 33434 + seq % 1000);
            if let Err(error) = guard.recheck(destination).await {
                interrupted = Some(error.to_string());
                emit(Event::Trace {
                    seq: u32::from(seq),
                    sample: TraceSample {
                        hop,
                        probe,
                        address: None,
                        rtt_ms: None,
                        outcome: PathOutcome::PathChanged,
                        icmp_type: None,
                        icmp_code: None,
                        message: Some(error.to_string()),
                    },
                })?;
                break 'hops;
            }
            let pinned = guard.clone();
            let target = target.ip();
            let timeout = Duration::from_millis(request.timeout_ms);
            let reply = tokio::task::spawn_blocking(move || {
                probes::trace(&pinned, target, hop, seq, timeout)
            })
            .await
            .context("Trace adapter stopped")?;
            let reply = match reply {
                Ok(reply) => {
                    count += 1;
                    reply
                }
                Err(error) => {
                    interrupted = Some(error.to_string());
                    emit(Event::Trace {
                        seq: u32::from(seq),
                        sample: TraceSample {
                            hop,
                            probe,
                            address: None,
                            rtt_ms: None,
                            outcome: error_outcome(&error),
                            icmp_type: None,
                            icmp_code: None,
                            message: Some(error.to_string()),
                        },
                    })?;
                    break 'hops;
                }
            };
            if let Err(error) = guard.recheck(destination).await {
                interrupted = Some(error.to_string());
                emit(Event::Trace {
                    seq: u32::from(seq),
                    sample: TraceSample {
                        hop,
                        probe,
                        address: reply.from.map(|ip| ip.to_string()),
                        rtt_ms: None,
                        outcome: PathOutcome::PathChanged,
                        icmp_type: reply.icmp_type,
                        icmp_code: reply.icmp_code,
                        message: Some(error.to_string()),
                    },
                })?;
                break 'hops;
            }
            reached |= reply.outcome == PathOutcome::Reply;
            let unreachable = reply.outcome == PathOutcome::Unreachable;
            emit(Event::Trace {
                seq: u32::from(seq),
                sample: TraceSample {
                    hop,
                    probe,
                    address: reply.from.map(|ip| ip.to_string()),
                    rtt_ms: reply.rtt_ms,
                    outcome: reply.outcome,
                    icmp_type: reply.icmp_type,
                    icmp_code: reply.icmp_code,
                    message: None,
                },
            })?;
            if unreachable {
                interrupted =
                    Some("Explicit ICMP unreachable response; path visibility ends here".into());
                break 'hops;
            }
        }
        if reached {
            break;
        }
    }
    emit(Event::TraceSummary(TraceSummary {
        target: target.ip().to_string(),
        family: family_name(family),
        capability,
        physical: Some(guard.evidence()),
        max_hops: *max_hops,
        probes_sent: count,
        reached,
        completed: interrupted.is_none(),
        message: interrupted.clone().unwrap_or_else(|| {
            if reached {
                "Destination responded".into()
            } else {
                "Hop budget reached; silent hops remain unknown".into()
            }
        }),
    }))?;
    if let Some(error) = interrupted {
        anyhow::bail!("Trace stopped: {error}");
    }
    // Hop samples are not end-to-end ping attempts and are not mixed into RTT/loss.
    let summary = Summary::default();
    emit(Event::Summary(summary.clone()))?;
    emit(Event::Done)?;
    Ok(summary)
}

pub(super) async fn run_icmp(
    request: &DetailedRequest,
    emit: impl Fn(Event) -> Result<()>,
) -> Result<Summary> {
    let Probe::Icmp { host } = &request.probe else {
        anyhow::bail!("Expected ICMP request");
    };
    super::validate(request)?;
    let family = family(request, host);
    let ipv6 = family == AddressFamily::Ipv6;
    if matches!(request.egress, Egress::System)
        && request
            .path_context
            .as_ref()
            .and_then(|value| value.core_tun)
            == Some(true)
    {
        emit(Event::Echo{seq:0,observation:EchoObservation{target:host.clone(),family:family_name(family),from:None,outcome:PathOutcome::TunLocal,rtt_ms:None,ttl:None,proves_reachability:false,tun_local:true,physical:None,message:"TUN may answer echo locally; no echo is sent or counted as target latency/IPv6 reachability. Select a physical interface or bound DIRECT.".into()}})?;
        let summary = Summary::default();
        emit(Event::Summary(summary.clone()))?;
        emit(Event::Done)?;
        return Ok(summary);
    }
    let capability = probes::echo_capability(ipv6);
    ensure!(
        capability.supported,
        "ICMP unavailable with current permissions: {}",
        capability.reason.unwrap_or_default()
    );
    let guard = if matches!(request.egress, Egress::System) {
        None
    } else {
        Some(PhysicalGuard::new(physical_interface(request)?, ipv6).await?)
    };
    let mut fixed = request.clone();
    fixed.family = Some(family);
    let route = Route::new(&fixed)?;
    let route = if let Some(guard) = &guard {
        route.with_physical_guard(guard)
    } else {
        route
    };
    let target = tokio::time::timeout(
        Duration::from_secs(10),
        route.address(0, "icmp_target", host, 0, None, guard.is_some(), &emit),
    )
    .await
    .context("ICMP target resolution timed out")??;
    emit(Event::Started {
        egress: request.egress.to_string(),
        probe: format!("ICMP {}", target.ip()),
        note: if guard.is_some() {
            "Physical source/device binding; checked before and after every echo".into()
        } else {
            "System ICMP path cannot exclude other tunnel/local handling; responses are not target latency or IPv6 reachability proof".into()
        },
        running_config: false,
    })?;
    let mut attempts = vec![];
    for seq in 1..=request.count {
        if seq > 1 && request.interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(request.interval_ms)).await;
        }
        if let Some(guard) = &guard {
            guard.recheck(target).await?;
        }
        let pinned = guard.clone();
        let destination = target.ip();
        let timeout = Duration::from_millis(request.timeout_ms);
        let reply = tokio::task::spawn_blocking(move || {
            probes::echo(pinned.as_ref(), destination, seq as u16, timeout)
        })
        .await
        .context("ICMP adapter stopped")??;
        if let Some(guard) = &guard {
            guard.recheck(target).await?;
        }
        let confirmed = guard.is_some();
        let successful = confirmed && reply.outcome == PathOutcome::Reply;
        let rtt = if confirmed { reply.rtt_ms } else { None };
        emit(Event::Echo {
            seq,
            observation: EchoObservation {
                target: target.ip().to_string(),
                family: family_name(family),
                from: reply.from.map(|ip| ip.to_string()),
                outcome: if !confirmed && reply.outcome == PathOutcome::Reply {
                    PathOutcome::Unconfirmed
                } else {
                    reply.outcome.clone()
                },
                rtt_ms: rtt,
                ttl: reply.ttl,
                proves_reachability: successful,
                tun_local: false,
                physical: guard.as_ref().map(PhysicalGuard::evidence),
                message: if confirmed {
                    "Bound physical ICMP observation".into()
                } else {
                    "Reply path is unconfirmed; excluded from target RTT and IPv6 reachability conclusions".into()
                },
            },
        })?;
        if confirmed {
            let attempt = Attempt {
                seq,
                rtt_ms: rtt,
                detail: target.ip().to_string(),
                error: (!successful).then(|| {
                    "No correlated echo response; no inference about device existence".into()
                }),
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
#[path = "../test/nettool_path_test.rs"]
mod tests;
