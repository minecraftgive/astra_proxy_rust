import { useCallback, useEffect, useRef, useState } from "react";
import * as API from "./api";
import { usePolling } from "./usePolling";
import { readTheme, retainTheme } from "./resilience";
import type { LogChunk, LogCursor } from "./generated/management";
import Editor from "./components/Editor";
import Login from "./components/Login";
import Updates from "./components/Updates";
import Installation from "./components/Installation";
import ProgramInfo from "./components/ProgramInfo";
import NetworkRecovery from "./components/NetworkRecovery";
import Monitor from "./components/Monitor";
import NetworkTools from "./components/NetworkTools";
import { Details, Empty, Table } from "./components/common";
import {
  EXIT_INTERVAL,
  ExitProbeFields,
  clampInterval,
  exitChain,
  readExitProbe,
  retainExitProbe,
  useExitProbe,
  type ExitProbeSettings,
} from "./exitProbe";

type Row = Record<string, unknown>;
const pages = [
  ["overview", "概览"],
  ["instances", "配置档"],
  ["proxies", "代理组"],
  ["connections", "连接"],
  ["rules", "规则"],
  ["logs", "日志"],
  ["monitor", "监视器"],
  ["network", "网络工具"],
  ["tasks", "任务"],
  ["settings", "设置"],
] as const;
const kindName = {
  proxy: "代理客户端",
  rule: "规则核心",
  server: "代理服务端",
};
const fmt = (v: unknown) =>
  v == null ? "—" : typeof v === "object" ? JSON.stringify(v) : String(v);
