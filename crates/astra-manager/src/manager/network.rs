//! Owned diagnostic processes: cancellation is acknowledged only after reaping.
use super::{nettool_environment::Environment, DiagnosticPlan};
mod pinned;
mod selection;
use anyhow::{Context, Result};
use astra_model::nettool::{
    DetailedRequest, DiagnosticContext, Egress, Event, Probe, ProbeBinaryIdentity, Request,
};
use astra_platform::program_info::ProgramInfo;
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{mpsc, watch},
};

const LINE_BYTES: u64 = 60 * 1024;
const MAX_EVENTS: usize = 16_000;
const ERROR_BYTES: usize = 4096;
const INFO_BYTES: u64 = 16 * 1024;

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() || cancel.changed().await.is_err() {
            return;
        }
    }
}

fn cancellation_requested(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow() || cancel.has_changed().is_err()
}

impl DiagnosticPlan {
    pub(super) async fn network_diagnostic(
        self,
        request: impl Into<DetailedRequest>,
        output: mpsc::Sender<Event>,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        if cancellation_requested(&cancel) {
            return Ok(());
        }
        let mut environment = Environment::new(&self.runtime_dir, true)?;
        let result = self
            .network_diagnostic_in(request.into(), output, cancel, &mut environment)
            .await;
        environment.finish(result)
    }

