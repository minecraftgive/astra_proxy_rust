//! Bounded evidence collection. Only this backend produces findings.
use astra_model::{
    nettool::{DiagnosticContext, DnsDetail, Event},
    network_analysis::*,
    network_tls::TlsDetail,
};
use std::collections::{BTreeMap, BTreeSet};
mod rules;
mod topology;

#[derive(Default)]
pub(super) struct Collector {
    pub(super) projects: BTreeMap<(String, u8), Project>,
    findings: Vec<Finding>,
    topology: BTreeMap<String, TopologyPath>,
    single_index: u32,
    relevant: bool,
    pub(super) incomplete: bool,
    comparison: bool,
    collected_bytes: usize,
    collection_truncated: bool,
}
#[derive(Default)]
pub(super) struct Project {
    context: Option<DiagnosticContext>,
    policy: Option<(PathPolicy, EvidenceRef)>,
    pub(super) dns: BTreeMap<u32, DnsSample>,
    pub(super) tls: BTreeMap<u32, TlsSample>,
    pub(super) ips: Vec<IpSample>,
    connected: Option<EvidenceRef>,
    connected_interface: Option<String>,
    pub(super) complete: bool,
}
#[derive(Default)]
pub(super) struct DnsSample {
    question: String,
    server: String,
    rcode: String,
    records: BTreeSet<String>,
    expected: usize,
    retained: usize,
    valid: bool,
    truncated: bool,
    evidence: Vec<EvidenceRef>,
}
#[derive(Default)]
pub(super) struct TlsSample {
    name: String,
    address: Option<String>,
    fingerprint: Option<String>,
    verified: bool,
    truncated: bool,
    accept_leaf: bool,
    evidence: Vec<EvidenceRef>,
}
pub(super) struct IpSample {
    address: std::net::IpAddr,
    source: String,
    evidence: EvidenceRef,
    observed_path: Option<(Option<String>, Option<String>)>,
}
impl Project {
    fn observed_path(&self) -> Option<(Option<String>, Option<String>)> {
        let proxy = self.policy.as_ref().and_then(|(p, _)| {
            (p.matched_running && p.protocol != "direct" && self.connected.is_some())
                .then(|| p.leaf_tag.clone())
                .flatten()
        });
        (self.connected_interface.is_some() || proxy.is_some())
            .then(|| (self.connected_interface.clone(), proxy))
    }
    fn signature(&self) -> Option<String> {
        let c = self.context.as_ref()?;
        // Exclude query PID and sample time. Include full selection chain,
        // effective binding and core identity so a reload cannot compare equal.
        Some(
            serde_json::json!([
                c.actual_egress,
                c.selection_chain,
                c.effective_interface,
                c.running_core
                    .as_ref()
                    .map(|i| (&i.boot_id, i.applied.as_ref().map(|a| &a.digest))),
                c.config_digest
            ])
            .to_string(),
        )
    }
}
impl Collector {
    pub(super) fn relevant(&self) -> bool {
        self.relevant
    }
    pub(super) fn observe(&mut self, event: &Event) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        self.single_index = self.single_index.saturating_add(1);
        let (project_id, path_id, round, index, sampled_at_ms, event) = match event {
            Event::Project { project, detail } => {
                self.comparison |= project.comparison_requested;
                (
                    project.project_id.clone(),
                    project.path_id.clone(),
                    project.round,
                    project.index,
                    project.sampled_at_ms.clone(),
                    detail.as_ref(),
                )
            }
            Event::ProjectStatus {
                project_id,
                round,
                state,
                ..
            } => {
                if self.projects.len() >= 64
                    && !self.projects.contains_key(&(project_id.clone(), *round))
                {
                    self.incomplete = true;
                    self.collection_truncated = true;
                    return;
                }
                let p = self
                    .projects
                    .entry((project_id.clone(), *round))
                    .or_default();
                p.complete = state == "complete";
                self.incomplete |= matches!(state.as_str(), "failed" | "cancelled" | "skipped");
                return;
            }
            _ => (
                "single".into(),
                "selected".into(),
                1,
                self.single_index,
                now,
                event,
            ),
        };
        if self.projects.len() >= 64 && !self.projects.contains_key(&(project_id.clone(), round)) {
            self.incomplete = true;
            self.collection_truncated = true;
            return;
        }
        // Bound retained input across all projects, rather than merely limiting
        // the final JSON. DER and HTTP bodies are not retained by this collector.
        if !matches!(event, Event::Tls { detail, .. } if matches!(detail.as_ref(), TlsDetail::CertificateDer { .. }))
            && matches!(
                event,
                Event::Context(_)
                    | Event::Policy(_)
                    | Event::Dns { .. }
                    | Event::Tls { .. }
                    | Event::Resolution { .. }
                    | Event::Connection { .. }
                    | Event::PublicIp { .. }
                    | Event::Stun(_)
                    | Event::Nat(_)
                    | Event::Trace { .. }
                    | Event::TraceSummary(_)
                    | Event::Echo { .. }
            )
        {
            let mut bytes = ByteCount::default();
            if serde_json::to_writer(&mut bytes, event).is_err() {
                self.incomplete = true;
                return;
            }
            self.collected_bytes = self.collected_bytes.saturating_add(bytes.0);
            if self.collected_bytes > 2 * 1024 * 1024 {
                self.incomplete = true;
                self.collection_truncated = true;
                return;
            }
        }
        self.relevant |= matches!(
            event,
            Event::Context(_)
                | Event::Policy(_)
                | Event::Dns { .. }
                | Event::Tls { .. }
                | Event::Resolution { .. }
                | Event::Connection { .. }
                | Event::PublicIp { .. }
                | Event::Stun(_)
                | Event::Nat(_)
                | Event::Trace { .. }
                | Event::TraceSummary(_)
                | Event::Echo { .. }
        );
        let source = match event {
            Event::Context(_) | Event::Policy(_) => "manager_configuration",
            Event::Dns { .. } => "dns_response",
            Event::Resolution { .. } => "resolver_boundary",
            Event::Tls { .. } => "tls_verifier",
            Event::Connection { .. } => "connection",
            Event::PublicIp { .. } => "https_service",
            Event::Stun(_) => "stun_response",
            Event::Nat(_) => "nat_session",
            Event::Trace { .. } | Event::TraceSummary(_) => "native_path_probe",
            _ => "diagnostic_event",
        };
        let evidence = EvidenceRef {
            project_id: project_id.clone(),
            path_id,
            round,
            event_index: index,
            sampled_at_ms,
            source: source.into(),
        };
        self.topology_event(event, &evidence);
        let p = self.projects.entry((project_id, round)).or_default();
        match event {
            Event::Context(context) => p.context = Some((**context).clone()),
            Event::Policy(policy) => p.policy = Some((policy.clone(), evidence)),
            Event::Connection {
                seq,
                purpose,
                address,
                interface,
                ..
            } => {
                if purpose == "business_target" {
                    p.connected_interface = interface.clone();
                    p.connected = Some(evidence.clone());
                }
                if p.tls.len() < 64 || p.tls.contains_key(seq) {
                    // Auxiliary DNS TLS, redirects and the target may share seq.
                    // A new transport must never inherit a previous certificate.
                    p.tls.insert(
                        *seq,
                        TlsSample {
                            address: address
                                .as_ref()
                                .and_then(|a| endpoint_ip(a))
                                .map(|a| a.to_string()),
                            evidence: vec![evidence],
                            ..Default::default()
                        },
                    );
                } else {
                    self.incomplete = true;
                    self.collection_truncated = true;
                }
            }
            Event::Dns { seq, detail } if p.dns.len() < 64 || p.dns.contains_key(seq) => {
                let dns = p.dns.entry(*seq).or_default();
                match detail.as_ref() {
                    DnsDetail::Response {
                        transport,
                        server,
                        response_received,
                        parsed,
                        rcode,
                        questions,
                        answer_count,
                        authority_count,
                        additional_count,
                        truncated,
                        flags,
                        ..
                    } => {
                        // A new response starts a new sample (e.g. TCP retry).
                        *dns = DnsSample::default();
                        dns.valid = *response_received && *parsed && !questions.is_empty();
                        dns.truncated |= *truncated || flags.truncated;
                        dns.question = serde_json::to_string(questions)
                            .unwrap_or_default()
                            .to_ascii_lowercase();
                        dns.server = server.clone();
                        dns.rcode = rcode.clone();
                        dns.expected = (*answer_count as usize)
                            .saturating_add(*authority_count as usize)
                            .saturating_add(*additional_count as usize);
                        if *response_received && matches!(transport.as_str(), "udp" | "tcp") {
                            self.findings.push(rules::finding(FindingKind::PlaintextDns, vec![evidence.clone()], format!("DNS 请求通过 {transport} 到达 {server}。明文可见范围仅限该 DNS 传输链路段；代理隧道内外需分别看路径。")));
                        }
                    }
                    DnsDetail::Records { section, records } => {
                        dns.retained = dns.retained.saturating_add(records.len());
                        for record in records {
                            dns.truncated |= record.text_truncated;
                            // TTL changes and record ordering alone are not a DNS difference.
                            if section == "answer" {
                                let text = if matches!(
                                    record.record_type.as_str(),
                                    "CNAME" | "NS" | "PTR" | "DNAME" | "MX" | "SRV" | "SOA"
                                ) {
                                    record.text.to_ascii_lowercase()
                                } else {
                                    record.text.clone()
                                };
                                let value = format!(
                                    "{}|{}|{}|{}",
                                    record.name.to_ascii_lowercase(),
                                    record.record_type,
                                    record.class,
                                    text
                                );
                                if dns.records.len() < 256 || dns.records.contains(&value) {
                                    dns.records.insert(value);
                                } else {
                                    dns.truncated = true;
                                    self.collection_truncated = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
                push_ref(&mut dns.evidence, evidence);
            }
            Event::Tls { seq, detail } if p.tls.len() < 64 || p.tls.contains_key(seq) => {
                let tls = p.tls.entry(*seq).or_default();
                match detail.as_ref() {
                    TlsDetail::Handshake {
                        server_name,
                        verified,
                        system_chain_verified,
                        verification_attempted,
                        verification_error,
                        truncated,
                        bundled_comparison,
                        received_certificates,
                        retained_certificates,
                        ..
                    } => {
                        // Handshake precedes its leaf certificate. Clear stale
                        // evidence even when this handshake receives no chain.
                        tls.fingerprint = None;
                        tls.evidence.retain(|e| e.source == "connection");
                        tls.accept_leaf = *received_certificates > 0 && *retained_certificates > 0;
                        tls.name = server_name.to_ascii_lowercase();
                        tls.verified = *verified;
                        tls.truncated = *truncated;
                        if *verification_attempted {
                            if let Some(error) = verification_error {
                                self.findings.push(rules::finding(
                                    FindingKind::TlsVerificationFailed,
                                    vec![evidence.clone()],
                                    format!(
                                        "证书或握手签名验证器明确返回错误：{}",
                                        bounded(error, 1500)
                                    ),
                                ));
                            }
                            if let Some(other) = bundled_comparison {
                                if !truncated
                                    && other.offline
                                    && system_chain_verified
                                        .is_some_and(|system| system != other.verified)
                                {
                                    self.findings.push(rules::finding(FindingKind::TrustStoreDifference, vec![evidence.clone()], "同一证书链在系统根证书快照与内置根证书下的离线验证结果不同。".into()));
                                }
                            }
                        }
                    }
                    TlsDetail::Certificate {
                        index: 0,
                        sha256,
                        metadata_truncated,
                        ..
                    } if tls.accept_leaf => {
                        tls.fingerprint = Some(sha256.clone());
                        tls.truncated |= *metadata_truncated;
                    }
                    _ => {}
                }
                if !matches!(detail.as_ref(), TlsDetail::CertificateDer { .. }) {
                    push_ref(&mut tls.evidence, evidence);
                }
            }
            Event::Resolution {
                method,
                interface,
                resolver,
                addresses,
                ..
            } => {
                if !matches!(
                    method.as_str(),
                    "literal" | "explicit_bootstrap" | "remote_proxy"
                ) {
                    if let Some(expected) = p
                        .context
                        .as_ref()
                        .and_then(|c| c.effective_interface.as_ref())
                    {
                        if interface.as_ref().is_some_and(|actual| actual != expected) {
                            self.findings.push(rules::finding(
                                FindingKind::ResolutionPathMismatch,
                                vec![evidence.clone()],
                                format!(
                                    "声明绑定 {expected}，实际解析记录为 {method} / {}。",
                                    interface.as_deref().unwrap_or("未记录网卡")
                                ),
                            ));
                        }
                    }
                    // A completed constrained DNS resolution also exposes its
                    // plaintext resolver segment, even for TLS/HTTP tools.
                    if !addresses.is_empty()
                        && resolver.as_ref().is_some_and(|r| {
                            r.starts_with("udp://")
                                || r.starts_with("tcp://")
                                || r.parse::<std::net::SocketAddr>().is_ok()
                                || r.parse::<std::net::IpAddr>().is_ok()
                        })
                    {
                        self.findings.push(rules::finding(
                            FindingKind::PlaintextDns,
                            vec![evidence],
                            "已执行的辅助解析使用明文 DNS；见解析器边界记录中的服务器和绑定网卡。"
                                .into(),
                        ));
                    }
                }
            }
            Event::PublicIp {
                address, source, ..
            } => {
                if let Ok(address) = address.parse() {
                    if p.ips.len() < 64 {
                        p.ips.push(IpSample {
                            address,
                            source: source.clone(),
                            evidence,
                            observed_path: p.observed_path(),
                        });
                    }
                }
            }
            Event::Stun(stun) => {
                if stun.observation.response_received && stun.observation.error.is_none() {
                    if stun.proxy_session {
                        p.connected = Some(evidence.clone());
                    }
                    if let Some(address) = stun
                        .observation
                        .mapped_endpoint
                        .as_ref()
                        .and_then(|a| endpoint_ip(a))
                    {
                        if p.ips.len() < 64 {
                            p.ips.push(IpSample {
                                address,
                                source: "stun".into(),
                                evidence: evidence.clone(),
                                observed_path: p.observed_path(),
                            });
                        }
                    }
                }
            }
            Event::Dns { .. } | Event::Tls { .. } => {
                self.incomplete = true;
                self.collection_truncated = true;
            }
            _ => {}
        }
        // Limits apply while collecting, not only when serializing the report.
        if self.findings.len() > 128 {
            self.findings.truncate(128);
            self.incomplete = true;
        }
    }
    pub(super) fn finish(mut self, incomplete: bool) -> AnalysisReport {
        self.incomplete |= incomplete;
        if let Some(single) = self.projects.get_mut(&("single".into(), 1)) {
            single.complete = !incomplete;
        }
        self.derive_rules();
        let mut seen = BTreeSet::new();
        self.findings.retain(|f| {
            seen.insert(format!(
                "{:?}:{}",
                f.kind,
                f.evidence.first().map_or("", |e| e.path_id.as_str())
            ))
        });
        if self.findings.len() > 64 {
            self.findings.truncate(64);
            self.incomplete = true;
        }
        let rounds_complete = self.projects.keys().any(|(_, round)| *round == 2)
            && !self.incomplete
            && self.projects.values().all(|p| p.complete)
            && self
                .projects
                .keys()
                .filter(|(_, round)| *round == 2)
                .all(|(id, _)| {
                    self.projects
                        .get(&(id.clone(), 1))
                        .is_some_and(|p| p.complete)
                });
        let mut report = AnalysisReport {
            rules_version: RULES_VERSION,
            comparison_requested: self.comparison,
            comparison_status: if !self.comparison {
                "未执行对照；不推断 DNS 或证书无差异"
            } else if rounds_complete {
                "已采集对照；只有同名、时间及路径证据满足规则时产生发现"
            } else {
                "对照证据不完整或超时；不能推断无差异"
            }
            .into(),
            findings: self.findings,
            topology: self.topology.into_values().collect(),
            visibility: [
                (
                    "网关",
                    "通常可见连接的目的地址、端口、时序与流量大小；加密内容取决于协议。",
                ),
                (
                    "接入网络",
                    "可见经过该网络的连接元数据；明文协议内容可能可见。",
                ),
                (
                    "DNS 服务",
                    "可见向它提交的查询名称；是否加密影响中间链路的可见范围。",
                ),
                (
                    "代理",
                    "可见入口来源及转发目标；应用层端到端加密仍独立验证。",
                ),
                ("目标服务", "可见请求内容及该协议实际抵达服务的出口地址。"),
            ]
            .into_iter()
            .map(|(location, information)| ObserverVisibility {
                location: location.into(),
                information: information.into(),
            })
            .collect(),
            incomplete: self.incomplete,
            truncated: self.collection_truncated,
            notes: vec![
                "可信程度描述证据支持程度，严重程度描述影响；均不代表被监控概率。".into(),
                "普通 NAT、私有地址、单次超时或差异不证明监听发生；没有发现也不证明不存在观察者。"
                    .into(),
            ],
        };
        // One report item must fit a 64-KiB page, including decimal-string
        // counters and the envelope. Keep findings before topology detail.
        while serde_json::to_vec(&report).map_or(true, |bytes| bytes.len() > 52 * 1024) {
            report.truncated = true;
            report.incomplete = true;
            if let Some(path) = report
                .topology
                .iter_mut()
                .filter(|p| !p.edges.is_empty())
                .max_by_key(|p| p.edges.len())
            {
                path.edges.pop();
                path.truncated = true;
                path.nodes
                    .retain(|n| path.edges.iter().any(|e| e.from == n.id || e.to == n.id));
            } else if !report.topology.is_empty() {
                report.topology.pop();
            } else if !report.findings.is_empty() {
                report.findings.pop();
            } else {
                break;
            }
        }
        report.truncated |= report.topology.iter().any(|p| p.truncated);
        report.incomplete |= report.truncated;
        report
    }
}
// Preserve both temporal extremes, including malformed timestamps, after the
// reference cap. Otherwise a late certificate/record could pass a 30s window.
fn push_ref(refs: &mut Vec<EvidenceRef>, evidence: EvidenceRef) {
    refs.push(evidence);
    if refs.len() > 8 {
        refs.sort_by_key(|e| e.sampled_at_ms.parse::<u128>().unwrap_or(u128::MAX));
        refs.remove(1);
    }
}
#[derive(Default)]
struct ByteCount(usize);
impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
fn endpoint_ip(value: &str) -> Option<std::net::IpAddr> {
    value
        .parse()
        .ok()
        .or_else(|| value.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()))
}

#[cfg(test)]
#[path = "../test/network_analysis_test.rs"]
mod tests;

#[cfg(test)]
#[path = "../test/network_analysis_audit_test.rs"]
mod audit_tests;
