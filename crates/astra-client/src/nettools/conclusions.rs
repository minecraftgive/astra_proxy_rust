//! Concise presentation of reported outcomes, without inventing diagnostic findings.
use super::*;
use astra_management::v2::NetworkState;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputTone {
    Success,
    Warning,
    Error,
    Info,
    Muted,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputLine {
    pub text: String,
    pub tone: OutputTone,
}
impl OutputLine {
    fn new(tone: OutputTone, text: impl Into<String>) -> Self {
        Self {
            text: clean(&text.into()),
            tone,
        }
    }
}
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => clean(s),
        Value::Null => "未知".into(),
        _ => clean(&v.to_string()),
    }
}
fn number(v: &Value) -> u64 {
    v.as_u64().or_else(|| v.as_str()?.parse().ok()).unwrap_or(0)
}
fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

impl View {
    pub fn toggle_advanced(&mut self) {
        self.show_advanced = !self.show_advanced;
        self.scroll = 0;
    }
    pub fn output_rows(&self) -> Vec<OutputLine> {
        let mut rows = self.conclusion_rows();
        if self.show_advanced {
            rows.push(OutputLine::new(OutputTone::Info, "── 原始报告 ──"));
            rows.extend(self.report_rows());
        }
        rows
    }
    /// The left result column is independent of the report visibility switch.
    pub fn conclusion_rows(&self) -> Vec<OutputLine> {
        use OutputTone::*;
        let mut rows = Vec::new();
        if let Some(notice) = &self.context.notice {
            rows.push(OutputLine::new(Warning, notice));
        }
        if self.path_stale {
            rows.push(OutputLine::new(
                Warning,
                "注意：实例选择已变化，本次结果仍对应启动时的出口。",
            ));
        }
        match self.section {
            Section::Overview => {
                if let Some(state) = &self.context.network {
                    let connected = list(&state["interfaces"])
                        .filter(|i| i["connected"] == true)
                        .count();
                    rows.push(OutputLine::new(
                        Info,
                        format!("目标设备网络：{connected} 个已连接网卡"),
                    ));
                    if state["truncated"] == true {
                        rows.push(OutputLine::new(
                            Warning,
                            "网络信息不完整，展开高级信息查看已取得的记录。",
                        ));
                    }
                    for issue in list(&state["issues"]) {
                        rows.push(OutputLine::new(
                            Warning,
                            format!("网络信息提示：{}", text(&issue["message"])),
                        ));
                    }
                } else {
                    rows.push(OutputLine::new(Info, "正在读取目标设备网络状态…"));
                }
                if let Some(runtime) = &self.context.runtime {
                    rows.push(OutputLine::new(
                        Info,
                        format!(
                            "实例状态：模式 {} · 系统代理 {} · TUN {}",
                            text(&runtime["mode"]),
                            text(&runtime["system_proxy"]),
                            text(&runtime["tun"])
                        ),
                    ));
                }
                if let Some(core) = &self.context.core {
                    rows.push(OutputLine::new(
                        if core["status"] == "ok" {
                            Info
                        } else {
                            Warning
                        },
                        if core["status"] == "ok" {
                            "核心能力已确认"
                        } else {
                            "核心能力尚未确认；可用工具受限"
                        },
                    ));
                }
            }
            Section::Paths => {
                rows.push(OutputLine::new(
                    Info,
                    format!(
                        "本次探测出口：{}",
                        self.probe_path.as_deref().unwrap_or("尚未运行")
                    ),
                ));
                rows.push(OutputLine::new(
                    Info,
                    "展开高级信息查看 DNS、连接和逐跳路径；未测得的设备不会推断补齐。",
                ));
            }
            Section::Findings => rows.extend(
                self.plan_lines()
                    .into_iter()
                    .map(|s| OutputLine::new(Info, s)),
            ),
            Section::Tools => {
                if self.kind == Kind::Nat {
                    rows.push(OutputLine::new(
                        Info,
                        "NAT 将使用手选的物理网卡，测试服务会看到该网络的公网出口。",
                    ));
                    if self.nat_advanced && self.details.is_empty() {
                        rows.push(OutputLine::new(
                            Info,
                            "类型识别需要支持 RFC 5780 的服务；默认公共服务可能仅返回出口映射。",
                        ));
                    }
                }
            }
        }
        if let Some(state) = self.report_state {
            let (tone, message) = match state {
                NetworkState::Queued => (Info, "任务等待执行"),
                NetworkState::Running => (Info, "正在探测…"),
                // Completed execution is not proof that every probe succeeded.
                NetworkState::Complete => (Info, "任务已完成，具体结果如下"),
                NetworkState::Partial => (Warning, "任务部分完成，结果不完整"),
                NetworkState::Failed => (Error, "任务失败"),
                NetworkState::Cancelled => (Warning, "任务已取消，已有结果可能不完整"),
                NetworkState::Interrupted => (Warning, "任务已中断，结果不完整"),
            };
            rows.push(OutputLine::new(tone, message));
        } else if self.running() {
            rows.push(OutputLine::new(Info, "正在探测…"));
        }
        if matches!(
            self.section,
            Section::Tools | Section::Findings | Section::Paths
        ) {
            for (index, event) in self.details.iter().enumerate() {
                let current = super::result_format::unwrap(event);
                if current["event"] == "project_status"
                    && self.details[index + 1..].iter().any(|later| {
                        let later = super::result_format::unwrap(later);
                        later["event"] == "project_status"
                            && ["project_id", "path_id", "round"]
                                .iter()
                                .all(|key| later[key] == current[key])
                    })
                {
                    continue;
                }
                let event_rows = event_conclusions(event);
                for row in event_rows {
                    if !rows.contains(&row) {
                        rows.push(row);
                    }
                }
            }
            if let Some(summary) = &self.summary {
                rows.push(summary_line(
                    summary.sent.into(),
                    summary.received.into(),
                    summary.avg_ms,
                ));
            }
            // Failures must remain visible even when no detailed event or summary arrived.
            for attempt in &self.attempts {
                if let Some(error) = &attempt.error {
                    let row = OutputLine::new(Error, format!("探测失败：{}", clean(error)));
                    if !rows.contains(&row) {
                        rows.push(row);
                    }
                }
            }
            if let Some(note) = &self.note {
                rows.push(OutputLine::new(Info, note));
            }
        }
        if let Some(error) = &self.error {
            rows.push(OutputLine::new(Error, format!("错误：{}", clean(error))));
        }
        if rows.is_empty() {
            rows.push(OutputLine::new(Info, "选择出口和工具后按 Enter 运行。"));
        }
        rows
    }
    /// Full existing report presentation for the right result column. Exported
    /// JSON remains the source for machine-readable, unabridged event data.
    pub fn report_rows(&self) -> Vec<OutputLine> {
        self.detailed_output()
            .into_iter()
            .map(|s| OutputLine::new(OutputTone::Muted, s))
            .collect()
    }
}

