// @vitest-environment jsdom
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import * as API from "./api";
import {
  exitProbeFields,
  useExitProbe,
  clampInterval,
  defaultExitProbe,
  exitChain,
  readExitProbe,
  retainExitProbe,
} from "./exitProbe";

describe("exit probe", () => {
  it("follows group selections to the final exit node", () => {
    const runtime = {
      mode: "rule",
      groups: [
        { name: "PROXY", selected: "Auto" },
        { name: "Auto", selected: "Tokyo" },
        { name: "Loop", selected: "PROXY" },
      ],
    };
    expect(exitChain(runtime)).toEqual(["PROXY", "Auto", "Tokyo"]);
    expect(exitChain({ ...runtime, mode: "direct" })).toEqual(["DIRECT"]);
    expect(exitChain(null)).toEqual([]);
  });
  it("keeps the shared defaults and clamps the interval", () => {
    // Node's own experimental localStorage can shadow jsdom's; use a plain store.
    const store = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
      removeItem: (key: string) => void store.delete(key),
    });
    expect(readExitProbe()).toEqual(defaultExitProbe);
    expect(defaultExitProbe).toEqual({
      ip: false,
      latency: true,
      interval: 30,
      keepAlive: true,
    });
    expect(clampInterval(1)).toBe(5);
    expect(clampInterval(99999)).toBe(3600);
    retainExitProbe({ ...defaultExitProbe, ip: true, interval: 60 });
    expect(readExitProbe()).toMatchObject({ ip: true, interval: 60 });
    vi.unstubAllGlobals();
  });
});

describe("exit probe header fields", () => {
  const off = { ...defaultExitProbe, ip: false, latency: false };
  it("shows found results and keeps places only for enabled settings", () => {
    expect(exitProbeFields(off, undefined, "r", null)).toEqual({
      ip: null,
      latency: null,
    });
    const once = { round: "r", latency: { ms: 40 }, periodic: false };
    expect(exitProbeFields(off, "203.0.113.7", "r", once)).toEqual({
      ip: "203.0.113.7",
      latency: { ms: 40 },
    });
    // Interval results hide when latency goes off; another node's never show.
    expect(exitProbeFields(off, undefined, "r", { ...once, periodic: true }))
      .toEqual({ ip: null, latency: null });
    expect(exitProbeFields(defaultExitProbe, undefined, "other", once)).toEqual(
      { ip: null, latency: "pending" },
    );
  });
});

describe("useExitProbe", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });
  const target = (outbound: string) => ({
    agent: "a",
    instance: "p",
    pid: "42",
    outbound,
  });
  const off = { ...defaultExitProbe, ip: false, latency: false };

  it("probes IP and latency once per connect or switch with settings off", async () => {
    const query = vi
      .spyOn(API, "query")
      .mockResolvedValue({
        type: "exit_probe",
        data: { connect_ms: "33", ip: "203.0.113.7" },
      } as never);
    const { result } = renderHook(() => useExitProbe(target("Tokyo"), off));
    await waitFor(() =>
      expect(result.current).toEqual({
        ip: "203.0.113.7",
        latency: { ms: 33 },
      }),
    );
    expect(query).toHaveBeenCalledTimes(1);
    expect(query.mock.calls[0][2]).toMatchObject({
      outbound: "Tokyo",
      ip: true,
    });
  });

  it("drops a switched-away node's round so its answer never shows", async () => {
    const pending: { resolve: (v: unknown) => void; signal: AbortSignal }[] =
      [];
    vi.spyOn(API, "query").mockImplementation(
      (_agent, _type, _data, signal) =>
        new Promise((resolve) => pending.push({ resolve, signal: signal! })) as never,
    );
    const { result, rerender } = renderHook(
      ({ outbound }) => useExitProbe(target(outbound), off),
      { initialProps: { outbound: "Tokyo" } },
    );
    await waitFor(() => expect(pending).toHaveLength(1));
    rerender({ outbound: "Osaka" });
    await waitFor(() => expect(pending).toHaveLength(2));
    expect(pending[0].signal.aborted).toBe(true);
    await act(async () => {
      pending[0].resolve({
        type: "exit_probe",
        data: { connect_ms: "9", ip: "198.51.100.1" },
      });
    });
    expect(result.current).toEqual({ ip: null, latency: null });
    await act(async () => {
      pending[1].resolve({
        type: "exit_probe",
        data: { connect_ms: "21", ip: "203.0.113.9" },
      });
    });
    await waitFor(() =>
      expect(result.current).toEqual({
        ip: "203.0.113.9",
        latency: { ms: 21 },
      }),
    );
  });
});
