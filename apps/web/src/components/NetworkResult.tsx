/** Single-column result, mirroring `astra-client/src/nettools/result.rs`.
 * Findings come from the backend analysis event; nothing is derived here. */
import type { NetworkResult as Item } from "../generated/management";
import {
  NAT_HEADERS,
  NAT_TABLE,
  behaviorLabel,
  natLabel,
  natType,
} from "./networkNat";

type Row = Record<string, any>;
export type Tone = "success" | "warning" | "error" | "info" | "muted";
export type Line = { text: string; tone: Tone };

const fmt = (value: unknown) =>
  value == null
    ? "未知"
    : typeof value === "object"
      ? JSON.stringify(value)
      : String(value);
const num = (value: unknown): number | null => {
  const n = typeof value === "string" ? Number(value) : value;
  return typeof n === "number" && Number.isFinite(n) ? n : null;
};

/** Batch results wrap each sub-event in `project` envelopes. */
export function unwrap(event: Row): Row {
  let value = event;
  for (let depth = 0; depth < 8 && value?.event === "project"; depth++)
    value = value.detail ?? {};
  return value;
}

export function latest(items: Item[], match: (event: Row) => boolean) {
  for (let i = items.length - 1; i >= 0; i--) {
    const event = unwrap(items[i].data as Row);
    if (match(event)) return event;
  }
  return null;
}

