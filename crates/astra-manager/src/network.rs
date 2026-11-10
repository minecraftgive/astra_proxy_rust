//! Agent-owned diagnostic jobs. Transport lifetimes never own probe lifetimes.
use anyhow::{Context, Result};
mod analysis;
use astra_management::v2::*;
use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::{mpsc, watch, Mutex, Semaphore};

#[derive(Debug)]
pub(crate) struct CleanupUnconfirmed(pub String);
impl std::fmt::Display for CleanupUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CleanupUnconfirmed {}

type Reservation = Pin<
    Box<
        dyn Future<
                Output = std::result::Result<
                    tokio::sync::OwnedSemaphorePermit,
                    tokio::sync::AcquireError,
                >,
            > + Send,
    >,
>;
enum Admission {
    Ready(tokio::sync::OwnedSemaphorePermit),
    Waiting(Reservation),
}
impl Admission {
    /// Poll while still in admission, before persistence/spawn yields. Tokio's
    /// semaphore records the waiter now, so later ordinary work cannot take the
    /// spare unit while an earlier two-unit NAT job has not been scheduled yet.
    async fn reserve(slots: Arc<Semaphore>, units: u32) -> Result<Self> {
        let mut wait: Reservation = Box::pin(slots.acquire_many_owned(units));
        match std::future::poll_fn(|cx| Poll::Ready(wait.as_mut().poll(cx))).await {
            Poll::Ready(result) => Ok(Self::Ready(result.context("Diagnostic scheduler closed")?)),
            Poll::Pending => Ok(Self::Waiting(wait)),
        }
    }
    fn waiting(&self) -> bool {
        matches!(self, Self::Waiting(_))
    }
    async fn acquire(self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        match self {
            Self::Ready(permit) => Ok(permit),
            Self::Waiting(wait) => wait.await.context("Diagnostic scheduler closed"),
        }
    }
}
/// Reservation state moves into the worker as one ownership unit.
struct WorkAdmission {
    units: u32,
    admission: Admission,
    queued: Option<tokio::sync::OwnedSemaphorePermit>,
}
struct NatTicket(Arc<AtomicUsize>);
impl Drop for NatTicket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

