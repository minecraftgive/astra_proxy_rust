import { useEffect, useRef, useState, type ReactNode } from "react";
import * as API from "../api";
import { ToolDetails } from "./NetworkDiagnosticDetails";
import {
  conclusions,
  ConclusionLines,
  latestConclusions,
} from "./NetworkConclusions";
import NetworkBatchForm from "./NetworkBatchForm";
import NetworkToolFields from "./NetworkToolFields";
import {
  buildRequest,
  requireToolCapabilities,
  stunPresets,
  type Form,
  type DiagnosticRequest,
} from "./networkToolsModel";
import { usePolling } from "../usePolling";
import {
  canonicalIp,
  needsResolutionCapability,
  validateLegacyRequest,
} from "./networkToolsEligibility";
import type {
  CoreQueryStatus,
  FakeIpLookupReply,
  Hello,
  NetworkCoreReply,
  NetworkBatchRequest,
  NetworkReport,
  NetworkResult,
  Snapshot as NetworkSnapshot,
} from "../generated/management";

type Row = Record<string, any>;
type Props = { agent: string; instance: string; online: boolean };
const sections = ["概览", "工具箱", "路径", "诊断"] as const;
const terminal = (state?: string) =>
  Boolean(state && !["running", "queued"].includes(state));
const fmt = (value: unknown) =>
  value == null
    ? "未知"
    : typeof value === "object"
      ? JSON.stringify(value)
      : String(value);
const states: Record<string, string> = {
  queued: "排队中",
  running: "运行中",
  complete: "完成",
  partial: "部分完成",
  failed: "失败",
  cancelled: "已取消",
  interrupted: "已中断",
};
const stamp = (value?: string) =>
  value ? new Date(Number(value)).toLocaleTimeString() : "未知";
