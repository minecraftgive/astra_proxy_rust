import type {
  AddressFamily,
  Egress,
  Probe,
  Interface,
  NetworkDiagnosticsRequest,
  Snapshot,
} from "../generated/management";

export type ToolKind = Probe["kind"];
export type DiagnosticRequest = NetworkDiagnosticsRequest;
export type Form = {
  kind: ToolKind;
  target: string;
  egress: string;
  record: string;
  server: string;
  resolver: string;
  family: AddressFamily | "";
  count: number;
  timeout: number;
  ports: string;
  serverName: string;
  method: "HEAD" | "GET";
  compareBundled: boolean;
  stunServer: string;
  natServers: string;
  natInterface: string;
  natAdvanced: boolean;
  maxHops: number;
  probesPerHop: number;
};
export const tools: [ToolKind, string][] = [
  ["tcp", "TCP"],
  ["tcp_ports", "TCP 多端口"],
  ["dns", "DNS"],
  ["tls", "TLS / 证书"],
  ["http", "HTTP"],
  ["https_ip", "HTTPS 公网 IP"],
  ["icmp", "ICMP"],
  ["stun", "UDP / STUN 出口"],
  ["nat", "NAT 分级检测"],
  ["trace", "逐跳"],
  ["ipv6", "IPv6 可达性"],
];
export const stunPresets = "stun.cloudflare.com:3478\nstun.l.google.com:19302";
export function defaultTarget(kind: ToolKind) {
  if (["http", "https_ip"].includes(kind))
    return kind === "https_ip"
      ? "https://api.ipify.org"
      : "https://example.com/";
  return ["tcp", "tls", "ipv6"].includes(kind)
    ? "example.com:443"
    : "example.com";
}
export function physicalInterfaces(
  snapshot: Snapshot | null,
  family: AddressFamily,
): Interface[] {
  return (
    snapshot?.interfaces.filter(
      (item) =>
        item.kind === "physical" &&
        item.connected &&
        item.addresses.some((address) => {
          const v6 = address.address.includes(":");
          return (
            v6 === (family === "ipv6") &&
            !["0.0.0.0", "::", "::1"].includes(address.address) &&
            !address.address.startsWith("127.")
          );
        }),
    ) ?? []
  );
}
function bounded(value: number, min: number, max: number, label: string) {
  if (!Number.isInteger(value) || value < min || value > max)
    throw new Error(`${label}须为 ${min}–${max}`);
  return value;
}
function endpoint(target: string) {
  const match = target.trim().match(/^(\[[^\]]+\]|[^:\s]+):(\d+)$/);
  if (!match || !match[1] || Number(match[2]) < 1 || Number(match[2]) > 65535)
    throw new Error("TCP / TLS 目标需要 host:port 或 [IPv6]:port");
  return { host: match[1].replace(/^\[|\]$/g, ""), port: Number(match[2]) };
}
export function buildRequest(
  form: Form,
  agent: string,
  snapshot: Snapshot | null,
): DiagnosticRequest {
  const target = form.target.trim();
  let probe: Probe;
  let egress: Egress =
    form.egress === "system"
      ? { kind: "system" }
      : {
          kind: form.egress.startsWith("iface:") ? "interface" : "outbound",
          name: form.egress.slice(form.egress.startsWith("iface:") ? 6 : 9),
        };
  let family = form.family || null;
  let count = ["nat", "trace"].includes(form.kind)
    ? 1
    : bounded(form.count, 1, 50, "次数");
  let timeout =
    form.kind === "nat"
      ? 30000
      : bounded(form.timeout, 100, 30000, "超时毫秒数");
  if (!["nat", "stun"].includes(form.kind) && !target)
    throw new Error("请填写目标");
  switch (form.kind) {
    case "tcp":
    case "ipv6":
      probe = { kind: form.kind, ...endpoint(target) };
      if (form.kind === "ipv6") family = "ipv6";
      break;
    case "tcp_ports": {
      const values = form.ports
        .trim()
        .split(/[\s,，]+/)
        .filter(Boolean);
      if (
        !values.length ||
        values.length > 16 ||
        values.some(
          (value) =>
            !/^\d+$/.test(value) || Number(value) < 1 || Number(value) > 65535,
        )
      )
        throw new Error("填写最多 16 个有效端口，以逗号或空格分隔");
      probe = {
        kind: "tcp_ports",
        host: target.replace(/^\[|\]$/g, ""),
        ports: [...new Set(values.map(Number))],
      };
      break;
    }
    case "dns":
      if (!form.server.trim()) throw new Error("请填写 DNS 解析器");
      probe = {
        kind: "dns",
        name: target,
        record: form.record,
        server: form.server.trim(),
      };
      break;
    case "tls":
      probe = {
        kind: "tls",
        ...endpoint(target),
        server_name: form.serverName.trim() || null,
        compare_bundled: form.compareBundled,
      };
      break;
    case "http":
    case "https_ip": {
      let url: URL;
      try {
        url = new URL(target);
      } catch {
        throw new Error("请填写完整 HTTP / HTTPS URL");
      }
      if (
        !["http:", "https:"].includes(url.protocol) ||
        (form.kind === "https_ip" && url.protocol !== "https:") ||
        url.username ||
        url.password
      )
        throw new Error(
          "使用 HTTP / HTTPS URL；公网 IP 工具仅接受 HTTPS，URL 不含账号密码",
        );
      probe =
        form.kind === "http"
          ? {
              kind: "http",
              url: target,
              method: form.method,
              compare_bundled: form.compareBundled,
            }
          : {
              kind: "https_ip",
              url: target,
              compare_bundled: form.compareBundled,
            };
      break;
    }
    case "icmp":
      probe = { kind: "icmp", host: target };
      break;
    case "stun":
      if (!form.stunServer.trim()) throw new Error("请填写 STUN 服务");
      probe = { kind: "stun", server: form.stunServer.trim() };
      break;
    case "trace":
      probe = {
        kind: "trace",
        host: target,
        max_hops: bounded(form.maxHops, 1, 30, "最大跳数"),
        probes_per_hop: bounded(form.probesPerHop, 1, 3, "每跳次数"),
      };
      count = 1;
      break;
    case "nat": {
      family = form.family || "ipv4";
      if (!form.natInterface) throw new Error("请选择物理网卡");
      if (
        !snapshot ||
        snapshot.agent_id !== agent ||
        !physicalInterfaces(snapshot, family).some(
          (item) => item.id === form.natInterface,
        )
      )
        throw new Error(
          "所选物理网卡已失效，或不属于目标 Agent / 不支持所选地址族",
        );
      const servers = form.natServers
        .trim()
        .split(/[\s,，]+/)
        .filter(Boolean);
      if (!servers.length || servers.length > 4)
        throw new Error("填写 1–4 个 STUN 服务");
      probe = {
        kind: "nat",
        request: {
          agent_id: snapshot.agent_id,
          interface_id: form.natInterface,
          family,
          servers,
          advanced: form.natAdvanced,
        },
      };
      egress = { kind: "interface", name: form.natInterface };
      count = 1;
      timeout = 30000;
      break;
    }
  }
  return {
    egress,
    probe,
    family,
    resolver: form.resolver.trim() || null,
    count,
    interval_ms: "1000",
    timeout_ms: String(timeout),
  };
}
export function requireToolCapabilities(
  request: DiagnosticRequest,
  capabilities: string[],
) {
  const has = (capability: string) => capabilities.includes(capability);
  const probe = request.probe;
  const encrypted =
    (probe.kind === "dns" && /^(tls|dot|https):\/\//i.test(probe.server)) ||
    /^(tls|dot|https):\/\//i.test(request.resolver ?? "");
  if (
    (!["tcp", "dns"].includes(probe.kind) || request.family || encrypted) &&
    (!has("network_diagnostics_jobs_v1") ||
      !has("network_diagnostics_tools_v1"))
  )
    throw new Error(
      "目标 Agent 未声明新版工具 / 地址族 / 系统信任库能力；未提交任务",
    );
  if (probe.kind === "nat" && !has("network_diagnostics_nat_v1"))
    throw new Error("目标 Agent 未声明物理网卡 NAT 能力；未提交任务");
  if (
    ["trace", "icmp"].includes(probe.kind) &&
    !has("network_diagnostics_path_v1")
  )
    throw new Error("目标 Agent 未声明 TUN 感知路径能力；未提交任务");
}