type CoreEvent = astra_model::nettool::Event;
#[derive(Clone)]
enum Completion {
    Recorded,
    OutcomeStorageFailed(String),
    CleanupUnconfirmed(String),
}
impl Completion {
    fn error(&self) -> Option<anyhow::Error> {
        match self {
            Self::Recorded => None,
            Self::OutcomeStorageFailed(message) => Some(crate::error::message(
                ErrorCode::OutcomeUnknown,
                format!("Diagnostic outcome could not be recorded: {message}"),
            )),
            Self::CleanupUnconfirmed(message) => Some(crate::error::classified(
                ErrorCode::OutcomeUnknown,
                CleanupUnconfirmed(message.clone()).into(),
            )),
        }
    }
}
struct Job {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<Option<Completion>>,
    handle: tokio::task::JoinHandle<()>,
}
pub(crate) struct Scheduler {
    store: crate::database::Database,
    state_dir: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
    running: Arc<Semaphore>,
    waiting: Arc<Semaphore>,
    projects: Arc<Semaphore>,
    nat_pending: Arc<AtomicUsize>,
    paused: AtomicBool,
    pause_gate: Arc<Mutex<()>>,
    cleanup_failure: std::sync::Mutex<Option<String>>,
    outcome_failures: std::sync::Mutex<HashMap<String, String>>,
}
pub(crate) struct Pause {
    owner: Arc<Scheduler>,
    _serial: tokio::sync::OwnedMutexGuard<()>,
    _running: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for Pause {
    fn drop(&mut self) {
        self.owner.paused.store(false, Ordering::SeqCst);
    }
}

impl Scheduler {
    /// One slot per actual ordinary probe, including projects within a batch.
    pub(crate) async fn project_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        self.projects
            .clone()
            .acquire_owned()
            .await
            .context("Diagnostic project scheduler closed")
    }
    /// Legacy active tools also participate in NAT isolation. This guard is
    /// retained until the original operation has finished, without replaying it.
    pub(crate) async fn legacy_permit(
        &self,
    ) -> Result<(
        tokio::sync::OwnedSemaphorePermit,
        tokio::sync::OwnedSemaphorePermit,
    )> {
        self.available()?;
        let running = Admission::reserve(self.running.clone(), 1)
            .await?
            .acquire()
            .await?;
        self.available()?;
        let project = self.project_permit().await?;
        self.available()?;
        Ok((running, project))
    }
    pub(crate) fn new(store: crate::database::Database, state_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            store,
            state_dir,
            jobs: Mutex::new(HashMap::new()),
            running: Arc::new(Semaphore::new(2)),
            waiting: Arc::new(Semaphore::new(4)),
            projects: Arc::new(Semaphore::new(4)),
            nat_pending: Arc::new(AtomicUsize::new(0)),
            paused: AtomicBool::new(false),
            pause_gate: Arc::new(Mutex::new(())),
            cleanup_failure: Default::default(),
            outcome_failures: Default::default(),
        })
    }
    pub(crate) fn record_legacy_cleanup<T>(&self, result: &Result<T>) {
        if let Some(error) = result
            .as_ref()
            .err()
            .filter(|error| error.chain().any(|cause| cause.is::<CleanupUnconfirmed>()))
        {
            *self.cleanup_failure.lock().unwrap() = Some(format!("{error:#}"));
        }
    }
    pub(crate) fn outcome_failure(&self, id: &str) -> Option<String> {
        self.outcome_failures.lock().unwrap().get(id).cloned()
    }
    fn available(&self) -> Result<()> {
        anyhow::ensure!(
            self.outcome_failures.lock().unwrap().is_empty(),
            crate::error::message(
                ErrorCode::Conflict,
                "Diagnostic outcome storage failed; recover storage before admitting more work"
            )
        );
        anyhow::ensure!(self.cleanup_failure.lock().unwrap().is_none(), crate::error::message(ErrorCode::Conflict, "Prior diagnostic process cleanup is unconfirmed; inspect the retained failure before restarting the Agent"));
        anyhow::ensure!(
            !self.paused.load(Ordering::SeqCst),
            crate::error::message(
                ErrorCode::Conflict,
                "Diagnostics suspended for program maintenance"
            )
        );
        anyhow::ensure!(
            !astra_management::maintenance::pending(&self.state_dir)?,
            crate::error::message(ErrorCode::Conflict, "Maintenance is pending")
        );
        Ok(())
    }
    #[cfg(test)]
    pub(crate) async fn submit<F, Fut>(
        self: &Arc<Self>,
        id: String,
        fingerprint: String,
        run: F,
    ) -> Result<Task>
    where
        F: FnOnce(mpsc::Sender<CoreEvent>, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.submit_mode(id, fingerprint, false, run).await
    }

    pub(crate) async fn submit_mode<F, Fut>(
        self: &Arc<Self>,
        id: String,
        fingerprint: String,
        exclusive: bool,
        run: F,
    ) -> Result<Task>
    where
        F: FnOnce(mpsc::Sender<CoreEvent>, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().await;
        if let Some(error) = self.outcome_failure(&id) {
            return Err(crate::error::message(ErrorCode::OutcomeUnknown, error));
        }
        // Reconnects return the original outcome even during maintenance or overload.
        if let Some(task) = self.store.network_existing(&id, &fingerprint).await? {
            return Ok(task);
        }
        self.available()?;
        anyhow::ensure!(
            jobs.len() < 6,
            crate::error::message(
                ErrorCode::Conflict,
                "Diagnostic scheduler busy (2 running, 4 queued)"
            )
        );
        let units = if exclusive { 2 } else { 1 };
        let admission = Admission::reserve(self.running.clone(), units).await?;
        let queued = if admission.waiting() {
            Some(
                self.waiting
                    .clone()
                    .try_acquire_owned()
                    .map_err(|error| match error {
                        tokio::sync::TryAcquireError::NoPermits => crate::error::message(
                            ErrorCode::Conflict,
                            "Diagnostic queue full (4 waiting)",
                        ),
                        tokio::sync::TryAcquireError::Closed => {
                            anyhow::anyhow!("Diagnostic queue closed")
                        }
                    })?,
            )
        } else {
            None
        };
        let task = self.store.begin_network(&id, &fingerprint).await?;
        let nat_ticket = exclusive.then(|| {
            self.nat_pending.fetch_add(1, Ordering::SeqCst);
            NatTicket(self.nat_pending.clone())
        });
        let (cancel, receiver) = watch::channel(false);
        let (done, completion) = watch::channel(None);
        let owner = self.clone();
        let operation = id.clone();
        let stop = cancel.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let handle = tokio::spawn(async move {
            let _nat_ticket = nat_ticket;
            let result = owner
                .work(
                    &operation,
                    stop,
                    receiver,
                    deadline,
                    WorkAdmission {
                        units,
                        admission,
                        queued,
                    },
                    run,
                )
                .await;
            let completion = match result {
                Ok(()) => Completion::Recorded,
                Err(error) if error.chain().any(|cause| cause.is::<CleanupUnconfirmed>()) => {
                    Completion::CleanupUnconfirmed(format!("{error:#}"))
                }
                Err(error) => Completion::OutcomeStorageFailed(format!("{error:#}")),
            };
            if let Completion::OutcomeStorageFailed(error) = &completion {
                owner
                    .outcome_failures
                    .lock()
                    .unwrap()
                    .insert(operation.clone(), error.clone());
            }
            if let Some(error) = completion.error() {
                eprintln!("Diagnostic {operation} cleanup or outcome persistence failed: {error}");
            }
            owner.jobs.lock().await.remove(&operation);
            done.send_replace(Some(completion));
        });
        jobs.insert(
            id,
            Job {
                cancel,
                done: completion,
                handle,
            },
        );
        Ok(task)
    }

    async fn work<F, Fut>(
        &self,
        id: &str,
        stop: watch::Sender<bool>,
        mut cancel: watch::Receiver<bool>,
        deadline: tokio::time::Instant,
        reservation: WorkAdmission,
        run: F,
    ) -> Result<()>
    where
        F: FnOnce(mpsc::Sender<CoreEvent>, watch::Receiver<bool>) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let WorkAdmission {
            units,
            admission,
            queued,
        } = reservation;
        if tokio::time::Instant::now() >= deadline {
            return self
                .store
                .network_state(
                    id,
                    NetworkState::Partial,
                    "Diagnostic deadline exceeded while queued; no probe started",
                )
                .await;
        }
        if admission.waiting() && self.nat_pending.load(Ordering::SeqCst) > 0 {
            self.store
                .network_state(
                    id,
                    NetworkState::Queued,
                    if units == 2 {
                        "NAT is waiting for active diagnostics to finish"
                    } else {
                        "等待 NAT 检测完成"
                    },
                )
                .await?;
        }
        let permit = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return self.store.network_state(id, NetworkState::Cancelled, "Cancelled while queued; no probe started").await,
            _ = tokio::time::sleep_until(deadline) => return self.store.network_state(id, NetworkState::Partial, "Diagnostic deadline exceeded while queued; no probe started").await,
            permit = admission.acquire() => permit?,
        };
        drop(queued);
        if *cancel.borrow() {
            return self
                .store
                .network_state(
                    id,
                    NetworkState::Cancelled,
                    "Cancelled before probe started",
                )
                .await;
        }
        if let Err(error) = self.available() {
            return self
                .store
                .network_state(id, NetworkState::Interrupted, &error.to_string())
                .await;
        }
        self.store
            .network_state(id, NetworkState::Running, "Diagnostic running")
            .await?;
        if tokio::time::Instant::now() >= deadline {
            return self
                .store
                .network_state(
                    id,
                    NetworkState::Partial,
                    "Diagnostic deadline exceeded before probe started",
                )
                .await;
        }
        let (output, mut events) = mpsc::channel(16);
        let operation = run(output, cancel.clone());
        tokio::pin!(operation);
        let mut timed_out = false;
        let mut persistence_error = None;
        let mut evidence_truncated = false;
        let mut evidence_partial = false;
        let mut analysis = analysis::Collector::default();
        let result = loop {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline), if !timed_out => {
                    timed_out = true;
                    stop.send_replace(true);
                }
                event = events.recv(), if !events.is_closed() || !events.is_empty() => {
                    if let Some(event) = event {
                        analysis.observe(&event);
                        evidence_truncated |= truncated_evidence(&event);
                        evidence_partial |= partial_evidence(&event);
                        if persistence_error.is_none() {
                            let persisted=match serde_json::to_value(event) {
                                Ok(value)=>self.store.append_network(id,browser_numbers(value)).await,
                                Err(error)=>Err(error.into()),
                            };
                            if let Err(error)=persisted {
                                persistence_error=Some(error);stop.send_replace(true);
                            }
                        }
                    }
                }
                result = &mut operation => break result,
            }
        };
        // Retain the cleanup latch before any further report writes can fail.
        let cleanup_failed = result
            .as_ref()
            .err()
            .filter(|error| error.chain().any(|cause| cause.is::<CleanupUnconfirmed>()))
            .map(|error| format!("{error:#}"));
        if let Some(error) = &cleanup_failed {
            *self.cleanup_failure.lock().unwrap() = Some(error.clone());
        }
        // The producer has reaped its child before returning, including cancellation.
        while let Ok(event) = events.try_recv() {
            analysis.observe(&event);
            evidence_truncated |= truncated_evidence(&event);
            evidence_partial |= partial_evidence(&event);
            if persistence_error.is_none() {
                if let Err(error) = self
                    .store
                    .append_network(id, browser_numbers(serde_json::to_value(event)?))
                    .await
                {
                    persistence_error = Some(error);
                }
            }
        }
        if persistence_error.is_none() && analysis.relevant() {
            let retained = self.store.network_report(id, None).await?;
            let analysis = analysis.finish(
                evidence_truncated
                    || retained.truncated
                    || evidence_partial
                    || timed_out
                    || *cancel.borrow()
                    || result.is_err(),
            );
            if analysis.truncated {
                evidence_truncated = true;
            }
            if let Err(error) = self
                .store
                .append_network(
                    id,
                    browser_numbers(serde_json::to_value(CoreEvent::Analysis(Box::new(
                        analysis,
                    )))?),
                )
                .await
            {
                persistence_error = Some(error);
            }
        }
        drop(permit);
        let result = persistence_error.map_or(result, Err);
        let (state, message) = match result {
            Err(error) => (
                NetworkState::Failed,
                format!("Diagnostic failed: {error:#}"),
            ),
            Ok(()) if timed_out => (
                NetworkState::Partial,
                "Diagnostic deadline exceeded; partial results retained".into(),
            ),
            Ok(()) if *cancel.borrow() => (
                NetworkState::Cancelled,
                "Cancelled; diagnostic process exited; partial results retained".into(),
            ),
            Ok(()) if evidence_partial => (
                NetworkState::Partial,
                "Diagnostic evidence is incomplete; available results retained".into(),
            ),
            Ok(()) => (NetworkState::Complete, "Diagnostic completed".into()),
        };
        if evidence_truncated {
            self.store.network_truncated(id).await?;
        }
        self.store.network_state(id, state, &message).await?;
        if let Some(error) = cleanup_failed {
            return Err(CleanupUnconfirmed(error).into());
        }
        Ok(())
    }

    pub(crate) async fn cancel(&self, id: &str) -> Result<Option<NetworkReport>> {
        let completion = {
            let jobs = self.jobs.lock().await;
            jobs.get(id).map(|job| {
                job.cancel.send_replace(true);
                job.done.clone()
            })
        };
        let Some(mut completion) = completion else {
            return Ok(None);
        };
        completion
            .wait_for(|v| v.is_some())
            .await
            .map_err(|error| {
                crate::error::classified(
                    ErrorCode::OutcomeUnknown,
                    anyhow::Error::new(error)
                        .context("Diagnostic worker exited without a recorded outcome"),
                )
            })?;
        if let Some(error) = completion.borrow().as_ref().unwrap().error() {
            return Err(error);
        }
        Ok(Some(self.store.network_report(id, None).await?))
    }

    /// Block admission before cancelling, then join every worker before replacement.
    pub(crate) async fn pause(self: &Arc<Self>) -> Result<Pause> {
        let serial = self.pause_gate.clone().lock_owned().await;
        let jobs = {
            let mut jobs = self.jobs.lock().await;
            self.paused.store(true, Ordering::SeqCst);
            jobs.drain()
                .map(|(_, job)| {
                    job.cancel.send_replace(true);
                    job
                })
                .collect::<Vec<_>>()
        };
        let mut failure = self.cleanup_failure.lock().unwrap().clone();
        for mut job in jobs {
            if let Err(error) = job.handle.await {
                failure = Some(format!("Diagnostic worker failed: {error}"));
            }
            match job.done.wait_for(|v| v.is_some()).await {
                Ok(result) => {
                    if let Some(error) = result.as_ref().and_then(Completion::error) {
                        failure = Some(format!("{error:#}"));
                    }
                }
                Err(error) => {
                    failure = Some(format!(
                        "Diagnostic worker exited without a recorded outcome: {error}"
                    ));
                }
            }
        }
        failure = failure.or_else(|| self.cleanup_failure.lock().unwrap().clone());
        anyhow::ensure!(
            failure.is_none(),
            crate::error::message(
                ErrorCode::OutcomeUnknown,
                format!(
                    "Cannot replace programs before diagnostic cleanup: {}",
                    failure.unwrap_or_default()
                )
            )
        );
        // Legacy/header probes are owned outside jobs, but hold these same
        // permits until their processes are reaped. Maintenance joins them too.
        let running = self
            .running
            .clone()
            .acquire_many_owned(2)
            .await
            .context("Diagnostic scheduler closed")?;
        if let Some(error) = self.cleanup_failure.lock().unwrap().clone() {
            return Err(crate::error::classified(
                ErrorCode::OutcomeUnknown,
                CleanupUnconfirmed(format!(
                    "Cannot replace programs before legacy diagnostic cleanup: {error}"
                ))
                .into(),
            ));
        }
        Ok(Pause {
            owner: self.clone(),
            _serial: serial,
            _running: running,
        })
    }
}