fn summary_line(sent: u64, received: u64, avg: Option<f64>) -> OutputLine {
    // Packet counts do not determine tool success: NAT filtering intentionally
    // loses replies, a STUN error is still a reply, and trace owns its summary.
    OutputLine::new(
        OutputTone::Info,
        format!(
            "探测计数：发出 {sent} 次 · 收到 {received} 次{}",
            avg.map(|ms| format!(" · 平均耗时 {ms:.2} ms"))
                .unwrap_or_default()
        ),
    )
}

fn event_conclusions(original: &Value) -> Vec<OutputLine> {
    use OutputTone::*;
    let e = super::result_format::unwrap(original);
    let d = &e["detail"];
    let mut rows = Vec::new();
    match e["event"].as_str() {
        Some("analysis") => {
            if e["incomplete"] == true || e["truncated"] == true {
                rows.push(OutputLine::new(
                    Warning,
                    "诊断证据不完整；未报告的问题不能视为已排除。",
                ));
            }
            for f in list(&e["findings"]) {
                let (tone, label) = match f["severity"].as_str() {
                    Some("risk") => (Error, "风险"),
                    Some("needs_verification") => (Warning, "待核实"),
                    _ => (Info, "信息"),
                };
                rows.push(OutputLine::new(
                    tone,
                    format!(
                        "[{label}] {}：{}",
                        text(&f["title"]),
                        text(&f["explanation"])
                    ),
                ));
                if let Some(s) = f["recommendation"].as_str().filter(|s| !s.is_empty()) {
                    rows.push(OutputLine::new(Info, format!("建议：{}", clean(s))));
                }
            }
            if list(&e["findings"]).next().is_none() {
                rows.push(OutputLine::new(
                    Info,
                    "本次未产生诊断发现；未测项目仍无法判断。",
                ));
            }
            rows.extend(list(&e["notes"]).map(|s| OutputLine::new(Warning, text(s))));
        }
        Some("summary") => rows.push(summary_line(
            number(&e["sent"]),
            number(&e["received"]),
            e["avg_ms"].as_f64(),
        )),
        Some("attempt") if !e["error"].is_null() => rows.push(OutputLine::new(
            Error,
            format!("探测失败：{}", text(&e["error"])),
        )),
        Some("tcp") => {
            let r = &e["result"];
            let (tone, label) = match r["outcome"].as_str() {
                Some("connected") => (Success, "连接成功"),
                Some("refused") => (Error, "连接被拒绝"),
                Some("timed_out") => (Warning, "连接超时"),
                Some("unreachable") => (Error, "无法到达"),
                _ => (Error, "连接失败"),
            };
            rows.push(OutputLine::new(
                tone,
                format!("TCP {}:{}：{label}", text(&r["host"]), text(&r["port"])),
            ));
        }
        Some("dns") => match d["kind"].as_str() {
            Some("response") => {
                let (tone, label) = match d["outcome"].as_str() {
                    Some("resolved") if d["parsed"] == true && d["truncated"] != true => {
                        (Success, "解析成功")
                    }
                    Some("resolved") => (Warning, "解析证据不完整，请核实结果"),
                    Some("nxdomain") => (Warning, "域名不存在"),
                    Some("no_records") | Some("empty") => (Warning, "没有匹配记录"),
                    Some("server_failure") | Some("dns_error") => (Error, "服务器返回错误"),
                    _ => (Warning, "收到响应，请核实结果"),
                };
                rows.push(OutputLine::new(
                    tone,
                    format!("DNS：{label} · {} 条答案", number(&d["answer_count"])),
                ));
                if d["truncated"] == true {
                    rows.push(OutputLine::new(Warning, "DNS 响应证据已截断。"));
                }
            }
            Some("records") if d["section"] == "answer" => {
                for r in list(&d["records"]) {
                    // Answers are the result of a DNS query, while TTL/flags/provenance are details.
                    let value = text(&r["text"]);
                    let preview: String = value.chars().take(160).collect();
                    rows.push(OutputLine::new(
                        Info,
                        format!(
                            "DNS {}：{preview}{}",
                            text(&r["record_type"]),
                            if value.chars().count() > 160 || r["text_truncated"] == true {
                                "…（展开查看详细记录）"
                            } else {
                                ""
                            }
                        ),
                    ));
                }
            }
            Some("failure") => rows.push(OutputLine::new(
                Error,
                format!("DNS 失败：{}", text(&d["message"])),
            )),
            _ => {}
        },
        Some("tls") if d["kind"] == "handshake" => {
            let verified = d["verified"] == true;
            rows.push(OutputLine::new(
                if verified { Success } else { Error },
                format!(
                    "TLS {}：{}",
                    text(&d["server_name"]),
                    if verified {
                        "证书验证通过"
                    } else if d["verification_attempted"] == false {
                        "握手未完成，证书尚未验证"
                    } else {
                        "验证或握手失败"
                    }
                ),
            ));
            for key in ["verification_error", "handshake_error"] {
                if let Some(s) = d[key].as_str() {
                    rows.push(OutputLine::new(Error, s));
                }
            }
            if d["truncated"] == true {
                rows.push(OutputLine::new(Warning, "证书证据已截断。"));
            }
            let comparison = &d["bundled_comparison"];
            if let (Some(system), Some(bundled)) = (
                d["system_chain_verified"].as_bool(),
                comparison["verified"].as_bool(),
            ) {
                if system != bundled {
                    rows.push(OutputLine::new(
                        Warning,
                        "系统与内置根证书验证结果不同；差异本身不表示监听。",
                    ));
                }
            }
        }
        Some("tls") if d["kind"] == "trust" => rows.extend(
            list(&d["load_errors"])
                .map(|e| OutputLine::new(Warning, format!("根证书加载问题：{}", text(e)))),
        ),
        Some("http") => match d["kind"].as_str() {
            Some("response") => {
                let status = number(&d["status"]);
                let (tone, label) = match status {
                    200..=299 => (Success, "请求成功"),
                    300..=399 => (Info, "返回跳转"),
                    400..=599 => (Error, "返回错误状态"),
                    _ => (Warning, "请核实响应"),
                };
                rows.push(OutputLine::new(
                    tone,
                    format!("HTTP {}：{label}（{status}）", text(&d["service"])),
                ));
            }
            Some("failure") => rows.push(OutputLine::new(
                Error,
                format!("HTTP 失败：{}", text(&d["message"])),
            )),
            Some("redirect") if d["followed"] == false => rows.push(OutputLine::new(
                Warning,
                format!("未继续跳转：{}", text(&d["reason"])),
            )),
            _ => {}
        },
        Some("public_ip") => rows.push(OutputLine::new(
            Info,
            format!(
                "HTTPS 公网出口：{}（仅代表该服务的观察）",
                text(&e["address"])
            ),
        )),
        Some("stun") => {
            let o = &e["observation"];
            if let Some(error) = o["error"].as_str() {
                rows.push(OutputLine::new(
                    Error,
                    format!("STUN 失败：{}", clean(error)),
                ));
            } else if o["response_received"] == true && o["mapped_endpoint"].is_string() {
                rows.push(OutputLine::new(
                    Info,
                    format!(
                        "STUN 公网出口：{} · 此项不判定 NAT 类型",
                        text(&o["mapped_endpoint"])
                    ),
                ));
            } else {
                rows.push(OutputLine::new(
                    Warning,
                    "STUN 未取得有效出口映射；不能判断 NAT 类型。",
                ));
            }
        }
        Some("nat") if e["kind"] == "report" => rows.extend(nat_conclusion(&e["report"])),
        Some("trace_summary") => rows.push(OutputLine::new(
            if e["reached"] == true {
                Success
            } else {
                Warning
            },
            format!(
                "逐跳探测 {}：{}",
                text(&e["target"]),
                if e["reached"] == true {
                    "已到达目标"
                } else {
                    "未确认到达；中间无响应不代表设备缺失"
                }
            ),
        )),
        Some("echo") => {
            let r = &e["observation"];
            rows.push(OutputLine::new(
                if r["proves_reachability"] == true {
                    Success
                } else {
                    Warning
                },
                format!(
                    "ICMP {}：{}",
                    text(&r["target"]),
                    if r["proves_reachability"] == true {
                        "目标可达"
                    } else {
                        "未能确认目标可达"
                    }
                ),
            ));
        }
        Some("fake_ip") => {
            let reply = &e["reply"];
            if reply["status"] != "ok" {
                rows.push(OutputLine::new(
                    Warning,
                    format!("Fake-IP 查询未完成：{}", text(&reply["message"])),
                ));
            }
            for r in list(&reply["mappings"]) {
                rows.push(OutputLine::new(
                    if r["domain"].is_string() {
                        Success
                    } else {
                        Warning
                    },
                    format!(
                        "{} → {}",
                        text(&r["address"]),
                        r["domain"]
                            .as_str()
                            .map(clean)
                            .unwrap_or_else(|| "未查到映射，无法判断用途".into())
                    ),
                ));
            }
        }
        Some("project_status") => {
            let (tone, label) = match e["state"].as_str() {
                Some("failed") => (Error, "失败"),
                Some("partial") => (Warning, "部分完成"),
                Some("cancelled") | Some("interrupted") => (Warning, "已停止"),
                Some("complete") => (Info, "执行完成"),
                _ => (Info, "等待或执行中"),
            };
            rows.push(OutputLine::new(
                tone,
                format!(
                    "项目 {}：{label} · {}",
                    text(&e["project_id"]),
                    text(&e["message"])
                ),
            ));
        }
        _ => {}
    }
    if original["event"] == "project" {
        let p = &original["project"];
        for row in &mut rows {
            row.text = format!(
                "[{} · {} · 第 {} 轮] {}",
                text(&p["project_id"]),
                text(&p["path_id"]),
                text(&p["round"]),
                row.text
            );
        }
    }
    rows
}

