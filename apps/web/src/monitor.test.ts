import { describe, expect, it } from "vitest";
import type { MonitorSnapshot } from "./generated/management";
import {
  appendPoint,
  exportMonitor,
  HISTORY_LIMIT,
  meanDurationMs,
  monitorPoint,
} from "./monitor";

export const sample = (
  patch: Partial<MonitorSnapshot> = {},
): MonitorSnapshot => ({
  instance: "proxy",
  pid: 42,
  sampled_at_ms: "1000",
  uptime_secs: "100",
  cpu_percent: 12.5,
  memory_bytes: "67108864",
  virtual_memory_bytes: null,
  upload_bytes: "1000",
  download_bytes: "2000",
  active_connections: 3,
  total_connections: "10",
  error: null,
  internal: null,
  ...patch,
});

describe("monitor measurements", () => {
  it("computes cumulative mean latency without floating point counter loss", () => {
    expect(meanDurationMs("2469", "2")).toBe("1.234 ms");
    expect(meanDurationMs("9007199254740993000", "9007199254740993")).toBe(
      "1.000 ms",
    );
    expect(meanDurationMs("9007199254740993", "1")).toBe(
      "9007199254740.993 ms",
    );
    expect(meanDurationMs("0", "2")).toBe("0.000 ms");
    expect(meanDurationMs("1", "0")).toBeNull();
    expect(meanDurationMs("invalid", "1")).toBeNull();
  });
  it("uses exact internal counter deltas only within the same statistics epoch", () => {
    const internal = (epoch: string, value: string, sampled_at_ms: string) => ({
      epoch,
      metrics: [
        {
          name: "routing.requests",
          sampled_at_ms,
          value,
          kind: "counter" as const,
          unit: "count",
          description: "路由请求",
        },
        {
          name: "routing.cache_entries",
          sampled_at_ms,
          value: "2",
          kind: "gauge" as const,
          unit: "count",
          description: "缓存条目",
        },
      ],
    });
    const before = sample({
      internal: internal("a", "9007199254740993", "1000"),
    });
    const next = sample({
      sampled_at_ms: "1500",
      internal: internal("a", "9007199254740996", "1500"),
    });
    const point = monitorPoint(next, before);
    expect(point.internal_per_second["routing.requests"]).toBe(6);
    expect(point.internal_per_second["routing.cache_entries"]).toBeNull();
    expect(
      monitorPoint(
        { ...next, internal: internal("b", "9007199254740996", "1500") },
        before,
      ).internal_per_second["routing.requests"],
    ).toBeNull();
    expect(
      monitorPoint({ ...next, internal: internal("a", "1", "1500") }, before)
        .internal_per_second["routing.requests"],
    ).toBeNull();
  });
  it("uses worker capture timestamps even when network snapshots are cached or skewed", () => {
    const reading = (value: string, sampled_at_ms?: string) => ({
      epoch: "worker",
      metrics: [
        {
          name: "workers.1.routing.started",
          value,
          sampled_at_ms,
          kind: "counter" as const,
          unit: "count",
          description: "请求",
        },
      ],
    });
    const before = sample({ internal: reading("100", "1000") });
    const next = sample({
      sampled_at_ms: "1500",
      internal: reading("103", "2500"),
    });
    expect(
      monitorPoint(next, before).internal_per_second[
        "workers.1.routing.started"
      ],
    ).toBe(2);
    const cachedNetwork = { ...next, sampled_at_ms: "1000" };
    const updated = monitorPoint(cachedNetwork, before);
    expect(updated.internal_per_second["workers.1.routing.started"]).toBe(2);
    const history = appendPoint([monitorPoint(before, null)], updated);
    expect(history).toHaveLength(1);
    expect(history[0].internal?.metrics[0].value).toBe("103");
    for (const stamp of [undefined, "1000", "999"]) {
      expect(
        monitorPoint({ ...next, internal: reading("103", stamp) }, before)
          .internal_per_second["workers.1.routing.started"],
      ).toBeNull();
    }
    expect(
      monitorPoint(next, sample({ internal: reading("100") }))
        .internal_per_second["workers.1.routing.started"],
    ).toBeNull();
  });
  it("subtracts exact cumulative counters before converting to rates", () => {
    const before = sample({
      upload_bytes: "9007199254740993",
      download_bytes: "9007199254740995",
    });
    const point = monitorPoint(
      sample({
        sampled_at_ms: "1500",
        upload_bytes: "9007199254740994",
        download_bytes: "9007199254740998",
      }),
      before,
    );
    expect(point.upload_per_second).toBe(2);
    expect(point.download_per_second).toBe(6);
  });
  it.each([
    { pid: 43 },
    { uptime_secs: "99" },
    { sampled_at_ms: "1000" },
    { sampled_at_ms: "999" },
    { upload_bytes: "0" },
    { download_bytes: "0" },
    { upload_bytes: null },
    { pid: null },
  ])(
    "waits for a fresh baseline after resets or unavailable counters: %j",
    (patch) => {
      const point = monitorPoint(
        sample({
          sampled_at_ms: "2000",
          upload_bytes: "2000",
          download_bytes: "3000",
          ...patch,
        }),
        sample(),
      );
      expect(point.upload_per_second).toBeNull();
      expect(point.download_per_second).toBeNull();
    },
  );
  it("leaves first sample and rule core rates unavailable", () => {
    expect(monitorPoint(sample(), null).upload_per_second).toBeNull();
    const rule = sample({
      upload_bytes: null,
      download_bytes: null,
      active_connections: null,
      total_connections: null,
    });
    expect(
      monitorPoint({ ...rule, sampled_at_ms: "2000" }, rule)
        .download_per_second,
    ).toBeNull();
  });
  it("bounds history, ignores cached samples and resets a backwards clock", () => {
    let history = [monitorPoint(sample(), null)];
    for (let i = 1; i < 200; i++)
      history = appendPoint(
        history,
        monitorPoint(sample({ sampled_at_ms: String(1000 + i * 1000) }), null),
      );
    expect(history).toHaveLength(HISTORY_LIMIT);
    expect(appendPoint(history, history.at(-1)!)).toBe(history);
    expect(appendPoint(history, monitorPoint(sample(), null))).toHaveLength(1);
  });
  it("exports exact counters and sampling scope without authentication data", () => {
    const point = monitorPoint(
      sample({ upload_bytes: "9007199254740993" }),
      null,
    );
    const exported = JSON.parse(exportMonitor("agent", "proxy", 1000, [point]));
    expect(exported.samples[0].upload_bytes).toBe("9007199254740993");
    expect(exported.samples[0].upload_per_second).toBeNull();
    expect(exported.cpu_percent_basis).toContain("one logical CPU");
    expect(exported.scope).toContain("managed PID only");
    expect(exported).not.toHaveProperty("token");
  });
});