function eventLines(original: Row): Line[] {
  const e = unwrap(original);
  const d = e.detail ?? {};
  const lines: Line[] = [];
  const push = (tone: Tone, text: string) => lines.push({ tone, text });
  switch (e.event) {
    case "analysis": {
      if (e.incomplete || e.truncated)
        push("warning", "诊断证据不完整；未报告的问题不能视为已排除。");
      for (const f of e.findings ?? []) {
        const [tone, label]: [Tone, string] =
          f.severity === "risk"
            ? ["error", "风险"]
            : f.severity === "needs_verification"
              ? ["warning", "待核实"]
              : ["info", "信息"];
        const confidence =
          f.confidence === "high"
            ? "高"
            : f.confidence === "medium"
              ? "中"
              : "未知";
        push(
          tone,
          `[${label} · 置信度${confidence}] ${fmt(f.title)}：${fmt(f.explanation)}`,
        );
        if (f.recommendation) push("muted", `  建议：${f.recommendation}`);
      }
      if (!(e.findings ?? []).length)
        push("info", "分析未产生发现；未测项目仍无法判断。");
      for (const note of e.notes ?? []) push("muted", fmt(note));
      break;
    }
    case "tcp": {
      const r = e.result ?? {};
      const [tone, label]: [Tone, string] =
        r.outcome === "connected"
          ? ["success", "已连接"]
          : r.outcome === "refused"
            ? ["error", "被拒绝"]
            : r.outcome === "timed_out"
              ? ["warning", "超时"]
              : r.outcome === "unreachable"
                ? ["error", "不可达"]
                : ["error", "失败"];
      const ms = num(r.connect_ms);
      push(
        tone,
        `TCP ${fmt(r.host)}:${fmt(r.port)}  ${label}${ms != null ? ` · ${ms.toFixed(2)} ms` : ""}`,
      );
      break;
    }
    case "dns":
      if (d.kind === "response") {
        const [tone, label]: [Tone, string] =
          d.outcome === "resolved" && d.parsed && !d.truncated
            ? ["success", "解析成功"]
            : d.outcome === "resolved"
              ? ["warning", "解析证据不完整"]
              : d.outcome === "nxdomain"
                ? ["warning", "域名不存在"]
                : ["no_records", "empty"].includes(d.outcome)
                  ? ["warning", "没有匹配记录"]
                  : ["server_failure", "dns_error"].includes(d.outcome)
                    ? ["error", "服务器返回错误"]
                    : ["warning", "收到响应，请核实"];
        push(
          tone,
          `DNS ${label} · RCODE ${fmt(d.rcode)} · ${num(d.answer_count) ?? 0} 条答案`,
        );
        if (Array.isArray(d.cname_chain) && d.cname_chain.length)
          push("muted", `  CNAME ${d.cname_chain.join(" → ")}`);
      } else if (d.kind === "records" && d.section === "answer") {
        for (const r of d.records ?? [])
          push(
            "info",
            `  ${fmt(r.name)} ${fmt(r.record_type)} TTL ${fmt(r.ttl)}  ${fmt(r.text)}${r.text_truncated ? "…" : ""}`,
          );
      } else if (d.kind === "failure")
        push("error", `DNS 失败：${fmt(d.message)}`);
      break;
    case "tls":
      if (d.kind === "handshake") {
        const verified = d.verified === true;
        push(
          verified ? "success" : "error",
          `TLS ${fmt(d.server_name)} · ${d.protocol ?? "协议未知"} · ${verified ? "证书验证通过" : d.verification_attempted === false ? "握手未完成，证书未验证" : "验证或握手失败"}`,
        );
        for (const key of ["verification_error", "handshake_error"])
          if (d[key]) push("error", `  ${d[key]}`);
        if (
          typeof d.system_chain_verified === "boolean" &&
          typeof d.bundled_comparison?.verified === "boolean" &&
          d.system_chain_verified !== d.bundled_comparison.verified
        )
          push(
            "warning",
            "  系统与内置根证书验证结果不同；差异本身不表示监听。",
          );
      } else if (d.kind === "certificate" && Number(d.index) === 0)
        push(
          "muted",
          `  证书 ${fmt(d.subject)} · 签发 ${fmt(d.issuer)} · 有效期 ${fmt(d.not_before)} 至 ${fmt(d.not_after)}`,
        );
      else if (d.kind === "trust")
        for (const err of d.load_errors ?? [])
          push("warning", `根证书加载问题：${fmt(err)}`);
      break;
    case "http":
      if (d.kind === "response") {
        const status = num(d.status) ?? 0;
        const tone: Tone =
          status >= 200 && status < 300
            ? "success"
            : status >= 300 && status < 400
              ? "info"
              : status >= 400
                ? "error"
                : "warning";
        push(
          tone,
          `HTTP ${fmt(d.version)} ${fmt(d.method)} ${fmt(d.service)} → ${status}`,
        );
        const t = d.timings ?? {};
        const phases = [
          ["DNS", "dns_ms"],
          ["连接", "transport_connect_ms"],
          ["代理握手", "proxy_handshake_ms"],
          ["TLS", "tls_ms"],
          ["首字节", "response_headers_ms"],
          ["总计", "total_ms"],
        ]
          .map(([label, key]) => {
            const ms = num(t[key]);
            return ms == null ? null : `${label} ${ms.toFixed(1)}`;
          })
          .filter(Boolean);
        if (phases.length) push("muted", `  耗时（ms）${phases.join(" · ")}`);
      } else if (d.kind === "failure")
        push("error", `HTTP 失败（${fmt(d.phase)}）：${fmt(d.message)}`);
      else if (d.kind === "redirect")
        push(
          d.followed ? "muted" : "warning",
          `  跳转 ${fmt(d.from)} → ${fmt(d.to)}${d.followed ? "" : `（未继续：${fmt(d.reason)}）`}`,
        );
      break;
    case "public_ip":
      push(
        "success",
        `公网出口 IP：${fmt(e.address)}（${fmt(e.service)} 观察）`,
      );
      break;
    case "stun": {
      const o = e.observation ?? {};
      if (o.error) push("error", `STUN 失败：${o.error}`);
      else if (o.mapped_endpoint)
        push("info", `STUN 出口映射：${o.mapped_endpoint}`);
      else push("warning", "STUN 未取得出口映射");
      break;
    }
    // NAT reports are rendered by NatTypeTable.
    case "trace_summary":
      push(
        e.reached ? "success" : "warning",
        `逐跳 ${fmt(e.target)}：${e.reached ? "已到达目标" : "未确认到达；中间无响应不代表设备缺失"}`,
      );
      break;
    case "echo":
      if (e.observation?.proves_reachability !== true)
        push(
          "warning",
          `ICMP ${fmt(e.observation?.target)}：${fmt(e.observation?.message)}`,
        );
      break;
    case "project_status": {
      const [tone, label]: [Tone, string] =
        e.state === "failed"
          ? ["error", "失败"]
          : e.state === "partial"
            ? ["warning", "部分完成"]
            : ["cancelled", "interrupted"].includes(e.state)
              ? ["warning", "已停止"]
              : e.state === "complete"
                ? ["info", "完成"]
                : ["muted", "等待或执行中"];
      push(
        tone,
        `项目 ${fmt(e.project_id)}：${label}${e.message ? ` · ${e.message}` : ""}`,
      );
      break;
    }
  }
  if (original.event === "project") {
    const p = original.project ?? {};
    const round = Number(p.round) > 1 ? ` · 第 ${p.round} 轮` : "";
    for (const line of lines)
      line.text = `[${fmt(p.project_id)}${round}] ${line.text}`;
  }
  return lines;
}

