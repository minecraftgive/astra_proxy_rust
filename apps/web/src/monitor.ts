import type { MonitorSnapshot } from "./generated/management";

export const HISTORY_LIMIT = 120;
export type MonitorPoint = MonitorSnapshot & {
  upload_per_second: number | null;
  download_per_second: number | null;
  segment_start: boolean;
  internal_per_second: Record<string, number | null>;
};

const counter = (value: string | null | undefined): bigint | null =>
  typeof value === "string" && /^\d+$/.test(value) ? BigInt(value) : null;

export function sameProcess(a: MonitorSnapshot, b: MonitorSnapshot): boolean {
  const before = counter(a.uptime_secs);
  const after = counter(b.uptime_secs);
  return (
    a.pid !== null &&
    a.pid === b.pid &&
    before !== null &&
    after !== null &&
    after >= before
  );
}

/** Subtract integer counters before conversion: long-running cores can exceed 2^53. */
export function monitorPoint(
  sample: MonitorSnapshot,
  previous: MonitorSnapshot | null,
): MonitorPoint {
  let upload: number | null = null;
  let download: number | null = null;
  const internalRates: Record<string, number | null> = {};
  for (const metric of sample.internal?.metrics ?? [])
    internalRates[metric.name] = null;
  if (previous && sameProcess(previous, sample)) {
    const start = counter(previous.sampled_at_ms);
    const end = counter(sample.sampled_at_ms);
    const up0 = counter(previous.upload_bytes);
    const up1 = counter(sample.upload_bytes);
    const down0 = counter(previous.download_bytes);
    const down1 = counter(sample.download_bytes);
    if (sample.internal && previous.internal?.epoch === sample.internal.epoch) {
      const oldMetrics = new Map(
        previous.internal.metrics.map((metric) => [metric.name, metric]),
      );
      for (const metric of sample.internal.metrics) {
        const old = oldMetrics.get(metric.name);
        if (
          metric.kind !== "counter" ||
          metric.unit !== "count" ||
          old?.kind !== metric.kind ||
          old.unit !== metric.unit
        )
          continue;
        const before = counter(old.value);
        const after = counter(metric.value);
        // Worker RPCs can complete well after the network snapshot. Each
        // internal counter must use its own capture clock, never that timestamp.
        const metricStart = counter(old.sampled_at_ms);
        const metricEnd = counter(metric.sampled_at_ms);
        if (
          before === null ||
          after === null ||
          after < before ||
          metricStart === null ||
          metricEnd === null ||
          metricEnd <= metricStart
        )
          continue;
        const perSecond =
          Number(after - before) / (Number(metricEnd - metricStart) / 1000);
        internalRates[metric.name] = Number.isFinite(perSecond)
          ? perSecond
          : null;
      }
    }
    if (
      start !== null &&
      end !== null &&
      end > start &&
      up0 !== null &&
      up1 !== null &&
      up1 >= up0 &&
      down0 !== null &&
      down1 !== null &&
      down1 >= down0
    ) {
      const seconds = Number(end - start) / 1000;
      const up = Number(up1 - up0) / seconds;
      const down = Number(down1 - down0) / seconds;
      upload = Number.isFinite(up) ? up : null;
      download = Number.isFinite(down) ? down : null;
    }
  }
  return {
    ...sample,
    upload_per_second: upload,
    download_per_second: download,
    internal_per_second: internalRates,
    segment_start:
      !previous ||
      !sameProcess(previous, sample) ||
      Number(sample.sampled_at_ms) <= Number(previous.sampled_at_ms),
  };
}

export function appendPoint(
  history: MonitorPoint[],
  point: MonitorPoint,
): MonitorPoint[] {
  const latest = history.at(-1);
  if (
    latest?.sampled_at_ms === point.sampled_at_ms &&
    latest.pid === point.pid
  ) {
    // A cached network snapshot may accompany fresh worker counters. Keep the
    // newest internal reading without adding a duplicate point to the plots.
    return latest.internal === point.internal
      ? history
      : [...history.slice(0, -1), point];
  }
  if (latest && Number(point.sampled_at_ms) < Number(latest.sampled_at_ms))
    return [point];
  return [...history, point].slice(-HISTORY_LIMIT);
}

export function formatBytes(value: number | string | null): string {
  if (value === null) return "—";
  let n = Number(value);
  if (!Number.isFinite(n) || n < 0) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
  let unit = 0;
  while (n >= 1024 && unit < units.length - 1) {
    n /= 1024;
    unit++;
  }
  return `${n.toFixed(unit ? 1 : 0)} ${units[unit]}`;
}

/** Cumulative mean since the same epoch, truncated to microsecond precision. */
export function meanDurationMs(
  microseconds: string,
  completed: string,
): string | null {
  const total = counter(microseconds);
  const count = counter(completed);
  if (total === null || count === null || count === 0n) return null;
  const mean = total / count;
  return `${mean / 1000n}.${String(mean % 1000n).padStart(3, "0")} ms`;
}

export function exportMonitor(
  agent: string,
  instance: string,
  interval: number,
  samples: MonitorPoint[],
) {
  return JSON.stringify(
    {
      version: 1,
      exported_at: new Date().toISOString(),
      agent,
      instance,
      interval_ms: interval,
      scope: "CPU and memory: managed PID only; network: core runtime counters",
      cpu_percent_basis: "100% = one logical CPU",
      samples,
    },
    null,
    2,
  );
}
