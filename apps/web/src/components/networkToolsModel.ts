import type {
  AddressFamily,
  Egress,
  Interface,
  NetworkBatchProject,
  NetworkBatchRequest,
  NetworkDiagnosticsRequest,
  Probe,
  Snapshot,
} from "../generated/management";

export type ToolKind =
  | "icmp"
  | "tcp"
  | "dns"
  | "trace"
  | "http"
  | "https_ip"
  | "nat"
  | "fake_ip"
  | "full_check";
export type DiagnosticRequest = NetworkDiagnosticsRequest;
export type Form = {
  kind: ToolKind;
  egress: string;
  target: string;
  record: string;
  server: string;
  resolver: string;
  count: number;
  method: "HEAD" | "GET";
  maxHops: number;
  natInterface: string;
  natFamily: AddressFamily;
  natAdvanced: boolean;
  natServers: string;
  comparison: string;
  compareRounds: boolean;
  dualStack: boolean;
  includeNat: boolean;
};
export const tools: [ToolKind, string][] = [
  ["icmp", "Ping"],
  ["tcp", "TCP 连接"],
  ["dns", "DNS 查询"],
  ["trace", "逐跳路径"],
  ["http", "HTTP(S)"],
  ["https_ip", "公网 IP"],
  ["nat", "NAT 类型"],
  ["fake_ip", "Fake-IP"],
  ["full_check", "完整检查"],
];
export const RECORDS = [
  "A",
  "AAAA",
  "CNAME",
  "MX",
  "TXT",
  "NS",
  "HTTPS",
  "SOA",
];
export const stunPresets = "stun.cloudflare.com:3478, stun.l.google.com:19302";
export const PUBLIC_IP_URL = "https://api.ipify.org?format=json";
export const PUBLIC_IP6_URL = "https://api64.ipify.org?format=json";

export const defaultForm: Form = {
  kind: "icmp",
  egress: "system",
  target: "www.gstatic.com",
  record: "A",
  server: "1.1.1.1",
  resolver: "",
  count: 4,
  method: "GET",
  maxHops: 30,
  natInterface: "",
  natFamily: "ipv4",
  natAdvanced: true,
  natServers: stunPresets,
  comparison: "",
  compareRounds: false,
  dualStack: false,
  includeNat: false,
};

function host(target: string) {
  let value = target.trim();
  const scheme = value.indexOf("://");
  if (scheme >= 0) value = value.slice(scheme + 3).split("/")[0];
  if (value.startsWith("[")) return value.slice(1).split("]")[0];
  const colon = value.lastIndexOf(":");
  if (
    colon > 0 &&
    !value.slice(0, colon).includes(":") &&
    /^\d+$/.test(value.slice(colon + 1))
  )
    value = value.slice(0, colon);
  return value && !value.includes(",") ? value : "example.com";
}

export function defaultTarget(kind: ToolKind, previous = ""): string {
  const h = host(previous);
  switch (kind) {
    case "tcp":
      return `${h.includes(":") ? `[${h}]` : h}:443`;
    case "dns":
    case "icmp":
    case "trace":
      return h;
    case "http":
      return "https://example.com/";
    case "https_ip":
      return PUBLIC_IP_URL;
    default:
      return "";
  }
}

export function parseEgress(value: string): Egress {
  if (value.startsWith("iface:"))
    return { kind: "interface", name: value.slice(6) };
  if (value.startsWith("outbound:"))
    return { kind: "outbound", name: value.slice(9) };
  return { kind: "system" };
}

/** Trace and physical ICMP cannot look inside a proxy tunnel. */
export function egressAllowed(kind: ToolKind, egress: Egress): boolean {
  switch (kind) {
    case "trace":
      return (
        egress.kind === "interface" ||
        (egress.kind === "outbound" && egress.name === "DIRECT")
      );
    case "icmp":
      return egress.kind !== "outbound" || egress.name === "DIRECT";
    case "nat":
    case "fake_ip":
      return egress.kind === "system";
    default:
      return true;
  }
}

export function physicalInterfaces(
  snapshot: Snapshot | null,
  family: AddressFamily,
): Interface[] {
  if (!snapshot || snapshot.truncated) return [];
  return snapshot.interfaces.filter(
    (item) =>
      item.kind === "physical" &&
      item.connected &&
      item.addresses.some((address) => {
        const v6 = address.address.includes(":");
        return v6 === (family === "ipv6");
      }),
  );
}

