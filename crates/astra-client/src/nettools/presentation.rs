use super::*;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Overview,
    Tools,
    Paths,
    Findings,
}
impl Section {
    pub const ALL: [Self; 4] = [Self::Overview, Self::Tools, Self::Paths, Self::Findings];
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Tools => "工具箱",
            Self::Paths => "路径",
            Self::Findings => "诊断",
        }
    }
}
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => clean(text),
        Value::Null => "未知".into(),
        value => clean(&value.to_string()),
    }
}
pub(super) fn event_lines(event: &Value) -> Vec<String> {
    if let Some(lines) = super::result_format::event_lines(event) {
        return lines;
    }
    if event["event"] == "fake_ip" {
        let reply = &event["reply"];
        let mut lines = vec![format!(
            "Fake-IP 核心映射 · {} · 采样 {}",
            core_status(&reply["status"]),
            text(&reply["sampled_at_ms"])
        )];
        if let Some(message) = reply["message"].as_str() {
            lines.push(clean(message));
        }
        if let Some(mappings) = reply["mappings"].as_array() {
            for mapping in mappings {
                lines.push(format!(
                    "{} → {}",
                    text(&mapping["address"]),
                    mapping["domain"]
                        .as_str()
                        .map(clean)
                        .unwrap_or_else(|| "未查询到映射（不能据此判断地址用途）".into())
                ));
            }
        }
        return lines;
    }
    if event["event"] != "dns" {
        return std::iter::once(if event["event"] == "resolution" {
            "解析路径记录（入口引导与业务目标分别记录）".into()
        } else {
            "探测快照（与实例当前状态分开记录）".into()
        })
        .chain(
            event
                .as_object()
                .into_iter()
                .flatten()
                .map(|(key, value)| format!("{key}: {}", text(value))),
        )
        .collect();
    }
    let detail = event.get("detail").unwrap_or(event);
    let mut lines = vec![];
    if let Some(records) = detail["records"].as_array() {
        lines.push(format!(
            "DNS #{} · {}",
            text(&event["seq"]),
            text(&detail["section"])
        ));
        for record in records {
            lines.push(format!(
                "{} {} {} TTL={} {}{}",
                text(&record["name"]),
                text(&record["class"]),
                text(&record["record_type"]),
                text(&record["ttl"]),
                text(&record["text"]),
                if record["text_truncated"] == true {
                    " [已截断]"
                } else {
                    ""
                }
            ));
        }
    } else {
        lines.push(format!(
            "DNS #{} · {}",
            text(&event["seq"]),
            text(&detail["kind"])
        ));
        if let Some(fields) = detail.as_object() {
            for (key, value) in fields {
                if key != "kind" {
                    lines.push(format!("  {key}: {}", text(value)));
                }
            }
        }
    }
    lines
}
fn core_status(value: &Value) -> &'static str {
    match value.as_str() {
        Some("ok") => "读取成功",
        Some("unsupported") => "核心不支持",
        Some("unconfirmed") => "能力无法确认",
        Some("communication_error") => "通信失败",
        Some("protocol_error") => "协议错误",
        Some("core_error") => "核心查询失败",
        Some("invalid_request") => "请求无效",
        _ => "状态未知",
    }
}
impl View {
    pub fn section_output(&self) -> Vec<String> {
        let mut lines = vec![];
        if let Some(notice) = &self.context.notice {
            lines.push(notice.clone());
        }
        match self.section {
            Section::Overview => {
                if let Some(state) = &self.context.network {
                    lines.push(format!(
                        "目标 Agent：{} · 采样时间 {} · DNS采样 {}",
                        text(&state["agent_id"]),
                        text(&state["sampled_at_ms"]),
                        text(&state["dns_sampled_at_ms"])
                    ));
                    if state["truncated"] == true {
                        lines.push("系统信息已截断".into());
                    }
                    for category in ["interfaces", "dns", "routes", "issues"] {
                        lines.push(format!("[{category}]"));
                        if let Some(items) = state[category].as_array() {
                            for item in items {
                                lines.push(text(item));
                            }
                        }
                    }
                } else {
                    lines.push("正在读取目标 Agent 网络状态；不枚举客户端网卡".into());
                }
                self.runtime_lines(&mut lines);
                if let Some(core) = &self.context.core {
                    lines.push(format!(
                        "核心能力（{}）：{}",
                        core_status(&core["status"]),
                        text(core)
                    ));
                }
            }
            Section::Paths => {
                self.runtime_lines(&mut lines);
                lines.push(format!(
                    "本次探测选择：{}",
                    self.probe_path.as_deref().unwrap_or("尚未运行")
                ));
                if let Some(context) = self
                    .details
                    .iter()
                    .map(super::result_format::unwrap)
                    .find(|e| e["event"] == "context")
                {
                    lines.push(format!(
                        "配置来源={} · 摘要={} · 核心={}",
                        text(&context["config_observation"]),
                        text(&context["config_digest"]),
                        text(&context["running_core"]["boot_id"])
                    ));
                    lines.push(format!(
                        "固定网卡={} · 来源={}",
                        text(&context["effective_interface"]),
                        text(&context["interface_source"])
                    ));
                    if let Some(chain) = context["selection_chain"].as_array() {
                        for hop in chain {
                            lines.push(format!(
                                "{} ({}) → {} · 依据={} · 网卡={}",
                                text(&hop["group"]),
                                text(&hop["mode"]),
                                text(&hop["selected"]),
                                text(&hop["source"]),
                                text(&hop["interface"])
                            ));
                        }
                    }
                }
                lines.push(
                    "实例组成员名不等于本次出口；固定链以任务快照为准，无法确认时拒绝运行。".into(),
                );
                let analyses = self
                    .details
                    .iter()
                    .map(super::result_format::unwrap)
                    .filter(|event| event["event"] == "analysis")
                    .collect::<Vec<_>>();
                if let Some(report) = analyses.last() {
                    lines.extend(super::result_format::analysis(report, true));
                } else {
                    for (label, kinds) in [
                        ("DNS 解析视图", vec!["resolution", "dns"]),
                        ("连接分段视图", vec!["connection", "tcp", "tls"]),
                        ("逐跳测量视图", vec!["trace", "trace_summary"]),
                    ] {
                        lines.push(format!("[{label}]"));
                        let mut found = false;
                        for event in &self.details {
                            let detail = super::result_format::unwrap(event);
                            if detail["event"]
                                .as_str()
                                .is_some_and(|kind| kinds.contains(&kind))
                            {
                                found = true;
                                lines.extend(event_lines(event));
                            }
                        }
                        if !found {
                            lines.push("尚无此类证据；不根据名称或超时补画设备。".into());
                        }
                    }
                }
                lines.push(
                    "NAT、TUN 和代理会话旁注不计作逐跳设备；不同视图独立展示实测、配置与未知来源。"
                        .into(),
                );
                if self.path_stale {
                    lines.push("运行快照已变化；已启动任务的路径未自动切换。".into());
                }
            }
            Section::Findings => {
                lines.extend(self.plan_lines());
                if let Some(id) = &self.operation_id {
                    lines.push(format!("任务 {id} · {:?}", self.report_state));
                }
                if let Some(note) = &self.note {
                    lines.push(clean(note));
                }
                if let Some(error) = &self.error {
                    lines.push(format!("错误：{}", clean(error)));
                }
                if let Some(report) = self
                    .details
                    .iter()
                    .rev()
                    .map(super::result_format::unwrap)
                    .find(|event| event["event"] == "analysis")
                {
                    lines.extend(super::result_format::analysis(report, false));
                } else {
                    lines.push("尚无统一诊断结论。运行诊断计划后由目标 Agent 的固定规则生成；未执行对照不表示没有差异。".into());
                }
            }
            Section::Tools => {}
        }
        lines
    }
    fn runtime_lines(&self, lines: &mut Vec<String>) {
        lines.push("实例当前选择（运行快照，独立于本次探测）".into());
        if let Some(runtime) = &self.context.runtime {
            lines.push(format!(
                "模式={} · 系统代理={} · TUN={}",
                text(&runtime["mode"]),
                text(&runtime["system_proxy"]),
                text(&runtime["tun"])
            ));
            if runtime["truncated"] == true {
                lines.push("运行快照截断，不能固定完整选择链".into());
            }
            if let Some(groups) = runtime["groups"].as_array() {
                for g in groups {
                    lines.push(format!(
                        "{} ({}) → {}",
                        text(&g["name"]),
                        text(&g["mode"]),
                        text(&g["selected"])
                    ));
                }
            }
        } else {
            lines.push("运行快照不可用或实例未运行".into());
        }
    }
}
