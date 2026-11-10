//! NAT type presentation. The backend report decides what was measured; this
//! module only names the traditional category and the fixed comparison table.
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NatKind {
    FullCone,
    RestrictedCone,
    PortRestrictedCone,
    Symmetric,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum NatType {
    Classified(NatKind),
    NoTranslation,
    Undetermined(String),
}

pub struct NatRow {
    pub kind: NatKind,
    pub name: &'static str,
    pub mapping: &'static str,
    pub inbound: &'static str,
    pub p2p: &'static str,
}

pub const NAT_TABLE: [NatRow; 4] = [
    NatRow {
        kind: NatKind::FullCone,
        name: "完全锥型 Full Cone",
        mapping: "单一稳定映射",
        inbound: "任意外部主机",
        p2p: "极好",
    },
    NatRow {
        kind: NatKind::RestrictedCone,
        name: "受限锥型 Restricted Cone",
        mapping: "复用映射",
        inbound: "仅已联系的 IP",
        p2p: "良好",
    },
    NatRow {
        kind: NatKind::PortRestrictedCone,
        name: "端口受限锥型 Port-Restricted Cone",
        mapping: "复用映射",
        inbound: "仅已联系的 IP+端口",
        p2p: "一般",
    },
    NatRow {
        kind: NatKind::Symmetric,
        name: "对称型 Symmetric",
        mapping: "每个目标不同映射",
        inbound: "仅该目标",
        p2p: "差",
    },
];

pub const NAT_HEADERS: [&str; 4] = ["NAT 类型", "映射行为", "入站接受", "P2P 兼容性"];

impl NatKind {
    pub fn row(self) -> &'static NatRow {
        NAT_TABLE
            .iter()
            .find(|row| row.kind == self)
            .unwrap_or(&NAT_TABLE[3])
    }
}

/// Classify a backend `network_nat::Report` (as JSON). A category is named only
/// when the backend reported a traditional type for a complete, stable
/// measurement on a verified physical path; otherwise the reason is returned.
pub fn nat_type(report: &Value) -> NatType {
    let verified = report["physical_path"]["binding_verified"] == true
        && report["physical_path"]["route_verified"] == true;
    if report["unstable"] == true {
        return NatType::Undetermined("映射不稳定，不能可靠分类".into());
    }
    if report["partial"] == true {
        return NatType::Undetermined("测量未完成，结果不完整".into());
    }
    if !verified {
        return NatType::Undetermined("物理网卡路径未确认".into());
    }
    match report["traditional_type"].as_str() {
        Some("full_cone_observed") => return NatType::Classified(NatKind::FullCone),
        Some("restricted_cone_observed") => return NatType::Classified(NatKind::RestrictedCone),
        Some("port_restricted_cone_observed") => {
            return NatType::Classified(NatKind::PortRestrictedCone)
        }
        Some("symmetric_observed") => return NatType::Classified(NatKind::Symmetric),
        Some("no_translation_observed") => return NatType::NoTranslation,
        _ => {}
    }
    if report["advanced_capable"] != true {
        return NatType::Undetermined(
            "测试服务未提供 RFC 5780 能力；请开启类型识别并使用支持的 STUN 服务".into(),
        );
    }
    // The backend names only complete, stable, traditional combinations;
    // unusual mapping/filtering pairs are shown as measured behaviours.
    NatType::Undetermined("映射/过滤组合不属于传统类别或证据不足".into())
}

impl NatType {
    pub fn label(&self) -> String {
        match self {
            NatType::Classified(kind) => kind.row().name.into(),
            NatType::NoTranslation => "未观察到地址转换".into(),
            NatType::Undetermined(reason) => format!("未能确定（{reason}）"),
        }
    }
    pub fn short_label(&self) -> &'static str {
        match self {
            NatType::Classified(NatKind::FullCone) => "完全锥型",
            NatType::Classified(NatKind::RestrictedCone) => "受限锥型",
            NatType::Classified(NatKind::PortRestrictedCone) => "端口受限锥型",
            NatType::Classified(NatKind::Symmetric) => "对称型",
            NatType::NoTranslation => "无地址转换",
            NatType::Undetermined(_) => "未能确定",
        }
    }
    pub fn highlighted(&self) -> Option<NatKind> {
        match self {
            NatType::Classified(kind) => Some(*kind),
            _ => None,
        }
    }
}

pub fn behavior_label(value: &Value) -> &'static str {
    match value.as_str() {
        Some("endpoint_independent") => "与目标无关",
        Some("address_dependent") => "与目标地址相关",
        Some("address_and_port_dependent") => "与目标地址和端口相关",
        _ => "未知",
    }
}
