import { describe, it, expect, vi, afterEach } from "vitest";
import {
  takeEvents,
  login,
  query,
  forgetSession,
  execute,
  draftCommand,
  signedIn,
} from "./api";
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
  it.each([
    undefined,
    { operation_id: "other", state: "applied", message: "saved" },
    { operation_id: "original", state: "invalid", message: "saved" },
  ])(
    "retains save identity when the outcome is missing or invalid",
    async (last_save) => {
      const fetch = vi.fn(
        async (_url, init) =>
          new Response(
            JSON.stringify({
              version: 2,
              id: JSON.parse(init.body).id,
              result: {
                Ok: {
                  type: "draft",
                  data: {
                    id: "draft",
                    instance: "one",
                    path: "proxy.toml",
                    original: "",
                    text: "",
                    revision: "r",
                    source_revision: "s",
                    kind: "proxy",
                    last_save,
                  },
                },
              },
            }),
          ),
      );
      vi.stubGlobal("fetch", fetch);
      await expect(
        draftCommand("agent", "save", {
          draft_id: "draft",
          operation_id: "original",
        }),
      ).rejects.toMatchObject({
        code: "outcome_unknown",
        operationId: "original",
      });
      expect(fetch).toHaveBeenCalledOnce();
    },
  );
  it.each([
    { type: "task" },
    { type: "task", data: { id: "other", state: "complete" } },
  ])("rejects an incomplete or mismatched operation result", async (reply) => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async (_url, init) =>
          new Response(
            JSON.stringify({
              version: 2,
              id: JSON.parse(init.body).id,
              result: { Ok: reply },
            }),
          ),
      ),
    );
    await expect(
      execute("agent", "restart", {}, "operation"),
    ).rejects.toMatchObject({
      code: "outcome_unknown",
      operationId: "operation",
    });
  });
  it.each(["truncated", "body lost", "invalid envelope", "wrong identity"])(
    "retains mutation identity after %s without replay",
    async (failure) => {
      const fetch = vi.fn(async (_url, init) => {
        const id = JSON.parse(init.body).id;
        if (failure === "truncated")
          return new Response('{"version":2,', { status: 200 });
        if (failure === "body lost")
          return {
            status: 200,
            ok: true,
            json: async () => {
              throw new TypeError("stream closed");
            },
          };
        return new Response(
          JSON.stringify({
            version: 2,
            id: failure === "wrong identity" ? "wrong" : id,
            result: {},
          }),
          { status: 200 },
        );
      });
      vi.stubGlobal("fetch", fetch);
      await expect(
        execute("agent", "restart", {}, "original-id"),
      ).rejects.toMatchObject({
        code: "outcome_unknown",
        operationId: "original-id",
      });
      expect(fetch).toHaveBeenCalledOnce();
    },
  );
  it("preserves a draft save operation ID and reports queries separately", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("broken json")),
    );
    await expect(
      draftCommand("agent", "save", { operation_id: "saved-id" }),
    ).rejects.toMatchObject({
      code: "outcome_unknown",
      operationId: "saved-id",
    });
    await expect(query("agent", "instances")).rejects.toMatchObject({
      code: "connection_failed",
    });
  });
  it("does not treat a generic gateway failure as a rejected save", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(
      JSON.stringify({ error: { code: "internal", message: "upstream failed" } }),
      { status: 502 },
    )));
    await expect(draftCommand("agent", "save", { operation_id: "accepted-id" }))
      .rejects.toMatchObject({ code: "outcome_unknown", operationId: "accepted-id" });
  });
  it("invalidates an expired session even when the 401 body is not JSON", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ token: "session" }))),
    );
    await login("password");
    expect(signedIn()).toBe(true);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response("Unauthorized", { status: 401 })),
    );
    await expect(query(null, "agents")).rejects.toMatchObject({
      code: "unauthorized",
    });
    expect(signedIn()).toBe(false);
  });
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