export function parsePorts(target: string): { host: string; ports: number[] } {
  const colon = target.lastIndexOf(":");
  const hostPart = colon > 0 ? target.slice(0, colon).trim() : "";
  if (!hostPart) throw new Error("请输入 主机:端口，多个端口用逗号分隔");
  const ports = target
    .slice(colon + 1)
    .split(",")
    .map((p) => p.trim());
  if (ports.some((p) => !/^\d+$/.test(p) || Number(p) < 1 || Number(p) > 65535))
    throw new Error("端口须为 1–65535");
  const values = ports.map(Number);
  if (
    !values.length ||
    values.length > 16 ||
    new Set(values).size !== values.length
  )
    throw new Error("最多指定 16 个不同端口");
  return { host: hostPart.replace(/^\[|\]$/g, ""), ports: values };
}

function bounded(value: number, min: number, max: number, label: string) {
  if (!Number.isInteger(value) || value < min || value > max)
    throw new Error(`${label}须为 ${min}–${max}`);
  return value;
}

export function canonicalIp(input: string): string | null {
  if (!input.includes(":")) {
    return /^(0|[1-9]\d{0,2})(\.(0|[1-9]\d{0,2})){3}$/.test(input) &&
      input.split(".").every((part) => Number(part) <= 255)
      ? input
      : null;
  }
  if (!/^[a-f\d:.]+$/i.test(input)) return null;
  try {
    return new URL(`http://[${input}]/`).hostname.slice(1, -1);
  } catch {
    return null;
  }
}

function natProbe(
  form: Form,
  agent: string,
  snapshot: Snapshot | null,
  servers: string,
): Probe {
  if (!form.natInterface) throw new Error("请选择物理网卡");
  if (
    !snapshot ||
    snapshot.agent_id !== agent ||
    !physicalInterfaces(snapshot, form.natFamily).some(
      (item) => item.id === form.natInterface,
    )
  )
    throw new Error("所选物理网卡已失效或不支持所选地址族");
  const list = servers
    .split(/[\s,，;]+/)
    .map((s) => s.trim())
    .filter(Boolean);
  if (!list.length || list.length > 4) throw new Error("填写 1–4 个 STUN 服务");
  return {
    kind: "nat",
    request: {
      agent_id: snapshot.agent_id,
      interface_id: form.natInterface,
      family: form.natFamily,
      servers: list,
      advanced: form.natAdvanced,
    },
  };
}

function request(
  egress: Egress,
  probe: Probe,
  options: Partial<DiagnosticRequest> = {},
): DiagnosticRequest {
  return {
    egress,
    probe,
    family: null,
    resolver: null,
    count: 1,
    interval_ms: "200",
    timeout_ms: "10000",
    ...options,
  };
}

export function buildRequest(
  form: Form,
  agent: string,
  snapshot: Snapshot | null,
): DiagnosticRequest {
  const target = form.target.trim();
  const egress = parseEgress(form.egress);
  if (!egressAllowed(form.kind, egress))
    throw new Error("此工具不支持所选出口");
  if (form.kind !== "nat" && !target) throw new Error("请填写目标");
  const resolver = form.resolver.trim() || null;
  const count = () => bounded(form.count, 1, 50, "次数");
  switch (form.kind) {
    case "icmp":
      return request(
        egress,
        { kind: "icmp", host: target },
        {
          count: count(),
          interval_ms: "1000",
          timeout_ms: "3000",
        },
      );
    case "tcp": {
      const { host, ports } = parsePorts(target);
      return request(
        egress,
        ports.length === 1
          ? { kind: "tcp", host, port: ports[0] }
          : { kind: "tcp_ports", host, ports },
        { count: count(), resolver, interval_ms: "1000", timeout_ms: "3000" },
      );
    }
    case "dns":
      if (!form.server.trim()) throw new Error("请填写 DNS 服务器");
      return request(
        egress,
        {
          kind: "dns",
          name: target,
          record: form.record,
          server: form.server.trim(),
        },
        { count: count(), timeout_ms: "3000" },
      );
    case "trace":
      return request(
        egress,
        {
          kind: "trace",
          host: target,
          max_hops: bounded(form.maxHops, 1, 30, "最大跳数"),
          probes_per_hop: 3,
        },
        { timeout_ms: "1000", interval_ms: "1000" },
      );
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
          "使用 HTTP / HTTPS URL；公网 IP 仅接受 HTTPS，URL 不含账号密码",
        );
      return request(
        egress,
        form.kind === "http"
          ? {
              kind: "http",
              url: target,
              method: form.method,
              compare_bundled: false,
            }
          : { kind: "https_ip", url: target, compare_bundled: false },
        { resolver, interval_ms: "1000" },
      );
    }
    case "nat": {
      const probe = natProbe(form, agent, snapshot, form.natServers);
      return request({ kind: "interface", name: form.natInterface }, probe, {
        family: form.natFamily,
        timeout_ms: "30000",
      });
    }
    default:
      throw new Error("此工具不创建单项探测");
  }
}

