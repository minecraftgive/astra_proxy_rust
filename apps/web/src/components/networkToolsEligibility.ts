import type { DiagnosticRequest } from "./networkToolsModel";

// Local syntax checks only; URL parsing never resolves or contacts a host.
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

const validPort = (port: string) =>
  /^\d+$/.test(port) && Number(port) >= 1 && Number(port) <= 65535;

function authorityHost(authority: string): string | null {
  if (!authority || /[\s@/?#\\]/.test(authority)) return null;
  if (authority.startsWith("[")) {
    const end = authority.indexOf("]");
    if (end < 0) return null;
    const host = authority.slice(1, end);
    const suffix = authority.slice(end + 1);
    if (
      !host.includes(":") ||
      canonicalIp(host) === null ||
      (suffix !== "" && !(suffix.startsWith(":") && validPort(suffix.slice(1))))
    )
      return null;
    return host;
  }
  if (/[\[\]]/.test(authority)) return null;
  if (canonicalIp(authority) !== null) return authority;
  const parts = authority.split(":");
  if (parts.length > 2 || (parts.length === 2 && !validPort(parts[1])))
    return null;
  return parts[0] || null;
}

function endpoint(server: string) {
  const parts = (server.trim() || "1.1.1.1").split("#");
  if (parts.length > 2) return null;
  const bootstrap = parts.length === 2;
  if (
    bootstrap &&
    (!parts[1].startsWith("bootstrap=") ||
      canonicalIp(parts[1].slice(10)) === null)
  )
    return null;
  const split = parts[0].indexOf("://");
  const scheme = split < 0 ? "udp" : parts[0].slice(0, split).toLowerCase();
  let authority = split < 0 ? parts[0] : parts[0].slice(split + 3);
  if (!["udp", "tcp", "tls", "dot", "https", "stun"].includes(scheme))
    return null;
  if (scheme === "https") authority = authority.split(/[/?]/)[0];
  else authority = authority.replace(/\/+$/, "");
  const host = authorityHost(authority);
  return host === null
    ? null
    : { scheme, literal: canonicalIp(host) !== null, bootstrap };
}

const encrypted = (server: string) =>
  /^(tls|dot|https):\/\//i.test(server.trim());
function endpointNeedsResolution(server: string, bound: boolean): boolean {
  if (encrypted(server)) return true;
  const parsed = endpoint(server);
  return parsed === null || parsed.bootstrap || (bound && !parsed.literal);
}

export function needsResolutionCapability(request: DiagnosticRequest): boolean {
  if (request.resolver != null) return true;
  const bound = request.egress.kind === "interface";
  const probe = request.probe;
  if ("host" in probe) return bound && canonicalIp(probe.host) === null;
  if ("url" in probe) {
    const rest = probe.url.split("://");
    const host =
      rest.length === 2 ? authorityHost(rest[1].split(/[/?#]/)[0]) : null;
    return bound && (host === null || canonicalIp(host) === null);
  }
  if (probe.kind === "nat")
    return probe.request.servers.some((server) =>
      endpointNeedsResolution(server, true),
    );
  if (probe.kind === "stun")
    return endpointNeedsResolution(
      probe.server.replace(/^stun:(?!\/\/)/i, ""),
      bound,
    );
  return probe.kind === "dns" && endpointNeedsResolution(probe.server, bound);
}

export function validateLegacyRequest(request: DiagnosticRequest) {
  if (request.family || !["tcp", "dns"].includes(request.probe.kind))
    throw new Error("旧 Agent 不支持此工具或地址族约束；未启动探测");
  if (request.egress.kind === "outbound")
    throw new Error(
      "旧 Agent 无法保证配置出口的解析路径，包括 DIRECT；未启动探测",
    );
  if (request.resolver != null)
    throw new Error(
      "旧 Agent 未声明新版解析能力，不支持约束解析器；未启动探测，也不会降级丢弃选项",
    );
  if (request.probe.kind === "dns") {
    const parsed = endpoint(request.probe.server);
    if (!parsed || !["udp", "tcp"].includes(parsed.scheme) || parsed.bootstrap)
      throw new Error(
        "旧 Agent 只支持无 bootstrap 的有效 UDP/TCP DNS 端点；未启动探测",
      );
  }
  if (needsResolutionCapability(request))
    throw new Error(
      "旧 Agent 未声明新版解析能力，无法约束网卡目标或 DNS 服务名的系统解析；请填写 IP 或升级后使用。未启动探测",
    );
}
