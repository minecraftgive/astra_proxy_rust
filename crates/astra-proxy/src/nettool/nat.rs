//! RFC 5780 observations on one manually selected physical interface.
//! Mapping reuses one unconnected socket; filtering starts with a fresh socket.
use super::{detailed, stun};
use anyhow::{ensure, Context, Result};
use astra_model::{
    nettool::{DetailedRequest, Egress, Event as ToolEvent, Summary},
    network_nat::{self as model, Behavior, Comparison, Observation, Report},
};
use astra_platform::diagnostic_path::PhysicalGuard;
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

const MAX_DISCARDED_PACKETS: u32 = 64;

fn behavior_candidate(
    answers: &[(SocketAddr, stun::Binding)],
) -> Option<&(SocketAddr, stun::Binding)> {
    // A basic-only service may appear before a behavior-capable service in the
    // user's list. Prefer a complete advertisement; advanced() still verifies
    // every alternate endpoint and CHANGE-REQUEST before trusting it.
    answers
        .iter()
        .find(|(primary, answer)| {
            answer.origin == Some(*primary)
                && answer.other.is_some_and(|other| {
                    other.is_ipv4() == primary.is_ipv4()
                        && other.ip() != primary.ip()
                        && other.port() != primary.port()
                })
        })
        .or_else(|| answers.first())
}

