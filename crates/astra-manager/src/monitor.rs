//! Read-only diagnostics sampled outside the serialized mutation actor.
use astra_management::v2::MonitorSnapshot;
use astra_model::CoreKind;
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub(crate) fn stopped(name: &str) -> MonitorSnapshot {
    MonitorSnapshot {
        instance: name.into(),
        sampled_at_ms: crate::store::now(),
        uptime_secs: "0".into(),
        ..Default::default()
    }
}
fn error(snapshot: &mut MonitorSnapshot, message: impl std::fmt::Display) {
    let previous = snapshot.error.take();
    snapshot.error = Some(match previous {
        Some(old) => format!("{old}; {message}"),
        None => message.to_string(),
    });
}
async fn process(pid: u32) -> anyhow::Result<astra_platform::monitor::ProcessSample> {
    tokio::task::spawn_blocking(move || {
        static SAMPLER: OnceLock<Mutex<astra_platform::monitor::Sampler>> = OnceLock::new();
        SAMPLER
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| std::io::Error::other("Monitor sampler unavailable"))?
            .sample(pid)
    })
    .await?
    .map_err(Into::into)
}
fn apply_process(
    snapshot: &mut MonitorSnapshot,
    sample: anyhow::Result<astra_platform::monitor::ProcessSample>,
) {
    match sample {
        Ok(sample) => {
            snapshot.cpu_percent = sample.cpu_percent;
            snapshot.memory_bytes = Some(sample.memory_bytes.to_string());
            snapshot.virtual_memory_bytes = sample.virtual_memory_bytes.map(|n| n.to_string());
        }
        Err(e) => error(snapshot, format!("Process metrics: {e:#}")),
    }
}
fn network_view(
    value: serde_json::Value,
    pid: u32,
) -> anyhow::Result<astra_model::runtime::Snapshot> {
    let view: astra_model::runtime::RuntimeView = serde_json::from_value(value)?;
    anyhow::ensure!(view.snapshot.pid == pid, "Core PID changed");
    if let Some(telemetry) = view.telemetry {
        anyhow::ensure!(telemetry.coverage == astra_model::runtime::Coverage::Complete,
            "Host network telemetry is {:?}; traffic rates are unavailable until all workers report", telemetry.coverage);
    }
    Ok(view.snapshot)
}
async fn network(
    socket: &Path,
    host: Option<&Path>,
    pid: u32,
) -> anyhow::Result<astra_model::runtime::Snapshot> {
    if let Some(host) = host {
        let value = astra_management::host_control::request(
            host,
            astra_management::host_control::Command::Telemetry,
        )
        .await?;
        network_view(value, pid)
    } else {
        let snapshot =
            astra_ipc::control::runtime(socket, astra_model::runtime::Command::Snapshot).await?;
        anyhow::ensure!(snapshot.pid == pid, "Core PID changed");
        Ok(snapshot)
    }
}
async fn internal(
    socket: &Path,
    host: Option<&Path>,
) -> anyhow::Result<astra_model::monitor::InternalSnapshot> {
    if let Some(host) = host {
        Ok(serde_json::from_value(
            astra_management::host_control::request(
                host,
                astra_management::host_control::Command::Monitor,
            )
            .await?,
        )?)
    } else {
        Ok(astra_ipc::control::monitor(socket).await?)
    }
}
pub(crate) async fn core(
    name: &str,
    kind: CoreKind,
    pid: Option<u32>,
    socket: &Path,
) -> anyhow::Result<MonitorSnapshot> {
    let mut snapshot = stopped(name);
    let Some(pid) = pid else {
        return Ok(snapshot);
    };
    snapshot.pid = Some(pid);
    let host_path = astra_management::host_control::endpoint(socket);
    let host = (kind == CoreKind::Proxy
        && astra_platform::platform::endpoint_path(&host_path).exists())
    .then_some(host_path.as_path());
    let (sample, status, runtime, internal) = tokio::join!(
        process(pid),
        tokio::time::timeout(
            Duration::from_secs(2),
            astra_ipc::control::request(socket, astra_model::control::Command::Status)
        ),
        async {
            if kind != CoreKind::Proxy {
                return Ok(None);
            }
            let result =
                tokio::time::timeout(Duration::from_secs(2), network(socket, host, pid)).await??;
            anyhow::ensure!(result.pid == pid, "Core PID changed");
            Ok::<_, anyhow::Error>(Some((result, crate::store::now())))
        },
        tokio::time::timeout(Duration::from_secs(3), internal(socket, host)),
    );
    apply_process(&mut snapshot, sample);
    match status {
        Ok(Ok(status)) if status.pid == pid => {
            snapshot.uptime_secs = status.uptime_secs.to_string()
        }
        Ok(Ok(_)) => anyhow::bail!("Core PID changed during monitor sample"),
        Ok(Err(e)) => error(&mut snapshot, format!("Core status: {e:#}")),
        Err(_) => error(&mut snapshot, "Core status timed out"),
    }
    match runtime {
        Ok(Some((runtime, sampled_at_ms))) => {
            // Worker diagnostics can take longer than the network observation.
            // Timestamp the byte counters when received, so that diagnostic
            // delays do not distort the frontend's traffic-rate denominator.
            snapshot.sampled_at_ms = sampled_at_ms;
            snapshot.upload_bytes = Some(runtime.upload.to_string());
            snapshot.download_bytes = Some(runtime.download.to_string());
            snapshot.active_connections = u32::try_from(runtime.active).ok();
            snapshot.total_connections = Some(runtime.total.to_string());
        }
        Ok(None) => {}
        Err(e) => error(&mut snapshot, format!("Network metrics: {e:#}")),
    }
    match internal {
        Ok(Ok(value)) => snapshot.internal = Some(value),
        Ok(Err(e)) => error(&mut snapshot, format!("Internal metrics: {e:#}")),
        Err(_) => error(&mut snapshot, "Internal metrics timed out"),
    }
    Ok(snapshot)
}
pub(crate) async fn server(name: &str, socket: &Path) -> anyhow::Result<MonitorSnapshot> {
    let mut snapshot = stopped(name);
    if !astra_platform::platform::endpoint_path(socket).exists() {
        return Ok(snapshot);
    }
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        astra_management::server::call(socket, astra_management::server::Action::Status),
    )
    .await;
    let status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            error(&mut snapshot, format!("Server status: {e:#}"));
            return Ok(snapshot);
        }
        Err(_) => {
            error(&mut snapshot, "Server status timed out");
            return Ok(snapshot);
        }
    };
    snapshot.pid = Some(status.pid);
    snapshot.sampled_at_ms = crate::store::now();
    snapshot.uptime_secs = status.uptime_secs.to_string();
    snapshot.upload_bytes = Some(status.upload.to_string());
    snapshot.download_bytes = Some(status.download.to_string());
    snapshot.active_connections = u32::try_from(status.active).ok();
    snapshot.total_connections = Some(status.accepted.to_string());
    let (sample, internal) = tokio::join!(
        process(status.pid),
        tokio::time::timeout(
            Duration::from_secs(2),
            astra_management::server::call(socket, astra_management::server::Action::Monitor)
        )
    );
    apply_process(&mut snapshot, sample);
    match internal {
        Ok(Ok(value)) if value.pid == status.pid => match serde_json::from_str(&value.logs) {
            Ok(internal) => snapshot.internal = Some(internal),
            Err(e) => error(&mut snapshot, format!("Internal metrics: {e}")),
        },
        Ok(Ok(_)) => anyhow::bail!("Server PID changed during monitor sample"),
        Ok(Err(e)) => error(&mut snapshot, format!("Internal metrics: {e:#}")),
        Err(_) => error(&mut snapshot, "Internal metrics timed out"),
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn monitor_combines_live_process_internal_and_network_observations() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("monitor.sock");
        let control = astra_ipc::control::Control::bind(Some(socket.clone())).unwrap();
        let task = tokio::spawn(async move {
            control
                .run_with_runtime(
                    |_| async { Ok(()) },
                    || async { Err("no config fixture".into()) },
                    |_| async { Err("no route fixture".into()) },
                    || async { Ok(()) },
                    |_| async {
                        Ok(astra_model::runtime::Snapshot {
                            pid: std::process::id(),
                            upload: 9_007_199_254_740_993,
                            download: 8192,
                            active: 2,
                            total: 3,
                            ..Default::default()
                        })
                    },
                )
                .await
                .unwrap();
        });
        let sample = core("live", CoreKind::Proxy, Some(std::process::id()), &socket)
            .await
            .unwrap();
        assert!(sample.error.is_none(), "{:?}", sample.error);
        assert_eq!(sample.pid, Some(std::process::id()));
        assert!(
            sample
                .memory_bytes
                .as_ref()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
        assert_eq!(sample.upload_bytes.as_deref(), Some("9007199254740993"));
        assert_eq!(sample.active_connections, Some(2));
        assert!(sample
            .internal
            .unwrap()
            .metrics
            .iter()
            .any(|m| m.name == "runtime.control_jobs"));
        astra_ipc::control::request(&socket, astra_model::control::Command::Stop)
            .await
            .unwrap();
        task.await.unwrap();
    }
    #[test]
    fn monitor_rejects_partial_worker_totals_instead_of_showing_false_rates() {
        let mut view = astra_model::runtime::RuntimeView {
            snapshot: astra_model::runtime::Snapshot {
                pid: 42,
                upload: 9_007_199_254_740_993,
                ..Default::default()
            },
            telemetry: Some(astra_model::runtime::RuntimeTelemetry {
                coverage: astra_model::runtime::Coverage::Partial,
                ..Default::default()
            }),
        };
        assert!(network_view(serde_json::to_value(&view).unwrap(), 42).is_err());
        view.telemetry.as_mut().unwrap().coverage = astra_model::runtime::Coverage::Complete;
        assert_eq!(
            network_view(serde_json::to_value(&view).unwrap(), 42)
                .unwrap()
                .upload,
            9_007_199_254_740_993
        );
        assert!(network_view(serde_json::to_value(&view).unwrap(), 43).is_err());
    }
    #[tokio::test]
    async fn monitor_stopped_core_has_unknown_metrics_without_opening_endpoint() {
        let sample = core(
            "stopped",
            CoreKind::Rule,
            None,
            Path::new("nonexistent-monitor.sock"),
        )
        .await
        .unwrap();
        assert_eq!(sample.pid, None);
        assert_eq!(sample.cpu_percent, None);
        assert_eq!(sample.memory_bytes, None);
        assert_eq!(sample.upload_bytes, None);
        assert_eq!(sample.error, None);
        assert_eq!(sample.uptime_secs, "0");
    }
    #[tokio::test]
    async fn monitor_stopped_server_has_unknown_metrics() {
        let root = tempfile::tempdir().unwrap();
        let sample = server("server", &root.path().join("missing.sock"))
            .await
            .unwrap();
        assert_eq!(sample.pid, None);
        assert_eq!(sample.upload_bytes, None);
        assert_eq!(sample.error, None);
    }
}
