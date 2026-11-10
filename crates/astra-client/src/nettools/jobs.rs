use super::*;
use crate::protocol;
use astra_management::v2 as v;
use astra_model::nettool::Event;
use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Default)]
pub struct Context {
    pub network: Option<Value>,
    pub runtime: Option<Value>,
    pub core: Option<Value>,
    pub capabilities: Vec<String>,
    pub notice: Option<String>,
}
pub(super) enum Update {
    Accepted(String),
    Rejected,
    Report(v::NetworkReport),
    Notice(String),
    Error(String),
    FakeIp(v::FakeIpLookupReply),
    Finished,
}
fn operation_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    format!(
        "network-{}-{}-{}",
        std::process::id(),
        astra_ipc::progress::now_ms(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
async fn send(
    sender: &mpsc::Sender<Update>,
    waker: &crate::waker::Waker,
    update: Update,
) -> anyhow::Result<()> {
    sender.send(update).await?;
    waker.wake();
    Ok(())
}

pub(super) async fn run(
    client: crate::network_diagnostics::Client,
    instance: String,
    request: v::NetworkDiagnosticsRequest,
    sender: mpsc::Sender<Update>,
    mut cancel: watch::Receiver<bool>,
    waker: crate::waker::Waker,
) {
    let result = async {
        let available = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(8), client.availability()) => result??,
            _ = async { let _ = cancel.wait_for(|stop| *stop).await; } => return Ok(()),
        };
        if *cancel.borrow() { return Ok(()); }
        let id = operation_id();
        let crate::network_diagnostics::Availability::Available { capabilities, .. } = &available else {
            anyhow::bail!("{}", crate::network_diagnostics::LEGACY_NOTICE);
        };
        crate::network_diagnostics::validate_capabilities(capabilities, &request)?;
        // Publish identity before the request: lost acceptance replies remain queryable.
        send(&sender, &waker, Update::Accepted(id.clone())).await?;
        let submission = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(8), client.request(v::RequestBody::Execute { operation_id: id.clone(), command: v::Command::NetworkDiagnostics { instance, request } })) => Some(result.unwrap_or_else(|_| Err(anyhow::anyhow!("outcome unknown: diagnostic acceptance timed out")))),
            _ = async { let _ = cancel.wait_for(|stop| *stop).await; } => None,
        };
        if let Some(Ok(response)) = &submission {
            anyhow::ensure!(matches!(response, v::ResponseBody::Task(task) if task.id == id), "Protocol error: task acceptance identity mismatch");
        }
        if let Some(Err(error)) = submission {
            let message = format!("{error:#}");
            if crate::network_diagnostics::submission_rejected(&error) {
                send(&sender, &waker, Update::Rejected).await?;
                return Err(error);
            }
            send(&sender, &waker, Update::Error(format!("接受回执未确认，正在查询原任务：{message}"))).await?;
        }
        observe(&client, &id, &sender, cancel, &waker).await?;
        Ok::<_, anyhow::Error>(())
    }.await;
    if let Err(error) = result {
        let _ = send(&sender, &waker, Update::Error(format!("{error:#}"))).await;
    }
    let _ = send(&sender, &waker, Update::Finished).await;
}

async fn observe(
    client: &crate::network_diagnostics::Client,
    id: &str,
    sender: &mpsc::Sender<Update>,
    mut cancel: watch::Receiver<bool>,
    waker: &crate::waker::Waker,
) -> anyhow::Result<()> {
    let cancellation_id = operation_id();
    let mut cursor = None;
    let mut cancel_confirmed = false;
    let mut retained_bytes = 0_usize;
    loop {
        let page = tokio::time::timeout(Duration::from_secs(8), client.report(id, cursor.clone()))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("report query timed out")));
        match page {
            Ok(report) => {
                for item in &report.items {
                    retained_bytes = retained_bytes.saturating_add(serde_json::to_vec(item)?.len());
                }
                anyhow::ensure!(
                    retained_bytes <= v::NETWORK_REPORT_BYTES,
                    "报告超过 2 MiB；已停止观察并保留原任务标识，没有重新执行或取消任务"
                );
                cursor = Some(report.cursor.clone());
                let more = report.has_more;
                let done = report.state.terminal() && !more;
                send(sender, waker, Update::Report(report)).await?;
                if done {
                    break;
                }
                // Never race cancellation ahead of admission: first establish that the original job exists.
                if *cancel.borrow() && !cancel_confirmed {
                    match tokio::time::timeout(
                        Duration::from_secs(8),
                        client.cancel(id, cancellation_id.clone()),
                    )
                    .await
                    {
                        Ok(Ok(_)) => cancel_confirmed = true,
                        result => {
                            send(
                                sender,
                                waker,
                                Update::Error(format!("取消尚未确认，保留任务并重查：{result:?}")),
                            )
                            .await?
                        }
                    }
                }
                if more {
                    continue;
                }
            }
            Err(error) => {
                send(
                    sender,
                    waker,
                    Update::Error(format!("报告暂不可读；任务标识保留，可重连查询：{error:#}")),
                )
                .await?
            }
        }
        if *cancel.borrow() {
            tokio::time::sleep(Duration::from_millis(750)).await;
        } else {
            tokio::select! { _ = tokio::time::sleep(Duration::from_millis(500)) => {}, _ = async { let _ = cancel.wait_for(|stop| *stop).await; } => {} }
        }
    }
    Ok(())
}

