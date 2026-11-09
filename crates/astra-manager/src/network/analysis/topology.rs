use super::*;
use astra_model::network_path::PathOutcome;
use sha2::{Digest, Sha256};
impl Collector {
    pub(super) fn topology_event(&mut self, event: &Event, reference: &EvidenceRef) {
        if self.topology.len() >= 3 && !self.topology.contains_key(&reference.path_id) {
            self.incomplete = true;
            return;
        }
        let path = self
            .topology
            .entry(reference.path_id.clone())
            .or_insert_with(|| TopologyPath {
                path_id: reference.path_id.clone(),
                nodes: vec![],
                edges: vec![],
                attachments: vec![],
                truncated: false,
            });
        let agent = node(path, "agent", "目标 Agent", "agent");
        match event {
            Event::Context(context) => {
                let mut previous = agent.clone();
                if let Some(interface) = &context.effective_interface {
                    let id = node(
                        path,
                        &format!("interface:{interface}"),
                        interface,
                        "interface",
                    );
                    edge(
                        path,
                        &previous,
                        &id,
                        PathSection::Connection,
                        EvidenceLevel::Configuration,
                        "声明绑定网卡；实际发包证据单独记录",
                        reference,
                    );
                    previous = id;
                }
                for hop in &context.selection_chain {
                    let id = node(
                        path,
                        &format!("group:{}:{}", hop.group, hop.selected),
                        &format!("{} → {}", hop.group, hop.selected),
                        "proxy_group",
                    );
                    edge(
                        path,
                        &previous,
                        &id,
                        PathSection::Connection,
                        EvidenceLevel::Configuration,
                        &format!("{} / {}", hop.mode, hop.source),
                        reference,
                    );
                    previous = id;
                }
                if let astra_model::nettool::Egress::Outbound(tag) = &context.actual_egress {
                    let id = node(path, &format!("outbound:{tag}"), tag, "configured_egress");
                    edge(
                        path,
                        &previous,
                        &id,
                        PathSection::Connection,
                        EvidenceLevel::Configuration,
                        "本次固定的配置出口；不代表路径中存在同名设备",
                        reference,
                    );
                }
            }
            Event::Resolution {
                purpose,
                host,
                method,
                resolver,
                addresses,
                ..
            } => {
                let label = resolver.as_deref().unwrap_or(if method == "system" {
                    "系统解析器（内部路径不可见）"
                } else if method.contains("proxy") {
                    "代理远端解析（内部路径不可见）"
                } else {
                    "IP / 引导地址"
                });
                let literal = matches!(method.as_str(), "literal" | "explicit_bootstrap");
                let id = node(
                    path,
                    &format!("resolver:{purpose}:{label}"),
                    label,
                    if literal { "address_input" } else { "resolver" },
                );
                let level = if literal {
                    EvidenceLevel::Configuration
                } else if matches!(method.as_str(), "explicit_resolver" | "interface_resolver")
                    && !addresses.is_empty()
                    && resolver.is_some()
                {
                    EvidenceLevel::Measured
                } else {
                    EvidenceLevel::Unknown
                };
                edge(
                    path,
                    &agent,
                    &id,
                    PathSection::Dns,
                    level,
                    &format!("{purpose}: {host} · {method}"),
                    reference,
                );
            }
            Event::Dns { detail, .. } => {
                if let DnsDetail::Response {
                    server,
                    transport,
                    response_received: true,
                    ..
                } = detail.as_ref()
                {
                    let id = node(path, &format!("dns:{server}"), server, "dns_service");
                    edge(
                        path,
                        &agent,
                        &id,
                        PathSection::Dns,
                        EvidenceLevel::Measured,
                        &format!("收到 {transport} DNS 响应；中间链路不由此推断"),
                        reference,
                    );
                }
            }
            Event::Connection {
                purpose,
                host,
                address,
                address_source,
                ..
            } => {
                let label = address.as_deref().unwrap_or(host);
                let target = node(
                    path,
                    &format!("connection:{purpose}:{host}:{label}"),
                    label,
                    if purpose == "proxy_entry" {
                        "proxy_entry"
                    } else {
                        "target"
                    },
                );
                if address_source.starts_with("proxy_") || address_source == "remote_dns_unknown" {
                    let gap = node(path, "proxy-gap", "代理之后的不可见路径", "unknown");
                    edge(
                        path,
                        &agent,
                        &gap,
                        PathSection::Connection,
                        EvidenceLevel::Unknown,
                        "代理传输完成；不生成代理之后的中间节点",
                        reference,
                    );
                    edge(
                        path,
                        &gap,
                        &target,
                        PathSection::Connection,
                        EvidenceLevel::Unknown,
                        "代理确认目标连接；目标内部解析/转发路径可能不可见",
                        reference,
                    );
                } else {
                    edge(
                        path,
                        &agent,
                        &target,
                        PathSection::Connection,
                        if address.as_ref().and_then(|a| endpoint_ip(a)).is_some() {
                            EvidenceLevel::Measured
                        } else {
                            EvidenceLevel::Unknown
                        },
                        &format!("{purpose}: 实际 socket 对端 ({address_source})；不是逐跳拓扑"),
                        reference,
                    );
                }
            }
            Event::Trace { sample, .. } => {
                let label = sample
                    .address
                    .clone()
                    .unwrap_or_else(|| format!("TTL {} 未响应", sample.hop));
                let id = node(
                    path,
                    &format!("ttl:{}:{label}", sample.hop),
                    &label,
                    "ttl_sample",
                );
                let measured = matches!(
                    sample.outcome,
                    PathOutcome::Reply | PathOutcome::TimeExceeded | PathOutcome::Unreachable
                ) && sample.address.is_some();
                edge(
                    path,
                    &agent,
                    &id,
                    PathSection::Trace,
                    if measured {
                        EvidenceLevel::Measured
                    } else {
                        EvidenceLevel::Unknown
                    },
                    &format!(
                        "TTL {} 第 {} 次独立采样；不推断相邻设备连线",
                        sample.hop, sample.probe
                    ),
                    reference,
                );
            }
            Event::TraceSummary(summary) if !summary.capability.supported || !summary.completed => {
                attachment(path, "path_limit", &summary.message, reference);
            }
            Event::Nat(astra_model::network_nat::Event::Report { report }) => {
                attachment(
                    path,
                    "nat",
                    &format!(
                        "网卡 {}；映射 {:?}；过滤 {:?}；所测目标间 {:?}。不推断 NAT 设备数量。",
                        report.interface_name, report.mapping, report.filtering, report.comparison
                    ),
                    reference,
                );
            }
            Event::Echo { observation, .. }
                if observation.tun_local || !observation.proves_reachability =>
            {
                attachment(path, "icmp_limit", &observation.message, reference);
            }
            _ => {}
        }
    }
}
fn node(path: &mut TopologyPath, id: &str, label: &str, role: &str) -> String {
    // Truncating an identifier can merge different endpoints sharing a prefix.
    let id = if id.len() > 512 {
        format!("node:{:x}", Sha256::digest(id.as_bytes()))
    } else {
        id.to_owned()
    };
    if !path.nodes.iter().any(|n| n.id == id) {
        if path.nodes.len() >= 96 {
            path.truncated = true;
            return "unretained".into();
        }
        path.nodes.push(TopologyNode {
            id: id.clone(),
            label: bounded(label, 512),
            role: role.into(),
        });
    }
    id
}
fn edge(
    path: &mut TopologyPath,
    from: &str,
    to: &str,
    section: PathSection,
    level: EvidenceLevel,
    description: &str,
    evidence: &EvidenceRef,
) {
    if from == "unretained" || to == "unretained" {
        return;
    }
    let description = bounded(description, 1024);
    if let Some(edge) = path.edges.iter_mut().find(|e| {
        e.from == from
            && e.to == to
            && e.section == section
            && e.level == level
            && e.description == description
    }) {
        push_ref(&mut edge.evidence, evidence.clone());
        return;
    }
    if path.edges.len() >= 128 {
        path.truncated = true;
        return;
    }
    path.edges.push(TopologyEdge {
        from: from.into(),
        to: to.into(),
        section,
        level,
        description,
        evidence: vec![evidence.clone()],
    });
}
fn attachment(path: &mut TopologyPath, kind: &str, label: &str, evidence: &EvidenceRef) {
    if path.attachments.len() >= 16 {
        path.truncated = true;
        return;
    }
    path.attachments.push(TopologyAttachment {
        kind: kind.into(),
        label: bounded(label, 1500),
        evidence: vec![evidence.clone()],
    });
}