    async fn network_diagnostic_in(
        mut self,
        mut request: DetailedRequest,
        output: mpsc::Sender<Event>,
        mut cancel: watch::Receiver<bool>,
        environment: &mut Environment,
    ) -> Result<()> {
        self.config_path = environment.absolute(&self.config_path)?;
        // Never load a full profile in the subprocess. A pinned request contains
        // only one confirmed node and explicit path metadata.
        request.config.clear();
        request.pinned = None;
        request.interface_resolvers.clear();
        request.path_context = None;
        if cancellation_requested(&cancel) {
            return Ok(());
        }
        // The installed executable may differ from a still-running core. Only
        // that executable's own explicit declaration selects its stream format.
        let mut command = environment.command(&self.executable)?;
        command
            .arg("--program-info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut metadata = environment
            .spawn(&mut command)
            .context("Start diagnostic executable capability query")?;
        let Some(info) = program_info(&mut metadata, &mut cancel).await? else {
            return Ok(());
        };
        let mode = stream_mode(&info, &request)?;
        if requires_tools(&request) {
            require_capability(
                &info,
                astra_model::control::NETWORK_DIAGNOSTICS_TOOLS_CAPABILITY,
            )?;
        }
        if matches!(request.probe, Probe::Nat { .. }) {
            require_capability(
                &info,
                astra_model::control::NETWORK_DIAGNOSTICS_NAT_CAPABILITY,
            )?;
        }
        if matches!(request.probe, Probe::Trace { .. } | Probe::Icmp { .. }) {
            require_capability(
                &info,
                astra_model::control::NETWORK_DIAGNOSTICS_PATH_CAPABILITY,
            )?;
        }
        if matches!(request.egress, Egress::Outbound(_)) {
            require_capability(
                &info,
                astra_model::control::NETWORK_DIAGNOSTICS_PINNED_CAPABILITY,
            )?;
        }
        if requires_resolution(&request) {
            anyhow::ensure!(
                mode == StreamMode::Detailed,
                crate::error::message(
                    astra_management::v2::ErrorCode::Unsupported,
                    "Unsupported: constrained resolution cannot use a legacy diagnostic stream"
                )
            );
            require_capability(
                &info,
                astra_model::control::NETWORK_DIAGNOSTICS_RESOLUTION_CAPABILITY,
            )?;
        }
        let mut context = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            context = self.diagnostic_context(&request, info, mode) => context,
        };
        if matches!(
            request.probe,
            Probe::Trace { .. } | Probe::Icmp { .. } | Probe::Ipv6 { .. } | Probe::Nat { .. }
        ) {
            request.path_context = Some(self.path_context(&context).await);
        }
        if matches!(request.egress, Egress::Outbound(_)) {
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return Ok(()),
                result = tokio::time::timeout(Duration::from_secs(8), self.prepare_pinned(&mut request, &mut context)) => result.context("Pinned diagnostic context timed out")??,
            }
        }
        // OS collectors own bounded subprocesses. Poll them to completion even
        // if cancellation arrives so their explicit reapers are not abandoned.
        if cancellation_requested(&cancel) {
            return Ok(());
        }
        self.sample_resolvers(&mut request, &mut context).await;
        if cancellation_requested(&cancel) {
            return Ok(());
        }
        if request.pinned.is_some() {
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return Ok(()),
                result = tokio::time::timeout(Duration::from_secs(8), self.verify_pinned(&request, &context)) => result.context("Pinned diagnostic revalidation timed out")??,
            }
        }
        let payload = if mode == StreamMode::Legacy {
            serde_json::to_vec(&request.request)?
        } else {
            serde_json::to_vec(&request)?
        };
        anyhow::ensure!(
            payload.len() <= 512 * 1024,
            "Pinned diagnostic request exceeds limit"
        );
        let running_config = context.config_used && context.config_observation == "matched_running";
        let policy = request.pinned.as_ref().map(|pin| {
            use astra_model::config::protocol::OutboundConfig;
            let (protocol, encrypted) = match pin.node.as_ref() {
                Some(OutboundConfig::Socks5 { .. }) => ("socks5", Some(false)),
                Some(OutboundConfig::Http { .. }) => ("http_connect", Some(false)),
                Some(_) => ("unsupported_proxy", None),
                None => ("direct", None),
            };
            astra_model::network_analysis::PathPolicy {
                protocol: protocol.into(),
                encrypted,
                certificate_verification: None,
                matched_running: running_config,
                leaf_tag: Some(pin.leaf_tag.clone()),
            }
        });
        if matches!(request.probe, Probe::Nat { .. }) {
            context.interface_source = Some("nat_manual_interface".into());
        }
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            result = output.send(Event::Context(Box::new(context))) => result.context("Diagnostic result receiver closed")?,
        }
        if cancellation_requested(&cancel) {
            return Ok(());
        }
        if let Some(policy) = policy {
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => return Ok(()),
                result = output.send(Event::Policy(policy)) => result.context("Diagnostic result receiver closed")?,
            }
        }
        let mut command = environment.command(&self.executable)?;
        command
            .arg(mode.flag())
            .arg(self.config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = environment
            .spawn(&mut command)
            .context("Start diagnostic core")?;
        run_child(&mut child, payload, output, running_config, cancel).await
    }

    async fn diagnostic_context(
        &mut self,
        request: &Request,
        info: ProgramInfo,
        mode: StreamMode,
    ) -> DiagnosticContext {
        let mut context = DiagnosticContext {
            sampled_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .to_string(),
            running_core: None,
            config_digest: None,
            config_observation: "saved_not_applied".into(),
            actual_egress: request.egress.clone(),
            egress_source: "requested".into(),
            config_used: false,
            selection_chain: vec![],
            effective_interface: match &request.egress {
                Egress::Interface(name) => Some(name.clone()),
                _ => None,
            },
            interface_source: matches!(request.egress, Egress::Interface(_))
                .then(|| "explicit_interface".into()),
            probe_binary: ProbeBinaryIdentity {
                name: info.build.name,
                version: info.build.version,
                compiled_at: info.build.compiled_at,
                target: info.build.target,
                profile: info.build.profile,
                rustc: info.build.rustc,
                git_revision: info.build.git_revision,
                capability_process_id: info.system.process_id,
                executable: info.system.executable,
                capabilities: info
                    .features
                    .into_iter()
                    .filter(|f| f.supported)
                    .map(|f| f.id)
                    .collect(),
            },
            legacy_details: mode == StreamMode::Legacy,
        };
        let Some((socket, pid)) = self.control.clone() else {
            context.config_digest = Some(astra_ipc::control::config_digest(&self.text).to_string());
            return context;
        };
        // Pair the snapshot with identities on both sides. A reload/restart or
        // unavailable Identity cannot turn a stale profile into applied evidence.
        let sampled = async {
            let before = astra_ipc::control::identity(&socket)
                .await
                .context("Read core identity before configuration")?;
            context.running_core = Some(before.clone());
            self.refresh().await?;
            let after = astra_ipc::control::identity(&socket)
                .await
                .context("Read core identity after configuration")?;
            anyhow::ensure!(
                Some(after.process.pid) == pid,
                "Core process changed during context sampling"
            );
            let digest = astra_ipc::control::config_digest(&self.text);
            let matched = before.boot_id == after.boot_id
                && before.process == after.process
                && before.applied == after.applied
                && after.applied.as_ref().is_some_and(|applied| {
                    applied.digest == digest && applied.path == self.config_path
                });
            Ok::<_, anyhow::Error>((after, digest, matched))
        };
        match tokio::time::timeout(Duration::from_secs(5), sampled).await {
            Ok(Ok((identity, digest, matched))) => {
                context.running_core = Some(identity);
                if matched {
                    context.config_digest = Some(digest.to_string());
                    context.config_observation = "matched_running".into();
                } else {
                    context.config_observation =
                        "unconfirmed: core or applied configuration changed during sampling".into();
                }
            }
            Ok(Err(error)) => context.config_observation = format!("unconfirmed: {error:#}"),
            Err(_) => {
                context.config_observation = "unconfirmed: core context sampling timed out".into()
            }
        }
        context
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamMode {
    Detailed,
    Legacy,
}
impl StreamMode {
    fn flag(self) -> &'static str {
        match self {
            Self::Detailed => "--network-diagnostics-stream",
            Self::Legacy => "--nettool-stream",
        }
    }
}

