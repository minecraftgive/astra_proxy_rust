export type Data = Record<string, unknown>;
export type Tagged = { type: string; data?: unknown };
import type {
  Task,
  Draft,
  Event as ManagementEvent,
  SaveOutcome,
} from "./generated/management";
export type {
  Agent,
  Instance,
  Draft,
  Task,
  Event as ManagementEvent,
} from "./generated/management";
export class ApiError extends Error {
  constructor(
    public code: string,
    message: string,
    public details?: SaveOutcome,
  ) {
    super(message);
  }
}
let session: string | null = null;
export const signedIn = () => session !== null;
export const forgetSession = () => {
  session = null;
};
export async function login(password: string) {
  const r = await fetch("/api/v2/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
    cache: "no-store",
  });
  const data = await r.json();
  if (!r.ok)
    throw new ApiError(
      data.error?.code ?? "login_failed",
      data.error?.message ?? "登录失败",
    );
  session = data.token;
}
export async function logout() {
  try {
    await fetch("/api/v2/logout", {
      method: "POST",
      headers: { Authorization: `Bearer ${session}` },
    });
  } finally {
    session = null;
  }
}
export async function request<T = unknown>(
  agent: string | null,
  body: Tagged,
  signal?: AbortSignal,
): Promise<{ type: string; data: T }> {
  const id = crypto.randomUUID();
  let r: Response;
  try {
    r = await fetch("/api/v2/request", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${session}`,
      },
      body: JSON.stringify({ version: 2, id, agent_id: agent, body }),
      signal,
      cache: "no-store",
    });
  } catch (e) {
    if (e instanceof DOMException && e.name === "AbortError") throw e;
    throw new ApiError(
      "outcome_unknown",
      "连接中断。操作可能已执行，请查询原任务，勿重复提交。",
    );
  }
  const data = await r.json();
  if (r.status === 401) session = null;
  if (!r.ok)
    throw new ApiError(
      data.error?.code ?? "request_failed",
      data.error?.message ?? "请求失败",
    );
  if (data.version !== 2 || data.id !== id)
    throw new ApiError("version_mismatch", "管理接口版本或请求身份不一致");
  if (data.result.Err)
    throw new ApiError(
      data.result.Err.code,
      data.result.Err.message,
      data.result.Err.details,
    );
  return data.result.Ok;
}
export const query = <T>(
  agent: string | null,
  type: string,
  data?: unknown,
  signal?: AbortSignal,
) =>
  request<T>(
    agent,
    { type: "query", data: { type, ...(data === undefined ? {} : { data }) } },
    signal,
  );
export const execute = (
  agent: string,
  type: string,
  data?: unknown,
  operationId: string = crypto.randomUUID(),
) =>
  request<Task>(agent, {
    type: "execute",
    data: {
      operation_id: operationId,
      command: { type, ...(data === undefined ? {} : { data }) },
    },
  });
export const draftCommand = (agent: string, type: string, data: unknown) =>
  request<Draft>(agent, { type: "drafts", data: { type, data } });

/** Decode complete SSE records only. CRLF and network chunk boundaries are supported. */
export function takeEvents(buffer: string): {
  events: { event: string; data: string }[];
  rest: string;
} {
  const normalized = buffer.replace(/\r\n/g, "\n");
  const parts = normalized.split("\n\n");
  const rest = parts.pop() ?? "";
  return {
    events: parts
      .map((block) => {
        let event = "message";
        const data: string[] = [];
        for (const line of block.split("\n")) {
          if (line.startsWith("event:")) event = line.slice(6).trim();
          if (line.startsWith("data:")) data.push(line.slice(5).trimStart());
        }
        return { event, data: data.join("\n") };
      })
      .filter((e) => e.data !== ""),
    rest,
  };
}
export async function watch(
  agent: string,
  signal: AbortSignal,
  onBatch: (events: ManagementEvent[], gap: boolean) => void,
  onStatus: (status: string) => void,
) {
  let cursor: string | undefined;
  let backoff = 1000;
  while (!signal.aborted && session) {
    try {
      const params = new URLSearchParams({ agent_id: agent });
      if (cursor) params.set("after", cursor);
      const r = await fetch(`/api/v2/events?${params}`, {
        headers: { Authorization: `Bearer ${session}` },
        signal,
        cache: "no-store",
      });
      if (r.status === 401) {
        session = null;
        onStatus("登录已过期");
        return;
      }
      if (!r.ok || !r.body) throw new Error("事件连接失败");
      const reader = r.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      backoff = 1000;
      onStatus("已连接");
      try {
        while (!signal.aborted) {
          const chunk = await reader.read();
          if (chunk.done) break;
          buffer += decoder.decode(chunk.value, { stream: true });
          const parsed = takeEvents(buffer);
          buffer = parsed.rest;
          if (buffer.length > 4 * 1024 * 1024)
            throw new Error("事件超出大小限制");
          for (const event of parsed.events) {
            if (event.event === "expired") {
              session = null;
              onStatus("登录已过期");
              return;
            }
            if (event.event === "disconnected") {
              onStatus("管理服务离线 · 显示最后状态");
              continue;
            }
            if (event.event === "batch") {
              const response = JSON.parse(event.data);
              const batch = response.result?.Ok;
              if (batch?.type === "events") {
                cursor = batch.data.cursor;
                onBatch(batch.data.events, batch.data.resync_required);
                onStatus("已连接");
              } else if (response.result?.Err) {
                onStatus(response.result.Err.message);
              }
            }
          }
        }
      } finally {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
      }
    } catch (e) {
      if (signal.aborted) return;
      onStatus(e instanceof Error ? e.message : "连接中断");
    }
    if (!signal.aborted)
      await new Promise<void>((resolve) => {
        const done = () => {
          clearTimeout(timer);
          signal.removeEventListener("abort", done);
          resolve();
        };
        const timer = setTimeout(done, backoff);
        signal.addEventListener("abort", done, { once: true });
      });
    backoff = Math.min(backoff * 2, 30000);
  }
}