function retained(key: string): string {
  try {
    return sessionStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
}
function retain(key: string, id: string) {
  try {
    if (id) sessionStorage.setItem(key, id);
    else sessionStorage.removeItem(key);
  } catch {
    /* Identity still remains on screen. */
  }
}
function runtimeSelection(runtime: Row | null) {
  if (!runtime) return null;
  return JSON.stringify({
    pid: runtime?.pid,
    mode: runtime?.mode,
    tun: runtime?.tun,
    system_proxy: runtime?.system_proxy,
    allow_lan: runtime?.allow_lan,
    groups: runtime?.groups
      ?.map((g: Row) => ({
        name: g.name,
        mode: g.mode,
        selected: g.selected,
      }))
      .sort((a: Row, b: Row) => a.name.localeCompare(b.name)),
  });
}
type PathObservation = {
  runtime: string | null;
  boot_id: string | null;
  applied_digest: string | null;
};
function observePath(
  runtime: Row | null,
  core: NetworkCoreReply | null,
): PathObservation {
  return {
    runtime: runtimeSelection(runtime),
    boot_id: core?.identity?.boot_id ?? null,
    applied_digest: core?.identity?.applied_digest ?? null,
  };
}
const coreLabels: Record<CoreQueryStatus, string> = {
  ok: "能力已确认",
  unconfirmed: "能力无法确认（能力列表缺失）",
  unsupported: "明确不支持",
  communication_error: "通信失败",
  protocol_error: "协议错误",
  core_error: "核心执行失败",
  invalid_request: "请求无效",
};
function coreFailure(instance: string, error: unknown): NetworkCoreReply {
  return {
    schema_version: 1,
    instance,
    sampled_at_ms: String(Date.now()),
    status:
      error instanceof API.ApiError && Object.hasOwn(coreLabels, error.code)
        ? (error.code as CoreQueryStatus)
        : "communication_error",
    message: error instanceof Error ? error.message : fmt(error),
    identity: null,
  };
}
async function readCore(agent: string, instance: string, signal?: AbortSignal) {
  const r = await API.query<NetworkCoreReply>(
    agent,
    "network_core",
    { instance },
    signal,
  );
  const data = r.data;
  if (
    r.type !== "network_core" ||
    data?.schema_version !== 1 ||
    data.instance !== instance ||
    !Object.hasOwn(coreLabels, data.status) ||
    (data.status === "ok" && !data.identity) ||
    (data.identity &&
      (data.identity.version !== 1 ||
        !data.identity.boot_id ||
        typeof data.identity.pid !== "number" ||
        (data.identity.capabilities != null &&
          (!Array.isArray(data.identity.capabilities) ||
            !data.identity.capabilities.every((c) => typeof c === "string")))))
  )
    throw new API.ApiError(
      "protocol_error",
      "核心身份响应的版本、实例或能力字段不匹配",
    );
  return data.status === "ok" && data.identity?.capabilities == null
    ? { ...data, status: "unconfirmed" as const }
    : data;
}
function fakeIpStatus(core: NetworkCoreReply): CoreQueryStatus {
  if (core.status !== "ok") return core.status;
  if (core.identity?.capabilities == null) return "unconfirmed";
  return core.identity.capabilities.includes("fake_ip_lookup_v1")
    ? "ok"
    : "unsupported";
}
function CoreStatus({
  core,
  bridge,
  advanced = false,
}: {
  advanced?: boolean;
  core: NetworkCoreReply | null;
  bridge: boolean;
}) {
  return (
    <div className="network-core-status">
      <p
        role="status"
        className={
          core?.status === "ok"
            ? "network-tone-success"
            : "network-tone-warning"
        }
      >
        核心状态：
        {core
          ? coreLabels[core.status]
          : bridge
            ? "正在读取"
            : "Agent 未提供核心能力查询，核心能力无法确认"}
      </p>
      {core && (
        <>
          <p>
            Fake-IP 查询：{coreLabels[fakeIpStatus(core)]} · 采样{" "}
            {stamp(core.sampled_at_ms)}
          </p>
          {core.message && <p className="muted">{core.message}</p>}
          {advanced && <CoreEvidence core={core} />}
        </>
      )}
    </div>
  );
}
function CoreEvidence({ core }: { core: NetworkCoreReply | null }) {
  return core ? (
    <>
      <dl className="network-facts">
        <div>
          <dt>核心启动标识</dt>
          <dd>{fmt(core.identity?.boot_id)}</dd>
        </div>
        <div>
          <dt>已应用配置摘要</dt>
          <dd>{fmt(core.identity?.applied_digest)}</dd>
        </div>
      </dl>
      <details>
        <summary>核心身份原始证据</summary>
        <pre>{JSON.stringify(core, null, 2)}</pre>
      </details>
    </>
  ) : (
    <p className="network-tone-muted">尚无核心身份快照。</p>
  );
}
function egressLabel(value: Row): string {
  if (value.kind === "system") return "目标 Agent 系统路由";
  if (value.kind === "interface") return `目标 Agent 网卡 ${value.name}`;
  if (value.kind === "outbound") return `配置出口 ${value.name}`;
  return "未知出口";
}
const resolutionPurposes: Record<string, string> = {
  business_target: "业务目标",
  proxy_entry: "代理入口",
  dns_service: "DNS 服务名",
};
function ResolutionEvidence({ event }: { event: Row }) {
  return (
    <dl className="network-facts">
      <div>
        <dt>解析对象</dt>
        <dd>{resolutionPurposes[event.purpose] ?? fmt(event.purpose)}</dd>
      </div>
      <div>
        <dt>主机名</dt>
        <dd>{fmt(event.host)}</dd>
      </div>
      <div>
        <dt>解析方式</dt>
        <dd>{fmt(event.method)}</dd>
      </div>
      <div>
        <dt>解析器</dt>
        <dd>{event.resolver ?? "未指定（参见解析方式）"}</dd>
      </div>
      <div>
        <dt>解析地址</dt>
        <dd>
          {Array.isArray(event.addresses) ? event.addresses.join(", ") : "未知"}
        </dd>
      </div>
      <div>
        <dt>出口网卡</dt>
        <dd>{event.interface ?? "未指定网卡"}</dd>
      </div>
    </dl>
  );
}

export function DiagnosticResult({
  item,
  advanced = false,
}: {
  item: NetworkResult;
  advanced?: boolean;
}) {
  const rawEvent = item.data as Row;
  const project = rawEvent.event === "project" ? rawEvent.project : null;
  let event = rawEvent;
  for (
    let depth = 0;
    depth < 4 && event.event === "project" && event.detail;
    depth++
  )
    event = event.detail;
  const summary = conclusions(event);
  if (!advanced)
    return summary.length ? (
      <article className="card card-pad network-result">
        {project && (
          <p className="network-tone-muted">
            项目 {project.project_id} · 路径 {project.path_id} · 第{" "}
            {project.round} 轮
          </p>
        )}
        <ConclusionLines rows={summary} />
      </article>
    ) : null;
  const specialized = [
    "tcp",
    "tls",
    "http",
    "public_ip",
    "stun",
    "nat",
    "trace",
    "trace_summary",
    "echo",
    "project_status",
    "connection",
    "context",
    "analysis",
  ].includes(event.event);
  const detail =
    event.detail && typeof event.detail === "object"
      ? (event.detail as Row)
      : null;
  return (
    <article
      className={`card card-pad network-result network-tone-${summary.some((row) => row.tone === "error") ? "error" : summary.some((row) => row.tone === "warning") ? "warning" : (summary[0]?.tone ?? "muted")}`}
    >
      <header>
        <strong>
          {event.event === "dns"
            ? `DNS #${fmt(event.seq)} · ${fmt(detail?.kind)}`
            : event.event === "resolution"
              ? `解析 #${fmt(event.seq)} · ${resolutionPurposes[event.purpose] ?? fmt(event.purpose)}`
              : fmt(event.event)}
        </strong>
        <span className="muted"> · {stamp(item.sampled_at_ms)}</span>
      </header>
      {project && (
        <p>
          项目 {project.project_id} · 路径 {project.path_id} · 第{" "}
          {project.round} 轮 · 证据 #{project.index}
        </p>
      )}
      {specialized ? (
        <ToolDetails event={event} />
      ) : event.event === "resolution" ? (
        <ResolutionEvidence event={event} />
      ) : detail?.records ? (
        <>
          <p className="muted">表格可横向滚动；长记录内容可独立滚动。</p>
          <div
            className="table-scroll"
            role="region"
            aria-label="DNS 记录"
            tabIndex={0}
          >
            <table>
              <thead>
                <tr>
                  <th>区段</th>
                  <th>名称</th>
                  <th>类型</th>
                  <th>类</th>
                  <th>TTL</th>
                  <th>内容</th>
                </tr>
              </thead>
              <tbody>
                {detail.records.map((r: Row, i: number) => (
                  <tr key={i}>
                    <td>{fmt(detail.section)}</td>
                    <td>{fmt(r.name)}</td>
                    <td>{fmt(r.record_type)}</td>
                    <td>{fmt(r.class)}</td>
                    <td>{fmt(r.ttl)}</td>
                    <td>
                      <div className="network-record" tabIndex={0}>
                        {fmt(r.text)}
                        {r.text_truncated && " [已截断]"}
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      ) : (
        <dl className="network-facts">
          {Object.entries(detail ?? event)
            .filter(([key]) => key !== "event")
            .map(([key, value]) => (
              <div key={key}>
                <dt>
                  {key === "rtt_ms" ? "连接 / 握手 / 查询耗时（ms）" : key}
                </dt>
                <dd>{fmt(value)}</dd>
              </div>
            ))}
        </dl>
      )}
      <details>
        <summary>原始证据</summary>
        <pre>{JSON.stringify(rawEvent, null, 2)}</pre>
      </details>
    </article>
  );
}

/** The conclusion stays in place when the original report is opened beside it. */
export function DiagnosticColumns({
  items,
  advanced,
  conclusionIntro,
  reportIntro,
}: {
  items: NetworkResult[];
  advanced: boolean;
  conclusionIntro?: ReactNode;
  reportIntro?: ReactNode;
}) {
  return (
    <>
      {advanced && (
        <p className="network-tone-muted">
          两栏可分别上下滚动；窄屏可左右滑动查看。
        </p>
      )}
      <div
        className="network-result-scroll"
        role="region"
        aria-label="结论与原始报告"
        tabIndex={0}
      >
        <div
          className={`network-result-columns${advanced ? " is-expanded" : ""}`}
        >
          <section
            className="network-result-column"
            aria-label="结论"
            tabIndex={0}
          >
            <h2>结论</h2>
            {conclusionIntro}
            {latestConclusions(items).map((item) => (
              <DiagnosticResult key={item.id} item={item} />
            ))}
            {!items.length && (
              <p className="network-tone-muted">尚无探测结论。</p>
            )}
          </section>
          {advanced && (
            <section
              className="network-result-column"
              aria-label="原始报告"
              tabIndex={0}
            >
              <h2>原始报告</h2>
              {reportIntro}
              {items.map((item) => (
                <DiagnosticResult key={item.id} item={item} advanced />
              ))}
              {!items.length && (
                <p className="network-tone-muted">尚无探测报告。</p>
              )}
            </section>
          )}
        </div>
      </div>
    </>
  );
}

export default function NetworkTools({ agent, instance, online }: Props) {
  const storageKey = `astra-network-job:${agent}:${instance}`;
  const [section, setSection] = useState<(typeof sections)[number]>("概览");
  const [advanced, setAdvanced] = useState(false);
  const [hello, setHello] = useState<Hello | null>(null);
  const [network, setNetwork] = useState<NetworkSnapshot | null>(null);
  const [runtime, setRuntime] = useState<Row | null>(null);
  const [core, setCore] = useState<NetworkCoreReply | null>(null);
  const [outbounds, setOutbounds] = useState<string[]>([]);
  const [snapshotError, setSnapshotError] = useState("");
  const [error, setError] = useState("");
  const [form, setForm] = useState<Form>({
    kind: "tcp",
    egress: "system",
    target: "example.com:443",
    server: "udp://1.1.1.1:53",
    resolver: "",
    record: "A",
    count: 1,
    timeout: 3000,
    family: "",
    ports: "80,443",
    serverName: "",
    method: "HEAD",
    compareBundled: false,
    stunServer: "stun.cloudflare.com:3478",
    natServers: stunPresets,
    natInterface: "",
    natAdvanced: true,
    maxHops: 30,
    probesPerHop: 3,
  });
  const updateForm = (patch: Partial<Form>) =>
    setForm((old) => ({ ...old, ...patch }));
  const [batchSubmitted, setBatchSubmitted] = useState(0);
  const [operation, setOperation] = useState(() => retained(storageKey));
  const [operationScope, setOperationScope] = useState(`${agent}/${instance}`);
  const [lookup, setLookup] = useState("");
  const [report, setReport] = useState<NetworkReport | null>(null);
  const [items, setItems] = useState<NetworkResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [path, setPath] = useState("");
  const [selectionAtStart, setSelectionAtStart] =
    useState<PathObservation | null>(null);
  const [fakeIpInput, setFakeIpInput] = useState("");
  const [fakeIpBusy, setFakeIpBusy] = useState(false);
  const [fakeIpResult, setFakeIpResult] = useState<FakeIpLookupReply | null>(
    null,
  );
  const fakeIpController = useRef<AbortController | null>(null);
  const cursor = useRef<string | null>(null);
  const reportBytes = useRef(0);
  const cancellation = useRef("");
  const scope = useRef("");
  const submission = useRef(0);
  scope.current = `${agent}/${instance}`;
  const supported =
    hello?.capabilities.includes("network_diagnostics_jobs_v1") ?? false;
  const interfaceSupported =
    hello?.capabilities.includes("network_diagnostics_interface_ip_v1") ??
    false;
  const coreBridge = hello?.capabilities.includes("core_network_v1") ?? false;
  const finished = terminal(report?.state) && !report?.has_more;
  const active = Boolean(
    operation && operationScope === `${agent}/${instance}` && !finished,
  );
  const observed = observePath(runtime, core);
  const staleReasons = selectionAtStart
    ? [
        selectionAtStart.boot_id &&
        observed.boot_id &&
        selectionAtStart.boot_id !== observed.boot_id
          ? "核心已重启"
          : null,
        selectionAtStart.applied_digest &&
        observed.applied_digest &&
        selectionAtStart.applied_digest !== observed.applied_digest
          ? "已应用配置已变化"
          : null,
        selectionAtStart.runtime &&
        observed.runtime &&
        selectionAtStart.runtime !== observed.runtime
          ? "运行模式或代理选择已变化"
          : null,
      ].filter(Boolean)
    : [];
  const stale = staleReasons.length > 0;
  const pathUnconfirmed =
    selectionAtStart &&
    (!selectionAtStart.boot_id ||
      !selectionAtStart.applied_digest ||
      !selectionAtStart.runtime ||
      !observed.boot_id ||
      !observed.applied_digest ||
      !observed.runtime);
  const contextEvent = items
    .map((item) => item.data as Row)
    .find((event) => event.event === "context");
  const startedEvent = items
    .map((item) => item.data as Row)
    .find((event) => event.event === "started");
  const reportedPath = contextEvent?.actual_egress
    ? egressLabel(contextEvent.actual_egress)
    : startedEvent?.egress;
  const selectedOutbounds = [
    ...new Set([
      "DIRECT",
      ...outbounds,
      ...(runtime?.groups ?? []).map((g: Row) => g.name),
    ]),
  ];
  useEffect(() => {
    submission.current++;
    setHello(null);
    setNetwork(null);
    setRuntime(null);
    setCore(null);
    setOutbounds([]);
    updateForm({ egress: "system", resolver: "", natInterface: "" });
    setOperation(retained(storageKey));
    setOperationScope(`${agent}/${instance}`);
    setReport(null);
    setItems([]);
    cursor.current = null;
    reportBytes.current = 0;
    setError("");
    setSnapshotError("");
    setLookup("");
    setBusy(false);
    setCancelling(false);
    setPath("");
    setSelectionAtStart(null);
    setFakeIpInput("");
    setFakeIpResult(null);
    setFakeIpBusy(false);
    cancellation.current = "";
    return () => {
      fakeIpController.current?.abort();
    };
  }, [agent, instance, storageKey]);
  useEffect(() => {
    if (!contextEvent) return;
    if (contextEvent.actual_egress)
      setPath(egressLabel(contextEvent.actual_egress));
    setSelectionAtStart((previous) => ({
      runtime: previous?.runtime ?? null,
      boot_id: contextEvent.running_core?.boot_id ?? null,
      applied_digest:
        contextEvent.config_observation === "matched_running"
          ? (contextEvent.config_digest ?? null)
          : null,
    }));
  }, [contextEvent]);
  usePolling(
    async (signal) => {
      const notices: string[] = [];
      try {
        const response = await API.query<Hello>(
          agent,
          "hello",
          undefined,
          signal,
        );
        if (response.type !== "hello" || response.data.role !== "agent")
          throw new Error("协议错误：需要目标 Agent 能力响应");
        if (signal.aborted) return;
        setHello(response.data);
        if (response.data.capabilities.includes("network_state_v1")) {
          try {
            const state = await API.query<NetworkSnapshot>(
              agent,
              "network_state",
              undefined,
              signal,
            );
            if (
              state.type !== "network_state" ||
              state.data.agent_id !== agent ||
              state.data.schema_version !== 1
            )
              throw new Error("协议错误：网络状态响应类型不匹配");
            if (!signal.aborted) setNetwork(state.data);
          } catch (e) {
            if (!signal.aborted) {
              setNetwork(null);
              notices.push(
                `系统网络信息：${fmt(e instanceof Error ? e.message : e)}`,
              );
            }
          }
        } else {
          setNetwork(null);
          notices.push(
            "目标 Agent 不支持网络状态；不会使用浏览器本机网卡代替。",
          );
        }
        if (instance) {
          try {
            const nodes = await API.query<string[]>(
              agent,
              "node_tags",
              { instance },
              signal,
            );
            if (
              nodes.type !== "node_tags" ||
              !Array.isArray(nodes.data) ||
              !nodes.data.every((tag) => typeof tag === "string")
            )
              throw new Error("配置出口目录响应无效");
            if (!signal.aborted) setOutbounds(nodes.data);
          } catch (e) {
            if (!signal.aborted) {
              setOutbounds([]);
              notices.push(
                `配置出口目录：${e instanceof Error ? e.message : fmt(e)}`,
              );
            }
          }
          try {
            const r = await API.query<{ kind: string; snapshot: Row }>(
              agent,
              "runtime",
              { instance },
              signal,
            );
            if (r.type !== "runtime" || r.data.kind !== "client")
              throw new Error("实例未提供代理运行快照");
            if (!signal.aborted) setRuntime(r.data.snapshot);
          } catch (e) {
            if (!signal.aborted) {
              setRuntime(null);
              notices.push(
                `运行快照：${e instanceof Error ? e.message : fmt(e)}`,
              );
            }
          }
          if (response.data.capabilities.includes("core_network_v1")) {
            try {
              const identity = await readCore(agent, instance, signal);
              if (!signal.aborted) setCore(identity);
            } catch (e) {
              if (!signal.aborted) {
                setCore(coreFailure(instance, e));
              }
            }
          } else setCore(null);
        }
        if (!signal.aborted) setSnapshotError(notices.join("\n"));
      } catch (e) {
        if (!signal.aborted) {
          setHello(null);
          setNetwork(null);
          setRuntime(null);
          setCore(null);
          setOutbounds([]);
          setSnapshotError(e instanceof Error ? e.message : fmt(e));
        }
      }
    },
    online && agent ? 5000 : null,
    [agent, instance, online],
  );
  const refreshReport = usePolling(
    async (signal) => {
      try {
        let more = true;
        while (more && !signal.aborted) {
          const r = await API.query<NetworkReport>(
            agent,
            "network_report",
            { operation_id: operation, cursor: cursor.current },
            signal,
          );
          if (
            r.type !== "network_report" ||
            r.data.operation_id !== operation ||
            r.data.schema_version !== 1
          )
            throw new Error("协议错误：报告身份或类型不匹配");
          if (signal.aborted) return;
          const page = r.data;
          if (
            !Object.hasOwn(states, page.state) ||
            typeof page.has_more !== "boolean" ||
            typeof page.expired !== "boolean" ||
            typeof page.cursor !== "string" ||
            !Array.isArray(page.items) ||
            page.items.some(
              (item) =>
                !item ||
                typeof item.id !== "string" ||
                typeof item.sampled_at_ms !== "string" ||
                !item.data ||
                typeof item.data !== "object" ||
                Array.isArray(item.data) ||
                typeof (item.data as Row).event !== "string",
            )
          )
            throw new Error("协议错误：报告状态、游标或结果事件结构无效");
          if (new TextEncoder().encode(JSON.stringify(page)).length > 64 * 1024)
            throw new Error("协议错误：报告单页超过 64 KiB 上限");
          const sequence = (value: string) => {
            if (!/^(0|[1-9][0-9]{0,19})$/.test(value))
              throw new Error("协议错误：报告游标或结果标识无效");
            const number = BigInt(value);
            if (number > 18446744073709551615n)
              throw new Error("协议错误：报告游标超出范围");
            return number;
          };
          const previous = sequence(cursor.current ?? "0");
          const next = sequence(page.cursor);
          let last = previous;
          for (const item of page.items) {
            const id = sequence(item.id);
            if (id <= last || id > next)
              throw new Error("协议错误：报告结果重复、乱序或超出游标");
            last = id;
          }
          if (
            next < previous ||
            (!page.expired && next !== last) ||
            (page.has_more && (next <= previous || page.items.length === 0))
          )
            throw new Error(
              "协议错误：报告分页未前进或游标不匹配，请稍后重查原任务",
            );
          const bytes = page.items.reduce(
            (total, item) =>
              total + new TextEncoder().encode(JSON.stringify(item)).length,
            0,
          );
          if (reportBytes.current + bytes > 2 * 1024 * 1024)
            throw new Error(
              "报告超过 2 MiB 接收上限；已保留原任务和已接收证据，请重查 Agent 状态",
            );
          reportBytes.current += bytes;
          cursor.current = page.cursor;
          more = page.has_more;
          setItems((old) => [...old, ...page.items]);
          setReport(page);
          setError("");
          if (terminal(page.state)) setCancelling(false);
        }
      } catch (e) {
        if (!signal.aborted)
          setError(
            `报告尚未确认，请保留任务标识并重查：${e instanceof Error ? e.message : fmt(e)}`,
          );
      }
    },
    online &&
      supported &&
      operation &&
      operationScope === `${agent}/${instance}` &&
      !finished
      ? 750
      : null,
    [agent, instance, operation, operationScope, online],
  );
  const run = async (batch?: NetworkBatchRequest) => {
    if (!agent || !instance || !online || !hello || busy || active) return;
    const generation = scope.current;
    const attempt = ++submission.current;
    const id = crypto.randomUUID();
    let request: DiagnosticRequest;
    try {
      request = batch
        ? (batch.projects[0].request as DiagnosticRequest)
        : buildRequest(form, agent, network);
    } catch (error) {
      setError(error instanceof Error ? error.message : String(error));
      return;
    }
    const requests = batch
      ? batch.projects.map((project) => project.request as DiagnosticRequest)
      : [request];
    const probe = request.probe;
    const chosen = request.egress;
    if (
      requests.some(
        ({ egress: chosen }) =>
          (chosen.kind === "interface" &&
            !network?.interfaces.some(
              (i) => i.id === chosen.name && i.connected,
            )) ||
          (chosen.kind === "outbound" &&
            !selectedOutbounds.includes(chosen.name)),
      )
    ) {
      setError("所选出口已不可用，请重新选择。");
      return;
    }
    setBusy(true);
    setError("");
    let submitted = false;
    try {
      const latest = await API.query<Hello>(agent, "hello");
      if (generation !== scope.current || attempt !== submission.current)
        return;
      if (
        latest.type !== "hello" ||
        latest.data?.role !== "agent" ||
        !Array.isArray(latest.data.capabilities) ||
        !latest.data.capabilities.every(
          (capability) => typeof capability === "string",
        )
      )
        throw new API.ApiError(
          "protocol_error",
          "协议错误：需要目标 Agent 的最新能力响应",
        );
      setHello(latest.data);
      const capabilities = latest.data.capabilities;
      const background = capabilities.includes("network_diagnostics_jobs_v1");
      if (
        batch &&
        (!background || !capabilities.includes("network_diagnostics_batch_v1"))
      )
        throw new API.ApiError(
          "unsupported",
          "目标 Agent 未声明批量诊断能力；未提交任务。",
        );
      for (const request of requests) {
        const chosen = request.egress;
        if (!background) validateLegacyRequest(request);
        requireToolCapabilities(request, capabilities);
        if (
          needsResolutionCapability(request) &&
          (!background ||
            !capabilities.includes("network_diagnostics_resolution_v1"))
        )
          throw new API.ApiError(
            "unsupported",
            "目标 Agent 未声明新版解析能力，不能执行指定解析器、加密 DNS 或网卡域名请求；未提交任务，也不会降级丢弃选项。",
          );
        if (
          chosen.kind === "outbound" &&
          (!background ||
            !capabilities.includes("network_diagnostics_pinned_v1"))
        )
          throw new API.ApiError(
            "unsupported",
            "目标 Agent 未声明固定路径能力，不能执行配置出口；未提交任务。",
          );
        if (
          chosen.kind === "interface" &&
          background &&
          !capabilities.includes("network_diagnostics_interface_ip_v1")
        )
          throw new API.ApiError(
            "unsupported",
            "目标 Agent 未声明指定网卡诊断能力；未提交任务。",
          );
      }
      setItems([]);
      setReport(null);
      cursor.current = null;
      reportBytes.current = 0;
      setPath(
        batch
          ? batch.projects
              .map(
                (project) =>
                  `${project.path_id}: ${egressLabel((project.request as DiagnosticRequest).egress)}`,
              )
              .filter((v, i, a) => a.indexOf(v) === i)
              .join("；")
          : egressLabel(chosen),
      );
      setSelectionAtStart(observePath(runtime, core));
      cancellation.current = "";
      retain(storageKey, background ? id : "");
      setOperationScope(generation);
      setOperation(background ? id : "");
      submitted = true;
      if (requests.some((request) => request.probe.kind === "nat"))
        updateForm({ natInterface: "" });
      if (batch) setBatchSubmitted((old) => old + 1);
      const r = await API.request<Row>(agent, {
        type: "execute",
        data: {
          operation_id: id,
          command: {
            type: batch
              ? "network_diagnostics_batch"
              : background
                ? "network_diagnostics"
                : "net_tool",
            data: {
              instance,
              request:
                batch ??
                (background
                  ? request
                  : {
                      egress: chosen,
                      probe,
                      count: request.count,
                      interval_ms: request.interval_ms,
                      timeout_ms: request.timeout_ms,
                    }),
            },
          },
        },
      });
      if (generation !== scope.current || attempt !== submission.current)
        return;
      if (background) {
        if (r.type !== "task" || r.data.id !== id)
          throw new Error("协议错误：任务接受回执不匹配");
        refreshReport();
      } else {
        if (r.type !== "net_tool" || !Array.isArray(r.data))
          throw new Error("协议错误：旧网络工具响应不匹配");
        setItems(
          r.data.map((data, index) => ({
            id: String(index + 1),
            sampled_at_ms: String(Date.now()),
            data,
          })),
        );
      }
    } catch (e) {
      if (generation === scope.current && attempt === submission.current) {
        setError(e instanceof Error ? e.message : fmt(e));
        if (
          submitted &&
          e instanceof API.ApiError &&
          ["invalid_request", "unsupported", "conflict"].includes(e.code)
        ) {
          setOperation("");
          retain(storageKey, "");
        }
      }
    } finally {
      if (generation === scope.current && attempt === submission.current)
        setBusy(false);
    }
  };
  const queryFakeIp = async () => {
    if (!agent || !instance || !online || !coreBridge || fakeIpBusy) return;
    const addresses = fakeIpInput
      .trim()
      .split(/[\s,，]+/)
      .filter(Boolean);
    const result = (
      status: CoreQueryStatus,
      message: string,
    ): FakeIpLookupReply => ({
      schema_version: 1,
      instance,
      sampled_at_ms: String(Date.now()),
      status,
      message,
      boot_id: null,
      mappings: [],
    });
    if (
      !addresses.length ||
      addresses.length > 64 ||
      addresses.some((address) => canonicalIp(address) === null)
    ) {
      setFakeIpResult(
        result(
          "invalid_request",
          "请填写 1–64 个有效 IPv4 / IPv6 地址，以空格、逗号或换行分隔；不接受域名。",
        ),
      );
      return;
    }
    const controller = new AbortController();
    fakeIpController.current?.abort();
    fakeIpController.current = controller;
    setFakeIpBusy(true);
    setFakeIpResult(null);
    try {
      // Re-read identity on every click; Agent capability is not Core capability.
      const identity = await readCore(agent, instance, controller.signal);
      if (controller.signal.aborted) return;
      setCore(identity);
      const capability = fakeIpStatus(identity);
      if (capability !== "ok") {
        setFakeIpResult(
          result(
            capability,
            identity.message ??
              "目标核心未明确声明 fake_ip_lookup_v1；未发送映射查询。",
          ),
        );
        return;
      }
      const response = await API.query<FakeIpLookupReply>(
        agent,
        "fake_ip_lookup",
        { instance, addresses },
        controller.signal,
      );
      if (controller.signal.aborted) return;
      const data = response.data;
      if (
        response.type !== "fake_ip_lookup" ||
        data?.schema_version !== 1 ||
        data.instance !== instance ||
        !Object.hasOwn(coreLabels, data.status) ||
        !Array.isArray(data.mappings) ||
        data.mappings.length > 64 ||
        (data.status === "ok" &&
          (data.boot_id !== identity.identity?.boot_id ||
            data.mappings.length !== addresses.length ||
            data.mappings.some(
              (mapping, index) =>
                !mapping ||
                typeof mapping.address !== "string" ||
                canonicalIp(mapping.address) !==
                  canonicalIp(addresses[index]) ||
                (mapping.domain !== null && typeof mapping.domain !== "string"),
            )))
      )
        throw new API.ApiError(
          "protocol_error",
          "映射响应的实例、核心启动标识或地址列表不匹配；请重新查询。",
        );
      setFakeIpResult(data);
    } catch (e) {
      if (!controller.signal.aborted) {
        const failure = coreFailure(instance, e);
        setFakeIpResult(result(failure.status, failure.message ?? "查询失败"));
      }
    } finally {
      if (!controller.signal.aborted) setFakeIpBusy(false);
    }
  };
  const cancel = async () => {
    if (
      !operation ||
      !report ||
      terminal(report.state) ||
      !supported ||
      operationScope !== scope.current
    )
      return;
    const generation = scope.current;
    cancellation.current ||= crypto.randomUUID();
    setCancelling(true);
    try {
      await API.request(agent, {
        type: "execute",
        data: {
          operation_id: cancellation.current,
          command: { type: "cancel_task", data: { operation_id: operation } },
        },
      });
      if (generation === scope.current) refreshReport();
    } catch (e) {
      if (generation === scope.current) {
        setError(`取消尚未确认：${e instanceof Error ? e.message : fmt(e)}`);
        setCancelling(false);
      }
    }
  };
  const exportReport = () => {
    const payload = { report: report ? { ...report, items: [] } : null, items };
    let data = JSON.stringify(payload, null, 2);
    if (new TextEncoder().encode(data).length > 2 * 1024 * 1024)
      data = JSON.stringify(payload);
    if (new TextEncoder().encode(data).length > 2 * 1024 * 1024) {
      setError(
        "报告超过 2 MiB，无法完整导出；可复制任务摘要。没有生成截断文件。",
      );
      return;
    }
    const url = URL.createObjectURL(
      new Blob([data], { type: "application/json" }),
    );
    const link = document.createElement("a");
    link.href = url;
    link.download = `network-${operation || "legacy"}.json`;
    link.click();
    URL.revokeObjectURL(url);
  };
  return (
    <div
      className="network-tools"
      tabIndex={-1}
      onKeyDownCapture={(event) => {
        if (
          !event.nativeEvent.isComposing &&
          event.ctrlKey &&
          !event.altKey &&
          !event.metaKey &&
          !event.shiftKey &&
          event.key.toLowerCase() === "d" &&
          !(event.target as HTMLElement).closest(
            "input, textarea, select, [contenteditable]:not([contenteditable='false']), [role='textbox']",
          )
        ) {
          event.preventDefault();
          event.stopPropagation();
          if (!event.repeat) setAdvanced((old) => !old);
        }
      }}
    >
      <nav className="network-tabs" aria-label="网络工具分区">
        {sections.map((tab) => (
          <button
            key={tab}
            aria-pressed={section === tab}
            onClick={() => setSection(tab)}
          >
            {tab}
          </button>
        ))}
      </nav>
      <div className="network-actions">
        <button
          aria-expanded={advanced}
          aria-keyshortcuts="Control+D"
          onClick={() => setAdvanced((old) => !old)}
        >
          {advanced ? "隐藏原始报告" : "显示原始报告"} (Ctrl+D)
        </button>
        <span className="network-tone-muted">
          {advanced ? "左侧结论 · 右侧原始报告" : "仅显示结论"}
        </span>
      </div>
      {snapshotError && (
        <p className="network-tone-warning" role="status">
          {snapshotError}
        </p>
      )}
      {section === "概览" && (
        <>
          <section className="card card-pad">
            <h2>目标 Agent 网络状态</h2>
            <p className="muted">
              节点 {agent} · 采样 {stamp(network?.sampled_at_ms)} · DNS 采样{" "}
              {stamp(network?.dns_sampled_at_ms)}
            </p>
            {network ? (
              <>
                <p className="network-tone-info">
                  网卡：
                  {network.interfaces
                    .map(
                      (nic) =>
                        `${nic.name}（${nic.connected ? "已连接" : "未连接"}）`,
                    )
                    .join("、") || "暂无记录"}
                </p>
                {advanced && (
                  <>
                    <div className="table-scroll">
                      <table>
                        <thead>
                          <tr>
                            <th>网卡</th>
                            <th>标识</th>
                            <th>类型</th>
                            <th>连接</th>
                            <th>地址</th>
                          </tr>
                        </thead>
                        <tbody>
                          {network.interfaces.map((i) => (
                            <tr key={i.id}>
                              <td>{i.name}</td>
                              <td>{i.id}</td>
                              <td>{i.kind}</td>
                              <td>{i.connected ? "已连接" : "未连接"}</td>
                              <td>
                                {i.addresses
                                  .map(
                                    (a) =>
                                      `${a.address}${a.prefix_len == null ? "" : `/${a.prefix_len}`}`,
                                  )
                                  .join(", ")}
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                    <h3>系统 DNS 与分域策略</h3>
                    <p className="muted">
                      用于展示，不声称完整复现操作系统解析器。
                    </p>
                    <div className="table-scroll">
                      <table>
                        <thead>
                          <tr>
                            <th>来源 / 作用域</th>
                            <th>网卡</th>
                            <th>域</th>
                            <th>解析器</th>
                            <th>优先级</th>
                          </tr>
                        </thead>
                        <tbody>
                          {network.dns.map((d, i) => (
                            <tr key={i}>
                              <td>
                                {d.source} / {d.scope}
                              </td>
                              <td>
                                {d.interface_id ??
                                  (d.scope === "global"
                                    ? "全局"
                                    : "未知 / 策略作用域")}
                              </td>
                              <td>{d.domains.join(", ")}</td>
                              <td>{d.servers.join(", ")}</td>
                              <td>{d.priority ?? "未知"}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </div>
                    <details>
                      <summary>路由证据</summary>
                      <pre>{JSON.stringify(network.routes, null, 2)}</pre>
                    </details>
                  </>
                )}
                {network.issues.map((issue, i) => (
                  <p key={i} role="status">
                    {issue.component}：{issue.message}
                  </p>
                ))}
                {network.truncated && (
                  <p className="network-tone-warning" role="status">
                    系统信息已截断
                  </p>
                )}
              </>
            ) : (
              <p>目标节点网络信息尚不可用。</p>
            )}
          </section>
          <section className="card card-pad">
            <h2>实例当前状态</h2>
            <p>
              模式 {fmt(runtime?.mode)} · 系统代理 {fmt(runtime?.system_proxy)}{" "}
              · TUN {fmt(runtime?.tun)}
            </p>
            {advanced && (
              <pre>
                {JSON.stringify(
                  runtime?.groups?.map((g: Row) => ({
                    name: g.name,
                    mode: g.mode,
                    selected: g.selected,
                  })) ?? [],
                  null,
                  2,
                )}
              </pre>
            )}
            <CoreStatus core={core} bridge={coreBridge} advanced={advanced} />
          </section>
        </>
      )}
      {(section === "工具箱" || section === "诊断") && (
        <>
          {section === "工具箱" ? (
            <section className="card card-pad">
              <h2>主动测试</h2>
              <p className="muted">
                仅点击运行后发送探测。需要解析域名时可指定解析器，DNS 服务名可用
                #bootstrap=IP 保留 TLS
                名称。配置出口与组会在任务启动时固定；无法证明运行选择时需改选具体节点。
              </p>
              <p className="muted">
                TCP 耗时包含连接和握手；代理路径还包含入口解析，不能当作目标
                RTT。代理 DNS 不支持
                UDP，不支持的路径会报告失败，不会改走系统路由。
              </p>
              {!supported && (
                <p className="network-tone-warning" role="status">
                  {hello
                    ? "旧 Agent：使用原工具，仅系统路由 TCP/UDP-TCP DNS 或网卡 IP 端点；系统域名解析路径不可确认，不支持配置出口、bootstrap、后台报告与确认取消。"
                    : "正在确认目标 Agent 能力。"}
                </p>
              )}
              <NetworkToolFields
                form={form}
                update={updateForm}
                network={network}
                outbounds={selectedOutbounds}
                supported={supported}
                interfaceSupported={interfaceSupported}
              />
              <div className="network-actions">
                <button
                  disabled={!hello || !online || !instance || busy || active}
                  onClick={() => void run()}
                >
                  运行
                </button>
                <button
                  disabled={
                    !supported ||
                    !active ||
                    !report ||
                    terminal(report.state) ||
                    cancelling
                  }
                  onClick={() => void cancel()}
                >
                  {cancelling ? "等待确认取消…" : "取消任务"}
                </button>
              </div>
            </section>
          ) : (
            <NetworkBatchForm
              agent={agent}
              instance={instance}
              network={network}
              outbounds={selectedOutbounds}
              base={form}
              disabled={!hello || !online || !instance || busy || active}
              submitted={batchSubmitted}
              onRun={(batch) => void run(batch)}
            />
          )}
          <section className="card card-pad">
            <p
              className={
                report?.state === "failed"
                  ? "network-tone-error"
                  : report?.state === "complete"
                    ? "network-tone-info"
                    : report && terminal(report.state)
                      ? "network-tone-warning"
                      : "network-tone-info"
              }
            >
              任务标识：<code>{operation || "尚未提交"}</code> ·{" "}
              {report
                ? states[report.state]
                : operation
                  ? "正在查询原任务"
                  : "未运行"}
            </p>
            <p>
              本次探测选择：
              {reportedPath || path || "重连任务请查看 started 证据"}
            </p>
            {stale && (
              <p className="network-tone-warning" role="status">
                路径快照已过时：{staleReasons.join("；")}
                。本次探测出口没有自动切换。
              </p>
            )}
            {pathUnconfirmed && (
              <p className="muted">
                当前核心或运行快照不完整，无法确认路径是否仍一致。
              </p>
            )}
            {report?.truncated && (
              <p className="network-tone-warning" role="status">
                报告已截断，不能作为完整证据。
              </p>
            )}
            {report?.expired && (
              <p className="network-tone-warning" role="status">
                报告证据已过期，保留任务身份以防重复执行。
              </p>
            )}
            <p>{report?.message}</p>
            <div className="network-actions">
              <label>
                重连已有任务
                <input
                  value={lookup}
                  onChange={(e) => setLookup(e.target.value)}
                />
              </label>
              <button
                disabled={!lookup.trim() || active || !online || !supported}
                onClick={() => {
                  setOperationScope(scope.current);
                  setOperation(lookup.trim());
                  retain(storageKey, lookup.trim());
                  setReport(null);
                  setItems([]);
                  setPath("");
                  setSelectionAtStart(null);
                  setError("");
                  setCancelling(false);
                  cancellation.current = "";
                  cursor.current = null;
                  reportBytes.current = 0;
                }}
              >
                查询报告
              </button>
              <button
                disabled={!active}
                onClick={() => {
                  setLookup(operation);
                  setOperation("");
                  retain(storageKey, "");
                  setReport(null);
                  setItems([]);
                  setPath("");
                  setSelectionAtStart(null);
                  setCancelling(false);
                  setError(
                    `已停止观察，任务 ${operation} 仍可能在 Agent 执行；可用原标识重连。`,
                  );
                }}
              >
                停止观察
              </button>
              <button disabled={!items.length} onClick={exportReport}>
                导出 JSON
              </button>
              <button
                disabled={!operation}
                onClick={() =>
                  void navigator.clipboard
                    .writeText(
                      `Agent: ${agent}\n任务: ${operation}\n状态: ${report ? states[report.state] : "未确认"}\n${report?.message ?? ""}`,
                    )
                    .catch((e) => setError(String(e)))
                }
              >
                复制摘要
              </button>
            </div>
          </section>
          <DiagnosticColumns items={items} advanced={advanced} />
        </>
      )}
      {section === "路径" && (
        <DiagnosticColumns
          items={items}
          advanced={advanced}
          conclusionIntro={
            <>
              <h3>实例当前选择</h3>
              <CoreStatus core={core} bridge={coreBridge} />
              <h3>本次探测选择</h3>
              <p>
                {reportedPath ||
                  path ||
                  "尚未启动；重连任务以报告中的 started 证据为准。"}
              </p>
              <p>
                当前组成员名不能证明本次探测的完整选择链，以任务路径证据为准。逐跳未响应保留未知；代理之后的不可见路径不补画设备。
              </p>
              {stale && (
                <p className="network-tone-warning" role="status">
                  路径快照已过时：{staleReasons.join("；")}
                  。已启动任务保持原出口。
                </p>
              )}
              {pathUnconfirmed && (
                <p className="muted">
                  当前核心或运行快照不完整，无法确认路径是否仍一致。
                </p>
              )}
            </>
          }
          reportIntro={
            <>
              <h3>实例当前选择快照</h3>
              <CoreEvidence core={core} />
              <pre>
                {JSON.stringify(
                  runtime?.groups?.map((g: Row) => ({
                    name: g.name,
                    mode: g.mode,
                    selected: g.selected,
                  })) ?? [],
                  null,
                  2,
                )}
              </pre>
              {selectionAtStart && (
                <dl className="network-facts">
                  <div>
                    <dt>探测时核心启动标识</dt>
                    <dd>{fmt(selectionAtStart.boot_id)}</dd>
                  </div>
                  <div>
                    <dt>探测时配置摘要</dt>
                    <dd>{fmt(selectionAtStart.applied_digest)}</dd>
                  </div>
                </dl>
              )}
              {contextEvent && (
                <>
                  <dl className="network-facts">
                    <div>
                      <dt>固定出口网卡</dt>
                      <dd>
                        {contextEvent.effective_interface ?? "未指定网卡"}
                      </dd>
                    </div>
                    <div>
                      <dt>网卡来源</dt>
                      <dd>{contextEvent.interface_source ?? "未记录"}</dd>
                    </div>
                    <div>
                      <dt>路径来源</dt>
                      <dd>{fmt(contextEvent.egress_source)}</dd>
                    </div>
                    <div>
                      <dt>配置来源</dt>
                      <dd>{fmt(contextEvent.config_observation)}</dd>
                    </div>
                  </dl>
                  {Array.isArray(contextEvent.selection_chain) &&
                    contextEvent.selection_chain.length > 0 && (
                      <>
                        <h3>本次固定选择链</h3>
                        <div
                          className="table-scroll"
                          role="region"
                          aria-label="本次固定选择链"
                          tabIndex={0}
                        >
                          <table>
                            <thead>
                              <tr>
                                <th>组</th>
                                <th>模式</th>
                                <th>固定选择</th>
                                <th>选择来源</th>
                                <th>网卡</th>
                              </tr>
                            </thead>
                            <tbody>
                              {contextEvent.selection_chain.map(
                                (hop: Row, i: number) => (
                                  <tr key={i}>
                                    <td>{fmt(hop.group)}</td>
                                    <td>{fmt(hop.mode)}</td>
                                    <td>{fmt(hop.selected)}</td>
                                    <td>{fmt(hop.source)}</td>
                                    <td>{hop.interface ?? "未指定"}</td>
                                  </tr>
                                ),
                              )}
                            </tbody>
                          </table>
                        </div>
                      </>
                    )}
                  <details>
                    <summary>本次路径快照证据</summary>
                    <pre>{JSON.stringify(contextEvent, null, 2)}</pre>
                  </details>
                </>
              )}
            </>
          }
        />
      )}
      {(section === "概览" || section === "路径") && (
        <section className="card card-pad">
          <h2>Fake-IP 映射查询</h2>
          <p className="muted">
            只读查询目标 Agent 的当前实例映射，仅点击查询时执行。最多 64 个
            IP；未命中不代表该地址不是 Fake-IP，也不会按地址段推断。
          </p>
          {!coreBridge && (
            <p className="network-tone-warning" role="status">
              Agent 未提供核心能力查询，不能确认 Fake-IP 支持。
            </p>
          )}
          <div className="network-form">
            <label>
              待查询 IP 地址
              <textarea
                rows={3}
                maxLength={4096}
                value={fakeIpInput}
                onChange={(e) => setFakeIpInput(e.target.value)}
                placeholder="每行一个 IPv4 / IPv6，也可用逗号分隔"
              />
            </label>
          </div>
          <button
            disabled={
              !online ||
              !instance ||
              !coreBridge ||
              fakeIpBusy ||
              !fakeIpInput.trim()
            }
            onClick={() => void queryFakeIp()}
          >
            {fakeIpBusy ? "查询中…" : "查询 Fake-IP 映射"}
          </button>
          {fakeIpResult && (
            <>
              <p
                className={
                  fakeIpResult.status === "ok"
                    ? "network-tone-success"
                    : "network-tone-warning"
                }
                role="status"
              >
                Fake-IP 查询结果：{coreLabels[fakeIpResult.status]} · 采样{" "}
                {stamp(fakeIpResult.sampled_at_ms)}
              </p>
              {fakeIpResult.message && <p>{fakeIpResult.message}</p>}
              {advanced && fakeIpResult.boot_id && (
                <p>
                  查询时核心：<code>{fakeIpResult.boot_id}</code>
                </p>
              )}
              {fakeIpResult.boot_id &&
                core?.identity?.boot_id &&
                fakeIpResult.boot_id !== core.identity.boot_id && (
                  <p className="network-tone-warning" role="status">
                    核心已重启，以上映射属于此前采样；需要手动重新查询。
                  </p>
                )}
              {fakeIpResult.status === "ok" && (
                <div
                  className="table-scroll"
                  role="region"
                  aria-label="Fake-IP 映射结果"
                  tabIndex={0}
                >
                  <table>
                    <thead>
                      <tr>
                        <th>地址</th>
                        <th>映射域名</th>
                      </tr>
                    </thead>
                    <tbody>
                      {fakeIpResult.mappings.map((mapping, index) => (
                        <tr key={index}>
                          <td>{mapping.address}</td>
                          <td>
                            {mapping.domain ??
                              "未命中（不能据此判断是否为 Fake-IP）"}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </>
          )}
        </section>
      )}
      {section === "诊断" && (
        <section className="card card-pad">
          <h2>能力说明</h2>
          <p>
            所有主动工具仅在点击后执行；批量诊断和拓扑结果以目标 Agent
            报告为准。普通超时、私有地址、DNS AD
            标志或一次差异都不代表监听已发生，也不表示本机完成 DNSSEC 验证。
          </p>
        </section>
      )}
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