fn stream_mode(info: &ProgramInfo, request: &Request) -> Result<StreamMode> {
    anyhow::ensure!(
        info.build.name == "astra-proxy",
        "Invalid diagnostic executable identity: expected astra-proxy"
    );
    let declarations: Vec<_> = info
        .features
        .iter()
        .filter(|f| f.id == astra_model::control::NETWORK_DIAGNOSTICS_STREAM_CAPABILITY)
        .collect();
    anyhow::ensure!(
        declarations.len() <= 1,
        "Invalid diagnostic capability metadata: duplicate declaration"
    );
    if declarations.first().is_some_and(|f| f.supported) {
        return Ok(StreamMode::Detailed);
    }
    let legacy_probe = match &request.probe {
        Probe::Tcp { .. } => true,
        Probe::Dns { server, .. } => {
            !server.contains('#')
                && server.split_once("://").is_none_or(|(scheme, _)| {
                    matches!(scheme.to_ascii_lowercase().as_str(), "udp" | "tcp")
                })
        }
        _ => false,
    };
    anyhow::ensure!(request.egress == Egress::System && legacy_probe,
        crate::error::message(astra_management::v2::ErrorCode::Unsupported, "Unsupported: this executable does not declare network_diagnostics_stream_v1; only system TCP and UDP/TCP DNS can use its legacy stream, without detailed results"));
    Ok(StreamMode::Legacy)
}

fn require_capability(info: &ProgramInfo, capability: &str) -> Result<()> {
    let declarations: Vec<_> = info
        .features
        .iter()
        .filter(|feature| feature.id == capability)
        .collect();
    anyhow::ensure!(
        declarations.len() <= 1,
        "Invalid duplicate {capability} capability"
    );
    anyhow::ensure!(
        declarations
            .first()
            .is_some_and(|feature| feature.supported),
        crate::error::message(
            astra_management::v2::ErrorCode::Unsupported,
            format!("Unsupported: diagnostic executable does not declare {capability}")
        )
    );
    Ok(())
}