fn nat_conclusion(r: &Value) -> Vec<OutputLine> {
    use OutputTone::*;
    let label = if r["advanced_capable"] == true && r["unstable"] == false && r["partial"] == false
    {
        match r["traditional_type"].as_str().filter(|_| {
            r["physical_path"]["binding_verified"] == true
                && r["physical_path"]["route_verified"] == true
        }) {
            Some("symmetric_observed") => Some("对称型"),
            Some("full_cone_observed") => Some("完全锥型"),
            Some("restricted_cone_observed") => Some("受限锥型"),
            Some("port_restricted_cone_observed") => Some("端口受限锥型"),
            Some("no_translation_observed") => Some("未观察到地址转换"),
            _ => None,
        }
    } else {
        None
    };
    let mut rows = vec![OutputLine::new(
        if label.is_some() { Info } else { Warning },
        format!(
            "NAT 类型：{}（网卡 {}，仅代表本次测量）",
            label.unwrap_or("未能确定"),
            text(&r["interface_name"])
        ),
    )];
    if label.is_none() {
        rows.push(OutputLine::new(
            Warning,
            if r["unstable"] == true {
                "映射不稳定，不能可靠分类。"
            } else if r["advanced_capable"] != true {
                "尚未取得类型检测所需的服务能力证据；请启用类型识别并使用支持 RFC 5780 的测试服务。"
            } else {
                "证据不足或映射/过滤组合不属于传统类别；展开高级信息查看行为测量。"
            },
        ));
        rows.extend(list(&r["notes"]).map(|s| OutputLine::new(Warning, text(s))));
    }
    rows
}
