//! Link display: the live instance chain and the probe path read from the
//! Agent's topology. No diagnostic rules live here.
use super::*;
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
/// The report is read as JSON: stored reports encode integer counts as
/// decimal strings for browsers, so typed decoding would reject them.
pub(super) fn latest_report(events: &[Value]) -> Option<Value> {
    events.iter().rev().find_map(|event| {
        let event = astra_client::nettools::unwrap(event);
        (event["event"] == "analysis").then(|| event.clone())
    })
}
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => clean(text),
        Value::Null => "未知".into(),
        value => clean(&value.to_string()),
    }
}
/// Compare a number that may arrive as a JSON number or a decimal string.
fn same(left: &Value, right: &Value) -> bool {
    let normal = |value: &Value| match value {
        Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    normal(left) == normal(right)
}
fn level(level: &Value) -> &'static str {
    match level.as_str() {
        Some("measured") => "实测",
        Some("configuration") => "配置推断（非实测）",
        _ => "未知",
    }
}
fn section(section: &Value) -> &'static str {
    match section.as_str() {
        Some("dns") => "DNS 解析",
        Some("trace") => "逐跳测量",
        _ => "连接分段",
    }
}
fn referenced<'a>(events: &'a [Value], evidence: &Value) -> Option<&'a Value> {
    events.iter().find_map(|event| {
        let mut event = event;
        for _ in 0..8 {
            if event["event"] != "project" {
                return None;
            }
            let project = &event["project"];
            if same(&project["project_id"], &evidence["project_id"])
                && same(&project["path_id"], &evidence["path_id"])
                && same(&project["round"], &evidence["round"])
                && same(&project["index"], &evidence["event_index"])
                && same(&project["sampled_at_ms"], &evidence["sampled_at_ms"])
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
    report: &Value,
    selection: &Selection,
    events: &[Value],
) -> Option<Vec<String>> {
    let path = report["topology"].get(selection.path)?;
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let mut lines = vec![format!("路径：{}", text(&path["path_id"]))];
    let mut evidence = Vec::new();
    match &selection.item {
        Item::Node(id) => {
            let node = list(&path["nodes"])
                .into_iter()
                .find(|node| node["id"] == *id.as_str())?;
            lines.push(format!(
                "节点：{} · {}",
                text(&node["label"]),
                text(&node["role"])
            ));
            for edge in list(&path["edges"])
                .into_iter()
                .filter(|edge| edge["from"] == *id.as_str() || edge["to"] == *id.as_str())
            {
                lines.push(format!(
                    "{} · {} · {}",
                    section(&edge["section"]),
                    level(&edge["level"]),
                    text(&edge["description"])
                ));
                evidence.extend(list(&edge["evidence"]));
            }
        }
        Item::Edge(index) => {
            let edge = path["edges"].get(*index)?;
            lines.push(format!(
                "{} · {}",
                section(&edge["section"]),
                level(&edge["level"])
            ));
            lines.push(text(&edge["description"]));
            evidence.extend(list(&edge["evidence"]));
        }
        Item::Attachment(index) => {
            let attachment = path["attachments"].get(*index)?;
            lines.push(format!(
                "旁注（不计作逐跳设备）：{} · {}",
                text(&attachment["kind"]),
                text(&attachment["label"])
            ));
            evidence.extend(list(&attachment["evidence"]));
        }
    }
    if path["truncated"] == true || report["truncated"] == true || report["incomplete"] == true {
        lines.push("证据不完整或已截断；不能据此确认未测路径。".into());
    }
    let mut seen: Vec<Value> = vec![];
    for reference in evidence {
        if seen.contains(&reference) {
            continue;
        }
        seen.push(reference.clone());
        lines.push(format!(
            "项目 {} · 路径 {} · 第 {} 轮 · 事件 {} · 采样 {} ms · 来源 {}",
            text(&reference["project_id"]),
            text(&reference["path_id"]),
            text(&reference["round"]),
            text(&reference["event_index"]),
            text(&reference["sampled_at_ms"]),
            text(&reference["source"])
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
fn chip(label: String, tone: &'static str, cx: &App) -> gpui::Div {
    let color = match tone {
        "warning" => cx.theme().warning,
        "muted" => cx.theme().muted_foreground,
        _ => cx.theme().foreground,
    };
    div()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .text_xs()
        .text_color(color)
        .child(label)
}
fn arrow(level: Option<&str>, cx: &App) -> (String, gpui::Hsla) {
    match level {
        Some("measured") => ("─›".into(), cx.theme().success),
        Some("configuration") => ("┄›".into(), cx.theme().muted_foreground),
        Some(_) => ("?›".into(), cx.theme().warning),
        None => ("›".into(), cx.theme().muted_foreground),
    }
}
impl View {
    fn path_button(id: usize, label: String, selection: Selection, cx: &Context<Self>) -> Button {
        // Long node names remain available in the tooltip and detail pane.
        let short: String = label.chars().take(42).collect();
        let short = if short != label {
            format!("{short}…")
        } else {
            short
        };
        Button::new(("path-item", id))
            .debug_selector(move || format!("network-path-{id}"))
            .label(short)
            .tooltip(label)
            .on_click(cx.listener(move |view, _, _, cx| {
                view.path.selection = Some(selection.clone());
                cx.notify();
            }))
    }
    /// Always visible: the link the selected egress (or the TUN-captured
    /// system route) takes through the instance.
    pub(super) fn live_chain_view(
        &self,
        chain: &astra_client::nettools::LiveChain,
        cx: &App,
    ) -> gpui::Div {
        let mut row = div()
            .id("network-live-chain")
            .debug_selector(|| "network-live-chain".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1();
        for (index, (label, tone)) in chain.steps().into_iter().enumerate() {
            if index > 0 {
                row = row.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("›"),
                );
            }
            row = row.child(chip(label, tone, cx));
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("当前链路"),
            )
            .child(row)
    }
    /// The path the last probe took, as reported by the Agent's topology.
    pub(super) fn path_view(&mut self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let (report, strips, operation) = self.tools(cx, |view| {
            (
                latest_report(&view.details),
                view.path_strips(),
                view.operation_id.clone(),
            )
        })?;
        if self.path.operation != operation {
            self.path.operation = operation;
            self.path.selection = None;
        }
        let report = report?;
        if strips.is_empty() {
            return None;
        }
        let mut root = div().flex().flex_col().gap_2().flex_shrink_0().child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("探测路径 · ─› 实测 · ┄› 配置推断 · ?› 未知 · 点击查看证据"),
        );
        let mut id = 0;
        for (path_index, strip) in strips.iter().enumerate() {
            if strips.len() > 1 || strip.truncated {
                root = root.child(div().text_xs().child(format!(
                    "路径 {}{}",
                    clean(&strip.path_id),
                    if strip.truncated { " · 已截断" } else { "" }
                )));
            }
            for row in &strip.rows {
                let mut line = div().flex().flex_wrap().items_center().gap_1().child(
                    div()
                        .w(px(36.))
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(astra_client::nettools::chain::section_label(&row.section)),
                );
                for step in &row.steps {
                    if let Some(edge) = step.edge {
                        let (symbol, color) = arrow(step.level.as_deref(), cx);
                        let selection = Selection {
                            path: path_index,
                            item: Item::Edge(edge),
                        };
                        line = line.child(
                            Button::new(("path-edge", id))
                                .debug_selector(move || format!("network-path-{id}"))
                                .label(symbol)
                                .text_color(color)
                                .tooltip(astra_client::nettools::chain::level_label(
                                    step.level.as_deref(),
                                ))
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.path.selection = Some(selection.clone());
                                    cx.notify();
                                })),
                        );
                        id += 1;
                    }
                    line = line.child(Self::path_button(
                        id,
                        step.label.clone(),
                        Selection {
                            path: path_index,
                            item: Item::Node(step.id.clone()),
                        },
                        cx,
                    ));
                    id += 1;
                }
                root = root.child(line);
            }
            for (index, attachment) in strip.attachments.iter().enumerate() {
                root = root.child(Self::path_button(
                    id,
                    format!("旁注：{attachment}"),
                    Selection {
                        path: path_index,
                        item: Item::Attachment(index),
                    },
                    cx,
                ));
                id += 1;
            }
        }
        if let Some(selection) = &self.path.selection {
            let lines = self
                .tools(cx, |view| detail(&report, selection, &view.details))
                .flatten();
            if let Some(lines) = lines {
                let mut pane = div()
                    .id("path-detail")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().primary)
                    .max_h(px(280.))
                    .overflow_y_scroll()
                    .child(
                        Button::new("path-close")
                            .label("关闭证据")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.path.selection = None;
                                cx.notify();
                            })),
                    );
                for line in lines {
                    pane = pane.child(div().text_xs().child(line));
                }
                root = root.child(pane);
            }
        }
        Some(root)
    }
}
