import { describe, it, expect, vi, afterEach } from "vitest";
import { takeEvents, login, query, forgetSession, execute } from "./api";
afterEach(() => {
  vi.unstubAllGlobals();
  forgetSession();
});
describe("event framing", () => {
  it("retains incomplete records and handles CRLF", () => {
    const first = takeEvents('event: batch\r\ndata: {"a":1}\r\n\r\nevent: bat');
    expect(first.events).toEqual([{ event: "batch", data: '{"a":1}' }]);
    expect(first.rest).toBe("event: bat");
    const second = takeEvents(first.rest + "ch\ndata: value\n\n");
    expect(second.events[0].data).toBe("value");
  });
  it("joins multiline data and ignores heartbeat comments", () => {
    expect(
      takeEvents(": keepalive\n\nevent: sample\ndata: a\ndata: b\n\n").events,
    ).toEqual([{ event: "sample", data: "a\nb" }]);
  });
});
describe("management identity", () => {
  it("rejects a mismatched response without silently accepting data", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(
            JSON.stringify({
              version: 2,
              id: "other",
              result: { Ok: { type: "agents", data: [] } },
            }),
            { status: 200 },
          ),
      ),
    );
    await expect(query(null, "agents")).rejects.toMatchObject({
      code: "version_mismatch",
    });
  });
  it("does not retry a mutation after a lost response", async () => {
    const fetch = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit) => {
        throw new Error("lost");
      },
    );
    vi.stubGlobal("fetch", fetch);
    await expect(
      execute("agent", "restart", { instance: "one" }, "stable-operation"),
    ).rejects.toMatchObject({ code: "outcome_unknown" });
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(
      JSON.parse(fetch.mock.calls[0]?.[1]?.body as string).body.data
        .operation_id,
    ).toBe("stable-operation");
  });
});
