//! Single-column result presentation shared by GUI and TUI. Findings come from
//! the backend analysis event; nothing here derives a diagnostic conclusion.
use super::*;
use astra_management::v2::NetworkState;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OutputTone {
    Success,
    Warning,
    Error,
    Info,
    Muted,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutputLine {
    pub text: String,
    pub tone: OutputTone,
}
impl OutputLine {
    pub fn new(tone: OutputTone, text: impl Into<String>) -> Self {
        Self {
            text: clean(&text.into()),
            tone,
        }
    }
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}
pub fn wrap_lines(lines: Vec<String>, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in lines {
        let mut row = String::new();
        let mut used = 0;
        for grapheme in line.graphemes(true) {
            let cells = display_width(grapheme);
            if used + cells > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push_str(grapheme);
            used += cells;
        }
        rows.push(row);
    }
    rows
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
fn float(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}
fn list(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// Batch results wrap each sub-event in `project` envelopes.
pub fn unwrap(event: &Value) -> &Value {
    let mut value = event;
    for _ in 0..8 {
        if value["event"] != "project" {
            break;
        }
        value = &value["detail"];
    }
    value
}

impl View {
    /// Latest backend analysis report (findings and topology), if any.
    pub fn analysis(&self) -> Option<&Value> {
        self.details
            .iter()
            .rev()
            .map(unwrap)
            .find(|event| event["event"] == "analysis")
    }
    /// Latest NAT report, if any.
    pub fn nat_report(&self) -> Option<&Value> {
        self.details
            .iter()
            .rev()
            .map(unwrap)
            .find(|event| event["event"] == "nat" && event["kind"] == "report")
            .map(|event| &event["report"])
    }
    pub fn path_strips(&self) -> Vec<chain::PathStrip> {
        self.analysis().map(chain::path_strips).unwrap_or_default()
    }
    pub fn output(&self) -> Vec<String> {
        self.output_rows().into_iter().map(|row| row.text).collect()
    }
    /// Kept for the DNS inspector query box.
    pub fn output_rows(&self) -> Vec<OutputLine> {
        self.result_rows()
    }
    pub fn result_rows(&self) -> Vec<OutputLine> {
        use OutputTone::*;
        let mut rows = Vec::new();
        if let Some(notice) = &self.context.notice {
            rows.push(OutputLine::new(Warning, notice));
        }
        if self.kind == Kind::Nat && self.details.is_empty() && !self.running() {
            rows.push(OutputLine::new(
                Info,
                "NAT 检测使用手选的物理网卡直连测试服务，服务会看到该网络的公网出口 IP。",
            ));
        }
        if self.path_stale {
            rows.push(OutputLine::new(
                Warning,
                "实例选择已变化；本次结果仍对应启动时的出口。",
            ));
        }
        if let Some(state) = self.report_state {
            let (tone, message) = match state {
                NetworkState::Queued => (Info, "任务等待执行"),
                NetworkState::Running => (Info, "正在探测…"),
                // Completed execution is not proof that every probe succeeded.
                NetworkState::Complete => (Info, "任务已完成"),
                NetworkState::Partial => (Warning, "任务部分完成，结果不完整"),
                NetworkState::Failed => (Error, "任务失败"),
                NetworkState::Cancelled => (Warning, "任务已取消，已有结果可能不完整"),
                NetworkState::Interrupted => (Warning, "任务已中断，结果不完整"),
            };
            rows.push(OutputLine::new(tone, message));
        } else if self.running() {
            rows.push(OutputLine::new(Info, "正在探测…"));
        }
        if let Some(summary) = &self.summary {
            let stats = match (summary.min_ms, summary.avg_ms, summary.max_ms) {
                (Some(min), Some(avg), Some(max)) => {
                    format!(" · min/avg/max {min:.2}/{avg:.2}/{max:.2} ms")
                }
                _ => String::new(),
            };
            rows.push(OutputLine::new(
                if summary.received == 0 && summary.sent > 0 {
                    Warning
                } else {
                    Info
                },
                format!(
                    "发送 {} · 接收 {} · 丢失 {}%{stats}",
                    summary.sent,
                    summary.received,
                    summary.loss_percent()
                ),
            ));
        }
        for attempt in &self.attempts {
            rows.push(match (&attempt.error, attempt.rtt_ms) {
                (None, Some(ms)) => OutputLine::new(
                    Muted,
                    format!("#{:<3} {:>9.2} ms  {}", attempt.seq, ms, attempt.detail),
                ),
                (error, _) => OutputLine::new(
                    Error,
                    format!(
                        "#{:<3} 失败  {}",
                        attempt.seq,
                        error.as_deref().unwrap_or_default()
                    ),
                ),
            });
        }
        let mut seen: std::collections::HashSet<OutputLine> = rows.iter().cloned().collect();
        // Only the latest status of each batch project/path/round is shown.
        let status_key = |event: &Value| {
            serde_json::json!([event["project_id"], event["path_id"], event["round"]]).to_string()
        };
        let mut latest_status = std::collections::HashMap::new();
        for (index, event) in self.details.iter().enumerate() {
            let current = unwrap(event);
            if current["event"] == "project_status" {
                latest_status.insert(status_key(current), index);
            }
        }
        let mut hops: Vec<(u64, Option<String>, Option<f64>)> = vec![];
        // Flushed at the trace summary, or at the end when a trace was cancelled
        // or failed before summarising, so partial hop evidence stays visible.
        let flush = |hops: &mut Vec<(u64, Option<String>, Option<f64>)>,
                     rows: &mut Vec<OutputLine>| {
            hops.sort_by_key(|(hop, ..)| *hop);
            for (hop, address, rtt) in hops.drain(..) {
                rows.push(OutputLine::new(
                    if address.is_some() { Info } else { Muted },
                    format!(
                        "跳 {hop:>2}  {:<40} {}",
                        address.unwrap_or_else(|| "*".into()),
                        rtt.map_or_else(|| "无响应".into(), |ms| format!("{ms:.2} ms"))
                    ),
                ));
            }
        };
        for (index, event) in self.details.iter().enumerate() {
            let current = unwrap(event);
            if current["event"] == "project_status"
                && latest_status.get(&status_key(current)) != Some(&index)
            {
                continue;
            }
            if current["event"] == "trace" {
                let sample = &current["sample"];
                let hop = number(&sample["hop"]);
                let address = sample["address"].as_str().map(clean);
                let rtt = float(&sample["rtt_ms"]);
                match hops.iter_mut().find(|(h, ..)| *h == hop) {
                    Some(entry) => {
                        entry.1 = entry.1.take().or(address);
                        entry.2 = match (entry.2, rtt) {
                            (Some(a), Some(b)) => Some(a.min(b)),
                            (a, b) => a.or(b),
                        };
                    }
                    None => hops.push((hop, address, rtt)),
                }
                continue;
            }
            if current["event"] == "trace_summary" {
                flush(&mut hops, &mut rows);
            }
            for row in event_rows(event) {
                if seen.insert(row.clone()) {
                    rows.push(row);
                }
            }
        }
        flush(&mut hops, &mut rows);
        if let Some(note) = &self.note {
            rows.push(OutputLine::new(Muted, note));
        }
        if let Some(error) = &self.error {
            rows.push(OutputLine::new(Error, format!("错误：{}", clean(error))));
        }
        if rows.is_empty() {
            rows.push(OutputLine::new(Info, "选择工具和出口后按 Enter 运行。"));
        }
        rows
    }
}

fn event_rows(original: &Value) -> Vec<OutputLine> {
    use OutputTone::*;
    let e = unwrap(original);
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
                let confidence = match f["confidence"].as_str() {
                    Some("high") => "高",
                    Some("medium") => "中",
                    _ => "未知",
                };
                rows.push(OutputLine::new(
                    tone,
                    format!(
                        "[{label} · 置信度{confidence}] {}：{}",
                        text(&f["title"]),
                        text(&f["explanation"])
                    ),
                ));
                if let Some(s) = f["recommendation"].as_str().filter(|s| !s.is_empty()) {
                    rows.push(OutputLine::new(Muted, format!("  建议：{}", clean(s))));
                }
            }
            if list(&e["findings"]).next().is_none() {
                rows.push(OutputLine::new(
                    Info,
                    "分析未产生发现；未测项目仍无法判断。",
                ));
            }
            rows.extend(list(&e["notes"]).map(|s| OutputLine::new(Muted, text(s))));
        }
        Some("tcp") => {
            let r = &e["result"];
            let (tone, label) = match r["outcome"].as_str() {
                Some("connected") => (Success, "已连接"),
                Some("refused") => (Error, "被拒绝"),
                Some("timed_out") => (Warning, "超时"),
                Some("unreachable") => (Error, "不可达"),
                _ => (Error, "失败"),
            };
            rows.push(OutputLine::new(
                tone,
                format!(
                    "TCP {}:{}  {label}{}",
                    text(&r["host"]),
                    text(&r["port"]),
                    float(&r["connect_ms"])
                        .map(|ms| format!(" · {ms:.2} ms"))
                        .unwrap_or_default()
                ),
            ));
        }
        Some("dns") => match d["kind"].as_str() {
            Some("response") => {
                let (tone, label) = match d["outcome"].as_str() {
                    Some("resolved") if d["parsed"] == true && d["truncated"] != true => {
                        (Success, "解析成功")
                    }
                    Some("resolved") => (Warning, "解析证据不完整"),
                    Some("nxdomain") => (Warning, "域名不存在"),
                    Some("no_records") | Some("empty") => (Warning, "没有匹配记录"),
                    Some("server_failure") | Some("dns_error") => (Error, "服务器返回错误"),
                    _ => (Warning, "收到响应，请核实"),
                };
                rows.push(OutputLine::new(
                    tone,
                    format!(
                        "DNS {label} · RCODE {} · {} 条答案",
                        text(&d["rcode"]),
                        number(&d["answer_count"])
                    ),
                ));
            }
            Some("records") if d["section"] == "answer" => {
                for r in list(&d["records"]) {
                    let value = text(&r["text"]);
                    let preview: String = value.chars().take(160).collect();
                    rows.push(OutputLine::new(
                        Info,
                        format!(
                            "  {} {} TTL {}  {preview}{}",
                            text(&r["name"]),
                            text(&r["record_type"]),
                            text(&r["ttl"]),
                            if value.chars().count() > 160 || r["text_truncated"] == true {
                                "…"
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
                    "TLS {} · {} · {}",
                    text(&d["server_name"]),
                    d["protocol"]
                        .as_str()
                        .map(clean)
                        .unwrap_or_else(|| "协议未知".into()),
                    if verified {
                        "证书验证通过"
                    } else if d["verification_attempted"] == false {
                        "握手未完成，证书未验证"
                    } else {
                        "验证或握手失败"
                    }
                ),
            ));
            for key in ["verification_error", "handshake_error"] {
                if let Some(s) = d[key].as_str() {
                    rows.push(OutputLine::new(Error, format!("  {s}")));
                }
            }
            if let (Some(system), Some(bundled)) = (
                d["system_chain_verified"].as_bool(),
                d["bundled_comparison"]["verified"].as_bool(),
            ) {
                if system != bundled {
                    rows.push(OutputLine::new(
                        Warning,
                        "  系统与内置根证书验证结果不同；差异本身不表示监听。",
                    ));
                }
            }
        }
        Some("tls") if d["kind"] == "certificate" && number(&d["index"]) == 0 => {
            rows.push(OutputLine::new(
                Muted,
                format!(
                    "  证书 {} · 签发 {} · 有效期 {} 至 {}",
                    text(&d["subject"]),
                    text(&d["issuer"]),
                    text(&d["not_before"]),
                    text(&d["not_after"])
                ),
            ));
        }
        Some("tls") if d["kind"] == "trust" => rows.extend(
            list(&d["load_errors"])
                .map(|e| OutputLine::new(Warning, format!("根证书加载问题：{}", text(e)))),
        ),
        Some("http") => match d["kind"].as_str() {
            Some("response") => {
                let status = number(&d["status"]);
                let tone = match status {
                    200..=299 => Success,
                    300..=399 => Info,
                    400..=599 => Error,
                    _ => Warning,
                };
                rows.push(OutputLine::new(
                    tone,
                    format!(
                        "HTTP {} {} {} → {status}",
                        text(&d["version"]),
                        text(&d["method"]),
                        text(&d["service"])
                    ),
                ));
                let t = &d["timings"];
                let part =
                    |label: &str, key: &str| float(&t[key]).map(|ms| format!("{label} {ms:.1}"));
                let phases: Vec<String> = [
                    part("DNS", "dns_ms"),
                    part("连接", "transport_connect_ms"),
                    part("代理握手", "proxy_handshake_ms"),
                    part("TLS", "tls_ms"),
                    part("首字节", "response_headers_ms"),
                    part("总计", "total_ms"),
                ]
                .into_iter()
                .flatten()
                .collect();
                if !phases.is_empty() {
                    rows.push(OutputLine::new(
                        Muted,
                        format!("  耗时（ms）{}", phases.join(" · ")),
                    ));
                }
            }
            Some("failure") => rows.push(OutputLine::new(
                Error,
                format!(
                    "HTTP 失败（{}）：{}",
                    text(&d["phase"]),
                    text(&d["message"])
                ),
            )),
            Some("redirect") => rows.push(OutputLine::new(
                if d["followed"] == true {
                    Muted
                } else {
                    Warning
                },
                format!(
                    "  跳转 {} → {}{}",
                    text(&d["from"]),
                    text(&d["to"]),
                    if d["followed"] == true {
                        String::new()
                    } else {
                        format!("（未继续：{}）", text(&d["reason"]))
                    }
                ),
            )),
            _ => {}
        },
        Some("public_ip") => rows.push(OutputLine::new(
            Success,
            format!(
                "公网出口 IP：{}（{} 观察）",
                text(&e["address"]),
                text(&e["service"])
            ),
        )),
        Some("stun") => {
            let o = &e["observation"];
            if let Some(error) = o["error"].as_str() {
                rows.push(OutputLine::new(
                    Error,
                    format!("STUN 失败：{}", clean(error)),
                ));
            } else if o["mapped_endpoint"].is_string() {
                rows.push(OutputLine::new(
                    Info,
                    format!("STUN 出口映射：{}", text(&o["mapped_endpoint"])),
                ));
            } else {
                rows.push(OutputLine::new(Warning, "STUN 未取得出口映射"));
            }
        }
        Some("nat") if e["kind"] == "report" => rows.extend(nat_rows(&e["report"])),
        Some("trace_summary") => rows.push(OutputLine::new(
            if e["reached"] == true {
                Success
            } else {
                Warning
            },
            format!(
                "逐跳 {}：{}",
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
            if r["proves_reachability"] != true {
                rows.push(OutputLine::new(
                    Warning,
                    format!("ICMP {}：{}", text(&r["target"]), text(&r["message"])),
                ));
            }
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
                Some("complete") => (Info, "完成"),
                _ => (Muted, "等待或执行中"),
            };
            rows.push(OutputLine::new(
                tone,
                format!(
                    "项目 {}：{label}{}",
                    text(&e["project_id"]),
                    e["message"]
                        .as_str()
                        .filter(|m| !m.is_empty())
                        .map(|m| format!(" · {}", clean(m)))
                        .unwrap_or_default()
                ),
            ));
        }
        _ => {}
    }
    if original["event"] == "project" {
        let p = &original["project"];
        for row in &mut rows {
            row.text = format!(
                "[{}{}] {}",
                text(&p["project_id"]),
                if number(&p["round"]) > 1 {
                    format!(" · 第 {} 轮", number(&p["round"]))
                } else {
                    String::new()
                },
                row.text
            );
        }
    }
    rows
}

/// NAT verdict, the fixed comparison table (detected row marked) and the
/// measured behaviours.
pub fn nat_rows(report: &Value) -> Vec<OutputLine> {
    use OutputTone::*;
    let kind = nat::nat_type(report);
    let mut rows = vec![OutputLine::new(
        if kind.highlighted().is_some() || kind == nat::NatType::NoTranslation {
            Success
        } else {
            Warning
        },
        format!(
            "NAT 类型：{} · 网卡 {}",
            kind.label(),
            text(&report["interface_name"])
        ),
    )];
    let widths = [34, 18, 20, 10];
    let cell = |value: &str, width: usize| {
        let pad = width.saturating_sub(display_width(value));
        format!("{value}{}", " ".repeat(pad))
    };
    rows.push(OutputLine::new(
        Muted,
        format!(
            "  {}{}{}{}",
            cell(nat::NAT_HEADERS[0], widths[0]),
            cell(nat::NAT_HEADERS[1], widths[1]),
            cell(nat::NAT_HEADERS[2], widths[2]),
            nat::NAT_HEADERS[3]
        ),
    ));
    for row in &nat::NAT_TABLE {
        let detected = kind.highlighted() == Some(row.kind);
        rows.push(OutputLine::new(
            if detected { Success } else { Muted },
            format!(
                "{} {}{}{}{}",
                if detected { "▶" } else { " " },
                cell(row.name, widths[0]),
                cell(row.mapping, widths[1]),
                cell(row.inbound, widths[2]),
                row.p2p
            ),
        ));
    }
    rows.push(OutputLine::new(
        Muted,
        format!(
            "  测得映射：{} · 过滤：{}",
            nat::behavior_label(&report["mapping"]),
            nat::behavior_label(&report["filtering"])
        ),
    ));
    let mapped = list(&report["observations"])
        .find_map(|o| o["mapped_endpoint"].as_str())
        .map(clean);
    rows.push(OutputLine::new(
        Muted,
        format!(
            "  本地 {} · 公网映射 {}",
            text(&report["local_endpoint"]),
            mapped.unwrap_or_else(|| "未取得".into())
        ),
    ));
    rows.extend(list(&report["notes"]).map(|s| OutputLine::new(Muted, format!("  {}", text(s)))));
    rows
}
