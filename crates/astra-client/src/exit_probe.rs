//! Background probes of the active exit node for the header: TCP handshake
//! latency on an interval and, when enabled, the exit IP once per node.
//!
//! Each round is one `ExitProbe` query. The manager answers it from a probe
//! core that may stay loaded between rounds (keep-alive), so a round costs a
//! handshake rather than a new process.
//!
//! Every connect and node switch runs one round that looks up the exit IP and
//! times the handshake, whatever the settings say. The settings then add
//! latency rounds on an interval and keep retrying an IP lookup that failed.
//! A switch drops the round in flight, so an old node never reports late.
use std::{
    net::IpAddr,
    path::Path,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

/// After a failed round (manager busy, core restarting) try again this soon.
const RETRY: Duration = Duration::from_secs(10);
/// A round that never reports is abandoned after this long.
const ROUND_LIMIT: Duration =
    Duration::from_secs(astra_model::diagnostics::MAX_CONNECT_TIMEOUT_SECS + 17);
/// The probe core outlives the gap between rounds by this margin.
const KEEP_ALIVE_MARGIN: Duration = Duration::from_secs(30);

/// The node a round measures; a new connection or exit starts over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    pub profile: String,
    pub pid: u32,
    pub outbound: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Latency {
    Measured(u64),
    /// The latest TCP handshake through the node failed or timed out.
    Failed,
}

/// What the header should probe right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub ip: bool,
    /// Interval between latency rounds; `None` turns latency off.
    pub latency: Option<Duration>,
    /// Let the manager keep the probe core loaded between rounds.
    pub keep_alive: bool,
}
impl Plan {
    /// How long the probe core should wait for the next round.
    fn keep_alive(self) -> Option<Duration> {
        self.keep_alive
            .then(|| self.latency.unwrap_or(RETRY) + KEEP_ALIVE_MARGIN)
    }
}

/// A round's result: the probe, or the manager's error text.
type Outcome = Result<astra_model::diagnostics::Probe, String>;

struct Round {
    task: JoinHandle<()>,
    receiver: mpsc::Receiver<Outcome>,
    started: Instant,
    /// The round asked for the exit IP.
    ip: bool,
    /// The round that follows a connect or node switch.
    initial: bool,
}
impl Drop for Round {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Default)]
pub struct Prober {
    exit: Option<Exit>,
    round: Option<Round>,
    next: Option<Instant>,
    ip_found: bool,
    /// The connect/switch round has not answered yet.
    initial: bool,
    latency: Option<Latency>,
    /// `latency` came from an interval round, which the setting can hide.
    periodic: bool,
    /// The manager predates exit probes; stop asking until the exit changes.
    unsupported: bool,
    /// A newly measured IP for the client's per-node cache.
    pub found_ip: Option<(Exit, IpAddr)>,
}

impl Prober {
    /// Latest latency of `outbound` in `profile`, if it is the node being probed.
    pub fn latency(&self, profile: &str, outbound: &str) -> Option<Latency> {
        self.exit
            .as_ref()
            .filter(|exit| exit.profile == profile && exit.outbound == outbound)
            .and(self.latency)
    }
    /// True while a round is in flight.
    pub fn running(&self) -> bool {
        self.round.is_some()
    }
    /// Collect results and start the next round when one is due; true when
    /// something shown in the header changed.
    pub fn tick(
        &mut self,
        socket: &Path,
        target: Option<Exit>,
        plan: Plan,
        waker: &crate::waker::Waker,
    ) -> bool {
        let now = Instant::now();
        let mut changed = self.collect(plan, now);
        if target != self.exit {
            changed |= self.latency.is_some();
            // Dropping the old round aborts it: its node's answer cannot land here.
            *self = Self {
                initial: target.is_some(),
                exit: target,
                next: Some(now),
                found_ip: self.found_ip.take(),
                ..Default::default()
            };
        }
        // Turning latency off hides interval results, not the connect/switch one.
        if self.periodic && plan.latency.is_none() {
            self.latency = None;
            self.periodic = false;
            changed = true;
        }
        let Some(exit) = self.exit.clone() else {
            return changed;
        };
        let initial = self.initial;
        let ip = initial || (plan.ip && !self.ip_found);
        if self.unsupported || !(ip || plan.latency.is_some()) {
            self.round = None;
            return changed;
        }
        if self.round.is_none() && self.next.is_none_or(|next| next <= now) {
            self.round = Some(start(
                socket,
                &exit,
                ip,
                initial,
                plan.keep_alive(),
                waker.clone(),
            ));
        }
        changed
    }

    fn collect(&mut self, plan: Plan, now: Instant) -> bool {
        let Some(round) = &mut self.round else {
            return false;
        };
        let (asked_ip, initial) = (round.ip, round.initial);
        let outcome = match round.receiver.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(_) if now.duration_since(round.started) > ROUND_LIMIT => {
                Some(Err("exit probe timed out".into()))
            }
            Err(_) => None,
        };
        let Some(outcome) = outcome else {
            return false;
        };
        self.round = None;
        let interval = plan.latency.unwrap_or(RETRY);
        let mut changed = false;
        match outcome {
            Ok(probe) => {
                if initial || plan.latency.is_some() {
                    let latency = probe.connect_ms.map_or(Latency::Failed, Latency::Measured);
                    changed |= self.latency != Some(latency);
                    self.latency = Some(latency);
                    self.periodic = !initial;
                }
                if initial {
                    self.initial = false;
                }
                if let (Some(ip), Some(exit)) = (probe.ip, &self.exit) {
                    self.ip_found = true;
                    self.found_ip = Some((exit.clone(), ip));
                    changed = true;
                }
                // A failed IP lookup is retried soon while the IP setting is on.
                let wait = if asked_ip && plan.ip && !self.ip_found {
                    interval.min(RETRY)
                } else {
                    interval
                };
                self.next = Some(now + wait);
            }
            Err(error) => {
                if error.contains("unknown variant") {
                    self.unsupported = true;
                }
                self.next = Some(now + interval.min(RETRY));
            }
        }
        changed
    }
}

fn start(
    socket: &Path,
    exit: &Exit,
    ip: bool,
    initial: bool,
    keep_alive: Option<Duration>,
    waker: crate::waker::Waker,
) -> Round {
    let (sender, receiver) = mpsc::channel(1);
    let socket = socket.to_owned();
    let (profile, outbound) = (exit.profile.clone(), exit.outbound.clone());
    let task = tokio::spawn(async move {
        let result = crate::protocol::exit_probe(&socket, profile, outbound, ip, keep_alive)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(result).await;
        waker.wake();
    });
    Round {
        task,
        receiver,
        started: Instant::now(),
        ip,
        initial,
    }
}

#[cfg(test)]
#[path = "test/exit_probe_test.rs"]
mod tests;
