import { useEffect, useRef, useState } from "react";
import * as API from "../api";
import type { MonitorSnapshot } from "../generated/management";
import {
  appendPoint,
  exportMonitor,
  formatBytes,
  HISTORY_LIMIT,
  meanDurationMs,
  monitorPoint,
  sameProcess,
  type MonitorPoint,
} from "../monitor";
import { Empty } from "./common";

type Props = {
  agent: string;
  instance: string;
  online: boolean;
  onError: (error: unknown) => void;
};

const clock = (value: string) => {
  const date = new Date(Number(value));
  return Number.isFinite(date.getTime()) ? date.toLocaleTimeString() : "—";
};
const percent = (value: number | null) =>
  value === null ? "—" : `${value.toFixed(1)}%`;
const rate = (value: number | null) =>
  value === null ? "—" : `${formatBytes(value)}/s`;

const subsystemNames: Record<string, string> = {
  routing: "路由决策",
  rule: "规则引擎",
  rule_ipc: "规则 IPC",
  dns: "DNS",
  server: "服务端",
  runtime: "运行时",
  workers: "托管 worker（按代号）",
};
function InternalMetrics({ point }: { point: MonitorPoint | undefined }) {
  const groups = new Map<
    string,
    NonNullable<MonitorSnapshot["internal"]>["metrics"]
  >();
  for (const metric of point?.internal?.metrics ?? []) {
    const group = metric.name.split(".")[0];
    groups.set(group, [...(groups.get(group) ?? []), metric]);
  }
  const all = new Map(
    (point?.internal?.metrics ?? []).map((metric) => [metric.name, metric]),
  );
  const meanDuration = (
    metric: NonNullable<MonitorSnapshot["internal"]>["metrics"][number],
  ) => {
    if (
      !metric.name.endsWith(".duration_us") ||
      metric.unit !== "us" ||
      metric.kind !== "counter"
    )
      return null;
    const completed = all.get(
      `${metric.name.slice(0, -".duration_us".length)}.completed`,
    );
    return completed?.kind === "counter" && completed.unit === "count"
      ? meanDurationMs(metric.value, completed.value)
      : null;
  };
  return (
    <section
      className="card monitor-internal"
      aria-labelledby="monitor-internal-title"
    >
      <div className="card-pad">
        <h2 id="monitor-internal-title">程序内部统计</h2>
        <p className="muted monitor-caption">
          查看核心内的请求、缓存与运行计数。累计量与平均耗时从当前统计周期开始；平均耗时包含已完成的错误请求，不含取消的请求。速率按相邻采样计算。
        </p>
      </div>
      {groups.size === 0 ? (
        <p className="card-pad muted">
          内部统计暂不可用。请确认核心正在运行，且已更新到支持监视器的版本。
        </p>
      ) : (
        Array.from(groups, ([group, metrics]) => (
          <div key={group} className="monitor-internal-group">
            <h3>{subsystemNames[group] ?? group}</h3>
            <div className="table-scroll">
              <table>
                <thead>
                  <tr>
                    <th>指标</th>
                    <th>当前值</th>
                    <th>每秒增量</th>
                    <th>周期内平均耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {metrics.map((metric) => (
                    <tr key={metric.name}>
                      <td>
                        {metric.description}
                        <small className="mono muted">
                          {metric.name} ·{" "}
                          {metric.kind === "counter" ? "累计" : "当前"}
                        </small>
                      </td>
                      <td className="mono">
                        {metric.unit === "bytes"
                          ? formatBytes(metric.value)
                          : metric.value}
                        {metric.unit !== "bytes" && metric.unit !== "count"
                          ? ` ${metric.unit}`
                          : ""}
                      </td>
                      <td className="mono">
                        {point?.internal_per_second[metric.name] != null
                          ? `${point.internal_per_second[metric.name]!.toFixed(1)}/s`
                          : "—"}
                      </td>
                      <td className="mono">{meanDuration(metric) ?? "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        ))
      )}
    </section>
  );
}

/** All plots share the same timestamp domain so spikes can be compared directly. */
function Trace({
  label,
  points,
  series,
  format,
  minimum = 1,
}: {
  label: string;
  points: MonitorPoint[];
  series: { key: keyof MonitorPoint; color: string; name: string }[];
  format: (value: number) => string;
  minimum?: number;
}) {
  const values = points
    .flatMap((point) =>
      series.map(({ key }) =>
        point[key] === null ? null : Number(point[key]),
      ),
    )
    .filter((v): v is number => v !== null && Number.isFinite(v));
  const max = Math.max(minimum, ...values);
  const start = Number(points[0]?.sampled_at_ms ?? 0);
  const end = Number(points.at(-1)?.sampled_at_ms ?? start);
  return (
    <div className="monitor-trace">
      <div className="monitor-axis">
        <span>{format(max)}</span>
        <span>0</span>
      </div>
      <svg
        viewBox="0 0 560 100"
        preserveAspectRatio="none"
        role="img"
        aria-label={`${label}趋势，${points.length} 个采样点`}
      >
        {[0, 50, 100].map((y) => (
          <line
            key={y}
            x1="0"
            x2="560"
            y1={y}
            y2={y}
            className="monitor-gridline"
          />
        ))}
        {series.map(({ key, color, name }) => {
          let path = "";
          let connected = false;
          points.forEach((point, i) => {
            const raw = point[key];
            const value = raw === null ? null : Number(raw);
            if (value === null || !Number.isFinite(value)) {
              connected = false;
              return;
            }
            if (
              point.segment_start ||
              (i > 0 && !sameProcess(points[i - 1], point))
            )
              connected = false;
            const x =
              end > start
                ? ((Number(point.sampled_at_ms) - start) / (end - start)) * 560
                : 560;
            const y = 98 - Math.min(1, Math.max(0, value / max)) * 96;
            path += `${connected ? "L" : "M"}${x.toFixed(2)},${y.toFixed(2)} `;
            // A tiny horizontal segment also makes isolated samples visible.
            if (!connected) path += `l0.01,0 `;
            connected = true;
          });
          return (
            <path
              key={key}
              d={path}
              fill="none"
              stroke={color}
              strokeWidth="2"
              strokeLinecap="round"
              vectorEffect="non-scaling-stroke"
            >
              <title>{name}</title>
            </path>
          );
        })}
      </svg>
      {!values.length && (
        <span className="monitor-no-samples">等待有效采样</span>
      )}
      <div className="monitor-time">
        <span>{points[0] ? clock(points[0].sampled_at_ms) : "—"}</span>
        <span>{points.at(-1) ? clock(points.at(-1)!.sampled_at_ms) : "—"}</span>
      </div>
    </div>
  );
}

export default function Monitor(props: Props) {
  if (!props.instance)
    return <Empty>选择一个实例以查看性能、内存与网络。</Empty>;
  // Remount on either scope change; never show another node's buffered samples.
  return <MonitorSession key={`${props.agent}/${props.instance}`} {...props} />;
}

function MonitorSession({ agent, instance, online, onError }: Props) {
  const [points, setPoints] = useState<MonitorPoint[]>([]);
  const [interval, setIntervalMs] = useState(1000);
  const [paused, setPaused] = useState(false);
  const [visible, setVisible] = useState(document.visibilityState !== "hidden");
  const [error, setError] = useState("");
  const previous = useRef<MonitorSnapshot | null>(null);
  const lastReceived = useRef<number | null>(null);
  const cleared = useRef(0);
  const onErrorRef = useRef(onError);
  onErrorRef.current = onError;

  useEffect(() => {
    const update = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", update);
    return () => document.removeEventListener("visibilitychange", update);
  }, []);

  useEffect(() => {
    previous.current = null;
    if (!agent || !online || paused || !visible) return;
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let timeout: ReturnType<typeof setTimeout> | undefined;
    let abort: AbortController | undefined;
    const load = async () => {
      abort = new AbortController();
      let timedOut = false;
      const clearEpoch = cleared.current;
      timeout = setTimeout(() => {
        timedOut = true;
        abort?.abort();
      }, 10000);
      try {
        const response = await API.query<MonitorSnapshot>(
          agent,
          "monitor",
          { instance },
          abort.signal,
        );
        if (!active || clearEpoch !== cleared.current) return;
        if (response.type !== "monitor" || response.data.instance !== instance)
          throw new Error("监视器响应与当前实例不一致");
        const sample = response.data;
        // Use receipt time only to break long polling gaps; rates use source timestamps.
        if (
          lastReceived.current !== null &&
          performance.now() - lastReceived.current > interval * 3
        )
          previous.current = null;
        const point = monitorPoint(sample, previous.current);
        setPoints((old) => appendPoint(old, point));
        previous.current = sample;
        lastReceived.current = performance.now();
        setError(sample.error ?? "");
      } catch (e) {
        if (!active) return;
        previous.current = null;
        setError(
          timedOut
            ? "采样超时，将自动重试。"
            : e instanceof Error
              ? e.message
              : String(e),
        );
        if (e instanceof API.ApiError && e.code === "unauthorized")
          onErrorRef.current(e);
      } finally {
        clearTimeout(timeout);
        if (active) timer = setTimeout(load, interval);
      }
    };
    void load();
    return () => {
      active = false;
      clearTimeout(timer);
      clearTimeout(timeout);
      abort?.abort();
    };
  }, [agent, instance, interval, online, paused, visible]);

  const latest = points.at(-1);
  const blocked = paused || !online || !visible;
  const state = paused
    ? "已暂停"
    : !online
      ? "节点离线"
      : !visible
        ? "页面隐藏，已暂停"
        : error
          ? "采样异常"
          : "正在采样";
  const download = () => {
    const blob = new Blob([exportMonitor(agent, instance, interval, points)], {
      type: "application/json",
    });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `astra-monitor-${new Date().toISOString().replace(/[:.]/g, "-")}.json`;
    anchor.click();
    setTimeout(() => URL.revokeObjectURL(url), 0);
  };
  return (
    <div className="monitor">
      <section
        className="card card-pad monitor-controls"
        aria-label="监视器控制"
      >
        <div className="monitor-state" role="status">
          <span className={`dot ${error ? "err" : blocked ? "warn" : "ok"}`} />
          <strong>{state}</strong>
          <span className="muted">
            {points.length} / {HISTORY_LIMIT} 个采样点
          </span>
        </div>
        <div className="toolbar">
          <label className="monitor-interval">
            采样间隔{" "}
            <select
              className="input"
              value={interval}
              onChange={(e) => setIntervalMs(Number(e.target.value))}
            >
              <option value={1000}>1 秒</option>
              <option value={2000}>2 秒</option>
              <option value={5000}>5 秒</option>
            </select>
          </label>
          <button className="btn" onClick={() => setPaused((v) => !v)}>
            {paused ? "继续采样" : "暂停采样"}
          </button>
          <button
            className="btn"
            disabled={!points.length}
            onClick={() => {
              cleared.current++;
              previous.current = null;
              setPoints([]);
              setError("");
            }}
          >
            清空记录
          </button>
          <button className="btn" disabled={!points.length} onClick={download}>
            导出 JSON
          </button>
        </div>
      </section>
      {error && (
        <div className="notice-bar error-message" role="alert">
          {error} 已收到的数据保留；请留意采样时间。
        </div>
      )}
      <p className="monitor-caption muted">
        当前实例 <strong>{instance}</strong> · PID{" "}
        <span className="mono">{latest?.pid ?? "—"}</span> · 运行{" "}
        <span className="mono">{latest?.uptime_secs ?? "—"}</span> 秒 · 最近采样{" "}
        <span className="mono">
          {latest ? clock(latest.sampled_at_ms) : "—"}
        </span>
        {blocked && " · 显示暂停前记录"}
      </p>
      <section className="card monitor-panel" aria-labelledby="monitor-cpu">
        <div className="monitor-metrics">
          <h2 id="monitor-cpu">进程性能</h2>
          <dl>
            <div>
              <dt>CPU</dt>
              <dd>{percent(latest?.cpu_percent ?? null)}</dd>
            </div>
          </dl>
          <p className="muted">100% = 占用一个逻辑核心；首次采样等待基线。</p>
        </div>
        <Trace
          label="CPU"
          points={points}
          series={[{ key: "cpu_percent", color: "var(--accent)", name: "CPU" }]}
          format={(value) => `${value.toFixed(0)}%`}
          minimum={100}
        />
      </section>
      <section className="card monitor-panel" aria-labelledby="monitor-memory">
        <div className="monitor-metrics">
          <h2 id="monitor-memory">进程内存</h2>
          <dl>
            <div>
              <dt>驻留内存 RSS</dt>
              <dd>{formatBytes(latest?.memory_bytes ?? null)}</dd>
            </div>
            <div>
              <dt>虚拟内存</dt>
              <dd className="monitor-secondary">
                {formatBytes(latest?.virtual_memory_bytes ?? null)}
              </dd>
            </div>
          </dl>
          <p className="muted">曲线显示实际驻留内存；虚拟内存不是物理占用。</p>
        </div>
        <Trace
          label="驻留内存"
          points={points}
          series={[
            { key: "memory_bytes", color: "var(--ok)", name: "驻留内存 RSS" },
          ]}
          format={formatBytes}
        />
      </section>
      <section className="card monitor-panel" aria-labelledby="monitor-network">
        <div className="monitor-metrics">
          <h2 id="monitor-network">核心网络</h2>
          <dl>
            <div>
              <dt className="monitor-up">↑ 上传</dt>
              <dd>{rate(latest?.upload_per_second ?? null)}</dd>
            </div>
            <div>
              <dt className="monitor-down">↓ 下载</dt>
              <dd>{rate(latest?.download_per_second ?? null)}</dd>
            </div>
          </dl>
          <p className="muted">速率由相邻采样计算，等待第二次有效采样。</p>
        </div>
        <Trace
          label="网络吞吐"
          points={points}
          series={[
            { key: "upload_per_second", color: "var(--up)", name: "上传" },
            { key: "download_per_second", color: "var(--down)", name: "下载" },
          ]}
          format={rate}
        />
        <dl className="monitor-totals">
          <div>
            <dt>累计上传</dt>
            <dd>{formatBytes(latest?.upload_bytes ?? null)}</dd>
          </div>
          <div>
            <dt>累计下载</dt>
            <dd>{formatBytes(latest?.download_bytes ?? null)}</dd>
          </div>
          <div>
            <dt>活动连接</dt>
            <dd>{latest?.active_connections ?? "—"}</dd>
          </div>
          <div>
            <dt>累计连接</dt>
            <dd>{latest?.total_connections ?? "—"}</dd>
          </div>
        </dl>
      </section>
      <InternalMetrics point={latest} />
      <p className="muted monitor-caption">
        CPU 与内存仅统计受管 PID，不包含托管 worker
        子进程。网络采用核心运行时计数，可能汇总托管 worker
        流量；规则核心无网络统计时显示 —。记录仅保存在此页面，离开页面即清空。
      </p>
    </div>
  );
}
