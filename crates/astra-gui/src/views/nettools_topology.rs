//! Interactive rendering of the Agent's topology. No diagnostic rules live here.
use super::*;
use astra_model::network_analysis::{AnalysisReport, EvidenceLevel, EvidenceRef, PathSection};
use serde_json::Value;

#[derive(Default)]
pub(super) struct State {
    operation: Option<String>,
    pub(super) selection: Option<Selection>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Item {
    Node(String),
    Edge(usize),
    Attachment(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) path: usize,
    pub(super) item: Item,
}

fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
fn latest(events: &[Value]) -> Option<AnalysisReport> {
    events.iter().rev().find_map(|event| {
        let mut event = event;
        for _ in 0..8 {
            if event["event"] != "project" {
                break;
            }
            event = &event["detail"];
        }
        (event["event"] == "analysis")
            .then(|| serde_json::from_value(event.clone()).ok())
            .flatten()
    })
}
fn level(level: EvidenceLevel) -> &'static str {
    match level {
        EvidenceLevel::Measured => "实测",
        EvidenceLevel::Configuration => "配置推断（非实测）",
        EvidenceLevel::Unknown => "未知",
    }
}
fn section(section: PathSection) -> &'static str {
    match section {
        PathSection::Dns => "DNS 解析",
        PathSection::Connection => "连接分段",
        PathSection::Trace => "逐跳测量",
    }
}
fn referenced<'a>(events: &'a [Value], evidence: &EvidenceRef) -> Option<&'a Value> {
    events.iter().find_map(|event| {
        let mut event = event;
        for _ in 0..8 {
            if event["event"] != "project" {
                return None;
            }
            let project = &event["project"];
            if project["project_id"] == evidence.project_id
                && project["path_id"] == evidence.path_id
                && project["round"] == evidence.round
                && project["index"] == evidence.event_index
                && project["sampled_at_ms"] == evidence.sampled_at_ms
            {
                return Some(&event["detail"]);
            }
            event = &event["detail"];
        }
        None
    })
}
fn preview(event: &Value) -> String {
    fn omit_der(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    if key == "base64" && value.is_string() {
                        *value = Value::String("证书 DER 保留在 JSON 导出中".into());
                    } else {
                        omit_der(value);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(omit_der),
            _ => {}
        }
    }
    let mut event = event.clone();
    omit_der(&mut event);
    let text = serde_json::to_string_pretty(&event).unwrap_or_default();
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(16_384).collect();
    if chars.next().is_some() {
        result.push_str("\n详情预览已截断；保留的完整证据见 JSON 导出。");
    }
    result
}
pub(super) fn detail(
    report: &AnalysisReport,
    selection: &Selection,
    events: &[Value],
) -> Option<Vec<String>> {
    let path = report.topology.get(selection.path)?;
    let mut lines = vec![format!("路径：{}", clean(&path.path_id))];
    let mut evidence = Vec::new();
    match &selection.item {
        Item::Node(id) => {
            let node = path.nodes.iter().find(|node| &node.id == id)?;
            lines.push(format!(
                "节点：{} · {}",
                clean(&node.label),
                clean(&node.role)
            ));
            for edge in path
                .edges
                .iter()
                .filter(|edge| &edge.from == id || &edge.to == id)
            {
                lines.push(format!(
                    "{} · {} · {}",
                    section(edge.section),
                    level(edge.level),
                    clean(&edge.description)
                ));
                evidence.extend(edge.evidence.iter().cloned());
            }
        }
        Item::Edge(index) => {
            let edge = path.edges.get(*index)?;
            lines.push(format!("{} · {}", section(edge.section), level(edge.level)));
            lines.push(clean(&edge.description));
            evidence.extend(edge.evidence.iter().cloned());
        }
        Item::Attachment(index) => {
            let attachment = path.attachments.get(*index)?;
            lines.push(format!(
                "旁注（不计作逐跳设备）：{} · {}",
                clean(&attachment.kind),
                clean(&attachment.label)
            ));
            evidence.extend(attachment.evidence.iter().cloned());
        }
    }
    if path.truncated || report.truncated || report.incomplete {
        lines.push("证据不完整或已截断；不能据此确认未测路径。".into());
    }
    let mut seen = vec![];
    for reference in evidence {
        if seen.contains(&reference) {
            continue;
        }
        seen.push(reference.clone());
        lines.push(format!(
            "项目 {} · 路径 {} · 第 {} 轮 · 事件 {} · 采样 {} ms · 来源 {}",
            clean(&reference.project_id),
            clean(&reference.path_id),
            reference.round,
            reference.event_index,
            clean(&reference.sampled_at_ms),
            clean(&reference.source)
        ));
        if let Some(event) = referenced(events, &reference) {
            lines.push(preview(event));
        } else {
            lines.push("此引用的原始事件未保留在当前页；仅展示后端提供的引用，不补造证据。".into());
        }
    }
    if seen.is_empty() {
        lines.push("后端未提供此项的证据引用。未知边不代表探测成功。".into());
    }
    Some(lines)
}
impl View {
    fn topology_button(
        id: usize,
        label: String,
        selection: Selection,
        cx: &Context<Self>,
    ) -> Button {
        // Long node names remain available in the tooltip and detail pane.
        let short: String = label.chars().take(42).collect();
        let short = if short != label {
            format!("{short}…")
        } else {
            short
        };
        Button::new(("topology-item", id))
            .debug_selector(move || format!("network-topology-{id}"))
            .label(short)
            .tooltip(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                view.topology.selection = Some(selection.clone());
                cx.notify();
            }))
    }
    pub(super) fn topology_view(&mut self, cx: &mut Context<Self>) -> gpui::Div {
        let (report, operation) = self
            .tools(cx, |view| {
                (latest(&view.details), view.operation_id.clone())
            })
            .unwrap_or_default();
        if self.topology.operation != operation {
            self.topology.operation = operation;
            self.topology.selection = None;
        }
        let mut root = div().flex().flex_col().gap_2().flex_shrink_0().child(
            div()
                .text_sm()
                .child("路径证据 · 点击节点或连接查看项目、采样时间与原始证据"),
        );
        let Some(report) = report else {
            return root.child(
                div()
                    .text_xs()
                    .child("诊断完成后显示后端提供的路径；尚无拓扑不表示网络安全或可达。"),
            );
        };
        let mut paths = div()
            .id("topology-paths")
            .flex()
            .flex_col()
            .gap_2()
            .max_h(px(420.))
            .overflow_y_scroll();
        let mut id = 0;
        for (path_index, path) in report.topology.iter().enumerate() {
            let mut group = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .border_1()
                .rounded_md()
                .border_color(cx.theme().border)
                .child(format!(
                    "路径 {}{}",
                    clean(&path.path_id),
                    if path.truncated { " · 已截断" } else { "" }
                ));
            for kind in [
                PathSection::Dns,
                PathSection::Connection,
                PathSection::Trace,
            ] {
                group = group.child(div().text_sm().child(section(kind)));
                let mut any = false;
                for (edge_index, edge) in path
                    .edges
                    .iter()
                    .enumerate()
                    .filter(|(_, edge)| edge.section == kind)
                {
                    any = true;
                    let node = |node_id: &str| {
                        path.nodes
                            .iter()
                            .find(|node| node.id == node_id)
                            .map(|node| format!("{} ({})", clean(&node.label), clean(&node.role)))
                            .unwrap_or_else(|| clean(node_id))
                    };
                    let mut row = div().flex().flex_wrap().items_center().gap_2();
                    for (label, item) in [
                        (node(&edge.from), Item::Node(edge.from.clone())),
                        (format!("→ {} →", level(edge.level)), Item::Edge(edge_index)),
                        (node(&edge.to), Item::Node(edge.to.clone())),
                    ] {
                        row = row.child(Self::topology_button(
                            id,
                            label,
                            Selection {
                                path: path_index,
                                item,
                            },
                            cx,
                        ));
                        id += 1;
                    }
                    group = group.child(row);
                }
                if !any {
                    group = group.child(div().text_xs().child("尚无此类路径证据"));
                }
            }
            for (index, attachment) in path.attachments.iter().enumerate() {
                group = group.child(Self::topology_button(
                    id,
                    format!("旁注（非逐跳设备）：{}", clean(&attachment.label)),
                    Selection {
                        path: path_index,
                        item: Item::Attachment(index),
                    },
                    cx,
                ));
                id += 1;
            }
            paths = paths.child(group);
        }
        if let Some(selection) = &self.topology.selection {
            let lines = self
                .tools(cx, |view| detail(&report, selection, &view.details))
                .flatten();
            if let Some(lines) = lines {
                let mut pane = div()
                    .id("topology-detail")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().primary)
                    .max_h(px(340.))
                    .overflow_y_scroll()
                    .child(
                        Button::new("topology-close")
                            .label("关闭证据详情")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.topology.selection = None;
                                cx.notify();
                            })),
                    );
                for line in lines {
                    pane = pane.child(div().text_xs().child(line));
                }
                root = root.child(pane);
            }
        }
        root.child(paths)
    }
}
