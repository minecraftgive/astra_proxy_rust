/** Link display: the always-visible instance chain and the probe path read
 * from the Agent topology. Clicking a step shows the backend evidence. */
import { useEffect, useState } from "react";
import type { NetworkResult } from "../generated/management";
import {
  arrow,
  chainSteps,
  levelLabel,
  pathStrips,
  sectionLabel,
  type LiveChain,
} from "./networkChain";

type Row = Record<string, any>;
type Selection = {
  path: number;
  edge?: number;
  node?: string;
  attachment?: number;
};

export function LiveChainView({
  chain,
  nat,
}: {
  chain: LiveChain;
  nat?: string;
}) {
  const steps = chainSteps(chain);
  return (
    <div
      className="network-chain"
      aria-label="当前链路"
      data-testid="network-live-chain"
    >
      <span className="network-chain-title">当前链路</span>
      {steps.map(([label, tone], i) => (
        <span key={i} className="network-chain-step-wrap">
          {i > 0 && <span className="network-chain-arrow">›</span>}
          <span className={`network-chain-step is-${tone}`}>
            {label}
            {nat && label.startsWith("网卡 ") && (
              <span className="network-chain-badge">NAT：{nat}</span>
            )}
          </span>
        </span>
      ))}
    </div>
  );
}

function referenced(items: NetworkResult[], evidence: Row): Row | null {
  for (const item of items) {
    let event = item.data as Row;
    for (let depth = 0; depth < 8 && event?.event === "project"; depth++) {
      const p = event.project ?? {};
      if (
        p.project_id === evidence.project_id &&
        p.path_id === evidence.path_id &&
        String(p.round) === String(evidence.round) &&
        String(p.index) === String(evidence.event_index) &&
        p.sampled_at_ms === evidence.sampled_at_ms
      )
        return event.detail ?? null;
      event = event.detail;
    }
  }
  return null;
}

function Evidence({
  analysis,
  selection,
  items,
}: {
  analysis: Row;
  selection: Selection;
  items: NetworkResult[];
}) {
  const path = analysis.topology?.[selection.path];
  if (!path) return null;
  const lines: string[] = [];
  let evidence: Row[] = [];
  if (selection.edge !== undefined) {
    const edge = path.edges?.[selection.edge];
    if (!edge) return null;
    lines.push(
      `${sectionLabel(edge.section)} · ${levelLabel(edge.level)}`,
      edge.description ?? "",
    );
    evidence = edge.evidence ?? [];
  } else if (selection.node !== undefined) {
    const node = (path.nodes ?? []).find((n: Row) => n.id === selection.node);
    lines.push(
      `节点：${node?.label ?? selection.node} · ${node?.role ?? "未知"}`,
    );
    for (const edge of (path.edges ?? []).filter(
      (e: Row) => e.from === selection.node || e.to === selection.node,
    )) {
      lines.push(
        `${sectionLabel(edge.section)} · ${levelLabel(edge.level)} · ${edge.description ?? ""}`,
      );
      evidence = evidence.concat(edge.evidence ?? []);
    }
  } else if (selection.attachment !== undefined) {
    const attachment = path.attachments?.[selection.attachment];
    if (!attachment) return null;
    lines.push(
      `旁注（不计作逐跳设备）：${attachment.kind} · ${attachment.label}`,
    );
    evidence = attachment.evidence ?? [];
  }
  if (path.truncated || analysis.truncated || analysis.incomplete)
    lines.push("证据不完整或已截断；不能据此确认未测路径。");
  const unique = evidence.filter(
    (e, i) =>
      evidence.findIndex((o) => JSON.stringify(o) === JSON.stringify(e)) === i,
  );
  return (
    <div className="network-evidence" role="region" aria-label="路径证据">
      {lines.map((line, i) => (
        <p key={i}>{line}</p>
      ))}
      {unique.map((e, i) => {
        const event = referenced(items, e);
        return (
          <details key={i} open={i === 0}>
            <summary>
              项目 {e.project_id} · 路径 {e.path_id} · 第 {e.round} 轮 · 来源{" "}
              {e.source}
            </summary>
            {event ? (
              <pre>{JSON.stringify(event, null, 2).slice(0, 16384)}</pre>
            ) : (
              <p className="muted">
                此引用的原始事件未保留在当前页；不补造证据。
              </p>
            )}
          </details>
        );
      })}
      {!unique.length && (
        <p className="muted">
          后端未提供此项的证据引用。未知边不代表探测成功。
        </p>
      )}
    </div>
  );
}

export function ProbePathView({
  analysis,
  items,
  operation,
}: {
  analysis: Row | null;
  items: NetworkResult[];
  operation: string;
}) {
  const [selection, setSelection] = useState<Selection | null>(null);
  useEffect(() => setSelection(null), [operation]);
  const strips = pathStrips(analysis);
  if (!analysis || !strips.length) return null;
  return (
    <section className="network-path" aria-label="探测路径">
      <p className="muted">
        探测路径 · ─› 实测 · ┄› 配置推断 · ?› 未知 · 点击查看证据
      </p>
      {strips.map((strip, p) => (
        <div key={p} className="network-path-group">
          {(strips.length > 1 || strip.truncated) && (
            <p className="muted">
              路径 {strip.pathId}
              {strip.truncated && " · 已截断"}
            </p>
          )}
          {strip.rows.map((row, r) => (
            <div key={r} className="network-chain">
              <span className="network-chain-title">
                {sectionLabel(row.section)}
              </span>
              {row.steps.map((step, s) => (
                <span key={s} className="network-chain-step-wrap">
                  {step.edge !== null && (
                    <button
                      type="button"
                      className={`network-chain-arrow is-${step.level ?? "unknown"}`}
                      title={levelLabel(step.level)}
                      onClick={() =>
                        setSelection({ path: p, edge: step.edge! })
                      }
                    >
                      {arrow(step.level)}
                    </button>
                  )}
                  <button
                    type="button"
                    className="network-chain-step"
                    onClick={() => setSelection({ path: p, node: step.id })}
                  >
                    {step.label}
                  </button>
                </span>
              ))}
            </div>
          ))}
          {strip.attachments.map((attachment, a) => (
            <button
              key={a}
              type="button"
              className="network-chain-step is-muted"
              onClick={() => setSelection({ path: p, attachment: a })}
            >
              旁注：{attachment}
            </button>
          ))}
        </div>
      ))}
      {selection && (
        <>
          <button
            type="button"
            className="btn sm"
            onClick={() => setSelection(null)}
          >
            关闭证据
          </button>
          <Evidence analysis={analysis} selection={selection} items={items} />
        </>
      )}
    </section>
  );
}
