// @vitest-environment jsdom
import { render, screen, cleanup } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { ErrorBoundary, readTheme, retainTheme } from "./resilience";
import NetworkRecovery from "./components/NetworkRecovery";
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});
it.each(["recovered", "pending", "conflict"])(
  "displays %s network recovery evidence",
  (state) => {
    render(
      <NetworkRecovery
        details={{
          network_recovery: {
            state,
            timestamp_ms: 1700000000000,
            message: "Route cleanup result",
            journal: "C:/Astra/recovery.json",
          },
        }}
      />,
    );
    const content = screen.getByRole(
      state === "recovered" ? "status" : "alert",
    ).textContent;
    expect(content).toContain("Route cleanup result");
    expect(content).toContain("C:/Astra/recovery.json");
    expect(content).toContain("2023");
  },
);
it("browser storage failures fall back to a usable in-memory theme", () => {
  vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
    throw new DOMException("denied");
  });
  vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
    throw new DOMException("quota");
  });
  expect(readTheme()).toBe("system");
  expect(() => retainTheme("dark")).not.toThrow();
});
it("render failures expose a recovery action without clearing storage", () => {
  vi.spyOn(console, "error").mockImplementation(() => {});
  const clear = vi.spyOn(Storage.prototype, "clear");
  function Broken(): never {
    throw new Error("render failed");
  }
  render(
    <ErrorBoundary>
      <Broken />
    </ErrorBoundary>,
  );
  expect(screen.getByRole("alert").textContent).toContain("后台实例继续运行");
  expect(screen.getByRole("button", { name: "重新打开界面" })).toBeTruthy();
  expect(clear).not.toHaveBeenCalled();
});
