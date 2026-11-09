//! Shared asynchronous diagnostic API. Dropping an observer never cancels its task.
use anyhow::{ensure, Result};
use astra_management::{v2 as v, ManagerClient};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

mod eligibility;
pub(crate) use eligibility::{encrypted_dns, validate_legacy, LEGACY_NOTICE};

/// Keep the server's structured error available to callers. Transport and
/// protocol failures deliberately remain distinct from a rejected submission.
#[derive(Debug)]
pub struct ApiFailure(pub v::ApiError);

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}: {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for ApiFailure {}

pub(crate) fn submission_rejected(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ApiFailure>().is_some_and(|error| {
        matches!(
            error.0.code,
            v::ErrorCode::Unsupported | v::ErrorCode::InvalidRequest | v::ErrorCode::Conflict
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available {
        epoch: String,
        capabilities: Vec<String>,
    },
    /// Keep offering the legacy NetTool interface on this Agent.
    Legacy { epoch: String },
}

/// The target is captured when the page starts observing a task. Switching the
/// UI to another Agent must not redirect an outstanding report or cancellation.
#[derive(Debug, Clone)]
pub struct Client {
    manager: ManagerClient,
    agent_id: Option<String>,
}

impl Client {
    pub fn new(socket: &Path) -> Self {
        let (endpoint, agent_id) = crate::protocol::target().unwrap_or_else(|| {
            (
                if socket.file_name().is_some_and(|n| n == "manager.sock") {
                    socket.with_file_name("management.sock")
                } else {
                    socket.into()
                },
                None,
            )
        });
        Self::for_target(endpoint, agent_id)
    }

    pub fn for_target(endpoint: PathBuf, agent_id: Option<String>) -> Self {
        Self {
            manager: ManagerClient::new(endpoint),
            agent_id,
        }
    }

    pub(crate) async fn request(&self, body: v::RequestBody) -> Result<v::ResponseBody> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut request = v::Request::new(
            format!(
                "network-{}-{}-{}",
                std::process::id(),
                astra_ipc::progress::now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            body,
        );
        request.agent_id = self.agent_id.clone();
        self.manager
            .request_v2(request)
            .await?
            .result
            .map_err(|error| ApiFailure(error).into())
    }

    /// Check the selected Agent, not the Controller's own capabilities. Do not
    /// treat malformed replies or transport errors as a legacy Agent.
    pub async fn availability(&self) -> Result<Availability> {
        match self.request(v::RequestBody::Query(v::Query::Hello)).await? {
            v::ResponseBody::Hello(hello) => {
                ensure!(
                    hello.version == v::VERSION,
                    "Protocol error: unsupported Hello version"
                );
                ensure!(
                    hello.role == "agent",
                    "Select an Agent before running diagnostics"
                );
                Ok(
                    if hello
                        .capabilities
                        .iter()
                        .any(|c| c == v::NETWORK_DIAGNOSTICS_CAPABILITY)
                    {
                        Availability::Available {
                            epoch: hello.epoch,
                            capabilities: hello.capabilities,
                        }
                    } else {
                        Availability::Legacy { epoch: hello.epoch }
                    },
                )
            }
            _ => anyhow::bail!("Protocol error: expected Agent Hello"),
        }
    }

    /// The caller retains operation_id before sending. On a lost response,
    /// query that ID; a retry must use the same ID and the same request.
    /// A Running Task is an acceptance receipt, not a completed diagnostic.
    pub async fn submit(
        &self,
        operation_id: String,
        instance: String,
        request: impl Into<v::NetworkDiagnosticsRequest>,
    ) -> Result<v::Task> {
        let Availability::Available { capabilities, .. } = self.availability().await? else {
            anyhow::bail!("Unsupported: this Agent supports legacy network tools only");
        };
        let request = request.into();
        validate_capabilities(&capabilities, &request)?;
        let response = self
            .request(v::RequestBody::Execute {
                operation_id: operation_id.clone(),
                command: v::Command::NetworkDiagnostics { instance, request },
            })
            .await?;
        task_response(response, &operation_id)
    }

    pub async fn submit_batch(
        &self,
        operation_id: String,
        instance: String,
        request: v::NetworkBatchRequest,
    ) -> Result<v::Task> {
        let Availability::Available { capabilities, .. } = self.availability().await? else {
            anyhow::bail!("Unsupported: this Agent supports legacy network tools only");
        };
        validate_batch_capabilities(&capabilities, &request)?;
        let response = self
            .request(v::RequestBody::Execute {
                operation_id: operation_id.clone(),
                command: v::Command::NetworkDiagnosticsBatch { instance, request },
            })
            .await?;
        task_response(response, &operation_id)
    }

    pub async fn task(&self, operation_id: &str) -> Result<v::Task> {
        let response = self
            .request(v::RequestBody::Query(v::Query::Task {
                operation_id: operation_id.into(),
            }))
            .await?;
        task_response(response, operation_id)
    }

    /// Cursor reads are incremental. An empty page while Running is not EOF;
    /// retain its cursor and poll again. Expired reports retain the outcome.
    pub async fn report(
        &self,
        operation_id: &str,
        cursor: Option<String>,
    ) -> Result<v::NetworkReport> {
        let previous = cursor.clone();
        let response = self
            .request(v::RequestBody::Query(v::Query::NetworkReport {
                operation_id: operation_id.into(),
                cursor,
            }))
            .await?;
        let report = report_response(response, operation_id)?;
        validate_page(&report, previous.as_deref())?;
        Ok(report)
    }

    /// Returns only after the Agent confirms a terminal report, including child
    /// process reaping. The final state may be Complete if completion won the race.
    /// A transport error leaves the outcome unknown: query report to reconnect.
    pub async fn cancel(
        &self,
        operation_id: &str,
        cancellation_id: String,
    ) -> Result<v::NetworkReport> {
        ensure!(
            cancellation_id != operation_id,
            "Cancellation needs its own operation ID"
        );
        let response = self
            .request(v::RequestBody::Execute {
                operation_id: cancellation_id,
                command: v::Command::CancelTask {
                    operation_id: operation_id.into(),
                },
            })
            .await?;
        let report = match response {
            v::ResponseBody::Cancellation(v::Cancellation::AlreadyFinished(_)) => {
                self.report(operation_id, None).await?
            }
            v::ResponseBody::Cancellation(v::Cancellation::UnsafeToCancel(message)) => {
                anyhow::bail!("Cancellation was not confirmed: {message}")
            }
            other => report_response(other, operation_id)?,
        };
        ensure!(
            report.state.terminal(),
            "Protocol error: cancellation returned before diagnostic termination"
        );
        validate_page(&report, None)?;
        Ok(report)
    }
}

/// Reject malformed pagination before any UI can retain duplicate evidence or
/// spin through an unbounded sequence of non-advancing pages.
pub(crate) fn validate_page(report: &v::NetworkReport, previous: Option<&str>) -> Result<()> {
    let before = previous
        .unwrap_or("0")
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("Protocol error: invalid prior diagnostic cursor"))?;
    let after = report
        .cursor
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("Protocol error: invalid diagnostic page cursor"))?;
    ensure!(
        after >= before && (!report.has_more || after > before),
        "Protocol error: diagnostic pagination did not advance"
    );
    ensure!(
        serde_json::to_vec(report)?.len() <= v::NETWORK_PAGE_BYTES,
        "Protocol error: diagnostic page exceeds 64 KiB"
    );
    let mut last = before;
    for item in &report.items {
        let index = item
            .id
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("Protocol error: invalid diagnostic item identity"))?;
        ensure!(
            index > last && index <= after,
            "Protocol error: duplicated or out-of-order diagnostic result"
        );
        ensure!(
            item.data.is_object() && item.data["event"].is_string(),
            "Protocol error: invalid diagnostic event"
        );
        last = index;
    }
    ensure!(
        report.items.is_empty() || last == after,
        "Protocol error: diagnostic cursor does not match the last result"
    );
    ensure!(
        !report.has_more || !report.items.is_empty(),
        "Protocol error: empty diagnostic page claims more data"
    );
    ensure!(
        !report.items.is_empty() || report.expired || after == before,
        "Protocol error: diagnostic page skipped evidence without expiry"
    );
    ensure!(
        !report.expired || (report.state.terminal() && report.items.is_empty() && !report.has_more),
        "Protocol error: expired diagnostic report is not a terminal empty page"
    );
    Ok(())
}

/// Older Agents can ignore unknown request fields. Reject new options before
/// submission instead of accidentally losing the caller's resolution constraint.
pub(crate) fn validate_capabilities(
    capabilities: &[String],
    request: &v::NetworkDiagnosticsRequest,
) -> Result<()> {
    use astra_model::nettool::{Egress, Probe};
    let has = |cap: &str| capabilities.iter().any(|item| item == cap);
    if matches!(request.egress, Egress::Outbound(_)) {
        ensure!(
            has("network_diagnostics_pinned_v1"),
            "Unsupported: Agent 未声明固定出口能力；未提交探测"
        );
    }
    let encrypted_dns = encrypted_dns(&request.probe);
    let new_tool = !matches!(request.probe, Probe::Tcp { .. } | Probe::Dns { .. });
    let encrypted_resolver = request
        .resolver
        .as_deref()
        .is_some_and(eligibility::encrypted_endpoint);
    if new_tool || request.family.is_some() || encrypted_dns || encrypted_resolver {
        ensure!(
            has("network_diagnostics_tools_v1"),
            "Unsupported: Agent 未声明新版网络工具与系统根证书能力；未提交探测"
        );
    }
    if matches!(request.probe, Probe::Nat { .. }) {
        ensure!(
            has("network_diagnostics_nat_v1"),
            "Unsupported: Agent 未声明物理网卡 NAT 能力"
        );
    }
    if matches!(request.probe, Probe::Trace { .. } | Probe::Icmp { .. }) {
        ensure!(
            has("network_diagnostics_path_v1"),
            "Unsupported: Agent 未声明 TUN 感知路径能力"
        );
    }
    if eligibility::needs_resolution(request) {
        ensure!(
            has("network_diagnostics_resolution_v1"),
            "Unsupported: Agent 未声明约束解析能力；未提交探测"
        );
    }
    Ok(())
}

fn task_response(response: v::ResponseBody, operation_id: &str) -> Result<v::Task> {
    match response {
        v::ResponseBody::Task(task) => {
            ensure!(
                task.id == operation_id,
                "Protocol error: task identity mismatch"
            );
            Ok(task)
        }
        _ => anyhow::bail!("Protocol error: expected diagnostic task receipt"),
    }
}

fn report_response(response: v::ResponseBody, operation_id: &str) -> Result<v::NetworkReport> {
    match response {
        v::ResponseBody::NetworkReport(report) => {
            ensure!(
                report.operation_id == operation_id,
                "Protocol error: report identity mismatch"
            );
            ensure!(
                report.schema_version == 1,
                "Protocol error: unsupported diagnostic report schema"
            );
            Ok(report)
        }
        _ => anyhow::bail!("Protocol error: expected diagnostic report"),
    }
}

#[cfg(test)]
#[path = "test/network_diagnostics_test.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "test/network_live_test.rs"]
mod live_tests;

pub(crate) fn validate_batch_capabilities(
    capabilities: &[String],
    request: &v::NetworkBatchRequest,
) -> Result<()> {
    ensure!(
        capabilities
            .iter()
            .any(|capability| capability == v::NETWORK_BATCH_CAPABILITY),
        "Unsupported: Agent 未声明批量诊断能力；未提交探测"
    );
    ensure!(
        !request.projects.is_empty() && request.projects.len() <= 32,
        "请选择 1–32 个诊断项目"
    );
    for project in &request.projects {
        validate_capabilities(capabilities, &project.request)?;
    }
    Ok(())
}
