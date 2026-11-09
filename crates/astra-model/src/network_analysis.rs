//! Shared, versioned findings. Frontends render these conclusions without
//! reimplementing diagnostic rules or assigning a surveillance probability.
use serde::{Deserialize, Serialize};

pub const RULES_VERSION: u16 = 1;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectContext {
    pub project_id: String,
    pub path_id: String,
    pub round: u8,
    /// One-based event position within this project and round.
    pub index: u32,
    pub sampled_at_ms: String,
    pub comparison_requested: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    PlaintextDns,
    PlaintextProxySegment,
    CertificateVerificationDisabled,
    TlsVerificationFailed,
    TrustStoreDifference,
    DnsDifference,
    CertificateDifference,
    ResolutionPathMismatch,
    Ipv4Ipv6EgressDifference,
    HttpsStunEgressDifference,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Risk,
    NeedsVerification,
    Information,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub project_id: String,
    pub path_id: String,
    pub round: u8,
    pub event_index: u32,
    pub sampled_at_ms: String,
    pub source: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub confidence: Confidence,
    pub severity: Severity,
    pub title: String,
    pub explanation: String,
    pub alternatives: Vec<String>,
    pub recommendation: String,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    Measured,
    Configuration,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSection {
    Dns,
    Connection,
    Trace,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyNode {
    pub id: String,
    pub label: String,
    pub role: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyEdge {
    pub from: String,
    pub to: String,
    pub section: PathSection,
    pub level: EvidenceLevel,
    pub description: String,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyAttachment {
    pub kind: String,
    pub label: String,
    pub evidence: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyPath {
    pub path_id: String,
    pub nodes: Vec<TopologyNode>,
    pub edges: Vec<TopologyEdge>,
    pub attachments: Vec<TopologyAttachment>,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverVisibility {
    pub location: String,
    pub information: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub rules_version: u16,
    pub comparison_requested: bool,
    pub comparison_status: String,
    pub findings: Vec<Finding>,
    pub topology: Vec<TopologyPath>,
    pub visibility: Vec<ObserverVisibility>,
    /// Missing evidence, unfinished comparison rounds, or a collection/work cap.
    /// Absence of a finding in this report is never a negative safety result.
    pub incomplete: bool,
    /// Includes collection and comparison limits, not only JSON/page clipping.
    pub truncated: bool,
    pub notes: Vec<String>,
}
/// Manager-owned configuration evidence; no credentials or entire node config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathPolicy {
    pub protocol: String,
    pub encrypted: Option<bool>,
    pub certificate_verification: Option<bool>,
    pub matched_running: bool,
    pub leaf_tag: Option<String>,
}
