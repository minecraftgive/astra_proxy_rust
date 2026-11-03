// @vitest-environment jsdom
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { usePolling } from "./usePolling";
beforeEach(() => {
  vi.useFakeTimers();
  Object.defineProperty(document, "hidden", {
    configurable: true,
    value: false,
  });
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});
it("waits for completion and coalesces event bursts without overlap", async () => {
  let finish!: () => void;
  const fn = vi.fn(
    () =>
      new Promise<void>((resolve) => {
        finish = resolve;
      }),
  );
  const { result } = renderHook(() => usePolling(fn, 1000, []));
  expect(fn).toHaveBeenCalledTimes(1);
  await act(async () => {
    vi.advanceTimersByTime(5000);
    result.current();
    result.current();
  });
  expect(fn).toHaveBeenCalledTimes(1);
  await act(async () => {
    finish();
  });
  await act(async () => {
    vi.advanceTimersByTime(0);
  });
  expect(fn).toHaveBeenCalledTimes(2);
  await act(async () => {
    finish();
  });
  await act(async () => {
    vi.advanceTimersByTime(999);
  });
  expect(fn).toHaveBeenCalledTimes(2);
  await act(async () => {
    vi.advanceTimersByTime(1);
  });
  expect(fn).toHaveBeenCalledTimes(3);
});
it("aborts when hidden and resumes immediately when visible", async () => {
  const signals: AbortSignal[] = [];
  const fn = vi.fn(async (signal: AbortSignal) => {
    signals.push(signal);
  });
  const { result, unmount } = renderHook(() => usePolling(fn, 1000, []));
  await act(async () => {});
  await act(async () => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: true,
    });
    document.dispatchEvent(new Event("visibilitychange"));
    result.current();
    vi.advanceTimersByTime(5000);
  });
  expect(signals[0].aborted).toBe(true);
  expect(fn).toHaveBeenCalledTimes(1);
  await act(async () => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      value: false,
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
  expect(fn).toHaveBeenCalledTimes(2);
  unmount();
  expect(signals[1].aborted).toBe(true);
  await act(async () => {
    vi.advanceTimersByTime(5000);
  });
  expect(fn).toHaveBeenCalledTimes(2);
});
it("cancels the old scope and uses the latest callback", async () => {
  const signals: AbortSignal[] = [];
  const calls: string[] = [];
  const { rerender } = renderHook(
    ({ scope, label }) =>
      usePolling(
        async (signal) => {
          signals.push(signal);
          calls.push(label);
        },
        1000,
        [scope],
      ),
    { initialProps: { scope: "a", label: "first" } },
  );
  await act(async () => {});
  rerender({ scope: "a", label: "latest" });
  await act(async () => {
    vi.advanceTimersByTime(1000);
  });
  expect(calls).toEqual(["first", "latest"]);
  rerender({ scope: "b", label: "new" });
  expect(signals[1].aborted).toBe(true);
  expect(calls).toEqual(["first", "latest", "new"]);
});
it("does not run disabled polling or on an initially hidden document", async () => {
  const fn = vi.fn(async () => {});
  const { rerender } = renderHook(
    ({ interval }) => usePolling(fn, interval, []),
    { initialProps: { interval: null as number | null } },
  );
  expect(fn).not.toHaveBeenCalled();
  Object.defineProperty(document, "hidden", {
    configurable: true,
    value: true,
  });
  rerender({ interval: 1000 });
  await act(async () => {
    vi.advanceTimersByTime(5000);
  });
  expect(fn).not.toHaveBeenCalled();
});