pub(super) trait Guard {
    fn bind(&self) -> std::io::Result<UdpSocket>;
    async fn check(&self, target: SocketAddr) -> Result<()>;
    fn destination(&self, target: SocketAddr) -> std::io::Result<SocketAddr> {
        Ok(target)
    }
    fn evidence(&self) -> Option<astra_model::network_path::PhysicalPathEvidence> {
        None
    }
}
impl Guard for PhysicalGuard {
    fn destination(&self, target: SocketAddr) -> std::io::Result<SocketAddr> {
        PhysicalGuard::destination(self, target)
    }
    fn evidence(&self) -> Option<astra_model::network_path::PhysicalPathEvidence> {
        Some(PhysicalGuard::evidence(self))
    }
    fn bind(&self) -> std::io::Result<UdpSocket> {
        self.bind_udp()
    }
    async fn check(&self, target: SocketAddr) -> Result<()> {
        Ok(self.recheck(target).await?)
    }
}
struct Engine<'a, G, F> {
    guard: &'a G,
    report: &'a mut Report,
    emit: &'a F,
    observed: HashMap<(SocketAddr, SocketAddr), SocketAddr>,
    retries: &'a [Duration],
    next_transaction: tokio::time::Instant,
}
impl<G: Guard, F: Fn(ToolEvent) -> Result<()>> Engine<'_, G, F> {
    async fn check_path(&mut self, target: SocketAddr) -> Result<()> {
        let result = self.guard.check(target).await;
        self.report.physical_path = self.guard.evidence();
        result
    }
    async fn exchange(
        &mut self,
        socket: &UdpSocket,
        target: SocketAddr,
        expected: SocketAddr,
        change: u32,
        phase: &str,
    ) -> Result<Option<stun::Binding>> {
        // RFC 5780 section 5: at most ten new transactions per second.
        tokio::time::sleep_until(self.next_transaction).await;
        self.next_transaction = tokio::time::Instant::now() + Duration::from_millis(100);
        let target = self.guard.destination(target)?;
        let expected = self.guard.destination(expected)?;
        let id = stun::transaction();
        let packet = stun::request(id, change);
        let started = Instant::now();
        let local = socket.local_addr()?;
        let mut observation = Observation {
            phase: phase.into(),
            server: target.to_string(),
            expected_source: expected.to_string(),
            source: None,
            local_endpoint: local.to_string(),
            mapped_endpoint: None,
            other_address: None,
            response_origin: None,
            transaction_id: stun::id_text(id),
            response_received: false,
            attempts: 0,
            discarded_packets: 0,
            elapsed_ms: 0,
            error: None,
        };
        let mut buffer = [0; stun::MAX_PACKET + 1];
        let mut binding = None;
        for wait in self.retries {
            self.check_path(target).await?;
            // Also verify the alternate source's route before authorizing the test.
            if expected != target {
                self.check_path(expected).await?;
            }
            socket.send_to(&packet, target).await?;
            observation.attempts += 1;
            let until = tokio::time::Instant::now() + *wait;
            loop {
                match tokio::time::timeout_at(until, socket.recv_from(&mut buffer)).await {
                    Err(_) => break,
                    Ok(Err(error)) => return Err(error.into()),
                    Ok(Ok((length, source))) => {
                        match stun::decode(&buffer[..length], id, source, expected) {
                            Ok(reply) => {
                                self.check_path(target).await?;
                                if expected != target {
                                    self.check_path(expected).await?;
                                }
                                observation.response_received = true;
                                observation.source = Some(source.to_string());
                                observation.mapped_endpoint = Some(reply.mapped.to_string());
                                observation.other_address = reply.other.map(|s| s.to_string());
                                observation.response_origin = reply.origin.map(|s| s.to_string());
                                // Compare repeats of the SAME destination/session only.
                                if let Some(before) =
                                    self.observed.insert((local, target), reply.mapped)
                                {
                                    if before != reply.mapped {
                                        self.report.unstable = true;
                                    }
                                }
                                binding = Some(reply);
                                break;
                            }
                            Err(_) => {
                                if let Some(code) =
                                    stun::error_code(&buffer[..length], id, source, expected)
                                        .or_else(|| {
                                            stun::error_code(&buffer[..length], id, source, target)
                                        })
                                {
                                    self.check_path(target).await?;
                                    observation.response_received = true;
                                    observation.source = Some(source.to_string());
                                    observation.error = Some(format!(
                                        "STUN server rejected the request (code {code})"
                                    ));
                                    break;
                                }
                                observation.discarded_packets += 1;
                                if observation.discarded_packets >= MAX_DISCARDED_PACKETS {
                                    observation.error = Some(
                                        "Invalid-source/transaction/length packet budget exhausted"
                                            .into(),
                                    );
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if binding.is_some() || observation.error.is_some() {
                break;
            }
        }
        observation.elapsed_ms = started.elapsed().as_millis() as u64;
        if binding.is_none() && observation.error.is_none() {
            observation.error =
                Some("No valid STUN response within bounded retransmissions".into());
        }
        (self.emit)(ToolEvent::Nat(model::Event::Observation {
            observation: observation.clone(),
        }))?;
        self.report.observations.push(observation);
        Ok(binding)
    }
    async fn filtering_response(
        &mut self,
        socket: &UdpSocket,
        primary: SocketAddr,
        expected: SocketAddr,
        change: u32,
        phase: &str,
    ) -> Result<Option<stun::Binding>> {
        let binding = self
            .exchange(socket, primary, expected, change, phase)
            .await?;
        if binding.is_none() {
            let observation = self
                .report
                .observations
                .last()
                .context("Missing filtering observation")?;
            // Only bounded silence can support a negative filtering result.
            // A server rejection or exhausted packet-validation budget is not
            // evidence that the network filtered an alternate-source reply.
            ensure!(
                !observation.response_received
                    && observation.discarded_packets < MAX_DISCARDED_PACKETS,
                "Filtering response was rejected or invalid; classification withheld"
            );
        }
        Ok(binding)
    }
    async fn basic(
        &mut self,
        socket: &UdpSocket,
        endpoints: &[SocketAddr],
    ) -> Result<Vec<(SocketAddr, stun::Binding)>> {
        let mut answers = vec![];
        for endpoint in endpoints {
            if let Some(reply) = self
                .exchange(socket, *endpoint, *endpoint, 0, "basic_binding")
                .await?
            {
                if self
                    .exchange(socket, *endpoint, *endpoint, 0, "same_target_repeat")
                    .await?
                    .is_none()
                {
                    self.report.partial = true;
                }
                answers.push((*endpoint, reply));
            } else {
                self.report.partial = true;
            }
        }
        self.report.comparison = if self.report.unstable {
            Comparison::Unstable
        } else if answers.len() < 2 {
            Comparison::Insufficient
        } else if answers
            .iter()
            .all(|(_, reply)| reply.mapped == answers[0].1.mapped)
        {
            Comparison::Consistent
        } else {
            Comparison::Different
        };
        Ok(answers)
    }
    async fn advanced(
        &mut self,
        mapping: &UdpSocket,
        primary: SocketAddr,
        initial: &stun::Binding,
    ) -> Result<()> {
        let other = initial.other.context(
            "Server does not advertise RFC 5780 OTHER-ADDRESS; only basic evidence is available",
        )?;
        ensure!(
            initial.origin == Some(primary),
            "RFC 5780 RESPONSE-ORIGIN is absent or inconsistent"
        );
        ensure!(
            other.is_ipv4() == primary.is_ipv4()
                && other.ip() != primary.ip()
                && other.port() != primary.port(),
            "RFC 5780 requires two distinct IPs and ports"
        );
        let alternate_ip = SocketAddr::new(other.ip(), primary.port());
        let alternate_port = SocketAddr::new(primary.ip(), other.port());
        // Capability validation uses a separate socket and deliberately primes
        // all four endpoints. It must NEVER prime the filtering session.
        let capability = self.guard.bind()?;
        for destination in [primary, alternate_ip, alternate_port, other] {
            let response = self
                .exchange(
                    &capability,
                    destination,
                    destination,
                    0,
                    "capability_endpoint",
                )
                .await?
                .context("Alternate STUN endpoint is unavailable; classification withheld")?;
            ensure!(
                response.origin == Some(destination),
                "Alternate endpoint omitted matching RESPONSE-ORIGIN"
            );
            let advertised = response
                .other
                .context("Alternate endpoint omitted OTHER-ADDRESS")?;
            ensure!(
                advertised.ip() != destination.ip()
                    && advertised.port() != destination.port()
                    && [primary, alternate_ip, alternate_port, other].contains(&advertised),
                "Inconsistent RFC 5780 alternate endpoint set"
            );
        }
        ensure!(
            self.exchange(&capability, primary, other, 6, "capability_change_ip_port")
                .await?
                .is_some(),
            "CHANGE-REQUEST change-IP/port was not verified; filtering classification withheld"
        );
        ensure!(
            self.exchange(
                &capability,
                primary,
                alternate_port,
                2,
                "capability_change_port"
            )
            .await?
            .is_some(),
            "CHANGE-REQUEST change-port was not verified; filtering classification withheld"
        );
        self.report.advanced_capable = true;
        let second = self
            .exchange(
                mapping,
                alternate_ip,
                alternate_ip,
                0,
                "mapping_alternate_ip",
            )
            .await?
            .context("Mapping test II unavailable")?;
        let third = self
            .exchange(mapping, other, other, 0, "mapping_alternate_ip_port")
            .await?
            .context("Mapping test III unavailable")?;
        // Repeat every tested destination, avoiding a transient remap being
        // mistaken for destination dependence.
        for destination in [primary, alternate_ip, other] {
            ensure!(
                self.exchange(
                    mapping,
                    destination,
                    destination,
                    0,
                    "mapping_stability_repeat"
                )
                .await?
                .is_some(),
                "Mapping repeat unavailable"
            );
        }
        if !self.report.unstable {
            self.report.mapping =
                if initial.mapped == second.mapped && second.mapped == third.mapped {
                    Behavior::EndpointIndependent
                } else if second.mapped == third.mapped {
                    Behavior::AddressDependent
                } else {
                    Behavior::AddressAndPortDependent
                };
        }
        let filtering = self.guard.bind()?;
        ensure!(
            filtering.local_addr()? != mapping.local_addr()?
                && filtering.local_addr()? != capability.local_addr()?,
            "Filtering requires a fresh local UDP session"
        );
        let baseline = self
            .exchange(&filtering, primary, primary, 0, "filtering_initial")
            .await?
            .context("Fresh filtering session has no baseline response")?;
        let change_both = self
            .filtering_response(&filtering, primary, other, 6, "filtering_change_ip_port")
            .await?;
        let behavior = if change_both.is_some() {
            Behavior::EndpointIndependent
        } else {
            if self
                .filtering_response(
                    &filtering,
                    primary,
                    alternate_port,
                    2,
                    "filtering_change_port",
                )
                .await?
                .is_some()
            {
                Behavior::AddressDependent
            } else {
                Behavior::AddressAndPortDependent
            }
        };
        let repeated = self
            .exchange(&filtering, primary, primary, 0, "filtering_control_repeat")
            .await?
            .context("Filtering control failed; loss cannot establish filtering behavior")?;
        ensure!(
            baseline.mapped == repeated.mapped,
            "Filtering mapping changed; classification withheld"
        );
        // Reverify alternate server responses on the primed capability socket
        // after a negative result to distinguish server failure from filtering.
        ensure!(
            self.exchange(&capability, primary, other, 6, "capability_recheck")
                .await?
                .is_some(),
            "Alternative response capability disappeared"
        );
        ensure!(
            self.exchange(
                &capability,
                primary,
                alternate_port,
                2,
                "capability_port_recheck"
            )
            .await?
            .is_some(),
            "Alternative port response capability disappeared"
        );
        if !self.report.unstable {
            self.report.filtering = behavior;
        }
        self.report.traditional_type = match (self.report.mapping, self.report.filtering) {
            (Behavior::EndpointIndependent, filtering)
                if filtering != Behavior::Unknown && initial.mapped == mapping.local_addr()? =>
            {
                // A firewall may filter replies without translating addresses.
                // Do not describe that path as a cone NAT (or as unrestricted).
                Some("no_translation_observed".into())
            }
            (Behavior::EndpointIndependent, Behavior::EndpointIndependent) => {
                Some("full_cone_observed".into())
            }
            (Behavior::EndpointIndependent, Behavior::AddressDependent) => {
                Some("restricted_cone_observed".into())
            }
            (Behavior::EndpointIndependent, Behavior::AddressAndPortDependent) => {
                Some("port_restricted_cone_observed".into())
            }
            (Behavior::AddressAndPortDependent, Behavior::AddressAndPortDependent) => {
                Some("symmetric_observed".into())
            }
            _ => None,
        };
        Ok(())
    }
}
pub async fn run(
    request: &DetailedRequest,
    options: &model::Request,
    emit: &impl Fn(ToolEvent) -> Result<()>,
) -> Result<Summary> {
    options.validate().map_err(anyhow::Error::msg)?;
    let started = Instant::now();
    let mut report = Report { interface_id: options.interface_id.clone(), interface_name: options.interface_id.clone(), family: options.family,
        local_endpoint: None, physical_path: None, comparison: Comparison::Insufficient, mapping: Behavior::Unknown, filtering: Behavior::Unknown,
        traditional_type: None, advanced_capable: false, unstable: false, partial: false, elapsed_ms: 0, observations: vec![],
        notes: vec!["Direct access through the manually selected physical interface; services observe that network's public address. No binding-lifetime, hairpin or ALG test. Results describe this sample, not router identity.".into()] };
    let retries = [
        Duration::from_millis(500),
        Duration::from_secs(1),
        Duration::from_secs(2),
    ];
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let ipv6 = matches!(options.family, model::AddressFamily::Ipv6);
        let guard = PhysicalGuard::new(&options.interface_id, ipv6).await?;
        report.interface_name = guard.name().to_owned();
        report.physical_path = Some(guard.evidence());
        let mut constrained = request.clone();
        constrained.egress = Egress::Interface(options.interface_id.clone());
        constrained.pinned = None;
        let mut endpoints = vec![];
        for (index, server) in options.servers.iter().enumerate() {
            let (host, port) = stun::endpoint(server)?;
            let endpoint = detailed::resolve_physical_bound(
                &constrained,
                &options.interface_id,
                &host,
                port,
                Some(ipv6),
                index as u32 + 1,
                &guard,
                emit,
            )
            .await?;
            guard.recheck(endpoint).await?;
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        }
        let mapping = guard.bind_udp()?;
        report.local_endpoint = Some(mapping.local_addr()?.to_string());
        let mut engine = Engine {
            guard: &guard,
            report: &mut report,
            emit,
            observed: HashMap::new(),
            retries: &retries,
            next_transaction: tokio::time::Instant::now(),
        };
        let answers = engine.basic(&mapping, &endpoints).await?;
        if options.advanced {
            let (primary, initial) = behavior_candidate(&answers)
                .context("No basic response; advanced behavior is unknown")?;
            if let Err(error) = engine.advanced(&mapping, *primary, initial).await {
                engine.report.partial = true;
                engine.report.traditional_type = None;
                engine
                    .report
                    .notes
                    .push(format!("Advanced evidence incomplete: {error}"));
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    match outcome {
        Err(_) => {
            report.partial = true;
            report
                .notes
                .push("NAT 30-second total deadline reached; partial evidence retained".into());
        }
        Ok(Err(error)) => {
            report.partial = true;
            report
                .notes
                .push(format!("Stopped without changing interface: {error}"));
        }
        Ok(Ok(())) => {}
    }
    if report.unstable {
        report.comparison = Comparison::Unstable;
        report.mapping = Behavior::Unknown;
        report.filtering = Behavior::Unknown;
        report.traditional_type = None;
    }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    let summary = Summary {
        sent: report.observations.iter().map(|v| v.attempts).sum(),
        received: report
            .observations
            .iter()
            .filter(|v| v.response_received)
            .count() as u32,
        ..Default::default()
    };
    emit(ToolEvent::Nat(model::Event::Report { report }))?;
    Ok(summary)
}

#[cfg(test)]
#[path = "../test/network_nat_test.rs"]
mod tests;