fn requires_resolution(request: &DetailedRequest) -> bool {
    // Older detailed cores echo encrypted DNS service URLs in their events.
    // Require the new capability even with literal/bootstrap addresses so URL
    // path/query credentials cannot enter the durable report through that code.
    if let Probe::Dns { server, .. } = &request.probe {
        if ["https://", "tls://", "dot://"]
            .iter()
            .any(|scheme| server.starts_with(scheme))
        {
            return true;
        }
    }
    if request.resolver.is_some() || matches!(request.egress, Egress::Outbound(_)) {
        return true;
    }
    if !matches!(request.egress, Egress::Interface(_)) {
        return false;
    }
    let literal = |host: &str| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok()
            || host.parse::<std::net::SocketAddr>().is_ok()
    };
    match &request.probe {
        Probe::Tcp { host, .. } => !literal(host),
        Probe::Dns { server, .. } => {
            if server.contains("#bootstrap=") {
                return false;
            }
            let endpoint = server
                .split_once("://")
                .map_or(server.as_str(), |(_, rest)| rest)
                .split('/')
                .next()
                .unwrap_or_default();
            !literal(endpoint)
        }
        _ => true,
    }
}

fn requires_tools(request: &DetailedRequest) -> bool {
    let encrypted = |value: &str| {
        ["https://", "tls://", "dot://"]
            .iter()
            .any(|prefix| value.trim().starts_with(prefix))
    };
    request.family.is_some()
        || !matches!(request.probe, Probe::Tcp { .. } | Probe::Dns { .. })
        || matches!(&request.probe, Probe::Dns { server, .. } if encrypted(server))
        || request
            .resolver
            .as_ref()
            .is_some_and(|resolver| encrypted(resolver))
}

async fn program_info(
    child: &mut Child,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<ProgramInfo>> {
    let result = tokio::select! {
        biased;
        _ = cancelled(cancel) => None,
        result = tokio::time::timeout(Duration::from_secs(5), read_program_info(child)) => Some(result.context("Unable to confirm diagnostic executable capabilities: program-info timed out").and_then(|result| result)),
    };
    match result {
        Some(Ok(info)) => Ok(Some(info)),
        result => {
            terminate(child).await?;
            result.transpose()
        }
    }
}

async fn read_program_info(child: &mut Child) -> Result<ProgramInfo> {
    let stdout = child.stdout.take().context("Missing program-info output")?;
    let stderr = child.stderr.take().context("Missing program-info errors")?;
    let read = async {
        let mut bytes = vec![];
        stdout
            .take(INFO_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .context("Read program-info output")?;
        anyhow::ensure!(
            bytes.len() as u64 <= INFO_BYTES,
            "Invalid program-info output: exceeds 16 KiB"
        );
        Ok::<_, anyhow::Error>(bytes)
    };
    let (bytes, errors, status) = tokio::try_join!(read, drain_errors(stderr), async {
        child.wait().await.context("Wait for program-info process")
    })?;
    anyhow::ensure!(
        !bytes.is_empty() || status.success(),
        "Unable to confirm diagnostic executable capabilities: --program-info failed: {}",
        String::from_utf8_lossy(&errors).trim()
    );
    let info: ProgramInfo =
        serde_json::from_slice(&bytes).context("Invalid program-info protocol output")?;
    anyhow::ensure!(
        status.success(),
        "Diagnostic executable capability query failed: {}",
        String::from_utf8_lossy(&errors).trim()
    );
    Ok(info)
}

async fn drain_errors(mut stderr: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut saved = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let count = stderr
            .read(&mut chunk)
            .await
            .context("Read diagnostic errors")?;
        if count == 0 {
            break;
        }
        let keep = count.min(ERROR_BYTES.saturating_sub(saved.len()));
        saved.extend_from_slice(&chunk[..keep]);
    }
    Ok(saved)
}

async fn run_child(
    child: &mut Child,
    payload: Vec<u8>,
    output: mpsc::Sender<Event>,
    running: bool,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let result = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => None,
        result = communicate(child, payload, &output, running) => Some(result),
    };
    match result {
        Some(Ok(())) => {
            // Done means successful exit, not merely the last stdout record.
            tokio::select! {
                biased;
                _ = cancelled(&mut cancel) => Ok(()),
                result = output.send(Event::Done) => result.context("Diagnostic result receiver closed"),
            }
        }
        result => {
            // On any cancellation or protocol/I/O failure, stop the process and
            // explicitly wait. No producer, reader or reaper task is detached.
            terminate(child).await?;
            result.unwrap_or(Ok(()))
        }
    }
}