const bytes = (v: unknown) => {
  let n = Number(v ?? 0);
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (n >= 1024 && i < 4) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
};
const when = (v: string) => new Date(Number(v)).toLocaleString();
export default function App() {
  const [authenticated, setAuthenticated] = useState(API.signedIn());
  const [agents, setAgents] = useState<API.Agent[]>([]);
  const [agent, setAgent] = useState("");
  const [instances, setInstances] = useState<API.Instance[]>([]);
  const [selected, setSelected] = useState("");
  const [page, setPage] = useState<string>("overview");
  const [runtime, setRuntime] = useState<Row | null>(null);
  const [status, setStatus] = useState("尚未连接");
  const [error, setError] = useState("");
  const [events, setEvents] = useState<API.ManagementEvent[]>([]);
  const [tasks, setTasks] = useState<API.Task[]>([]);
  const [editing, setEditing] = useState<API.Instance | null>(null);
  const [search, setSearch] = useState("");
  const [paused, setPaused] = useState(false);
  const [logs, setLogs] = useState("");
  const [route, setRoute] = useState("");
  const [result, setResult] = useState<unknown>(null);
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [kind, setKind] = useState<API.Instance["kind"]>("proxy");
  const [busy, setBusy] = useState(false);
  const [theme, setTheme] = useState(readTheme);
  const [exitProbe, setExitProbe] = useState(readExitProbe);
  const updateExitProbe = (change: Partial<ExitProbeSettings>) =>
    setExitProbe((old) => {
      const next = { ...old, ...change };
      retainExitProbe(next);
      return next;
    });
  const epoch = useRef(0);
  const scope = useRef("");
  scope.current = `${agent}/${selected}`;
  const current = instances.find((i) => i.id === selected);
  const node = agents.find((a) => a.id === agent);
  const online = node?.online ?? false;
  const fail = useCallback((e: unknown) => {
    if (e instanceof DOMException && e.name === "AbortError") return;
    setError(e instanceof Error ? e.message : String(e));
    if (!API.signedIn()) setAuthenticated(false);
  }, []);
  const refresh = useCallback(
    async (signal?: AbortSignal) => {
      if (!agent) return;
      const generation = epoch.current;
      try {
        const r = await API.query<API.Instance[]>(
          agent,
          "instances",
          undefined,
          signal,
        );
        if (signal?.aborted || generation !== epoch.current) return;
        setInstances(r.data);
        setSelected((old) =>
          r.data.some((i) => i.id === old) ? old : (r.data[0]?.id ?? ""),
        );
      } catch (e) {
        if (!signal?.aborted && generation === epoch.current) fail(e);
      }
    },
    [agent, fail],
  );
  useEffect(() => {
    epoch.current++;
    setInstances([]);
    setSelected("");
    setRuntime(null);
    setEvents([]);
    setTasks([]);
    setEditing(null);
    setResult(null);
    setSearch("");
    setPaused(false);
    setLogs("");
    setStatus("连接中");
  }, [agent, authenticated]);
  const fallbackMs = status === "已连接" ? 30_000 : 10_000;
  const refreshAgents = usePolling(
    async (signal) => {
      try {
        const r = await API.query<API.Agent[]>(
          null,
          "agents",
          undefined,
          signal,
        );
        if (signal.aborted) return;
        setAgents(r.data);
        setAgent((old) =>
          r.data.some((a) => a.id === old) ? old : (r.data[0]?.id ?? ""),
        );
      } catch (e) {
        if (!signal.aborted) fail(e);
      }
    },
    authenticated ? fallbackMs : null,
    [authenticated, fail],
  );
  const refreshInstances = usePolling(
    refresh,
    authenticated && agent ? fallbackMs : null,
    [agent, authenticated],
  );
  const refreshRuntime = usePolling(
    async (signal) => {
      try {
        const r = await API.query<{ kind: string; snapshot: Row }>(
          agent,
          "runtime",
          { instance: selected },
          signal,
        );
        if (!signal.aborted) setRuntime(r.data.snapshot);
      } catch (e) {
        // Do not present the last successful snapshot as current after a read fails.
        if (!signal.aborted) setRuntime(null);
        if (
          !signal.aborted &&
          !(e instanceof API.ApiError && e.code === "unsupported")
        )
          fail(e);
      }
    },
    authenticated &&
      agent &&
      selected &&
      online &&
      current?.pid &&
      (!paused || page === "settings") &&
      (current.kind === "proxy" || current.kind === "server")
      ? 5_000
      : null,
    [
      agent,
      selected,
      online,
      paused,
      page,
      current?.pid,
      current?.kind,
      authenticated,
      fail,
    ],
  );
  useEffect(() => {
    setRuntime(null);
    setResult(null);
    setBusy(false);
    setLogs("");
    setError("");
  }, [agent, selected]);
  const logState = useRef<{ cursor: LogCursor | null; decoder: TextDecoder }>({
    cursor: null,
    decoder: new TextDecoder(),
  });
  useEffect(() => {
    logState.current = { cursor: null, decoder: new TextDecoder() };
  }, [page, agent, selected, paused]);
  const refreshLogs = usePolling(
    async (signal) => {
      const state = logState.current;
      try {
        const r = await API.query<LogChunk>(
          agent,
          "log_chunk",
          { instance: selected, cursor: state.cursor },
          signal,
        );
        if (signal.aborted) return;
        if (r.data.type === "data") {
          const data = r.data.data;
          state.cursor = data.cursor;
          if (data.reset) state.decoder = new TextDecoder();
          const text = state.decoder.decode(new Uint8Array(data.bytes), {
            stream: true,
          });
          setLogs((old) => {
            const combined =
              (data.reset ? "[日志已重新定位或轮转]\n" : old) + text;
            return combined.length > 2 * 1024 * 1024
              ? "[显示缓冲已截断；完整日志保存在节点]\n" +
                  combined.slice(-2 * 1024 * 1024)
              : combined;
          });
        } else if (r.data.type === "error") throw new Error(r.data.data);
      } catch (e) {
        if (!signal.aborted) fail(e);
      }
    },
    authenticated && page === "logs" && agent && selected && !paused
      ? 5_000
      : null,
    [page, agent, selected, paused, authenticated, fail],
  );
  const refreshTasks = usePolling(
    async (signal) => {
      try {
        const r = await API.query<API.Task[]>(
          agent,
          "tasks",
          { limit: 64 },
          signal,
        );
        if (!signal.aborted) setTasks(r.data);
      } catch (e) {
        if (!signal.aborted) fail(e);
      }
    },
    authenticated && agent && online ? fallbackMs : null,
    [authenticated, agent, online, fail],
  );
  useEffect(() => {
    if (!authenticated || !agent) return;
    const abort = new AbortController();
    void API.watch(
      agent,
      abort.signal,
      (batch, gap) => {
        if (abort.signal.aborted) return;
        setEvents((old) => [...old, ...batch].slice(-500));
        // Free-form event kinds do not expose a stable entity schema. Coalesce
        // each batch into one refresh per affected snapshot, including resync gaps.
        if (batch.length || gap) {
          refreshAgents();
          refreshInstances();
          refreshRuntime();
          refreshLogs();
          refreshTasks();
        }
      },
      (value) => {
        if (!abort.signal.aborted) {
          setStatus(value);
          if (!API.signedIn()) setAuthenticated(false);
        }
      },
    );
    return () => abort.abort();
  }, [
    agent,
    authenticated,
    refreshAgents,
    refreshInstances,
    refreshRuntime,
    refreshLogs,
    refreshTasks,
  ]);
  useEffect(() => {
    document.documentElement.dataset.theme = theme === "system" ? "" : theme;
    retainTheme(theme);
  }, [theme]);
  const run = async (type: string, data?: unknown, danger?: string) => {
    if (!agent || !online) return;
    if (danger && !window.confirm(`${node?.name ?? agent}\n${danger}`)) return;
    setBusy(true);
    setError("");
    const generation = epoch.current;
    const operationScope = scope.current;
    const target = agent;
    const operationId = crypto.randomUUID();
    try {
      const response = await API.execute(target, type, data, operationId);
      if (generation !== epoch.current || operationScope !== scope.current)
        return;
      if (response.type === "task") {
        setTasks((old) =>
          [
            response.data,
            ...old.filter((t) => t.id !== response.data.id),
          ].slice(0, 64),
        );
        if (
          response.data.state === "failed" ||
          response.data.state === "outcome_unknown" ||
          response.data.state === "needs_recovery"
        )
          setError(response.data.message);
        else if (response.data.message) setResult(response.data.message);
      } else {
        setResult(response.data);
        if (response.type === "update_status") {
          const update = response.data as unknown as {
            operation?: { phase: string; error?: string };
          };
          if (
            update.operation &&
            ["failed", "needs_recovery", "uncertain"].includes(
              update.operation.phase,
            )
          ) {
            setError(
              update.operation.error ?? "更新需要恢复；请查看节点的更新状态。",
            );
            return false;
          }
        }
      }
      await refresh();
      return response.type !== "task" || response.data.state === "complete";
    } catch (e) {
      if (generation === epoch.current && operationScope === scope.current) {
        if (e instanceof API.ApiError && e.code === "outcome_unknown")
          setTasks((old) =>
            [
              {
                id: operationId,
                state: "outcome_unknown" as const,
                message: "回复丢失，请查询任务结果",
                started_ms: Date.now().toString(),
                finished_ms: null,
              },
              ...old,
            ].slice(0, 64),
          );
        fail(e);
      }
      return false;
    } finally {
      if (generation === epoch.current && operationScope === scope.current)
        setBusy(false);
    }
  };
  const runtimeCommand = (command: unknown, danger?: string) =>
    current &&
    run(
      "runtime",
      { instance: current.id, command, expected_pid: current.pid },
      danger,
    );
  const capability = (cap: string, instance = current) => ({
    disabled: busy || !online || !instance?.capabilities.includes(cap),
    title: !instance?.capabilities.includes(cap)
      ? `所选实例未提供 ${cap} 能力`
      : !online
        ? "节点离线"
        : undefined,
  });
  const connections = ((runtime?.connections ?? []) as Row[]).filter((r) =>
    Object.values(r).some((v) =>
      fmt(v).toLowerCase().includes(search.toLowerCase()),
    ),
  );
  const groups = (runtime?.groups ?? []) as {
    name: string;
    mode: string;
    selected: string | null;
    testing: boolean;
    members: { name: string; alive: boolean | null; delay_ms: string | null }[];
  }[];
  // The connected profile's mode and exit chain, then the exit probe fields.
  const chain = current?.kind === "proxy" ? exitChain(runtime) : [];
  const exitNode = chain[chain.length - 1];
  const details = current?.details as
    { diagnostics_supported?: boolean } | null | undefined;
  const probe = useExitProbe(
    authenticated &&
      online &&
      current &&
      current.kind === "proxy" &&
      current.pid != null &&
      current.state === "Running" &&
      details?.diagnostics_supported &&
      runtime &&
      String(runtime.pid) === String(current.pid) &&
      exitNode
      ? {
          agent,
          instance: current.id,
          pid: String(current.pid),
          outbound: exitNode,
        }
      : null,
    exitProbe,
  );
  const modeLabel = (
    { rule: "规则", global: "全局", direct: "直连" } as Record<string, string>
  )[String(runtime?.mode)];
  if (!authenticated) return <Login onLogin={() => setAuthenticated(true)} />;
  return (
    <div className="app">
      <aside className="side">
        <div className="brand">
          <span className="brand-mark" />
          Astra <span className="pill">管理</span>
        </div>
        <label className="node-label" htmlFor="agent">
          受控节点
        </label>
        <select
          id="agent"
          className="input node-select"
          value={agent}
          onChange={(e) => setAgent(e.target.value)}
          aria-label="选择受控节点"
        >
          {agents.map((a) => (
            <option key={a.id} value={a.id}>
              {a.online ? "●" : "○"} {a.name || a.id}
            </option>
          ))}
        </select>
        {pages.map(([id, label], i) => (
          <button
            className={`nav ${page === id ? "on" : ""}`}
            key={id}
            onClick={() => {
              setPage(id);
              setEditing(null);
              setResult(null);
            }}
          >
            <span>{label}</span>
            <span className="key">{i + 1}</span>
          </button>
        ))}
        <div className="spacer" />
        <p className="node-info mono">{agent || "尚无节点"}</p>
        <button
          className="nav"
          onClick={() => API.logout().finally(() => setAuthenticated(false))}
        >
          退出登录
        </button>
      </aside>
      <header className="top">
        <span className={`dot ${online ? "ok" : "warn"}`} />
        <strong>{node?.name || "选择节点"}</strong>
        <span className="muted">/</span>
        <select
          className="input"
          value={selected}
          onChange={(e) => {
            setSelected(e.target.value);
            setEditing(null);
          }}
          aria-label="当前实例"
        >
          <option value="">选择实例</option>
          {instances.map((i) => (
            <option value={i.id} key={i.id}>
              {i.name} · {kindName[i.kind]}
            </option>
          ))}
        </select>
        {modeLabel && current?.kind === "proxy" && (
          <span className="pill acc">{modeLabel}</span>
        )}
        {chain.length > 0 && (
          <span className="top-exit">
            <span className="muted">出口 </span>
            <span className="top-chain">{chain.join(" ▸ ")}</span>{" "}
            <ExitProbeFields {...probe} />
          </span>
        )}
        <span className="top-context muted">
          {current?.state ?? "未选择实例"}
        </span>
        <button className="btn sm" onClick={() => void refresh()}>
          刷新
        </button>
      </header>
      <main>
        <div className="page">
          {error && (
            <div className="notice-bar error-message" role="alert">
              <span>{error}</span>
              <button className="btn sm" onClick={() => setError("")}>
                关闭
              </button>
            </div>
          )}
          {!online && agent && (
            <div className="notice-bar">
              节点离线，显示最后观测状态。运行中的核心不会因此停止。
            </div>
          )}
          {editing ? (
            <Editor
              key={`${agent}/${editing.id}`}
              agent={agent}
              instance={editing}
              onClose={() => setEditing(null)}
              onSaved={() => void refresh()}
              onError={fail}
            />
          ) : (
            <>
              <div className="page-head">
                <h1>{pages.find(([id]) => id === page)?.[1]}</h1>
                <div className="tools">
                  {current && (
                    <button
                      className="btn"
                      onClick={() => setEditing(current)}
                      {...capability("document")}
                    >
                      编辑配置
                    </button>
                  )}
                  {current && (
                    <button
                      className="btn primary"
                      {...capability(current.pid ? "stop" : "start")}
                      onClick={() =>
                        void run(
                          current.pid ? "stop" : "start",
                          { instance: current.id },
                          current.pid ? `停止 ${current.name}？` : undefined,
                        )
                      }
                    >
                      {current.pid ? "停止实例" : "启动实例"}
                    </button>
                  )}
                </div>
              </div>
              {agent && (
                <Installation
                  key={agent}
                  agent={agent}
                  nodeName={node?.name}
                  online={online}
                  active={page === "settings"}
                  onOpen={() => {
                    setPage("settings");
                    setResult(null);
                  }}
                  onError={fail}
                />
              )}
              {current && <NetworkRecovery details={current.details} />}
              {page === "network" && (
                <NetworkTools
                  key={`${agent}/${selected}`}
                  agent={agent}
                  instance={selected}
                  online={online}
                />
              )}
              {page === "monitor" && (
                <Monitor
                  agent={agent}
                  instance={selected}
                  online={online}
                  onError={fail}
                />
              )}
              {page === "overview" &&
                (current ? (
                  <>
                    <div className="card card-pad">
                      <div className="state">
                        <span
                          className={`dot ${current.pid && current.state === "Running" ? "ok" : ""}`}
                        />
                        <h2>{current.name}</h2>
                        <span className="pill">{kindName[current.kind]}</span>
                        <span className="pill">{current.state}</span>
                      </div>
                      <p className="mono muted">{current.config}</p>
                      <div className="toolbar">
                        <button
                          className="btn"
                          {...capability("validate")}
                          onClick={() =>
                            void run("validate", { instance: current.id })
                          }
                        >
                          校验
                        </button>
                        <button
                          className="btn"
                          {...capability("reload")}
                          onClick={() =>
                            void run("reload", { instance: current.id })
                          }
                        >
                          热重载
                        </button>
                        <button
                          className="btn"
                          {...capability("restart")}
                          onClick={() =>
                            void run(
                              "restart",
                              { instance: current.id },
                              `完整重启 ${current.name}？现有连接可能中断。`,
                            )
                          }
                        >
                          重启
                        </button>
                      </div>
                    </div>
                    <div className="metrics">
                      <Metric title="活动连接" value={fmt(runtime?.active)} />
                      <Metric title="累计上传" value={bytes(runtime?.upload)} />
                      <Metric
                        title="累计下载"
                        value={bytes(runtime?.download)}
                      />
                      <Metric title="进程" value={fmt(current.pid)} />
                    </div>
                    {current.kind === "proxy" && runtime && (
                      <div className="card card-pad">
                        <h2>运行控制</h2>
                        <div className="toolbar">
                          {["rule", "global", "direct"].map((mode) => (
                            <button
                              key={mode}
                              className={`btn ${runtime.mode === mode ? "primary" : ""}`}
                              {...capability("runtime")}
                              onClick={() =>
                                void runtimeCommand({ SetMode: mode })
                              }
                            >
                              {
                                (
                                  {
                                    rule: "规则",
                                    global: "全局",
                                    direct: "直连",
                                  } as Row
                                )[mode] as string
                              }
                            </button>
                          ))}
                        </div>
                        <div className="toolbar">
                          {[
                            ["SystemProxy", "system_proxy", "系统代理"],
                            ["Tun", "tun", "TUN"],
                            ["AllowLan", "allow_lan", "允许局域网"],
                          ].map(([command, key, label]) => (
                            <button
                              className="btn"
                              key={key}
                              {...capability("runtime")}
                              aria-pressed={Boolean(runtime[key])}
                              onClick={() =>
                                void runtimeCommand(
                                  { [command]: !runtime[key] },
                                  command === "Tun"
                                    ? `在节点 ${node?.name} 更改 TUN 状态？`
                                    : undefined,
                                )
                              }
                            >
                              {runtime[key] ? "关闭" : "开启"}
                              {label}
                            </button>
                          ))}
                        </div>
                        <p className="muted">
                          控制作用于当前节点，权限不足时需要在目标机器完成授权。
                        </p>
                      </div>
                    )}
                    {current.kind === "server" && (
                      <div className="card card-pad">
                        <h2>服务器监听</h2>
                        {((runtime?.listeners ?? []) as string[]).map((l) => (
                          <p className="mono" key={l}>
                            {l}
                          </p>
                        ))}
                        {!runtime && (
                          <p className="muted">启动服务器后查看监听状态。</p>
                        )}
                      </div>
                    )}
                  </>
                ) : (
                  <Empty>
                    选择或创建一个实例后查看状态。
                    <button
                      className="btn"
                      onClick={() => setPage("instances")}
                    >
                      管理配置档
                    </button>
                  </Empty>
                ))}
              {page === "instances" && (
                <>
                  <div className="toolbar">
                    <button
                      className="btn primary"
                      disabled={!online}
                      onClick={() => setCreating(!creating)}
                    >
                      新建配置档
                    </button>
                  </div>
                  {creating && (
                    <form
                      className="card card-pad toolbar"
                      onSubmit={(e) => {
                        e.preventDefault();
                        void run("create", { name, kind }).then((ok) => {
                          if (!ok) return;
                          setCreating(false);
                          setName("");
                        });
                      }}
                    >
                      <input
                        className="input"
                        aria-label="实例名称"
                        required
                        value={name}
                        onChange={(e) => setName(e.target.value)}
                        pattern="[a-zA-Z0-9_-]{1,24}"
                        placeholder="实例名称"
                      />
                      <select
                        className="input"
                        aria-label="实例种类"
                        value={kind}
                        onChange={(e) =>
                          setKind(e.target.value as API.Instance["kind"])
                        }
                      >
                        {Object.entries(kindName).map(([v, l]) => (
                          <option key={v} value={v}>
                            {l}
                          </option>
                        ))}
                      </select>
                      <button className="btn primary" disabled={busy}>
                        创建
                      </button>
                    </form>
                  )}
                  <Table
                    rows={instances as unknown as Row[]}
                    columns={[
                      ["name", "名称"],
                      ["kind", "种类"],
                      ["state", "状态"],
                      ["pid", "进程"],
                      ["config", "配置路径"],
                    ]}
                    action={(row) => (
                      <div className="toolbar">
                        <button
                          className="btn sm"
                          {...capability(
                            "document",
                            instances.find((i) => i.id === row.id),
                          )}
                          onClick={() => {
                            setSelected(String(row.id));
                            setEditing(instances.find((i) => i.id === row.id)!);
                          }}
                        >
                          编辑
                        </button>
                        <button
                          className="btn sm danger"
                          disabled={Boolean(row.pid) || busy || !online}
                          onClick={() =>
                            void run(
                              "remove",
                              { instance: row.id },
                              `移除 ${row.name} 的管理登记？`,
                            )
                          }
                        >
                          移除
                        </button>
                      </div>
                    )}
                  />
                </>
              )}
              {page === "proxies" &&
                (current?.kind === "proxy" ? (
                  <>
                    {groups.map((g) => (
                      <section className="card card-pad" key={g.name}>
                        <div className="page-head">
                          <h2>{g.name}</h2>
                          <span className="pill">{g.mode}</span>
                          <button
                            className="btn sm"
                            {...capability("runtime")}
                            disabled={
                              capability("runtime").disabled || g.testing
                            }
                            onClick={() =>
                              void runtimeCommand({ TestGroup: g.name })
                            }
                          >
                            {g.testing ? "测试中…" : "测速"}
                          </button>
                        </div>
                        <div className="member-grid">
                          {g.members.map((m) => (
                            <button
                              key={m.name}
                              className={`member-choice ${g.selected === m.name ? "selected" : ""}`}
                              {...capability("runtime")}
                              onClick={() =>
                                void runtimeCommand({
                                  Select: { group: g.name, member: m.name },
                                })
                              }
                            >
                              <strong>{m.name}</strong>
                              <span
                                className={
                                  m.alive === false ? "error-message" : "muted"
                                }
                              >
                                {m.alive === false
                                  ? "不可用"
                                  : m.delay_ms
                                    ? `${m.delay_ms} ms`
                                    : "尚未测试"}
                              </span>
                            </button>
                          ))}
                        </div>
                      </section>
                    ))}
                    {groups.length === 0 && (
                      <Empty>当前实例没有代理组数据。</Empty>
                    )}
                    <button
                      className="btn"
                      {...capability("subscriptions")}
                      onClick={() =>
                        void run("update_subscriptions", {
                          instance: current.id,
                        })
                      }
                    >
                      更新订阅
                    </button>
                    <button
                      className="btn"
                      {...capability("diagnostics")}
                      onClick={() =>
                        void run("diagnostics", {
                          instance: current.id,
                          targets: null,
                        })
                      }
                    >
                      连接诊断
                    </button>
                  </>
                ) : (
                  <Empty>代理组属于代理客户端。请切换实例。</Empty>
                ))}
              {page === "connections" && (
                <>
                  <div className="toolbar">
                    <input
                      className="input"
                      aria-label="筛选连接"
                      placeholder="筛选主机、出站或协议"
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                    />
                    <button className="btn" onClick={() => setPaused(!paused)}>
                      {paused ? "恢复实时显示" : "暂停显示"}
                    </button>
                    <button
                      className="btn danger"
                      {...capability("runtime")}
                      onClick={() =>
                        void runtimeCommand(
                          { Disconnect: null },
                          "关闭当前实例的全部连接？",
                        )
                      }
                    >
                      关闭全部
                    </button>
                  </div>
                  {current?.kind === "proxy" ? (
                    <>
                      <Table
                        rows={connections}
                        columns={[
                          ["host", "目标"],
                          ["port", "端口"],
                          ["network", "协议"],
                          ["outbound", "出站"],
                          ["upload", "上传字节"],
                          ["download", "下载字节"],
                        ]}
                        action={(row) => (
                          <button
                            className="btn sm"
                            {...capability("runtime")}
                            onClick={() =>
                              void runtimeCommand({
                                Disconnect: String(row.id),
                              })
                            }
                          >
                            关闭
                          </button>
                        )}
                      />
                      {runtime?.truncated === true && (
                        <p className="muted">
                          当前连接列表为有界采样，并非全部连接。
                        </p>
                      )}
                    </>
                  ) : (
                    <Empty>
                      此实例没有逐连接管理能力；总连接数可在概览查看。
                    </Empty>
                  )}
                </>
              )}
              {page === "rules" && (
                <>
                  <form
                    className="card card-pad toolbar"
                    onSubmit={(e) => {
                      e.preventDefault();
                      if (current)
                        void run("explain_route", {
                          instance: current.id,
                          request: {
                            host: route,
                            port: 443,
                            network: "Tcp",
                            inbound: "Mixed",
                            destination_ip: null,
                            process: null,
                          },
                        });
                    }}
                  >
                    <label htmlFor="route">测试目标</label>
                    <input
                      id="route"
                      className="input"
                      value={route}
                      required
                      placeholder="example.com"
                      onChange={(e) => setRoute(e.target.value)}
                    />
                    <button className="btn" {...capability("explain_route")}>
                      查询路由
                    </button>
                  </form>
                  <p className="muted">
                    查询使用目标节点的实际规则，不发送目标业务流量。
                  </p>
                </>
              )}
              {page === "logs" && (
                <>
                  <div className="toolbar">
                    <input
                      className="input"
                      aria-label="筛选日志"
                      placeholder="筛选日志"
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                    />
                    <button className="btn" onClick={() => setPaused(!paused)}>
                      {paused ? "恢复显示" : "暂停显示"}
                    </button>
                  </div>
                  <pre className="log-view">
                    {logs
                      .split("\n")
                      .filter((l) =>
                        l.toLowerCase().includes(search.toLowerCase()),
                      )
                      .join("\n") || "尚无日志"}
                  </pre>
                </>
              )}
              {page === "tasks" && (
                <>
                  {tasks.map((task) => (
                    <div className="card card-pad" key={task.id}>
                      <div className="page-head">
                        <strong>{task.state}</strong>
                        <span className="mono muted">{task.id}</span>
                        {task.state === "running" && (
                          <button
                            className="btn sm"
                            disabled={busy || !online}
                            onClick={() =>
                              void run("cancel_task", { operation_id: task.id })
                            }
                          >
                            请求取消
                          </button>
                        )}
                        <button
                          className="btn sm"
                          onClick={() =>
                            API.query<API.Task>(agent, "task", {
                              operation_id: task.id,
                            })
                              .then((r) =>
                                setTasks((old) =>
                                  old.map((t) =>
                                    t.id === task.id ? r.data : t,
                                  ),
                                ),
                              )
                              .catch(fail)
                          }
                        >
                          查询结果
                        </button>
                      </div>
                      <p>{task.message}</p>
                    </div>
                  ))}
                  <Table
                    rows={events.map((e) => ({
                      ...e,
                      time: when(e.timestamp_ms),
                    }))}
                    columns={[
                      ["time", "时间"],
                      ["kind", "事件"],
                      ["message", "说明"],
                    ]}
                  />
                </>
              )}
              {page === "settings" && (
                <>
                  <ProgramInfo
                    key={agent}
                    agent={agent}
                    nodeName={node?.name}
                    online={online}
                    instance={current}
                    runtime={runtime}
                    onError={fail}
                  />
                  <Updates
                    key={agent}
                    agent={agent}
                    online={online}
                    onError={fail}
                    onExecute={run}
                  />
                  <section className="card card-pad">
                    <h2>外观</h2>
                    <select
                      className="input"
                      aria-label="外观"
                      value={theme}
                      onChange={(e) => setTheme(e.target.value)}
                    >
                      <option value="system">跟随系统</option>
                      <option value="light">浅色</option>
                      <option value="dark">深色</option>
                    </select>
                  </section>
                  <section className="card card-pad exit-probe-settings">
                    <h2>出口探测</h2>
                    <label className="setting">
                      <input
                        type="checkbox"
                        checked={exitProbe.ip}
                        onChange={(e) =>
                          updateExitProbe({ ip: e.target.checked })
                        }
                      />
                      <span>
                        <strong>出口 IP</strong>
                        <span className="muted">
                          每次连接或切换节点都会查询一次出口
                          IP。开启时在顶栏保留其位置并重试失败的查询；关闭时只在查到后显示
                          IP。
                        </span>
                      </span>
                    </label>
                    <label className="setting">
                      <input
                        type="checkbox"
                        checked={exitProbe.latency}
                        onChange={(e) =>
                          updateExitProbe({ latency: e.target.checked })
                        }
                      />
                      <span>
                        <strong>出口 TCP 延迟</strong>
                        <span className="muted">
                          每次连接或切换节点都会测量一次经出口节点的 TCP
                          握手耗时。开启时按下方间隔重复测量，并在顶栏保留其位置。
                        </span>
                      </span>
                    </label>
                    <label className="setting">
                      <input
                        className="input"
                        type="number"
                        min={EXIT_INTERVAL.min}
                        max={EXIT_INTERVAL.max}
                        step={EXIT_INTERVAL.step}
                        value={exitProbe.interval}
                        aria-label="延迟测试间隔"
                        onChange={(e) =>
                          updateExitProbe({
                            interval: clampInterval(Number(e.target.value)),
                          })
                        }
                      />
                      <span>
                        <strong>延迟测试间隔</strong>
                        <span className="muted">
                          两次延迟测试的间隔秒数，5–3600。
                        </span>
                      </span>
                    </label>
                    <label className="setting">
                      <input
                        type="checkbox"
                        checked={exitProbe.keepAlive}
                        onChange={(e) =>
                          updateExitProbe({ keepAlive: e.target.checked })
                        }
                      />
                      <span>
                        <strong>探测进程保活</strong>
                        <span className="muted">
                          开启时保留一个已加载配置的探测进程，在两次测试之间休眠，每次只需一次握手。关闭时每次测试都启动并结束一个探测进程。停止探测后，休眠的探测进程会很快退出。
                        </span>
                      </span>
                    </label>
                    <p className="muted">
                      这些设置保存在当前浏览器，只影响本网页的顶栏。
                    </p>
                  </section>
                  <section className="card card-pad">
                    <h2>节点身份</h2>
                    <Details value={node} />
                    <h2>实例能力</h2>
                    <div className="toolbar">
                      {current?.capabilities.map((c) => (
                        <span className="pill" key={c}>
                          {c}
                        </span>
                      ))}
                    </div>
                    <p className="muted">
                      关闭此网页不停止节点或核心。网页监听与 TLS
                      由独立网页服务的配置控制。
                    </p>
                  </section>
                  <button
                    className="btn"
                    disabled={!online}
                    onClick={() =>
                      API.query(agent, "update_status")
                        .then((r) => setResult(r.data))
                        .catch(fail)
                    }
                  >
                    查看更新状态
                  </button>
                </>
              )}
              {result != null && (
                <section className="card card-pad">
                  <div className="page-head">
                    <h2>操作结果</h2>
                    <button className="btn sm" onClick={() => setResult(null)}>
                      收起
                    </button>
                  </div>
                  <Details value={result} />
                </section>
              )}
            </>
          )}
        </div>
      </main>
      <footer className="taskbar">
        <span className={`dot ${status === "已连接" ? "ok" : "warn"}`} />
        <span>{status}</span>
        <span className="right mono">
          {node?.name ?? agent} {current ? `/ ${current.name}` : ""}
        </span>
      </footer>
    </div>
  );
}
function Metric({ title, value }: { title: string; value: string }) {
  return (
    <div className="card metric">
      <span className="muted">{title}</span>
      <strong className="num">{value}</strong>
    </div>
  );
}
