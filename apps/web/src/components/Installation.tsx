import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as API from "../api";
import type {
  Hello,
  InstallationProgress,
  InstallationResult,
  InstallationScope,
  InstallationStatus,
  InstallationStep,
} from "../generated/management";
import { usePolling } from "../usePolling";

const roles = [
  ["client", "客户端"],
  ["server", "服务端"],
  ["controller", "控制器"],
  ["web", "网页服务"],
] as const;
const roleName = (role: string) =>
  roles.find(([id]) => id === role)?.[1] ?? role;
const steps: [InstallationStep, string][] = [
  ["preparing", "准备安装"],
  ["copying", "复制程序包"],
  ["verifying", "校验已安装文件"],
  ["publishing", "发布启动入口"],
  ["complete", "安装完成"],
];
/** Overall percentage: copying dominates, the short steps get fixed slices. */
export function progressPercent(progress: InstallationProgress): number {
  const current = Number(progress.current) || 0;
  const total = Number(progress.total) || 0;
  switch (progress.step) {
    case "preparing":
      return 2;
    case "copying":
      return total > 0
        ? 5 + Math.floor((80 * Math.min(current, total)) / total)
        : 5;
    case "verifying":
      return 88;
    case "publishing":
      return 96;
    case "complete":
      return 100;
  }
}
export function formatBytes(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`;
}
// Backend task and result messages are English protocol text.
const messages: Record<string, string> = {
  Executing: "正在执行安装…",
  Completed: "安装任务已完成",
  "Installation is ready for the next launch. Running processes, configuration, startup registration and the TUN service are unchanged.":
    "安装已就绪，下次启动时生效。运行中的进程、配置、开机启动和 TUN 服务均未改变。",
};
const localize = (message: string) => messages[message] ?? message;
const dismissedInMemory = new Set<string>();
const reminderKey = (agent: string) =>
  `astra-installation-reminder:${encodeURIComponent(agent)}`;
function reminderDismissed(agent: string) {
  if (dismissedInMemory.has(agent)) return true;
  try {
    return localStorage.getItem(reminderKey(agent)) === "dismissed";
  } catch {
    return false;
  }
}
function dismissReminder(agent: string) {
  dismissedInMemory.add(agent);
  try {
    localStorage.setItem(reminderKey(agent), "dismissed");
  } catch {
    // The current session still remembers the choice when browser storage is unavailable.
  }
}

const pendingTasks = new Map<string, API.Task>();
const taskKey = (agent: string) =>
  `astra-installation-task:${encodeURIComponent(agent)}`;
function readPendingTask(agent: string): API.Task | null {
  const remembered = pendingTasks.get(agent);
  if (remembered) return remembered;
  try {
    const saved = JSON.parse(
      localStorage.getItem(taskKey(agent)) ?? "null",
    ) as { id?: unknown; started_ms?: unknown } | null;
    if (
      saved &&
      typeof saved.id === "string" &&
      typeof saved.started_ms === "string"
    ) {
      return {
        id: saved.id,
        started_ms: saved.started_ms,
        finished_ms: null,
        state: "outcome_unknown",
        message: "正在查询此前提交的安装任务…",
      };
    }
  } catch {
    // An unavailable preference store does not prevent inspecting the target.
  }
  return null;
}
function retainPendingTask(agent: string, task: API.Task | null) {
  const pending =
    task &&
    ["running", "outcome_unknown", "needs_recovery"].includes(task.state);
  if (pending) pendingTasks.set(agent, task);
  else pendingTasks.delete(agent);
  try {
    if (pending)
      localStorage.setItem(
        taskKey(agent),
        JSON.stringify({ id: task.id, started_ms: task.started_ms }),
      );
    else localStorage.removeItem(taskKey(agent));
  } catch {
    // Keep the original operation ID in memory if storage is blocked.
  }
}

export default function Installation({
  agent,
  nodeName,
  online,
  active,
  showReminder = true,
  events = [],
  onOpen,
  onError,
}: {
  agent: string;
  events?: API.ManagementEvent[];
  nodeName?: string;
  online: boolean;
  active: boolean;
  showReminder?: boolean;
  onOpen: () => void;
  onError: (error: unknown) => void;
}) {
  const [status, setStatus] = useState<InstallationStatus | null>(null);
  const [packageId, setPackageId] = useState("");
  const [scope, setScope] = useState<InstallationScope>("user");
  const [loading, setLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [unavailable, setUnavailable] = useState("");
  const [error, setError] = useState("");
  const [localAction, setLocalAction] = useState("");
  const [task, setTask] = useState<API.Task | null>(() =>
    readPendingTask(agent),
  );
  const [result, setResult] = useState<InstallationResult | null>(null);
  const [dismissed, setDismissed] = useState(() => reminderDismissed(agent));
  const generation = useRef(0);
  const target = useRef(agent);
  target.current = agent;
  const taskRef = useRef(task);
  taskRef.current = task;
  const mounted = useRef(true);
  const readController = useRef<AbortController | null>(null);
  const inspectedTarget = useRef("");
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      readController.current?.abort();
    };
  }, []);
  useEffect(() => {
    generation.current++;
    setStatus(null);
    setPackageId("");
    setScope("user");
    setUnavailable("");
    setError("");
    setLocalAction("");
    setTask(readPendingTask(agent));
    setResult(null);
    setSubmitting(false);
    setDismissed(reminderDismissed(agent));
  }, [agent]);
  const current = useCallback(
    (version: number) =>
      mounted.current &&
      generation.current === version &&
      target.current === agent,
    [agent],
  );
  const acceptTask = useCallback(
    (next: API.Task | null) => {
      retainPendingTask(agent, next);
      setTask(next);
    },
    [agent],
  );
  const refresh = useCallback(async () => {
    readController.current?.abort();
    if (!agent || !online) return;
    const abort = new AbortController();
    readController.current = abort;
    const version = generation.current;
    const valid = () => current(version) && !abort.signal.aborted;
    setLoading(true);
    setError("");
    try {
      // Older nodes cannot decode the new query, so negotiate first.
      const hello = await API.query<Hello>(
        agent,
        "hello",
        undefined,
        abort.signal,
      );
      if (!valid()) return;
      if (!hello.data.capabilities.includes("installation")) {
        setUnavailable("此节点版本未提供安装功能，请在目标机更新完整程序包。");
        setStatus(null);
        return;
      }
      const reply = await API.query<InstallationStatus>(
        agent,
        "installation_status",
        undefined,
        abort.signal,
      );
      if (!valid()) return;
      setUnavailable("");
      setStatus(reply.data);
      // Count a first visit only after the target has successfully answered.
      // Keep this reminder visible until dismissed, but do not repeat it on return.
      dismissReminder(agent);
      setPackageId((previous) =>
        reply.data.packages.some((item) => item.id === previous)
          ? previous
          : (reply.data.packages[0]?.id ?? ""),
      );
    } catch (failure) {
      if (!valid()) return;
      if (failure instanceof API.ApiError && failure.code === "unsupported") {
        setUnavailable("此节点版本未提供安装功能，请在目标机更新完整程序包。");
        setStatus(null);
      } else {
        setError(failure instanceof Error ? failure.message : String(failure));
        if (!API.signedIn()) onError(failure);
      }
    } finally {
      if (valid()) setLoading(false);
    }
  }, [agent, online, current, onError]);
  useEffect(() => {
    if (online && agent && (active || inspectedTarget.current !== agent)) {
      inspectedTarget.current = agent;
      void refresh();
    }
    return () => readController.current?.abort();
  }, [refresh, active, online, agent]);

  const updateTask = useCallback(
    async (signal: AbortSignal) => {
      const pending = taskRef.current;
      if (!pending) return;
      const version = generation.current;
      try {
        const reply = await API.query<API.Task>(
          agent,
          "task",
          { operation_id: pending.id },
          signal,
        );
        if (
          signal.aborted ||
          !current(version) ||
          taskRef.current?.id !== pending.id
        )
          return;
        acceptTask(reply.data);
        setError("");
      } catch (failure) {
        if (signal.aborted || !current(version)) return;
        setError(failure instanceof Error ? failure.message : String(failure));
        if (!API.signedIn()) onError(failure);
      }
    },
    [agent, current, onError, acceptTask],
  );
  const queryTask = usePolling(
    updateTask,
    online &&
      !submitting &&
      task &&
      ["running", "outcome_unknown", "needs_recovery"].includes(task.state)
      ? 2000
      : null,
    [agent, online, submitting, task?.id],
  );
  useEffect(() => {
    if (task?.state === "complete") void refresh();
  }, [task?.id, task?.state, refresh]);

  // The latest structured step for the current operation from the event stream.
  const progress = useMemo(() => {
    if (!task) return null;
    let latest: InstallationProgress | null = null;
    for (const event of events) {
      if (
        event.operation_id === task.id &&
        event.payload?.type === "installation" &&
        (!latest || event.payload.data.step_index >= latest.step_index)
      )
        latest = event.payload.data;
    }
    return latest;
  }, [events, task]);

  const selected = status?.packages.find((item) => item.id === packageId);
  const destination = selected?.[scope];
  const pending =
    submitting ||
    (!!task &&
      ["running", "outcome_unknown", "needs_recovery"].includes(task.state));
  const localRequired =
    destination?.requires_elevation && !destination.installed;
  const remember = () => {
    dismissReminder(agent);
    setDismissed(true);
  };
  const install = async () => {
    if (
      !online ||
      pending ||
      loading ||
      !selected ||
      !destination ||
      destination.unavailable_reason ||
      localRequired
    )
      return;
    const version = generation.current;
    const operationId = crypto.randomUUID();
    const started = Date.now().toString();
    remember();
    setSubmitting(true);
    setError("");
    setLocalAction("");
    setResult(null);
    acceptTask({
      id: operationId,
      state: "running",
      message: "正在目标节点校验并安装完整包…",
      started_ms: started,
      finished_ms: null,
    });
    try {
      const reply = await API.request<InstallationResult | API.Task>(agent, {
        type: "execute",
        data: {
          operation_id: operationId,
          command: {
            type: "install_current_package",
            data: {
              package_id: selected.id,
              expected_release_id: selected.release_id,
              scope,
            },
          },
        },
      });
      if (!current(version)) return;
      if (reply.type === "installation_result") {
        const installed = reply.data as InstallationResult;
        if (
          !installed ||
          !["role", "directory", "entry", "message", "launch_command"].every(
            (key) =>
              typeof (installed as unknown as Record<string, unknown>)[key] ===
              "string",
          ) ||
          typeof installed.already_installed !== "boolean"
        ) {
          throw new API.ApiError(
            "outcome_unknown",
            "安装响应不完整，请查询原任务，勿重复安装。",
            undefined,
            operationId,
          );
        }
        setResult(installed);
        acceptTask({
          id: operationId,
          state: "complete",
          message: installed.message,
          started_ms: started,
          finished_ms: Date.now().toString(),
        });
      } else if (reply.type === "task") acceptTask(reply.data as API.Task);
      else
        throw new API.ApiError(
          "outcome_unknown",
          "未收到完整的安装结果，请查询原任务，勿重复安装。",
          undefined,
          operationId,
        );
    } catch (failure) {
      if (!current(version)) return;
      const message =
        failure instanceof Error ? failure.message : String(failure);
      if (
        failure instanceof API.ApiError &&
        failure.code === "outcome_unknown"
      ) {
        acceptTask({
          id: operationId,
          state: "outcome_unknown",
          message,
          started_ms: started,
          finished_ms: null,
        });
      } else {
        acceptTask(null);
        if (
          failure instanceof API.ApiError &&
          failure.code === "requires_local_action"
        )
          setLocalAction(message);
        else setError(message);
      }
      if (!API.signedIn()) onError(failure);
    } finally {
      if (current(version)) setSubmitting(false);
    }
  };

  if (!active) {
    if (!showReminder || !online || dismissed || !status) return null;
    return (
      <section
        className="notice-bar installation-reminder"
        aria-label="安装提醒"
      >
        <div>
          <strong>将 Astra 安装到 {nodeName || status?.target || agent}</strong>
          <p className="muted">
            {status.packages.length
              ? "查看目标节点已有完整包的安装状态，或安装到固定目录并创建启动入口。"
              : "查看各角色完整包的缺失原因，以及目标节点的安装信息。"}
          </p>
        </div>
        <div className="toolbar">
          <button
            className="btn primary"
            onClick={() => {
              remember();
              onOpen();
            }}
          >
            前往安装
          </button>
          <button className="btn" onClick={remember}>
            暂不安装
          </button>
        </div>
      </section>
    );
  }
  return (
    <section
      className="card card-pad installation"
      aria-labelledby="installation-heading"
    >
      <div className="page-head">
        <h2 id="installation-heading">安装</h2>
        <button
          className="btn sm"
          disabled={!online || loading || pending}
          onClick={() => void refresh()}
        >
          {loading ? "检查中…" : "刷新安装信息"}
        </button>
      </div>
      <p className="muted">
        安装 {nodeName || status?.target || agent}{" "}
        上已有的完整程序包，并创建启动入口。此操作不下载软件，也不迁移配置或重启当前服务。
      </p>
      {(!online || unavailable || (loading && !status)) && (
        <p className="notice-bar" role="status">
          {!online
            ? "节点离线，恢复连接后可检查安装信息。"
            : unavailable || "正在检查目标节点的完整包…"}
        </p>
      )}
      {error && (
        <p className="notice-bar error-message" role="alert">
          {error}
        </p>
      )}
      {status && (
        <>
          <p className="muted">
            目标：{status.target} · 执行账户：{status.account}
          </p>
          <ul className="installation-roles">
            {roles.map(([role, name]) => {
              const packages = status.packages.filter(
                (item) => item.role === role,
              );
              return (
                <li key={role}>
                  <strong>{name}</strong>
                  <span>
                    {packages.length
                      ? `完整包可用 · ${packages.map((item) => item.release_id).join("、")}`
                      : (status.unavailable_roles[role] ??
                        "目标节点没有此角色的完整包")}
                  </span>
                </li>
              );
            })}
          </ul>
          {status.warnings.map((warning, index) => (
            <p className="notice-bar" key={index}>
              {warning}
            </p>
          ))}
          {!!status.packages.length && (
            <>
              <div className="toolbar">
                <label htmlFor="installation-package">程序包</label>
                <select
                  id="installation-package"
                  className="input"
                  value={packageId}
                  disabled={pending || loading || !online}
                  onChange={(event) => {
                    setPackageId(event.target.value);
                    setLocalAction("");
                  }}
                >
                  {status.packages.map((item) => (
                    <option value={item.id} key={item.id}>
                      {roleName(item.role)} · {item.release_id}
                    </option>
                  ))}
                </select>
              </div>
              <fieldset
                className="installation-scopes"
                disabled={pending || loading || !online}
              >
                <legend>安装范围</legend>
                <label>
                  <input
                    type="radio"
                    name="installation-scope"
                    value="user"
                    checked={scope === "user"}
                    onChange={() => {
                      setScope("user");
                      setLocalAction("");
                    }}
                  />
                  仅目标账户
                </label>
                <label>
                  <input
                    type="radio"
                    name="installation-scope"
                    value="system"
                    checked={scope === "system"}
                    onChange={() => {
                      setScope("system");
                      setLocalAction("");
                    }}
                  />
                  目标机器的所有用户
                </label>
              </fieldset>
              {selected && (
                <p className="mono installation-path">
                  来源：{selected.source}
                </p>
              )}
              {destination && (
                <>
                  <dl className="installation-paths">
                    <div>
                      <dt>安装目录</dt>
                      <dd>{destination.directory ?? "不可用"}</dd>
                    </div>
                    <div>
                      <dt>启动入口</dt>
                      <dd>{destination.entry ?? "不可用"}</dd>
                    </div>
                  </dl>
                  {destination.installed && (
                    <p>此版本已安装，可重新校验并修复启动入口。</p>
                  )}
                  {destination.unavailable_reason && (
                    <p className="notice-bar">
                      {destination.unavailable_reason}
                    </p>
                  )}
                  {(localRequired || localAction) && (
                    <p className="notice-bar" role="alert">
                      {localAction || "此安装范围需要目标机管理员权限。"}{" "}
                      请在目标机 {status.target}{" "}
                      上打开本地安装入口完成授权；网页不会请求当前电脑的管理员权限。
                    </p>
                  )}
                  <button
                    className="btn primary"
                    disabled={
                      !online ||
                      pending ||
                      loading ||
                      !!error ||
                      !!destination.unavailable_reason ||
                      !!localRequired
                    }
                    onClick={() => void install()}
                  >
                    {submitting || task?.state === "running"
                      ? "安装中…"
                      : destination.installed
                        ? "校验并修复安装"
                        : "安装当前完整包"}
                  </button>
                </>
              )}
            </>
          )}
        </>
      )}
      {task && (!result || progress) && (
        <div
          className="installation-task"
          role={
            task.state === "failed" || task.state === "needs_recovery"
              ? "alert"
              : "status"
          }
          aria-busy={task.state === "running"}
        >
          {(task.state === "running" || progress) && (
            <InstallProgressView
              progress={progress}
              failed={task.state === "failed"}
            />
          )}
          {!result && <p>{localize(task.message)}</p>}
          {["outcome_unknown", "needs_recovery"].includes(task.state) && (
            <>
              <p>安装结果尚未确认。请查询原任务，勿重复提交。</p>
              <button className="btn" disabled={!online} onClick={queryTask}>
                查询安装结果
              </button>
            </>
          )}
          <details className="installation-task-details">
            <summary>任务详情</summary>
            <p className="mono muted">安装任务：{task.id}</p>
          </details>
        </div>
      )}
      {result && (
        <div className="installation-result">
          <p className="installation-result-title">
            <strong>
              {result.already_installed ? "此版本已安装并校验。" : "安装完成。"}
            </strong>{" "}
            后续请使用新入口启动。
          </p>
          <p className="muted">{localize(result.message)}</p>
          <dl className="installation-paths">
            <div>
              <dt>安装位置</dt>
              <dd>{result.directory}</dd>
            </div>
            <div>
              <dt>启动入口</dt>
              <dd>{result.entry}</dd>
            </div>
            <div>
              <dt>启动命令</dt>
              <dd>{result.launch_command}</dd>
            </div>
          </dl>
        </div>
      )}
    </section>
  );
}

function InstallProgressView({
  progress,
  failed,
}: {
  progress: InstallationProgress | null;
  failed: boolean;
}) {
  const percent = progress ? progressPercent(progress) : null;
  const active = progress?.step_index ?? -1;
  const current = Number(progress?.current) || 0;
  const total = Number(progress?.total) || 0;
  const label = !progress
    ? "正在连接目标节点…"
    : progress.step === "copying" && total > 0
      ? `${steps[1][1]} · ${formatBytes(current)} / ${formatBytes(total)}`
      : (steps.find(([step]) => step === progress.step)?.[1] ?? "");
  return (
    <div className="install-progress">
      <div className="install-progress-head">
        <strong>安装进度</strong>
        <span className="muted">{label}</span>
        {percent !== null && <span className="mono">{percent}%</span>}
      </div>
      <div
        className={`install-progress-bar${percent === null ? " indeterminate" : ""}${failed ? " failed" : percent === 100 ? " done" : ""}`}
        role="progressbar"
        aria-label="安装进度"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent ?? undefined}
      >
        <span style={percent === null ? undefined : { width: `${percent}%` }} />
      </div>
      <ol className="install-steps">
        {steps.map(([step, name], index) => {
          const state =
            index < active || (step === "complete" && index === active)
              ? "done"
              : index === active || (active < 0 && index === 0)
                ? failed
                  ? "failed"
                  : "active"
                : "pending";
          return (
            <li
              key={step}
              className={state}
              aria-current={state === "active" ? "step" : undefined}
            >
              {name}
            </li>
          );
        })}
      </ol>
    </div>
  );
}
