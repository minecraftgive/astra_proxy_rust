// 与 Rule Core 首条命中语义一致的简化版路由与出口解析。
import { ADSET, GEOSITE } from "./data.js";

function matchRule(r, host, proc) {
  const h = host.toLowerCase();
  const suffix = (s) => (s.startsWith(".") ? h.endsWith(s) : h === s || h.endsWith("." + s));
  switch (r.type) {
    case "domain": return h === r.value.toLowerCase();
    case "domain_suffix": return !!r.value && suffix(r.value.toLowerCase());
    case "domain_keyword": return !!r.value && h.includes(r.value.toLowerCase());
    case "process_name": return !!proc && proc.toLowerCase() === r.value.toLowerCase();
    case "rule_set": return ADSET.find(suffix) || false;
    case "geosite": return (GEOSITE[r.value] || []).find(suffix) || false;
    case "ip_cidr": return r.value.startsWith("10.") && /^10\./.test(h);
    case "geoip": return /^(114|223|119)\./.test(h);
    default: return false;
  }
}

/** 返回命中的规则序号（无命中为 -1）、命中路径和动作。 */
export function explain(rules, host, proc) {
  for (let i = 0; i < rules.length; i++) {
    const r = rules[i];
    const m = matchRule(r, host, proc);
    if (m) {
      const path = [`rules[${i + 1}]`, `${r.type}(${r.value})`];
      if (typeof m === "string") path.push(`展开 → ${m}`);
      return { index: i, path, action: r.action };
    }
  }
  if (/^(127\.|192\.168\.|::1)/.test(host)) return { index: -1, path: ["direct_local"], action: "direct" };
  return { index: -1, path: ["default_action"], action: "proxy:PROXY" };
}

export const groupOf = (doc, name) => doc.groups.find((g) => g.name === name);

export function selectedOf(g, rt) {
  if (g.mode === "auto") {
    const live = g.members.filter((m) => rt.alive[m] !== false && rt.delays[m] != null);
    return live.sort((a, b) => rt.delays[a] - rt.delays[b])[0] || g.members[0];
  }
  const s = rt.selected[g.name];
  return s && g.members.includes(s) ? s : g.members[0];
}

/** 沿代理组的选择一路解析到最终节点，例如 PROXY ▸ 自动-香港 ▸ hk-02。 */
export function chainFrom(doc, rt, name, depth = 0) {
  const g = groupOf(doc, name);
  if (!g || depth > 6) return [name];
  return [name, ...chainFrom(doc, rt, selectedOf(g, rt), depth + 1)];
}

export function outboundFor(profile, rt, rules, host, proc) {
  const d = profile.doc;
  if (d.general.standalone) return { chain: chainFrom(d, rt, d.general.standalone_outbound || "PROXY"), why: "独立模式" };
  if (rt.mode === "global") return { chain: chainFrom(d, rt, "PROXY"), why: "全局模式" };
  if (rt.mode === "direct") return { chain: ["DIRECT"], why: "直连模式" };
  const e = explain(rules, host, proc);
  const why = e.path.join(" ▸ ");
  if (e.action === "direct") return { chain: ["DIRECT"], why, e };
  if (e.action === "block") return { chain: ["REJECT"], why, e };
  return { chain: chainFrom(d, rt, e.action.slice(6)), why, e };
}

export function exitChain(profile, rt) {
  const d = profile.doc;
  if (d.general.standalone) return chainFrom(d, rt, d.general.standalone_outbound || "PROXY");
  return chainFrom(d, rt, rt.mode === "direct" ? "DIRECT" : "PROXY");
}
