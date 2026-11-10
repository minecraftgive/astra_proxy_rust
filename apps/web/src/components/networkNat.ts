/** NAT type presentation, mirroring `astra-client/src/nettools/nat.rs`. The
 * backend report decides what was measured; only its traditional type is named. */
type Row = Record<string, any>;
export type NatKind =
  "full_cone" | "restricted_cone" | "port_restricted_cone" | "symmetric";
export type NatType =
  | { kind: NatKind }
  | { kind: "no_translation" }
  | { kind: "undetermined"; reason: string };

export const NAT_HEADERS = ["NAT 类型", "映射行为", "入站接受", "P2P 兼容性"];
export const NAT_TABLE: {
  kind: NatKind;
  name: string;
  mapping: string;
  inbound: string;
  p2p: string;
}[] = [
  {
    kind: "full_cone",
    name: "完全锥型 Full Cone",
    mapping: "单一稳定映射",
    inbound: "任意外部主机",
    p2p: "极好",
  },
  {
    kind: "restricted_cone",
    name: "受限锥型 Restricted Cone",
    mapping: "复用映射",
    inbound: "仅已联系的 IP",
    p2p: "良好",
  },
  {
    kind: "port_restricted_cone",
    name: "端口受限锥型 Port-Restricted Cone",
    mapping: "复用映射",
    inbound: "仅已联系的 IP+端口",
    p2p: "一般",
  },
  {
    kind: "symmetric",
    name: "对称型 Symmetric",
    mapping: "每个目标不同映射",
    inbound: "仅该目标",
    p2p: "差",
  },
];

const traditional: Record<string, NatType> = {
  full_cone_observed: { kind: "full_cone" },
  restricted_cone_observed: { kind: "restricted_cone" },
  port_restricted_cone_observed: { kind: "port_restricted_cone" },
  symmetric_observed: { kind: "symmetric" },
  no_translation_observed: { kind: "no_translation" },
};

export function natType(report: Row): NatType {
  const verified =
    report.physical_path?.binding_verified === true &&
    report.physical_path?.route_verified === true;
  if (report.unstable === true)
    return { kind: "undetermined", reason: "映射不稳定，不能可靠分类" };
  if (report.partial === true)
    return { kind: "undetermined", reason: "测量未完成，结果不完整" };
  if (!verified) return { kind: "undetermined", reason: "物理网卡路径未确认" };
  const named = traditional[report.traditional_type];
  if (named) return named;
  if (report.advanced_capable !== true)
    return {
      kind: "undetermined",
      reason:
        "测试服务未提供 RFC 5780 能力；请开启类型识别并使用支持的 STUN 服务",
    };
  return {
    kind: "undetermined",
    reason: "映射/过滤组合不属于传统类别或证据不足",
  };
}

export function natLabel(type: NatType): string {
  if (type.kind === "undetermined") return `未能确定（${type.reason}）`;
  if (type.kind === "no_translation") return "未观察到地址转换";
  return NAT_TABLE.find((row) => row.kind === type.kind)!.name;
}

export const natShortLabel = (type: NatType) =>
  ({
    full_cone: "完全锥型",
    restricted_cone: "受限锥型",
    port_restricted_cone: "端口受限锥型",
    symmetric: "对称型",
    no_translation: "无地址转换",
    undetermined: "未能确定",
  })[type.kind];

export function behaviorLabel(value: unknown): string {
  return (
    {
      endpoint_independent: "与目标无关",
      address_dependent: "与目标地址相关",
      address_and_port_dependent: "与目标地址和端口相关",
    }[String(value)] ?? "未知"
  );
}
