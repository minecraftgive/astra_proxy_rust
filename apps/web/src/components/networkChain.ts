/** Network link display, mirroring `astra-client/src/nettools/chain.rs`.
 * The live chain comes from the instance runtime and the Agent's network
 * state; the probe path is a linear reading of the backend topology. */
import type { Egress } from "../generated/management";

type Row = Record<string, any>;
export const MAX_HOPS = 32;
export type Tone = "normal" | "muted" | "warning";
export type Capture = "tun" | "system_proxy" | "both" | "none";
export type LiveChain = {
  routing: string | null;
  capture: Capture;
  interface: string | null;
  interfaceSource: "requested" | "default_route" | null;
  viaRules: boolean;
  hops: { group: string; mode: string; selected: string }[];
  leaf: string | null;
  issue: string | null;
};

function interfaceName(network: Row, id: string): string {
  return (network.interfaces ?? []).find((i: Row) => i.id === id)?.name ?? id;
}

function defaultInterface(network: Row | null): string | null {
  if (!network?.interfaces || !network?.routes) return null;
  const virtual = (network.interfaces as Row[])
    .filter((i) => i.kind === "virtual" || i.kind === "loopback")
    .map((i) => i.id);
  const defaults: string[] = (network.routes as Row[])
    .filter((r) =>
      ["default", "0.0.0.0/0", "::/0", "0.0.0.0"].includes(r.destination),
    )
    .map((r) => r.interface_id)
    .filter((id): id is string => typeof id === "string");
  const id = defaults.find((d) => !virtual.includes(d)) ?? defaults[0];
  return id ? interfaceName(network, id) : null;
}

export function instanceEgress(runtime: Row | null): Egress {
  if (!runtime?.mode) return { kind: "system" };
  return {
    kind: "outbound",
    name: runtime.mode === "direct" ? "DIRECT" : "PROXY",
  };
}

export function liveChain(
  runtime: Row | null,
  network: Row | null,
  egress: Egress,
): LiveChain {
  const chain: LiveChain = {
    routing: null,
    capture: "none",
    interface: null,
    interfaceSource: null,
    viaRules: false,
    hops: [],
    leaf: null,
    issue: null,
  };
  if (egress.kind === "interface") {
    chain.interface = network
      ? interfaceName(network, egress.name)
      : egress.name;
    chain.interfaceSource = "requested";
  } else {
    chain.interface = defaultInterface(network);
    chain.interfaceSource = chain.interface ? "default_route" : null;
  }
  if (!runtime) {
    if (egress.kind !== "interface") chain.issue = "实例未运行或运行快照不可用";
    return chain;
  }
  chain.routing = typeof runtime.mode === "string" ? runtime.mode : null;
  const tun = runtime.tun === true;
  const proxy = runtime.system_proxy === true;
  chain.capture =
    tun && proxy ? "both" : tun ? "tun" : proxy ? "system_proxy" : "none";
  let start: string;
  if (egress.kind === "interface") {
    chain.capture = "none";
    return chain;
  } else if (egress.kind === "system") {
    if (!tun) return chain;
    const instance = instanceEgress(runtime);
    if (instance.kind !== "outbound") return chain;
    chain.viaRules = chain.routing === "rule";
    start = instance.name;
  } else start = egress.name;
  const groups: Row[] = Array.isArray(runtime.groups) ? runtime.groups : [];
  const seen = new Set<string>();
  // The instance default path is named by convention, not configuration: a
  // missing `PROXY` group is reported instead of shown as an exit node.
  const assumed = egress.kind === "system";
  let current = start;
  for (;;) {
    const group = groups.find((g) => g.name === current);
    if (!group) {
      if (assumed && !chain.hops.length && current !== "DIRECT")
        chain.issue = `未找到默认组 ${current}；实际出口由规则决定`;
      else chain.leaf = current;
      break;
    }
    if (seen.has(current) || chain.hops.length >= MAX_HOPS) {
      chain.issue = `选择链循环或超过 ${MAX_HOPS} 层`;
      break;
    }
    seen.add(current);
    const selected = typeof group.selected === "string" ? group.selected : "";
    if (!selected) {
      chain.issue = `${current} 无选择`;
      break;
    }
    chain.hops.push({ group: current, mode: group.mode ?? "", selected });
    current = selected;
  }
  if (runtime.truncated === true && !chain.issue)
    chain.issue = "运行快照已截断，选择链可能不完整";
  return chain;
}