fn truncated_evidence(event: &CoreEvent) -> bool {
    use astra_model::nettool::DnsDetail;
    match event {
        CoreEvent::Project { detail, .. } => truncated_evidence(detail),
        CoreEvent::Analysis(report) => report.truncated,
        CoreEvent::Dns { detail, .. } => match detail.as_ref() {
            DnsDetail::Response { truncated, .. } => *truncated,
            DnsDetail::Records { records, .. } => {
                records.iter().any(|record| record.text_truncated)
            }
            _ => false,
        },
        CoreEvent::Tls { detail, .. } => match detail.as_ref() {
            astra_model::network_tls::TlsDetail::Trust {
                load_errors_truncated,
                ..
            } => *load_errors_truncated,
            astra_model::network_tls::TlsDetail::Handshake { truncated, .. } => *truncated,
            astra_model::network_tls::TlsDetail::Certificate {
                metadata_truncated, ..
            } => *metadata_truncated,
            _ => false,
        },
        CoreEvent::Http { detail, .. } => matches!(
            detail.as_ref(),
            astra_model::network_tls::HttpDetail::Response {
                body_truncated: true,
                ..
            }
        ),
        _ => false,
    }
}

fn partial_evidence(event: &CoreEvent) -> bool {
    matches!(event, CoreEvent::Project { detail, .. } if partial_evidence(detail))
        || matches!(event, CoreEvent::ProjectStatus { state, .. } if matches!(state.as_str(), "failed" | "skipped"))
        || matches!(event, CoreEvent::Nat(astra_model::network_nat::Event::Report { report }) if report.partial)
        || matches!(event, CoreEvent::TraceSummary(summary) if !summary.completed)
}

