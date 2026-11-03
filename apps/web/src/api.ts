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
    public operationId?: string,
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
  let data;
  try {
    data = await r.json();
  } catch {
    throw new ApiError("login_failed", "登录响应不完整，请重新登录。");
  }
  if (!r.ok)
    throw new ApiError(
      data?.error?.code ?? "login_failed",
      data?.error?.message ?? "登录失败",
    );
  if (typeof data?.token !== "string" || !data.token)
    throw new ApiError("login_failed", "登录响应缺少有效会话，请重新登录。");
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
  const command = object(body.data);
  const input = body.type === "execute" ? command : object(command?.data);
  const operationId =
    typeof input?.operation_id === "string" ? input.operation_id : undefined;
  const mutation =
    body.type !== "query" &&
    !(body.type === "drafts" && command?.type === "read");
  const uncertain = () =>
    new ApiError(
      mutation ? "outcome_unknown" : "connection_failed",
      mutation
        ? `无法确认操作结果。请查询原任务或草稿，勿重复提交。${operationId ? `任务 ${operationId}` : ""}`
        : "无法读取完整的管理响应，请重新查询。",
      undefined,
      operationId,
    );
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
    if (!mutation && e instanceof DOMException && e.name === "AbortError")
      throw e;
    throw uncertain();
  }
  if (r.status === 401) {
    session = null;
    throw new ApiError("unauthorized", "登录已失效，请重新登录。");
  }
  // A gateway/server failure cannot prove that an accepted mutation rolled back.
  if (mutation && r.status >= 500) throw uncertain();
  let data: Record<string, any>;
  try {
    const parsed: unknown = await r.json();
    const value = object(parsed);
    if (!value) throw new Error("invalid response");
    data = value;
  } catch (error) {
    if (
      !mutation &&
      error instanceof DOMException &&
      error.name === "AbortError"
    )
      throw error;
    throw uncertain();
  }
  if (!r.ok)
    if (
      typeof data.error?.code === "string" &&
      typeof data.error?.message === "string"
    )
      throw new ApiError(
        data.error.code,
        data.error.message,
        undefined,
        operationId,
      );
    else throw uncertain();
  if (data.version !== 2 || data.id !== id)
    throw mutation
      ? uncertain()
      : new ApiError("version_mismatch", "管理接口版本或请求身份不一致");
  const result = object(data.result);
  if (!result) throw uncertain();
  const success = "Ok" in result;
  const failure = "Err" in result;
  if (success === failure) throw uncertain();
  if ("Err" in result) {
    if (
      typeof result.Err?.code !== "string" ||
      typeof result.Err?.message !== "string"
    )
      throw uncertain();
    throw new ApiError(
      result.Err.code,
      result.Err.message,
      result.Err.details,
      operationId,
    );
  }
  const reply = object(result.Ok);
  if (!reply || typeof reply.type !== "string" || !("data" in reply))
    throw uncertain();
  if (operationId && reply.data == null) throw uncertain();
  if (reply.type === "task") {
    const task = object(reply.data);
    if (
      !task ||
      !["id", "state", "message", "started_ms"].every(
        (key) => typeof task[key] === "string",
      ) ||
      ![
        "running",
        "complete",
        "failed",
        "outcome_unknown",
        "needs_recovery",
      ].includes(task.state) ||
      (operationId && task.id !== operationId)
    )
      throw uncertain();
  }
  if (
    body.type === "drafts" &&
    ["open", "read", "edit", "save", "apply"].includes(command?.type) &&
    reply.type !== "draft"
  )
    throw uncertain();
  if (reply.type === "draft") {
    const draft = object(reply.data);
    if (
      !draft ||
      ![
        "id",
        "instance",
        "path",
        "original",
        "text",
        "revision",
        "source_revision",
        "kind",
      ].every((key) => typeof draft[key] === "string") ||
      (input?.draft_id && draft.id !== input.draft_id)
    )
      throw uncertain();
    if (command?.type === "save") {
      const outcome = object(draft.last_save);
      if (
        !outcome ||
        outcome.operation_id !== operationId ||
        typeof outcome.message !== "string" ||
        ![
          "applied",
          "next_start",
          "rejected",
          "restored",
          "recovery_failed",
          "outcome_unknown",
        ].includes(outcome.state)
      )
        throw uncertain();
    }
  }
  return result.Ok;
}
function object(value: unknown): Record<string, any> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, any>)
    : undefined;
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
// Authentication uses a Bearer header; native EventSource cannot send it.
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
      if (!signal.aborted) onStatus("事件连接已断开，正在重连");
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
