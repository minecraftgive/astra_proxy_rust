use super::*;

impl Collector {
    pub(super) fn derive_rules(&mut self) {
        let mut found = vec![];
        for project in self.projects.values() {
            if let Some((policy, reference)) = &project.policy {
                if policy.matched_running {
                    if policy.certificate_verification == Some(false) {
                        found.push(finding(
                            FindingKind::CertificateVerificationDisabled,
                            vec![reference.clone()],
                            "当前生效配置明确关闭了该代理段的证书验证。".into(),
                        ));
                    }
                    if policy.encrypted == Some(false) {
                        if let Some(connected) = &project.connected {
                            found.push(finding(FindingKind::PlaintextProxySegment, vec![reference.clone(), connected.clone()], format!("当前配置与成功的实际连接共同确认 {} 代理协议层未加密；这不代表应用 TLS 已被解密。", policy.protocol)));
                        }
                    }
                }
            }
        }
        let entries: Vec<_> = self.projects.iter().collect();
        let mut comparisons = 65_536usize;
        'pairs: for (i, (left_key, left)) in entries.iter().enumerate() {
            for (right_key, right) in entries.iter().skip(i + 1) {
                if found.len() >= 128 {
                    self.incomplete = true;
                    self.collection_truncated = true;
                    break 'pairs;
                }
                if left_key.1 != right_key.1 {
                    continue;
                }
                if !self.compare_ips(left, right, &mut found, &mut comparisons) {
                    self.incomplete = true;
                    self.collection_truncated = true;
                    break 'pairs;
                }
                if self.incomplete
                    || !self.comparison
                    || left_key.1 != 1
                    || !left.complete
                    || !right.complete
                {
                    continue;
                }
                let left_signature = left.signature();
                let right_signature = right.signature();
                let (Some(left2), Some(right2)) = (
                    self.projects.get(&(left_key.0.clone(), 2)),
                    self.projects.get(&(right_key.0.clone(), 2)),
                ) else {
                    continue;
                };
                if !left2.complete
                    || !right2.complete
                    || left_signature.is_none()
                    || right_signature.is_none()
                    || left_signature != left2.signature()
                    || right_signature != right2.signature()
                {
                    continue;
                }
                'dns: for (seq, a) in &left.dns {
                    for (other_seq, b) in &right.dns {
                        if !spend(&mut comparisons) {
                            self.incomplete = true;
                            self.collection_truncated = true;
                            break 'pairs;
                        }
                        let (Some(a2), Some(b2)) = (left2.dns.get(seq), right2.dns.get(other_seq))
                        else {
                            continue;
                        };
                        let samples = [a, b, a2, b2];
                        if !samples.iter().all(|s| {
                            s.valid
                                && !s.truncated
                                && s.retained == s.expected
                                && !s.question.is_empty()
                                && s.question == a.question
                        }) {
                            continue;
                        }
                        if a.server != a2.server || b.server != b2.server {
                            continue;
                        }
                        if a.server == b.server && left_signature == right_signature {
                            continue;
                        }
                        let different = |x: &DnsSample, y: &DnsSample| {
                            x.rcode != y.rcode || x.records != y.records
                        };
                        if !different(a, b) || !different(a2, b2) {
                            continue;
                        }
                        let evidence = refs(samples.iter().flat_map(|s| s.evidence.iter()));
                        if within_window(&evidence) {
                            found.push(finding(FindingKind::DnsDifference, evidence, "同名同类型在两条对照解析路径上连续两轮结果不同，采样均在 30 秒内；已忽略 TTL 和记录排序差异。".into()));
                            break 'dns;
                        }
                    }
                }
                'tls: for (seq, a) in &left.tls {
                    for (other_seq, b) in &right.tls {
                        if !spend(&mut comparisons) {
                            self.incomplete = true;
                            self.collection_truncated = true;
                            break 'pairs;
                        }
                        let (Some(a2), Some(b2)) = (left2.tls.get(seq), right2.tls.get(other_seq))
                        else {
                            continue;
                        };
                        let samples = [a, b, a2, b2];
                        if !samples.iter().all(|s| {
                            !s.truncated
                                && !s.name.is_empty()
                                && s.name == a.name
                                && s.address.is_some()
                                && s.address == a.address
                                && s.fingerprint.is_some()
                        }) {
                            continue;
                        }
                        if a.fingerprint == b.fingerprint || a2.fingerprint == b2.fingerprint {
                            continue;
                        }
                        let evidence = refs(samples.iter().flat_map(|s| s.evidence.iter()));
                        if within_window(&evidence) {
                            found.push(finding(FindingKind::CertificateDifference, evidence, format!("同一名称 {}、同一实际目标 IP {} 在 30 秒内的两轮路径对照中证书指纹均不同。", a.name, a.address.as_deref().unwrap_or(""))));
                            break 'tls;
                        }
                    }
                }
            }
        }
        self.findings.extend(found);
    }
    fn compare_ips(
        &self,
        left: &Project,
        right: &Project,
        found: &mut Vec<Finding>,
        comparisons: &mut usize,
    ) -> bool {
        // Address observations themselves establish successful responses. A
        // failed unrelated project does not invalidate this pair's evidence.
        let (Some(a_path), Some(b_path)) = (left.signature(), right.signature()) else {
            return true;
        };
        for a in &left.ips {
            for b in &right.ips {
                if found.len() >= 128 {
                    return true;
                }
                if !spend(comparisons) {
                    return false;
                }
                if a.evidence.round != b.evidence.round {
                    continue;
                }
                if a.address.is_ipv4() == b.address.is_ipv4() {
                    if a_path == b_path
                        && a.address != b.address
                        && ((a.source == "https" && b.source == "stun")
                            || (a.source == "stun" && b.source == "https"))
                    {
                        found.push(finding(FindingKind::HttpsStunEgressDifference, vec![a.evidence.clone(), b.evidence.clone()], format!("同轮、同地址族、同一固定出口的 HTTPS 与 STUN 服务分别观察到 {} 和 {}。", a.address, b.address)));
                    }
                } else if a.source == b.source
                    && a.observed_path.is_some()
                    && b.observed_path.is_some()
                    && a.observed_path != b.observed_path
                {
                    found.push(finding(FindingKind::Ipv4Ipv6EgressDifference, vec![a.evidence.clone(), b.evidence.clone()], "IPv4 与 IPv6 均取得实际出口响应，且配置身份、固定成员或绑定网卡记录不同。仅地址族不同不会触发本项。".into()));
                }
            }
        }
        true
    }
}
fn spend(remaining: &mut usize) -> bool {
    if *remaining == 0 {
        false
    } else {
        *remaining -= 1;
        true
    }
}
fn refs<'a>(values: impl Iterator<Item = &'a EvidenceRef>) -> Vec<EvidenceRef> {
    let mut groups: BTreeMap<(String, u8), Vec<EvidenceRef>> = BTreeMap::new();
    for value in values {
        groups
            .entry((value.project_id.clone(), value.round))
            .or_default()
            .push(value.clone());
    }
    groups
        .into_values()
        .flat_map(|mut values| {
            values.sort_by_key(|e| e.sampled_at_ms.parse::<u128>().unwrap_or(u128::MAX));
            let first = values.first().cloned();
            let last = values.last().cloned();
            first.into_iter().chain(last)
        })
        .take(8)
        .collect()
}
fn within_window(evidence: &[EvidenceRef]) -> bool {
    let times: Option<Vec<u128>> = evidence
        .iter()
        .map(|e| e.sampled_at_ms.parse().ok())
        .collect();
    times.is_some_and(|times| {
        !times.is_empty() && times.iter().max().unwrap() - times.iter().min().unwrap() <= 30_000
    })
}
pub(super) fn finding(
    kind: FindingKind,
    evidence: Vec<EvidenceRef>,
    explanation: String,
) -> Finding {
    use FindingKind::*;
    let (confidence, severity, title, alternatives, recommendation) = match kind {
        PlaintextDns => (
            Confidence::High,
            Severity::Risk,
            "明文 DNS 暴露",
            vec!["明文链路段不等于全部解析路径；加密代理承载情况另见连接路径。"],
            "在需要保护的链路使用已验证的 DoT/DoH，并检查引导解析路径。",
        ),
        PlaintextProxySegment => (
            Confidence::High,
            Severity::Risk,
            "未加密代理段",
            vec!["应用层 TLS 可能仍提供端到端保护。"],
            "为该代理段选择经过验证的加密传输，并保持应用证书验证。",
        ),
        CertificateVerificationDisabled => (
            Confidence::High,
            Severity::Risk,
            "关闭证书验证",
            vec!["这是配置风险，不是监听已发生的证据。"],
            "恢复证书验证；需要自定义根时显式安装受信任证书。",
        ),
        TlsVerificationFailed => (
            Confidence::High,
            Severity::NeedsVerification,
            "TLS 验证失败",
            vec!["证书过期、名称不匹配、证书链不完整或根证书不可用都可能导致失败。"],
            "查看验证器错误、名称、证书链与目标 Agent 的根证书加载结果。",
        ),
        TrustStoreDifference => (
            Confidence::High,
            Severity::Information,
            "信任库差异",
            vec!["企业证书或用户自定义根通常会造成系统与内置根证书结果不同。"],
            "核对目标 Agent 账户的根证书来源；此差异不构成监听结论。",
        ),
        DnsDifference => (
            Confidence::Medium,
            Severity::NeedsVerification,
            "DNS 解析差异",
            vec!["CDN、地理调度、分域 DNS、缓存和服务更新均可能产生差异。"],
            "结合域策略、查询时间、服务器和所选出口核对结果。",
        ),
        CertificateDifference => (
            Confidence::Medium,
            Severity::NeedsVerification,
            "证书差异",
            vec!["证书轮换、同 IP 后端负载均衡或中间设备都可能造成差异。"],
            "检查两轮指纹、名称、目标 IP、发行者和实际路径；联系服务维护者核对。",
        ),
        ResolutionPathMismatch => (
            Confidence::High,
            Severity::Risk,
            "解析路径不符",
            vec!["只有记录明确冲突时触发；不可见的系统解析内部路径不能推断为冲突。"],
            "检查指定网卡与解析器绑定，修复冲突后重测。",
        ),
        Ipv4Ipv6EgressDifference => (
            Confidence::Medium,
            Severity::Information,
            "IPv4/IPv6 出口差异",
            vec!["双栈分流或不同的地址族路由策略可产生此现象。"],
            "核对双栈策略及两地址族的实际固定路径。",
        ),
        HttpsStunEgressDifference => (
            Confidence::Medium,
            Severity::Information,
            "HTTPS/STUN 出口差异",
            vec!["TCP/UDP 分流、服务侧 NAT 或代理出口池可产生此现象。"],
            "核对同轮协议路径；不要据此推断本地路由器类型或监听。",
        ),
    };
    Finding {
        kind,
        confidence,
        severity,
        title: title.into(),
        explanation,
        alternatives: alternatives.into_iter().map(str::to_owned).collect(),
        recommendation: recommendation.into(),
        evidence,
    }
}
