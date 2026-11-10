//! Network link display. The live chain is derived from the instance runtime
//! snapshot and the target Agent's network state; the probe path is a linear
//! reading of the backend-generated `AnalysisReport.topology`. Nothing here
//! invents devices: unknown segments stay unknown.
use astra_model::nettool::Egress;
use serde::Serialize;
use serde_json::Value;

/// Same bound as the manager's pinned selection rebuild.
pub const MAX_HOPS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capture {
    Tun,
    SystemProxy,
    Both,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupHop {
    pub group: String,
    pub mode: String,
    pub selected: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveChain {
    /// `rule`, `global` or `direct`; `None` when the runtime is unavailable.
    pub routing: Option<String>,
    pub capture: Capture,
    /// Explicit adapter egress, otherwise the target Agent's default-route adapter.
    pub interface: Option<String>,
    /// `requested` for an adapter egress, `default_route` for the routing table.
    pub interface_source: Option<String>,
    /// True when the chain is the instance's own rule-mode default path, so
    /// rules may still send individual connections elsewhere.
    pub via_rules: bool,
    pub hops: Vec<GroupHop>,
    pub leaf: Option<String>,
    pub issue: Option<String>,
}

fn interface_name(network: &Value, id: &str) -> String {
    network["interfaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|interface| interface["id"] == id)
        .and_then(|interface| interface["name"].as_str())
        .unwrap_or(id)
        .to_owned()
}

fn default_interface(network: &Value) -> Option<String> {
    let interfaces = network["interfaces"].as_array()?;
    let virtual_ids: Vec<&str> = interfaces
        .iter()
        .filter(|i| i["kind"] == "virtual" || i["kind"] == "loopback")
        .filter_map(|i| i["id"].as_str())
        .collect();
    let defaults: Vec<&str> = network["routes"]
        .as_array()?
        .iter()
        .filter(|route| {
            matches!(
                route["destination"].as_str(),
                Some("default" | "0.0.0.0/0" | "::/0" | "0.0.0.0")
            )
        })
        .filter_map(|route| route["interface_id"].as_str())
        .collect();
    // Prefer a non-virtual default route: with TUN enabled the first default
    // route is usually the TUN adapter itself, which is the capture stage.
    defaults
        .iter()
        .find(|id| !virtual_ids.contains(id))
        .or(defaults.first())
        .map(|id| interface_name(network, id))
}

/// `runtime` is the `Query::Runtime` client value, `network` the
/// `Query::NetworkState` snapshot; both are already polled by the page.
pub fn live_chain(runtime: Option<&Value>, network: Option<&Value>, egress: &Egress) -> LiveChain {
    let mut chain = LiveChain {
        routing: None,
        capture: Capture::None,
        interface: None,
        interface_source: None,
        via_rules: false,
        hops: vec![],
        leaf: None,
        issue: None,
    };
    match egress {
        Egress::Interface(id) => {
            chain.interface = Some(network.map_or_else(|| id.clone(), |n| interface_name(n, id)));
            chain.interface_source = Some("requested".into());
        }
        _ => {
            chain.interface = network.and_then(default_interface);
            chain.interface_source = chain.interface.as_ref().map(|_| "default_route".into());
        }
    }
    let Some(runtime) = runtime else {
        if !matches!(egress, Egress::Interface(_)) {
            chain.issue = Some("实例未运行或运行快照不可用".into());
        }
        return chain;
    };
    chain.routing = runtime["mode"].as_str().map(str::to_owned);
    chain.capture = match (runtime["tun"] == true, runtime["system_proxy"] == true) {
        (true, true) => Capture::Both,
        (true, false) => Capture::Tun,
        (false, true) => Capture::SystemProxy,
        (false, false) => Capture::None,
    };
    let start = match egress {
        // Adapter probes are bound below TUN; system-route probes only enter
        // the proxy core when TUN captures them.
        Egress::Interface(_) => {
            chain.capture = Capture::None;
            return chain;
        }
        Egress::System if !matches!(chain.capture, Capture::Tun | Capture::Both) => return chain,
        Egress::System => match instance_egress(Some(runtime)) {
            Egress::Outbound(tag) => {
                chain.via_rules = chain.routing.as_deref() == Some("rule");
                tag
            }
            _ => return chain,
        },
        Egress::Outbound(tag) => tag.clone(),
    };
    let groups = runtime["groups"].as_array().cloned().unwrap_or_default();
    let group = |name: &str| groups.iter().find(|g| g["name"] == name);
    // The instance default path is named by convention, not configuration: a
    // missing `PROXY` group is reported instead of shown as an exit node.
    let assumed = matches!(egress, Egress::System);
    let mut current = start;
    let mut seen = std::collections::HashSet::new();
    loop {
        let Some(found) = group(&current) else {
            if assumed && chain.hops.is_empty() && current != "DIRECT" {
                chain.issue = Some(format!("未找到默认组 {current}；实际出口由规则决定"));
            } else {
                chain.leaf = Some(current);
            }
            break;
        };
        if !seen.insert(current.clone()) || chain.hops.len() >= MAX_HOPS {
            chain.issue = Some(format!("选择链循环或超过 {MAX_HOPS} 层"));
            break;
        }
        let Some(selected) = found["selected"].as_str().filter(|s| !s.is_empty()) else {
            chain.issue = Some(format!("{current} 无选择"));
            break;
        };
        chain.hops.push(GroupHop {
            group: current.clone(),
            mode: found["mode"].as_str().unwrap_or_default().to_owned(),
            selected: selected.to_owned(),
        });
        current = selected.to_owned();
    }
    if runtime["truncated"] == true && chain.issue.is_none() {
        chain.issue = Some("运行快照已截断，选择链可能不完整".into());
    }
    chain
}

/// The egress the instance itself uses for proxied traffic, for the always
/// visible chain: Global → `PROXY`, Direct → `DIRECT`, Rule → rules then `PROXY`.
pub fn instance_egress(runtime: Option<&Value>) -> Egress {
    match runtime.and_then(|r| r["mode"].as_str()) {
        Some("direct") => Egress::Outbound("DIRECT".into()),
        Some(_) => Egress::Outbound("PROXY".into()),
        None => Egress::System,
    }
}

impl LiveChain {
    /// Display steps shared by all frontends: (label, tone) where tone is
    /// `normal`, `muted` (configuration) or `warning`.
    pub fn steps(&self) -> Vec<(String, &'static str)> {
        // Left to right in traffic order; the physical adapter is the last
        // local stage before the network.
        let mut steps = vec![("系统".to_owned(), "normal")];
        match self.capture {
            Capture::Tun => steps.push(("TUN".into(), "normal")),
            Capture::SystemProxy => steps.push(("系统代理".into(), "muted")),
            Capture::Both => steps.push(("TUN + 系统代理".into(), "normal")),
            Capture::None => {}
        }
        if self.via_rules {
            steps.push(("规则".into(), "muted"));
        }
        for hop in &self.hops {
            steps.push((format!("{} → {}", hop.group, hop.selected), "normal"));
        }
        if let Some(leaf) = &self.leaf {
            steps.push((leaf.clone(), "normal"));
        }
        if let Some(interface) = &self.interface {
            let source = match self.interface_source.as_deref() {
                Some("requested") => "指定网卡",
                _ => "默认路由",
            };
            steps.push((format!("网卡 {interface}（{source}）"), "normal"));
        }
        if let Some(issue) = &self.issue {
            steps.push((issue.clone(), "warning"));
        }
        steps
    }
    pub fn text(&self) -> String {
        self.steps()
            .into_iter()
            .map(|(label, _)| label)
            .collect::<Vec<_>>()
            .join(" › ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StripStep {
    pub id: String,
    pub label: String,
    pub role: String,
    /// Evidence level of the edge leading to this step (`None` for the first).
    pub level: Option<String>,
    /// Index into the topology path's `edges` for evidence lookup.
    pub edge: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StripRow {
    /// `connection`, `dns` or `trace`.
    pub section: String,
    pub steps: Vec<StripStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PathStrip {
    pub path_id: String,
    pub rows: Vec<StripRow>,
    /// NAT and other side notes; never counted as hops.
    pub attachments: Vec<String>,
    pub truncated: bool,
}

/// Linearise each topology path into chains per section. A chain starts at a
/// node with no incoming edge in that section and follows the first unused
/// outgoing edge; remaining edges start further rows, so nothing is dropped.
pub fn path_strips(analysis: &Value) -> Vec<PathStrip> {
    analysis["topology"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|path| {
            let nodes = path["nodes"].as_array().cloned().unwrap_or_default();
            let edges = path["edges"].as_array().cloned().unwrap_or_default();
            let node = |id: &str| nodes.iter().find(|n| n["id"] == id);
            let step = |id: &str, level: Option<String>, edge: Option<usize>| StripStep {
                id: id.to_owned(),
                label: node(id)
                    .and_then(|n| n["label"].as_str())
                    .unwrap_or(id)
                    .to_owned(),
                role: node(id)
                    .and_then(|n| n["role"].as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                level,
                edge,
            };
            let mut rows = vec![];
            for section in ["connection", "dns", "trace"] {
                let indices: Vec<usize> = (0..edges.len())
                    .filter(|&i| edges[i]["section"] == section)
                    .collect();
                let mut used = vec![false; edges.len()];
                loop {
                    let remaining: Vec<usize> =
                        indices.iter().copied().filter(|&i| !used[i]).collect();
                    let Some(&first) = remaining
                        .iter()
                        .find(|&&i| {
                            !remaining
                                .iter()
                                .any(|&j| edges[j]["to"] == edges[i]["from"])
                        })
                        .or(remaining.first())
                    else {
                        break;
                    };
                    let from = edges[first]["from"].as_str().unwrap_or_default();
                    let mut steps = vec![step(from, None, None)];
                    let mut current = first;
                    loop {
                        used[current] = true;
                        let to = edges[current]["to"].as_str().unwrap_or_default();
                        steps.push(step(
                            to,
                            edges[current]["level"].as_str().map(str::to_owned),
                            Some(current),
                        ));
                        match indices
                            .iter()
                            .copied()
                            .find(|&i| !used[i] && edges[i]["from"] == to)
                        {
                            Some(next) if steps.len() <= 64 => current = next,
                            _ => break,
                        }
                    }
                    rows.push(StripRow {
                        section: section.into(),
                        steps,
                    });
                }
            }
            PathStrip {
                path_id: path["path_id"].as_str().unwrap_or_default().to_owned(),
                rows,
                attachments: path["attachments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| {
                        format!(
                            "{}：{}",
                            a["kind"].as_str().unwrap_or("旁注"),
                            a["label"].as_str().unwrap_or_default()
                        )
                    })
                    .collect(),
                truncated: path["truncated"] == true,
            }
        })
        .collect()
}

pub fn level_label(level: Option<&str>) -> &'static str {
    match level {
        Some("measured") => "实测",
        Some("configuration") => "配置推断",
        Some(_) => "未知",
        None => "",
    }
}

pub fn section_label(section: &str) -> &'static str {
    match section {
        "dns" => "DNS",
        "trace" => "逐跳",
        _ => "连接",
    }
}

impl StripRow {
    pub fn text(&self) -> String {
        let mut text = format!("[{}] ", section_label(&self.section));
        for (index, step) in self.steps.iter().enumerate() {
            if index > 0 {
                text.push_str(match step.level.as_deref() {
                    Some("measured") => " ─› ",
                    Some("configuration") => " ┄› ",
                    _ => " ?› ",
                });
            }
            text.push_str(&step.label);
        }
        text
    }
}