export function resultLines(items: Item[]): Line[] {
  const lines: Line[] = [];
  const seen = new Set<string>();
  const add = (line: Line) => {
    const key = `${line.tone}\u0000${line.text}`;
    if (!seen.has(key)) {
      seen.add(key);
      lines.push(line);
    }
  };
  const events = items.map((item) => unwrap(item.data as Row));
  // Only the latest status of each batch project/path/round is shown.
  const latestStatus = new Map<string, number>();
  events.forEach((event, index) => {
    if (event.event === "project_status")
      latestStatus.set(
        JSON.stringify([event.project_id, event.path_id, event.round]),
        index,
      );
  });
  const hops = new Map<
    number,
    { address: string | null; rtt: number | null }
  >();
  // Hops are flushed at the trace summary, or at the end for a trace that was
  // cancelled or failed before summarising, so partial evidence stays visible.
  const flushHops = () => {
    for (const [hop, value] of [...hops.entries()].sort((a, b) => a[0] - b[0]))
      lines.push({
        tone: value.address ? "info" : "muted",
        text: `跳 ${String(hop).padStart(2)}  ${(value.address ?? "*").padEnd(40)} ${value.rtt == null ? "无响应" : `${value.rtt.toFixed(2)} ms`}`,
      });
    hops.clear();
  };
  events.forEach((event, index) => {
    if (
      event.event === "project_status" &&
      latestStatus.get(
        JSON.stringify([event.project_id, event.path_id, event.round]),
      ) !== index
    )
      return;
    if (event.event === "attempt") {
      const ms = num(event.rtt_ms);
      lines.push(
        event.error == null && ms != null
          ? {
              tone: "muted",
              text: `#${fmt(event.seq)}  ${ms.toFixed(2)} ms  ${event.detail ?? ""}`,
            }
          : {
              tone: "error",
              text: `#${fmt(event.seq)}  失败  ${fmt(event.error)}`,
            },
      );
      return;
    }
    if (event.event === "summary") {
      const sent = num(event.sent) ?? 0;
      const received = num(event.received) ?? 0;
      // Same rounding as Rust `Summary::loss_percent`.
      const loss = sent ? 100 - Math.floor((received * 100) / sent) : 0;
      const [min, avg, max] = [
        num(event.min_ms),
        num(event.avg_ms),
        num(event.max_ms),
      ];
      lines.push({
        tone: sent > 0 && received === 0 ? "warning" : "info",
        text: `发送 ${sent} · 接收 ${received} · 丢失 ${loss}%${min != null && avg != null && max != null ? ` · min/avg/max ${min.toFixed(2)}/${avg.toFixed(2)}/${max.toFixed(2)} ms` : ""}`,
      });
      return;
    }
    if (event.event === "trace") {
      const s = event.sample ?? {};
      const hop = num(s.hop) ?? 0;
      const entry = hops.get(hop) ?? { address: null, rtt: null };
      const rtt = num(s.rtt_ms);
      hops.set(hop, {
        address: entry.address ?? s.address ?? null,
        rtt:
          entry.rtt == null
            ? rtt
            : rtt == null
              ? entry.rtt
              : Math.min(entry.rtt, rtt),
      });
      return;
    }
    if (event.event === "trace_summary") flushHops();
    for (const line of eventLines(items[index].data as Row)) add(line);
  });
  flushHops();
  return lines;
}

export function NatTypeTable({ report }: { report: Row }) {
  const type = natType(report);
  const mapped = (report.observations ?? []).find(
    (o: Row) => o?.mapped_endpoint,
  )?.mapped_endpoint;
  return (
    <section className="nat-result" aria-label="NAT 类型">
      <p
        className={
          type.kind === "undetermined"
            ? "network-tone-warning"
            : "network-tone-success"
        }
      >
        <strong>NAT 类型：{natLabel(type)}</strong> · 网卡{" "}
        {fmt(report.interface_name)}
      </p>
      <div className="table-scroll">
        <table className="nat-table">
          <thead>
            <tr>
              {NAT_HEADERS.map((h) => (
                <th key={h}>{h}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {NAT_TABLE.map((row) => (
              <tr
                key={row.kind}
                className={type.kind === row.kind ? "is-detected" : undefined}
                aria-current={type.kind === row.kind ? "true" : undefined}
              >
                <td>{row.name}</td>
                <td>{row.mapping}</td>
                <td>{row.inbound}</td>
                <td>{row.p2p}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="muted">
        测得映射：{behaviorLabel(report.mapping)} · 过滤：
        {behaviorLabel(report.filtering)} · 本地 {fmt(report.local_endpoint)} ·
        公网映射 {mapped ?? "未取得"}
      </p>
      {(report.notes ?? []).map((note: unknown, i: number) => (
        <p key={i} className="muted">
          {fmt(note)}
        </p>
      ))}
    </section>
  );
}

export function ResultLines({ lines }: { lines: Line[] }) {
  return (
    <div className="network-lines" role="log" aria-label="结果">
      {lines.map((line, i) => (
        <div key={i} className={`network-tone-${line.tone}`}>
          {line.text}
        </div>
      ))}
    </div>
  );
}