impl View {
    pub fn export_json(&self) -> anyhow::Result<String> {
        let result = serde_json::json!({
            "schema_version":1,
            "instance":self.name,
            "operation_id":self.operation_id,
            "state":self.report_state,
            "observation_active":self.running(),
            "path_stale":self.path_stale,
            "live_chain":self.live_chain(),
            "details":self.details,
            "attempts":self.attempts,
            "summary":self.summary,
            "notice":self.note,
            "error":self.error,
        });
        let text = serde_json::to_string(&result)?;
        anyhow::ensure!(
            text.len() <= 2 * 1024 * 1024,
            "导出超过 2 MiB；请按任务分页读取原报告"
        );
        Ok(text)
    }

    pub fn copy_result(&mut self, json: bool) {
        let content = if json {
            self.export_json()
        } else {
            let text = self.output().join("\n");
            if text.len() > 2 * 1024 * 1024 {
                Err(anyhow::anyhow!("复制结果超过 2 MiB"))
            } else {
                Ok(text)
            }
        };
        match content.and_then(|text| crate::clipboard::copy(&text)) {
            Ok(()) => {
                self.note = Some(
                    if json {
                        "JSON 已复制（最多 2 MiB）"
                    } else {
                        "结果已复制"
                    }
                    .into(),
                )
            }
            Err(error) => self.error = Some(format!("复制失败：{error:#}")),
        }
    }