/** Fixed probes for the full check, mirroring `full_check.rs`. Proxy paths only
 * carry TCP-based DNS, so DNS uses TCP and DoH endpoints. */
function pathProbes(
  egress: Egress,
  resolver: string | null,
  dualStack: boolean,
) {
  const base = { resolver };
  const projects: [string, DiagnosticRequest][] = [
    [
      "dns-tcp",
      request(
        egress,
        {
          kind: "dns",
          name: "example.com",
          record: "A",
          server: "tcp://1.1.1.1:53",
        },
        base,
      ),
    ],
    [
      "dns-doh",
      request(
        egress,
        {
          kind: "dns",
          name: "example.com",
          record: "A",
          server: "https://1.1.1.1/dns-query",
        },
        base,
      ),
    ],
    [
      "tls",
      request(
        egress,
        {
          kind: "tls",
          host: "example.com",
          port: 443,
          server_name: null,
          compare_bundled: true,
        },
        base,
      ),
    ],
    [
      "http",
      request(
        egress,
        {
          kind: "http",
          url: "https://example.com/",
          method: "HEAD",
          compare_bundled: false,
        },
        base,
      ),
    ],
    [
      "ip4",
      request(
        egress,
        { kind: "https_ip", url: PUBLIC_IP_URL, compare_bundled: false },
        { ...base, family: "ipv4" },
      ),
    ],
  ];
  if (dualStack)
    projects.push([
      "ip6",
      request(
        egress,
        { kind: "https_ip", url: PUBLIC_IP6_URL, compare_bundled: false },
        { ...base, family: "ipv6" },
      ),
    ]);
  projects.push([
    "stun",
    request(
      egress,
      { kind: "stun", server: "stun.cloudflare.com:3478" },
      { ...base, family: "ipv4" },
    ),
  ]);
  return projects;
}

export function buildFullCheck(
  form: Form,
  agent: string,
  snapshot: Snapshot | null,
): NetworkBatchRequest {
  const primary = parseEgress(form.egress);
  const comparison = form.comparison ? parseEgress(form.comparison) : null;
  if (comparison && form.comparison === form.egress)
    throw new Error("对照出口与主出口相同；请选择另一个出口或关闭对照");
  const resolver = form.resolver.trim() || null;
  const projects: NetworkBatchProject[] = [];
  for (const [path, egress] of [
    ["a", primary],
    ...(comparison ? [["b", comparison]] : []),
  ] as [string, Egress][])
    for (const [name, req] of pathProbes(egress, resolver, form.dualStack))
      projects.push({
        id: `${path}-${name}`,
        path_id: `path-${path}`,
        request: req,
      });
  if (form.includeNat) {
    const probe = natProbe(form, agent, snapshot, stunPresets);
    projects.push({
      id: "nat",
      path_id: "path-nat",
      request: request({ kind: "interface", name: form.natInterface }, probe, {
        family: form.natFamily,
        timeout_ms: "30000",
      }),
    });
  }
  return { projects, comparison: form.compareRounds };
}

export function requireCapabilities(
  requests: DiagnosticRequest[],
  capabilities: string[],
  batch: boolean,
) {
  const has = (c: string) => capabilities.includes(c);
  if (
    !has("network_diagnostics_jobs_v1") ||
    !has("network_diagnostics_tools_v1") ||
    !has("network_diagnostics_resolution_v1")
  )
    throw new Error(
      "目标 Agent 版本过旧，不支持后台网络诊断；请更新后再使用网络工具",
    );
  if (batch && !has("network_diagnostics_batch_v1"))
    throw new Error("目标 Agent 未声明完整检查能力；未提交任务");
  for (const r of requests) {
    if (r.egress.kind === "outbound" && !has("network_diagnostics_pinned_v1"))
      throw new Error("目标 Agent 未声明固定出口能力；未提交任务");
    if (r.probe.kind === "nat" && !has("network_diagnostics_nat_v1"))
      throw new Error("目标 Agent 未声明物理网卡 NAT 能力；未提交任务");
    if (
      ["trace", "icmp"].includes(r.probe.kind) &&
      !has("network_diagnostics_path_v1")
    )
      throw new Error("目标 Agent 未声明 TUN 感知路径能力；未提交任务");
  }
}
