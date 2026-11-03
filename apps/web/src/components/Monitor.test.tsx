// @vitest-environment jsdom
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import * as API from "../api";
import Monitor from "./Monitor";
import type { MonitorSnapshot } from "../generated/management";

vi.mock("../api", async (original) => ({
  ...(await original<typeof API>()),
  query: vi.fn(),
}));
const sample = (patch: Partial<MonitorSnapshot> = {}): MonitorSnapshot => ({
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
const reply = (patch: Partial<MonitorSnapshot> = {}) => ({
  type: "monitor",
  data: sample(patch),
});
const advance = async (ms: number) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};
const flush = async () => {
  await act(async () => {
    await Promise.resolve();
  });
};
let visibility: DocumentVisibilityState;
beforeEach(() => {
  vi.useFakeTimers();
  vi.mocked(API.query).mockReset();
  visibility = "visible";
  vi.spyOn(document, "visibilityState", "get").mockImplementation(
    () => visibility,
  );
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});
const props = {
  agent: "agent",
  instance: "proxy",
  online: true,
  onError: vi.fn(),
};

it("never overlaps requests, and pause actually stops polling", async () => {
  let resolve!: (value: ReturnType<typeof reply>) => void;
  vi.mocked(API.query)
    .mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    )
    .mockResolvedValue(reply({ sampled_at_ms: "2000" }));
  render(<Monitor {...props} />);
  await advance(5000);
  expect(API.query).toHaveBeenCalledTimes(1);
  await act(async () => resolve(reply()));
  expect(screen.getByText("12.5%")).toBeTruthy();
  await advance(1000);
  expect(API.query).toHaveBeenCalledTimes(2);
  fireEvent.click(screen.getByRole("button", { name: "暂停采样" }));
  await advance(5000);
  expect(API.query).toHaveBeenCalledTimes(2);
  expect(screen.getByRole("status").textContent).toContain("已暂停");
  fireEvent.click(screen.getByRole("button", { name: "继续采样" }));
  await flush();
  expect(API.query).toHaveBeenCalledTimes(3);
});

it("aborts an old scope and discards late responses when selecting another node", async () => {
  let resolve!: (value: ReturnType<typeof reply>) => void;
  vi.mocked(API.query)
    .mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    )
    .mockResolvedValue(reply({ cpu_percent: 23 }));
  const { rerender } = render(<Monitor {...props} />);
  const signal = vi.mocked(API.query).mock.calls[0][3]!;
  rerender(<Monitor {...props} agent="another-agent" />);
  await flush();
  expect(signal.aborted).toBe(true);
  expect(screen.getByText("23.0%")).toBeTruthy();
  await act(async () => resolve(reply()));
  expect(screen.queryByText("12.5%")).toBeNull();
  expect(screen.getByRole("status").textContent).toContain("1 / 120");
});

it("suspends when hidden or offline and resets network baseline on resume", async () => {
  let index = 0;
  vi.mocked(API.query).mockImplementation(async () => {
    index++;
    return reply({
      sampled_at_ms: String(index * 1000),
      upload_bytes: String(index * 1024),
      download_bytes: String(index * 2048),
    });
  });
  const { rerender } = render(<Monitor {...props} />);
  await flush();
  await advance(1000);
  expect(screen.getByText("1.0 KiB/s")).toBeTruthy();
  visibility = "hidden";
  fireEvent(document, new Event("visibilitychange"));
  await advance(5000);
  expect(index).toBe(2);
  visibility = "visible";
  fireEvent(document, new Event("visibilitychange"));
  await flush();
  expect(index).toBe(3);
  expect(screen.queryByText("1.0 KiB/s")).toBeNull();
  rerender(<Monitor {...props} online={false} />);
  await advance(5000);
  expect(index).toBe(3);
  expect(screen.getByRole("status").textContent).toContain("节点离线");
});

it("retains last measurements with a visible stale error and recovers automatically", async () => {
  vi.mocked(API.query)
    .mockResolvedValueOnce(reply())
    .mockRejectedValueOnce(new Error("连接失败"))
    .mockResolvedValue(reply({ sampled_at_ms: "3000", cpu_percent: 25 }));
  render(<Monitor {...props} />);
  await flush();
  await advance(1000);
  expect(screen.getByRole("alert").textContent).toContain("连接失败");
  expect(screen.getByText("12.5%")).toBeTruthy();
  await advance(1000);
  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.getByText("25.0%")).toBeTruthy();
});

it("clears records even if a request is in flight, then uses a fresh rate baseline", async () => {
  let resolve!: (value: ReturnType<typeof reply>) => void;
  vi.mocked(API.query)
    .mockResolvedValueOnce(reply())
    .mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    )
    .mockResolvedValue(reply({ sampled_at_ms: "3000", cpu_percent: 30 }));
  render(<Monitor {...props} />);
  await flush();
  await advance(1000);
  fireEvent.click(screen.getByRole("button", { name: "清空记录" }));
  await act(async () => resolve(reply({ sampled_at_ms: "2000" })));
  expect(screen.getByRole("status").textContent).toContain("0 / 120");
  expect(
    screen.getByRole("button", { name: "导出 JSON" }).hasAttribute("disabled"),
  ).toBe(true);
  await advance(1000);
  expect(screen.getByText("30.0%")).toBeTruthy();
  expect(screen.getByRole("status").textContent).toContain("1 / 120");
});

it("aborts sampling when unmounted", async () => {
  vi.mocked(API.query).mockReturnValue(new Promise(() => {}));
  const { unmount } = render(<Monitor {...props} />);
  const signal = vi.mocked(API.query).mock.calls[0][3]!;
  unmount();
  expect(signal.aborted).toBe(true);
  await advance(20000);
  expect(API.query).toHaveBeenCalledTimes(1);
});

it("shows worker internal counters and pairs cumulative latency with completed requests", async () => {
  vi.mocked(API.query).mockResolvedValue(
    reply({
      internal: {
        epoch: "host-boot",
        metrics: [
          {
            name: "workers.2.routing.completed",
            value: "2",
            kind: "counter",
            unit: "count",
            description: "worker 2 · 路由完成",
          },
          {
            name: "workers.2.routing.duration_us",
            value: "2469",
            kind: "counter",
            unit: "us",
            description: "worker 2 · 路由耗时",
          },
          {
            name: "workers.3.routing.completed",
            value: "0",
            kind: "counter",
            unit: "count",
            description: "worker 3 · 路由完成",
          },
          {
            name: "workers.3.routing.duration_us",
            value: "0",
            kind: "counter",
            unit: "us",
            description: "worker 3 · 路由耗时",
          },
        ],
      },
    }),
  );
  render(<Monitor {...props} />);
  await flush();
  expect(screen.getByText("托管 worker（按代号）")).toBeTruthy();
  expect(screen.getByText("1.234 ms")).toBeTruthy();
  expect(screen.queryByText("0.000 ms")).toBeNull();
  expect(screen.getByText("周期内平均耗时")).toBeTruthy();
});