pub(crate) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|cancelled| *cancelled).await;
}

/// Validate public request bounds; pinned route and resolver checks run before sockets open.
pub(crate) fn validate(request: &NetworkDiagnosticsRequest) -> Result<()> {
    use astra_model::nettool::{Egress, Probe, MAX_COUNT};
    anyhow::ensure!(
        (1..=MAX_COUNT).contains(&request.count)
            && (100..=30_000).contains(&request.timeout_ms.parse::<u64>().map_err(|error| {
                crate::error::classified(ErrorCode::InvalidRequest, error.into())
            })?)
            && request
                .interval_ms
                .parse::<u64>()
                .map_err(|error| crate::error::classified(
                    ErrorCode::InvalidRequest,
                    error.into()
                ))?
                <= 10_000,
        crate::error::message(ErrorCode::InvalidRequest, "Network probe limits exceeded")
    );
    let valid_text = |s: &str| !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control);
    let valid_endpoint = |value: &str| {
        let authority = value
            .split_once("://")
            .map_or(value, |(_, rest)| rest)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        valid_text(value)
            && !authority.contains('@')
            && (value.starts_with("https://")
                || value.starts_with("http://")
                || !value
                    .split_once("://")
                    .map_or(value, |(_, rest)| rest)
                    .contains(['?', '/']))
    };
    if let Some(resolver) = &request.resolver {
        anyhow::ensure!(
            valid_endpoint(resolver),
            crate::error::message(
                ErrorCode::InvalidRequest,
                "Invalid diagnostic resolver: URL credentials are not accepted"
            )
        );
    }
    if let Egress::Interface(name) | Egress::Outbound(name) = &request.egress {
        anyhow::ensure!(
            !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
            crate::error::message(ErrorCode::InvalidRequest, "Invalid target Agent interface")
        );
    }
    match &request.probe {
        Probe::Tcp { host, port } => {
            anyhow::ensure!(
                valid_text(host) && *port != 0,
                crate::error::message(ErrorCode::InvalidRequest, "Invalid TCP target")
            );
        }
        Probe::Dns {
            name,
            record,
            server,
        } => {
            anyhow::ensure!(
                valid_text(name) && valid_text(record) && valid_endpoint(server),
                crate::error::message(ErrorCode::InvalidRequest, "Invalid DNS target")
            );
            anyhow::ensure!(
                !server.contains("://")
                    || ["udp://", "tcp://", "tls://", "dot://", "https://"]
                        .iter()
                        .any(|prefix| server.starts_with(prefix)),
                crate::error::message(ErrorCode::Unsupported, "Unknown DNS transport")
            );
        }
        Probe::Icmp { host } => anyhow::ensure!(
            valid_text(host),
            crate::error::message(ErrorCode::InvalidRequest, "Invalid ICMP target")
        ),
        Probe::TcpPorts { host, ports } => anyhow::ensure!(
            valid_text(host)
                && !ports.is_empty()
                && ports.len() <= 16
                && ports.iter().all(|port| *port != 0)
                && ports.iter().collect::<std::collections::HashSet<_>>().len() == ports.len(),
            crate::error::message(
                ErrorCode::InvalidRequest,
                "TCP requires 1–16 unique nonzero ports"
            )
        ),
        Probe::Tls {
            host,
            port,
            server_name,
            ..
        } => anyhow::ensure!(
            valid_text(host)
                && *port != 0
                && server_name.as_ref().is_none_or(|name| valid_text(name)),
            crate::error::message(ErrorCode::InvalidRequest, "Invalid TLS target")
        ),
        Probe::Http { url, method, .. } => {
            anyhow::ensure!(
                valid_endpoint(url) && (url.starts_with("https://") || url.starts_with("http://")),
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "HTTP requires a bounded HTTP(S) URL without credentials"
                )
            );
            anyhow::ensure!(
                matches!(method.as_str(), "GET" | "HEAD"),
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "Only GET and HEAD diagnostics are supported"
                )
            );
        }
        Probe::HttpsIp { url, .. } => anyhow::ensure!(
            valid_endpoint(url) && url.starts_with("https://"),
            crate::error::message(
                ErrorCode::InvalidRequest,
                "Public IP observation requires HTTPS without URL credentials"
            )
        ),
        Probe::Stun { server } => anyhow::ensure!(
            valid_text(server) && !server.contains(['@', '?', '#']),
            crate::error::message(ErrorCode::InvalidRequest, "Invalid STUN endpoint")
        ),
        Probe::Nat { request: nat } => {
            nat.validate()
                .map_err(|message| crate::error::message(ErrorCode::InvalidRequest, message))?;
            anyhow::ensure!(
                request.count == 1,
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "NAT is one bounded session; count must be 1"
                )
            );
            anyhow::ensure!(
                request.egress == Egress::Interface(nat.interface_id.clone()),
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "NAT must use its separately selected physical interface"
                )
            );
            anyhow::ensure!(
                request.family.is_none_or(|family| family == nat.family),
                crate::error::message(
                    ErrorCode::InvalidRequest,
                    "NAT address family differs from the physical selection"
                )
            );
        }
        Probe::Trace {
            host,
            max_hops,
            probes_per_hop,
        } => {
            anyhow::ensure!(valid_text(host) && (1..=30).contains(max_hops) && (1..=3).contains(probes_per_hop) && request.count == 1 && request.timeout_ms.parse::<u64>().map_err(|error| crate::error::classified(ErrorCode::InvalidRequest, error.into()))? <= 1000, crate::error::message(ErrorCode::InvalidRequest, "Trace requires 1–30 hops, 1–3 samples per hop, count=1 and timeout at most 1000 ms"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/network_scheduler_test.rs"]
mod tests;