/** Display steps in traffic order; the physical adapter is the last local stage. */
export function chainSteps(chain: LiveChain): [string, Tone][] {
  const steps: [string, Tone][] = [["系统", "normal"]];
  if (chain.capture === "tun") steps.push(["TUN", "normal"]);
  else if (chain.capture === "system_proxy") steps.push(["系统代理", "muted"]);
  else if (chain.capture === "both") steps.push(["TUN + 系统代理", "normal"]);
  if (chain.viaRules) steps.push(["规则", "muted"]);
  for (const hop of chain.hops)
    steps.push([`${hop.group} → ${hop.selected}`, "normal"]);
  if (chain.leaf) steps.push([chain.leaf, "normal"]);
  if (chain.interface)
    steps.push([
      `网卡 ${chain.interface}（${chain.interfaceSource === "requested" ? "指定网卡" : "默认路由"}）`,
      "normal",
    ]);
  if (chain.issue) steps.push([chain.issue, "warning"]);
  return steps;
}

export const chainText = (chain: LiveChain) =>
  chainSteps(chain)
    .map(([label]) => label)
    .join(" › ");

export type StripStep = {
  id: string;
  label: string;
  role: string;
  level: string | null;
  edge: number | null;
};
export type StripRow = { section: string; steps: StripStep[] };
export type PathStrip = {
  pathId: string;
  rows: StripRow[];
  attachments: string[];
  truncated: boolean;
};

/** Each section becomes chains starting at a node without an incoming edge;
 * leftover edges start further rows so no evidence is dropped. */
export function pathStrips(analysis: Row | null): PathStrip[] {
  return (analysis?.topology ?? []).map((path: Row) => {
    const nodes: Row[] = path.nodes ?? [];
    const edges: Row[] = path.edges ?? [];
    const step = (
      id: string,
      level: string | null,
      edge: number | null,
    ): StripStep => {
      const node = nodes.find((n) => n.id === id);
      return {
        id,
        label: node?.label ?? id,
        role: node?.role ?? "unknown",
        level,
        edge,
      };
    };
    const rows: StripRow[] = [];
    for (const section of ["connection", "dns", "trace"]) {
      const indices = edges
        .map((_, i) => i)
        .filter((i) => edges[i].section === section);
      const used = new Set<number>();
      for (;;) {
        const remaining = indices.filter((i) => !used.has(i));
        if (!remaining.length) break;
        const first =
          remaining.find(
            (i) => !remaining.some((j) => edges[j].to === edges[i].from),
          ) ?? remaining[0];
        const steps = [step(edges[first].from, null, null)];
        let current = first;
        for (;;) {
          used.add(current);
          const to = edges[current].to;
          steps.push(step(to, edges[current].level ?? null, current));
          const next = indices.find(
            (i) => !used.has(i) && edges[i].from === to,
          );
          if (next === undefined || steps.length > 64) break;
          current = next;
        }
        rows.push({ section, steps });
      }
    }
    return {
      pathId: path.path_id ?? "",
      rows,
      attachments: (path.attachments ?? []).map(
        (a: Row) => `${a.kind ?? "旁注"}：${a.label ?? ""}`,
      ),
      truncated: path.truncated === true,
    };
  });
}

export const levelLabel = (level: string | null) =>
  level === "measured"
    ? "实测"
    : level === "configuration"
      ? "配置推断"
      : level
        ? "未知"
        : "";
export const sectionLabel = (section: string) =>
  section === "dns" ? "DNS" : section === "trace" ? "逐跳" : "连接";
export const arrow = (level: string | null) =>
  level === "measured" ? "─›" : level === "configuration" ? "┄›" : "?›";

export function stripRowText(row: StripRow): string {
  return (
    `[${sectionLabel(row.section)}] ` +
    row.steps.map((s, i) => (i ? ` ${arrow(s.level)} ` : "") + s.label).join("")
  );
}