    pub(super) fn poll_jobs(&mut self) -> bool {
        let mut changed = false;
        if self.context_receiver.is_none() && Instant::now() >= self.next_refresh {
            let (sender, receiver) = mpsc::channel(1);
            let client = self.client.clone();
            let instance = self.name.clone();
            let waker = self.waker.clone();
            self.refresh = Some(tokio::spawn(async move {
                let mut context = Context::default();
                let result = tokio::time::timeout(Duration::from_secs(8), async {
                    let v::ResponseBody::Hello(hello) = client
                        .request(v::RequestBody::Query(v::Query::Hello))
                        .await?
                    else {
                        anyhow::bail!("Protocol error: expected Agent Hello");
                    };
                    anyhow::ensure!(hello.role == "agent", "Select a target Agent");
                    context.capabilities = hello.capabilities;
                    if context.capabilities.iter().any(|c| c == "network_state_v1") {
                        match client
                            .request(v::RequestBody::Query(v::Query::NetworkState))
                            .await?
                        {
                            v::ResponseBody::NetworkState(snapshot) => {
                                context.network = Some(serde_json::to_value(snapshot)?)
                            }
                            _ => {
                                anyhow::bail!("Protocol error: expected target Agent network state")
                            }
                        }
                    } else {
                        context.notice =
                            Some("此 Agent 未提供网络状态；不使用客户端本机网卡代替".into());
                    }
                    match client
                        .request(v::RequestBody::Query(v::Query::Runtime {
                            instance: instance.clone(),
                        }))
                        .await
                    {
                        Ok(v::ResponseBody::Runtime(v::RuntimeSnapshot::Client(runtime))) => {
                            context.runtime = Some(runtime)
                        }
                        Ok(_) => context.notice = Some("实例未提供代理运行快照".into()),
                        Err(error) => context.notice = Some(format!("运行快照不可读：{error:#}")),
                    }
                    if context
                        .capabilities
                        .iter()
                        .any(|cap| cap == "core_network_v1")
                    {
                        match client
                            .request(v::RequestBody::Query(v::Query::NetworkCore { instance }))
                            .await
                        {
                            Ok(v::ResponseBody::NetworkCore(core)) => {
                                context.core = Some(serde_json::to_value(core)?)
                            }
                            Ok(_) => {
                                context.notice = Some("协议错误：核心能力响应类型不匹配".into())
                            }
                            Err(error) => {
                                context.notice = Some(format!("核心能力读取失败：{error:#}"))
                            }
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                match result {
                    Ok(Ok(())) => {
                        if !context
                            .capabilities
                            .iter()
                            .any(|cap| cap == v::NETWORK_DIAGNOSTICS_CAPABILITY)
                        {
                            let notice = crate::network_diagnostics::LEGACY_NOTICE;
                            context.notice = Some(match context.notice.take() {
                                Some(existing) => format!("{notice}；{existing}"),
                                None => notice.into(),
                            });
                        }
                    }
                    Ok(Err(error)) => context.notice = Some(format!("网络状态读取失败：{error:#}")),
                    Err(_) => context.notice = Some("网络状态读取超时".into()),
                }
                let _ = sender.send(context).await;
                waker.wake();
            }));
            self.context_receiver = Some(receiver);
        }
        if let Some(receiver) = &mut self.context_receiver {
            if let Ok(context) = receiver.try_recv() {
                let selected = self.egresses[self.egress].clone();
                let comparison = self.comparison.map(|i| self.egresses[i].clone());
                self.egresses.retain(|e| !matches!(e, Egress::Interface(_)));
                if let Some(interfaces) = context
                    .network
                    .as_ref()
                    .and_then(|s| s["interfaces"].as_array())
                {
                    for interface in interfaces {
                        if interface["connected"] == true && interface["kind"] != "loopback" {
                            if let Some(id) = interface["id"].as_str() {
                                self.egresses.push(Egress::Interface(id.into()));
                            }
                        }
                    }
                }
                if matches!(selected, Egress::Interface(_)) && !self.egresses.contains(&selected) {
                    self.egresses.push(selected.clone());
                }
                self.egress = self
                    .egresses
                    .iter()
                    .position(|e| *e == selected)
                    .unwrap_or(0);
                self.comparison =
                    comparison.and_then(|c| self.egresses.iter().position(|e| *e == c));
                if self.running()
                    && (self.captured_runtime.as_ref().map(runtime_selection)
                        != context.runtime.as_ref().map(runtime_selection)
                        || self.captured_core.as_ref().map(core_identity)
                            != context.core.as_ref().map(core_identity))
                {
                    self.path_stale = true;
                }
                self.context = context;
                self.context_receiver = None;
                self.refresh = None;
                self.next_refresh = Instant::now() + Duration::from_secs(5);
                changed = true;
            }
        }
        let Some(mut updates) = self.updates.take() else {
            return changed;
        };
        while let Ok(update) = updates.try_recv() {
            changed = true;
            match update {
                Update::Accepted(id) => self.operation_id = Some(id),
                Update::Rejected => self.operation_id = None,
                Update::Notice(note) => self.note = Some(note),
                Update::Error(error) => self.error = Some(error),
                Update::FakeIp(reply) => self
                    .details
                    .push(serde_json::json!({"event":"fake_ip", "reply":reply})),
                Update::Report(report) => {
                    self.report_state = Some(report.state);
                    self.error = None;
                    if report.truncated || report.expired {
                        self.error = Some(format!(
                            "报告{}{}；不能视为完整数据",
                            if report.truncated { "已截断" } else { "" },
                            if report.expired { "已过期" } else { "" }
                        ));
                    }
                    if report.state.terminal() {
                        self.note = Some(report.message);
                    }
                    for item in report.items {
                        self.apply_event(item.data);
                    }
                }
                Update::Finished => {
                    self.task = None;
                    self.cancel = None;
                    return true;
                }
            }
        }
        if updates.is_closed() {
            self.task = None;
            self.cancel = None;
        } else {
            self.updates = Some(updates);
        }
        changed
    }
    fn apply_event(&mut self, event: Value) {
        match event["event"].as_str() {
            Some("started") => self.note = event["note"].as_str().map(str::to_owned),
            Some("attempt") => {
                if let Ok(Event::Attempt(attempt)) =
                    serde_json::from_value(protocol::native_numbers(event, None))
                {
                    self.attempts.push(attempt);
                }
            }
            Some("summary") => {
                if let Ok(Event::Summary(summary)) =
                    serde_json::from_value(protocol::native_numbers(event, None))
                {
                    self.summary = Some(summary);
                }
            }
            Some("done") => {}
            _ => self.details.push(event),
        }
    }
}
fn runtime_selection(value: &Value) -> Value {
    serde_json::json!({ "pid": value["pid"], "mode": value["mode"], "tun": value["tun"], "groups": value["groups"].as_array().map(|groups| groups.iter().map(|g| serde_json::json!({"name":g["name"],"selected":g["selected"]})).collect::<Vec<_>>()) })
}

fn core_identity(value: &Value) -> Value {
    serde_json::json!({ "boot_id":value["identity"]["boot_id"], "digest":value["identity"]["applied_digest"] })
}

/// Same durable identity and observation lifecycle as a single tool. A lost
/// acceptance response never causes a second batch to be submitted.
async fn run_batch(
    client: crate::network_diagnostics::Client,
    instance: String,
    request: v::NetworkBatchRequest,
    sender: mpsc::Sender<Update>,
    mut cancel: watch::Receiver<bool>,
    waker: crate::waker::Waker,
) {
    let result = async {
        let available = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(8), client.availability()) => result??,
            _ = async { let _ = cancel.wait_for(|stop| *stop).await; } => return Ok(()),
        };
        let crate::network_diagnostics::Availability::Available { capabilities, .. } = available else { anyhow::bail!("{}", crate::network_diagnostics::LEGACY_NOTICE); };
        crate::network_diagnostics::validate_batch_capabilities(&capabilities, &request)?;
        if *cancel.borrow() { return Ok(()); }
        let id = operation_id();
        send(&sender, &waker, Update::Accepted(id.clone())).await?;
        let submission = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(8), client.request(v::RequestBody::Execute { operation_id: id.clone(), command: v::Command::NetworkDiagnosticsBatch { instance, request } })) => Some(result.unwrap_or_else(|_| Err(anyhow::anyhow!("outcome unknown: batch acceptance timed out")))),
            _ = async { let _ = cancel.wait_for(|stop| *stop).await; } => None,
        };
        if let Some(Ok(response)) = &submission {
            anyhow::ensure!(matches!(response, v::ResponseBody::Task(task) if task.id == id), "Protocol error: batch acceptance identity mismatch");
        }
        if let Some(Err(error)) = submission {
            let message = format!("{error:#}");
            if crate::network_diagnostics::submission_rejected(&error) {
                send(&sender, &waker, Update::Rejected).await?;
                return Err(error);
            }
            send(&sender, &waker, Update::Error(format!("接受回执未确认，正在查询原批次：{message}"))).await?;
        }
        observe(&client, &id, &sender, cancel, &waker).await
    }.await;
    if let Err(error) = result {
        let _ = send(&sender, &waker, Update::Error(format!("{error:#}"))).await;
    }
    let _ = send(&sender, &waker, Update::Finished).await;
}
impl View {
    pub fn start_batch(&mut self, request: v::NetworkBatchRequest) {
        if self.running() {
            return;
        }
        if let Err(error) = crate::network_diagnostics::validate_batch_capabilities(
            &self.context.capabilities,
            &request,
        ) {
            self.error = Some(error.to_string());
            return;
        }
        self.reset_result();
        self.note = Some("完整检查：最多并发 4 项，总时限 180 秒；NAT 先独占执行".into());
        // A NAT selection is never carried over to another run.
        if request
            .projects
            .iter()
            .any(|p| matches!(p.request.probe, Probe::Nat { .. }))
        {
            self.nat_interface = None;
        }
        let (sender, receiver) = mpsc::channel(32);
        let (cancel, control) = watch::channel(false);
        let client = self.client.clone();
        let instance = self.name.clone();
        let waker = self.waker.clone();
        self.task = Some(tokio::spawn(run_batch(
            client, instance, request, sender, control, waker,
        )));
        self.cancel = Some(cancel);
        self.updates = Some(receiver);
    }
}
