/** Present recorded outcomes only; diagnostic rules and NAT classification live on the Agent. */
type Row = Record<string, any>;
export type Tone = "success" | "warning" | "error" | "info" | "muted";
export type Conclusion = { text: string; tone: Tone };
const labels: Record<Tone, string> = {
  success: "成功",
  warning: "注意",
  error: "失败",
  info: "信息",
  muted: "待确认",
};
const natTypes: Record<string, string> = {
  symmetric_observed: "对称型",
  full_cone_observed: "完全锥型",
  restricted_cone_observed: "受限锥型",
  port_restricted_cone_observed: "端口受限锥型",
  no_translation_observed: "未观察到地址转换",
};
export function natConclusion(r: Row): Conclusion {
  const name = natTypes[r.traditional_type];
  if (
    name &&
    r.advanced_capable &&
    !r.partial &&
    !r.unstable &&
    r.physical_path?.binding_verified &&
    r.physical_path?.route_verified
  ) {
    return {
      text: `NAT 类型：${name}（本次所选路径的观测结果）`,
      tone: "info",
    };
  }
  const reason = r.unstable
    ? "映射不稳定"
    : r.partial
      ? "检测未完整完成"
      : !r.advanced_capable
        ? "测试服务未确认支持类型识别；请使用支持 RFC 5780 的服务"
        : !r.physical_path?.binding_verified || !r.physical_path?.route_verified
          ? "物理路径未确认"
          : "已测得的行为不对应传统类型，查看原始报告中的映射与过滤行为";
  return { text: `NAT 类型：未能确定 · ${reason}`, tone: "warning" };
}
export function conclusions(event: Row): Conclusion[] {
  const one = (text: string, tone: Tone = "info"): Conclusion[] => [
    { text, tone },
  ];
  const d = event.detail ?? {};
  switch (event.event) {
    case "tcp": {
      const r = event.result ?? {};
      const states: Record<string, string> = {
        connected: "连接成功",
        refused: "连接被拒绝",
        timed_out: "连接超时",
        unreachable: "不可达",
        failed: "连接失败",
      };
      return one(
        `TCP ${r.host ?? ""}:${r.port ?? ""} · ${states[r.outcome] ?? "结果未确认"}${r.connect_ms != null ? ` · 耗时 ${r.connect_ms} ms` : ""}${r.message ? ` · ${r.message}` : ""}`,
        r.outcome === "connected"
          ? "success"
          : ["failed", "refused", "unreachable"].includes(r.outcome)
            ? "error"
            : "warning",
      );
    }
    case "tls":
      if (d.kind === "handshake")
        return one(
          `TLS ${d.server_name ?? ""} · ${d.verified ? "证书验证通过" : d.verification_attempted ? "验证或握手失败" : "未完成验证"}${d.verification_error || d.handshake_error ? ` · ${d.verification_error ?? d.handshake_error}` : ""}${d.truncated ? " · 证书证据已截断" : ""}`,
          d.verified
            ? "success"
            : d.verification_attempted
              ? "error"
              : "warning",
        );
      if (d.kind === "certificate" && (d.metadata_truncated || d.parse_error))
        return one(
          `证书元数据${d.parse_error ? `解析失败：${d.parse_error}` : "已截断"}`,
          "warning",
        );
      if (d.kind === "trust" && d.load_errors?.length)
        return one(`根证书加载问题：${d.load_errors.join("；")}`, "warning");
      return [];
    case "http":
      if (d.kind === "response")
        return one(
          `HTTP ${d.status} · ${d.service ?? ""}${d.body_truncated ? " · 正文预览已截断" : ""}`,
          d.status >= 400
            ? "error"
            : d.status >= 200 && d.status < 300
              ? "success"
              : "info",
        );
      if (d.kind === "failure") return one(`HTTP 失败：${d.message}`, "error");
      if (d.kind === "redirect" && !d.followed)
        return one(`HTTP 已停止跳转：${d.reason ?? "未继续请求"}`, "warning");
      return [];
    case "public_ip":
      return one(`公网出口 IP：${event.address ?? "未知"}`, "info");
    case "stun": {
      const observation = event.observation ?? {};
      if (observation.error)
        return one(
          `STUN 失败：${observation.error} · 单服务观察不判定 NAT 类型`,
          "error",
        );
      return one(
        `STUN 出口：${observation.mapped_endpoint ?? "未获得映射"} · 单服务观察不判定 NAT 类型`,
        observation.response_received && observation.mapped_endpoint
          ? "info"
          : "warning",
      );
    }
    case "nat":
      return event.kind === "report" ? [natConclusion(event.report ?? {})] : [];
    case "echo": {
      const o = event.observation ?? {};
      return o.tun_local
        ? one("TUN 本地应答，不能确认目标可达；耗时不计入统计。", "warning")
        : one(
            `${o.target ?? "目标"} · ${o.proves_reachability ? "本次应答已确认可达" : "未能确认可达"}${o.proves_reachability && o.rtt_ms != null ? ` · ${o.rtt_ms} ms` : ""}${o.message ? ` · ${o.message}` : ""}`,
            o.proves_reachability ? "success" : "warning",
          );
    }
    case "trace_summary":
      return one(
        `逐跳探测：${event.reached ? "已观察到目标应答" : "未确认到达目标"} · ${event.message ?? "未响应不代表设备不存在或故障"}`,
        event.reached ? "success" : "warning",
      );
    case "attempt":
      return event.error ? one(`探测失败：${event.error}`, "error") : [];
    case "summary": {
      const s = event.summary ?? event;
      return one(
        `探测计数：发出 ${s.sent ?? 0} 次，收到 ${s.received ?? 0} 次${s.avg_ms != null ? ` · 平均耗时 ${s.avg_ms} ms` : ""}（计数不单独判断工具结论）`,
        "info",
      );
    }
    case "dns":
      if (d.kind === "response")
        return one(
          `DNS ${d.rcode ?? "未知响应"} · ${d.parsed ? `回答 ${d.answer_count ?? 0} 条` : "响应无法解析"}${d.truncated ? " · 响应已截断" : ""}`,
          ["server_failure", "dns_error"].includes(d.outcome)
            ? "error"
            : d.parsed && d.outcome === "resolved" && !d.truncated
              ? "success"
              : "warning",
        );
      if (d.kind === "records" && d.section === "answer")
        return (d.records ?? []).map((r: Row) => ({
          text: `${r.name} ${r.record_type} → ${String(r.text ?? "").slice(0, 160)}${String(r.text ?? "").length > 160 ? "…（完整内容见原始报告）" : ""}${r.text_truncated ? " [已截断]" : ""}`,
          tone: r.text_truncated ? "warning" : "info",
        }));
      if (d.kind === "failure" || d.kind === "error")
        return one(
          `DNS 查询失败：${d.message ?? d.error ?? "未取得有效结果"}`,
          "error",
        );
      return [];
    case "project_status":
      return one(
        `项目 ${event.project_id} · ${({ complete: "完成", failed: "失败", partial: "部分完成", cancelled: "已取消", running: "运行中" } as Row)[event.state] ?? event.state}${event.message ? ` · ${event.message}` : ""}`,
        event.state === "failed"
          ? "error"
          : event.state === "complete"
            ? "info"
            : "warning",
      );
    case "analysis": {
      const r = event.report ?? event.analysis ?? event;
      const rows: Conclusion[] = [];
      if (r.incomplete || r.truncated)
        rows.push({
          text: "证据不完整或已截断，保留部分结论。",
          tone: "warning",
        });
      for (const f of r.findings ?? [])
        rows.push({
          text: `${f.title} · 可信程度：${({ high: "高", medium: "中", low: "低" } as Row)[f.confidence] ?? "未知"} · ${f.explanation}${f.recommendation ? ` · 建议：${f.recommendation}` : ""}`,
          tone:
            f.severity === "risk"
              ? "error"
              : f.severity === "needs_verification"
                ? "warning"
                : "info",
        });
      for (const note of r.notes ?? []) rows.push({ text: note, tone: "info" });
      return rows.length ? rows : one("暂无可支持的诊断结论。", "muted");
    }
    default:
      return [];
  }
}
export function ConclusionLines({ rows }: { rows: Conclusion[] }) {
  return (
    <>
      {rows.map((row, i) => (
        <p key={i} className={`network-conclusion network-tone-${row.tone}`}>
          <span className="network-tone-label">{labels[row.tone]}</span>
          {row.text}
        </p>
      ))}
    </>
  );
}

/** Old queued/running statuses stay in evidence, while the conclusion uses the latest state. */
export function latestConclusions<T extends { data: unknown }>(
  items: T[],
): T[] {
  const keys = items.map((item) => {
    let event = item.data as Row;
    for (
      let depth = 0;
      depth < 4 && event.event === "project" && event.detail;
      depth++
    )
      event = event.detail;
    return event.event === "project_status"
      ? JSON.stringify([event.project_id, event.path_id, event.round])
      : null;
  });
  const latest = new Map<string, number>();
  keys.forEach((key, i) => {
    if (key != null) latest.set(key, i);
  });
  return items.filter((_, i) => keys[i] == null || latest.get(keys[i]!) === i);
}