pub(super) async fn terminate(child: &mut Child) -> Result<()> {
    terminate_inner(child)
        .await
        .map_err(|error| crate::network::CleanupUnconfirmed(format!("{error:#}")).into())
}

async fn terminate_inner(child: &mut Child) -> Result<()> {
    if child
        .try_wait()
        .context("Inspect diagnostic process")?
        .is_none()
    {
        // start_kill may race a natural exit. In that case a successful wait is
        // still required before the caller can report a terminal outcome.
        if let Err(error) = child.start_kill() {
            if child
                .try_wait()
                .context("Inspect diagnostic process after termination error")?
                .is_none()
            {
                return Err(error)
                    .context("Terminate diagnostic process; cancellation is not confirmed");
            }
        }
    }
    child
        .wait()
        .await
        .context("Reap diagnostic process; cancellation is not confirmed")?;
    Ok(())
}

async fn communicate(
    child: &mut Child,
    payload: Vec<u8>,
    output: &mpsc::Sender<Event>,
    running: bool,
) -> Result<()> {
    let mut input = child.stdin.take().context("Missing diagnostic input")?;
    let stdout = child.stdout.take().context("Missing diagnostic output")?;
    let stderr = child.stderr.take().context("Missing diagnostic errors")?;
    let write = async move {
        input
            .write_all(&payload)
            .await
            .context("Write diagnostic request")?;
        input.shutdown().await.context("Close diagnostic input")?;
        Ok::<_, anyhow::Error>(())
    };
    let read = async {
        let mut reader = BufReader::new(stdout);
        let mut done = false;
        let mut count = 0;
        loop {
            let mut line = String::new();
            let bytes = (&mut reader)
                .take(LINE_BYTES + 1)
                .read_line(&mut line)
                .await
                .context("Read diagnostic output")?;
            if bytes == 0 {
                break;
            }
            anyhow::ensure!(
                bytes as u64 <= LINE_BYTES && line.ends_with('\n'),
                "Diagnostic event exceeds 60 KiB or is incomplete"
            );
            anyhow::ensure!(!done, "Diagnostic core emitted output after completion");
            count += 1;
            anyhow::ensure!(count <= MAX_EVENTS, "Diagnostic event count exceeds limit");
            let mut event: Event =
                serde_json::from_str(&line).context("Invalid diagnostic core event")?;
            anyhow::ensure!(
                !matches!(
                    event,
                    Event::Context(_)
                        | Event::Policy(_)
                        | Event::Project { .. }
                        | Event::ProjectStatus { .. }
                        | Event::Analysis(_)
                ),
                "Diagnostic core cannot supply manager context metadata"
            );
            if let Event::Started { running_config, .. } = &mut event {
                *running_config = running;
            }
            done = matches!(event, Event::Done);
            if !done {
                output
                    .send(event)
                    .await
                    .context("Diagnostic result receiver closed")?;
            }
        }
        Ok::<_, anyhow::Error>(done)
    };
    let errors = drain_errors(stderr);
    // Poll all pipes together: a verbose stderr or large stdin cannot block
    // stdout forever. Dropping this join on cancellation drops only I/O futures.
    let (_, done, errors, status) = tokio::try_join!(write, read, errors, async {
        child.wait().await.context("Wait for diagnostic process")
    })?;
    anyhow::ensure!(
        status.success(),
        "Diagnostic core failed: {}",
        String::from_utf8_lossy(&errors).trim()
    );
    anyhow::ensure!(done, "Diagnostic stream ended before completion");
    Ok(())
}

#[cfg(test)]
#[path = "../test/network_process_test.rs"]
mod tests;
