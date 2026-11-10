import { useEffect, useMemo, useRef, useState } from "react";
import * as API from "../api";
import { usePolling } from "../usePolling";
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
import { liveChain } from "./networkChain";
import { natShortLabel, natType } from "./networkNat";
import { LiveChainView, ProbePathView } from "./NetworkPath";
import {
  NatTypeTable,
  ResultLines,
  latest,
  resultLines,
} from "./NetworkResult";
import {
  RECORDS,
  buildFullCheck,
  buildRequest,
  canonicalIp,
  defaultForm,
  defaultTarget,
  egressAllowed,
  parseEgress,
  physicalInterfaces,
  requireCapabilities,
  tools,
  type DiagnosticRequest,
  type Form,
  type ToolKind,
} from "./networkToolsModel";

type Row = Record<string, any>;
type Props = { agent: string; instance: string; online: boolean };
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
function selectionKey(runtime: Row | null, core: NetworkCoreReply | null) {
  return JSON.stringify({
    pid: runtime?.pid,
    mode: runtime?.mode,
    tun: runtime?.tun,
    groups: (runtime?.groups ?? [])
      .map((g: Row) => [g.name, g.selected])
      .sort(),
    boot: core?.identity?.boot_id ?? null,
  });
}
const coreLabels: Record<CoreQueryStatus, string> = {
  ok: "能力已确认",
  unconfirmed: "能力无法确认",
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
    (data.status === "ok" && !data.identity)
  )
    throw new API.ApiError("protocol_error", "核心身份响应的版本或实例不匹配");
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
function egressLabel(value: string): string {
  if (value === "system") return "系统路由";
  if (value.startsWith("iface:")) return `网卡 · ${value.slice(6)}`;
  if (value.startsWith("outbound:")) return `出站 · ${value.slice(9)}`;
  return value;
}
/** Page-level validation of a report page; malformed pages never enter state. */
function validatePage(page: NetworkReport, previous: string) {
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
  const before = sequence(previous);
  const after = sequence(page.cursor);
  let last = before;
  for (const item of page.items) {
    const id = sequence(item.id);
    if (id <= last || id > after)
      throw new Error("协议错误：报告结果重复、乱序或超出游标");
    last = id;
  }
  if (
    after < before ||
    (!page.expired && after !== last) ||
    (page.has_more && (after <= before || !page.items.length))
  )
    throw new Error("协议错误：报告分页未前进或游标不匹配");
}

export default function NetworkTools({ agent, instance, online }: Props) {
  const storageKey = `astra-network-job:${agent}:${instance}`;
  const [hello, setHello] = useState<Hello | null>(null);
  const [network, setNetwork] = useState<NetworkSnapshot | null>(null);
  const [runtime, setRuntime] = useState<Row | null>(null);
  const [core, setCore] = useState<NetworkCoreReply | null>(null);
  const [outbounds, setOutbounds] = useState<string[]>([]);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");
  const [form, setForm] = useState<Form>(defaultForm);
  const update = (patch: Partial<Form>) =>
    setForm((old) => ({ ...old, ...patch }));
  const [operation, setOperation] = useState(() => retained(storageKey));
  const [report, setReport] = useState<NetworkReport | null>(null);
  const [items, setItems] = useState<NetworkResult[]>([]);
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [startedWith, setStartedWith] = useState<string | null>(null);
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
  scope.current = `${agent}/${instance}`;
  const capabilities = hello?.capabilities ?? [];
  const supported = capabilities.includes("network_diagnostics_jobs_v1");
  const finished = terminal(report?.state) && !report?.has_more;
  const active = Boolean(operation && !finished);
  const stale =
    active &&
    startedWith !== null &&
    startedWith !== selectionKey(runtime, core);

  const egresses = [
    "system",
    ...(network?.interfaces ?? [])
      .filter((i) => i.connected && i.kind !== "loopback")
      .map((i) => `iface:${i.id}`),
    ...[
      ...new Set([
        "DIRECT",
        ...outbounds,
        ...(runtime?.groups ?? []).map((g: Row) => g.name),
      ]),
    ].map((name) => `outbound:${name}`),
  ];
  const allowed = egresses.filter((e) =>
    egressAllowed(form.kind, parseEgress(e)),
  );
  // A vanished selection stays visible and fails at submission; it is never
  // silently replaced by another path (the Rust client behaves the same way).
  const egressGone = !egresses.includes(form.egress);
  const egressOptions =
    egressGone && egressAllowed(form.kind, parseEgress(form.egress))
      ? [...allowed, form.egress]
      : allowed;
  const chain = liveChain(
    runtime,
    network as Row | null,
    parseEgress(form.egress),
  );
  const analysis = latest(items, (e) => e.event === "analysis");
  const natReport =
    latest(items, (e) => e.event === "nat" && e.kind === "report")?.report ??
    null;
  const lines = useMemo(() => resultLines(items), [items]);
  const physical = physicalInterfaces(network, form.natFamily);

  useEffect(() => {
    setHello(null);
    setNetwork(null);
    setRuntime(null);
    setCore(null);
    setOutbounds([]);
    setForm(defaultForm);
    setOperation(retained(storageKey));
    setReport(null);
    setItems([]);
    cursor.current = null;
    reportBytes.current = 0;
    cancellation.current = "";
    setError("");
    setNotice("");
    setBusy(false);
    setCancelling(false);
    setStartedWith(null);
    setFakeIpInput("");
    setFakeIpResult(null);
    return () => fakeIpController.current?.abort();
  }, [agent, instance, storageKey]);

  // Only a tool change can make the selected egress unusable for the tool.
  useEffect(() => {
    if (!egressAllowed(form.kind, parseEgress(form.egress)) && allowed.length)
      update({ egress: allowed[0] });
  }, [form.kind]);

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
            if (state.type !== "network_state" || state.data.agent_id !== agent)
              throw new Error("协议错误：网络状态响应不匹配");
            if (!signal.aborted) setNetwork(state.data);
          } catch (e) {
            if (!signal.aborted) {
              setNetwork(null);
              notices.push(
                `网络状态：${e instanceof Error ? e.message : fmt(e)}`,
              );
            }
          }
        } else setNetwork(null);
        if (instance) {
          try {
            const nodes = await API.query<string[]>(
              agent,
              "node_tags",
              { instance },
              signal,
            );
            if (nodes.type !== "node_tags" || !Array.isArray(nodes.data))
              throw new Error("配置出口目录响应无效");
            if (!signal.aborted)
              setOutbounds(nodes.data.filter((t) => typeof t === "string"));
          } catch (e) {
            if (!signal.aborted) setOutbounds([]);
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
          } catch {
            if (!signal.aborted) setRuntime(null);
          }
          if (response.data.capabilities.includes("core_network_v1")) {
            try {
              const identity = await readCore(agent, instance, signal);
              if (!signal.aborted) setCore(identity);
            } catch (e) {
              if (!signal.aborted) setCore(coreFailure(instance, e));
            }
          } else setCore(null);
        }
        if (!response.data.capabilities.includes("network_diagnostics_jobs_v1"))
          notices.push(
            "目标 Agent 版本过旧，不支持后台网络诊断；请更新后再使用网络工具。",
          );
        if (!signal.aborted) setNotice(notices.join("\n"));
      } catch (e) {
        if (!signal.aborted) {
          setHello(null);
          setNotice(e instanceof Error ? e.message : fmt(e));
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
          validatePage(page, cursor.current ?? "0");
          const bytes = page.items.reduce(
            (total, item) =>
              total + new TextEncoder().encode(JSON.stringify(item)).length,
            0,
          );
          if (reportBytes.current + bytes > 2 * 1024 * 1024)
            throw new Error(
              "报告超过 2 MiB 接收上限；已保留原任务和已接收结果",
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
            `报告暂不可读，任务标识已保留：${e instanceof Error ? e.message : fmt(e)}`,
          );
      }
    },
    online && supported && operation && !finished ? 750 : null,
    [agent, instance, operation, online],
  );

  const run = async () => {
    if (!agent || !instance || !online || !hello || busy || active) return;
    const generation = scope.current;
    const id = crypto.randomUUID();
    let batch: NetworkBatchRequest | null = null;
    let request: DiagnosticRequest | null = null;
    try {
      if (form.kind === "full_check")
        batch = buildFullCheck(form, agent, network);
      else request = buildRequest(form, agent, network);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return;
    }
    const requests = batch
      ? batch.projects.map((p) => p.request as DiagnosticRequest)
      : [request!];
    const missing = requests.some(({ egress }) => {
      if (egress.kind === "interface")
        return !network?.interfaces.some(
          (i) => i.id === egress.name && i.connected,
        );
      return (
        egress.kind === "outbound" &&
        !egresses.includes(`outbound:${egress.name}`)
      );
    });
    if (missing) {
      setError("所选出口已不可用，请重新选择。");
      return;
    }
    setBusy(true);
    setError("");
    let submitted = false;
    try {
      // Re-read the target's capabilities on every submission.
      const latestHello = await API.query<Hello>(agent, "hello");
      if (generation !== scope.current) return;
      if (latestHello.type !== "hello" || latestHello.data?.role !== "agent")
        throw new API.ApiError(
          "protocol_error",
          "协议错误：需要目标 Agent 的最新能力响应",
        );
      setHello(latestHello.data);
      requireCapabilities(
        requests,
        latestHello.data.capabilities,
        Boolean(batch),
      );
      setItems([]);
      setReport(null);
      cursor.current = null;
      reportBytes.current = 0;
      cancellation.current = "";
      setStartedWith(selectionKey(runtime, core));
      retain(storageKey, id);
      setOperation(id);
      submitted = true;
      if (requests.some((r) => r.probe.kind === "nat"))
        update({ natInterface: "" });
      const r = await API.request<Row>(agent, {
        type: "execute",
        data: {
          operation_id: id,
          command: {
            type: batch ? "network_diagnostics_batch" : "network_diagnostics",
            data: { instance, request: batch ?? request },
          },
        },
      });
      if (generation !== scope.current) return;
      if (r.type !== "task" || r.data.id !== id)
        throw new Error("协议错误：任务接受回执不匹配");
      refreshReport();
    } catch (e) {
      if (generation === scope.current) {
        setError(e instanceof Error ? e.message : fmt(e));
        // Only definite rejections drop the identity; unknown outcomes stay queryable.
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
      if (generation === scope.current) setBusy(false);
    }
  };

  const cancel = async () => {
    if (!operation || !report || terminal(report.state)) return;
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

  const queryFakeIp = async () => {
    if (!agent || !instance || !online || fakeIpBusy) return;
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
      addresses.some((a) => canonicalIp(a) === null)
    ) {
      setFakeIpResult(
        result(
          "invalid_request",
          "请填写 1–64 个有效 IPv4 / IPv6 地址；不接受域名。",
        ),
      );
      return;
    }
    if (!capabilities.includes("core_network_v1")) {
      setFakeIpResult(
        result(
          "unsupported",
          "目标 Agent 未提供核心能力查询；未发送映射查询。",
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
      const identity = await readCore(agent, instance, controller.signal);
      if (controller.signal.aborted) return;
      setCore(identity);
      const capability = fakeIpStatus(identity);
      if (capability !== "ok") {
        setFakeIpResult(
          result(
            capability,
            identity.message ??
              "目标核心未声明 fake_ip_lookup_v1；未发送映射查询。",
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

  const exportReport = () => {
    const data = JSON.stringify({
      report: report ? { ...report, items: [] } : null,
      items,
      chain,
    });
    if (new TextEncoder().encode(data).length > 2 * 1024 * 1024) {
      setError("报告超过 2 MiB，无法完整导出。");
      return;
    }
    const url = URL.createObjectURL(
      new Blob([data], { type: "application/json" }),
    );
    const link = document.createElement("a");
    link.href = url;
    link.download = `network-${operation || "report"}.json`;
    link.click();
    URL.revokeObjectURL(url);
  };

  const setKind = (kind: ToolKind) =>
    setForm((old) => ({
      ...old,
      kind,
      target: defaultTarget(kind, old.target),
      count: ["icmp", "tcp"].includes(kind)
        ? old.count === 1
          ? 4
          : old.count
        : 1,
      natInterface: "",
    }));
  const k = form.kind;
  const showEgress = !["nat", "fake_ip"].includes(k);
  const showTarget = !["nat", "fake_ip", "full_check"].includes(k);
  const showNat = k === "nat" || (k === "full_check" && form.includeNat);
  const canRun = online && supported && Boolean(instance) && !busy;

  return (
    <div className="network-tools">
      <LiveChainView
        chain={chain}
        nat={natReport ? natShortLabel(natType(natReport)) : undefined}
      />
      {notice && (
        <p className="network-tone-warning" role="status">
          {notice}
        </p>
      )}
      <div className="card card-pad network-form">
        <div className="network-kinds" role="radiogroup" aria-label="工具">
          {tools.map(([value, label]) => (
            <button
              key={value}
              type="button"
              role="radio"
              aria-checked={k === value}
              className={`btn sm${k === value ? " primary" : ""}`}
              onClick={() => setKind(value)}
            >
              {label}
            </button>
          ))}
        </div>
        <div className="network-fields">
          {showEgress && (
            <label>
              出口
              <select
                value={form.egress}
                onChange={(e) => update({ egress: e.target.value })}
              >
                {egressOptions.map((e) => (
                  <option key={e} value={e}>
                    {egressLabel(e)}
                    {e === form.egress && egressGone ? "（不可用）" : ""}
                  </option>
                ))}
              </select>
            </label>
          )}
          {showTarget && (
            <label className="network-field-wide">
              目标
              <input
                value={form.target}
                placeholder={
                  k === "tcp"
                    ? "host:443 或 host:80,443,8080（最多 16 个端口）"
                    : k === "http"
                      ? "https://example.com/"
                      : "主机名或 IP"
                }
                onChange={(e) => update({ target: e.target.value })}
                onKeyDown={(e) => e.key === "Enter" && void run()}
              />
            </label>
          )}
          {k === "dns" && (
            <>
              <label>
                记录类型
                <select
                  value={form.record}
                  onChange={(e) => update({ record: e.target.value })}
                >
                  {RECORDS.map((r) => (
                    <option key={r}>{r}</option>
                  ))}
                </select>
              </label>
              <label className="network-field-wide">
                DNS 服务器
                <input
                  value={form.server}
                  placeholder="udp://IP · tcp://IP · tls://名称#bootstrap=IP · https://名称/dns-query"
                  onChange={(e) => update({ server: e.target.value })}
                />
              </label>
            </>
          )}
          {["icmp", "tcp", "dns"].includes(k) && (
            <label>
              次数
              <input
                type="number"
                min={1}
                max={50}
                value={form.count}
                onChange={(e) => update({ count: Number(e.target.value) })}
              />
            </label>
          )}
          {k === "trace" && (
            <label>
              最大跳数
              <input
                type="number"
                min={1}
                max={30}
                value={form.maxHops}
                onChange={(e) => update({ maxHops: Number(e.target.value) })}
              />
            </label>
          )}
          {k === "http" && (
            <label>
              方法
              <select
                value={form.method}
                onChange={(e) =>
                  update({ method: e.target.value as Form["method"] })
                }
              >
                <option>GET</option>
                <option>HEAD</option>
              </select>
            </label>
          )}
          {k === "full_check" && (
            <label>
              对照出口
              <select
                value={form.comparison}
                onChange={(e) => update({ comparison: e.target.value })}
              >
                <option value="">不对照</option>
                {egresses
                  .filter((e) => e !== form.egress)
                  .map((e) => (
                    <option key={e} value={e}>
                      {egressLabel(e)}
                    </option>
                  ))}
              </select>
            </label>
          )}
          {["tcp", "http", "https_ip", "full_check"].includes(k) && (
            <label className="network-field-wide">
              解析器（可选）
              <input
                value={form.resolver}
                placeholder="tcp://IP:53 · https://名称/dns-query#bootstrap=IP"
                onChange={(e) => update({ resolver: e.target.value })}
              />
            </label>
          )}
          {k === "full_check" && (
            <div className="network-checks">
              <label>
                <input
                  type="checkbox"
                  checked={form.compareRounds}
                  onChange={(e) => update({ compareRounds: e.target.checked })}
                />
                两轮对照
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={form.dualStack}
                  onChange={(e) => update({ dualStack: e.target.checked })}
                />
                IPv4/IPv6 双栈
              </label>
              <label>
                <input
                  type="checkbox"
                  checked={form.includeNat}
                  onChange={(e) =>
                    update({ includeNat: e.target.checked, natInterface: "" })
                  }
                />
                包含 NAT 检测
              </label>
            </div>
          )}
          {showNat && (
            <>
              <label>
                物理网卡
                <select
                  value={form.natInterface}
                  onChange={(e) => update({ natInterface: e.target.value })}
                >
                  <option value="">请选择物理网卡</option>
                  {physical.map((i) => (
                    <option key={i.id} value={i.id}>
                      {i.name} · {i.id}
                    </option>
                  ))}
                </select>
              </label>
              <label>
                地址族
                <select
                  value={form.natFamily}
                  onChange={(e) =>
                    update({
                      natFamily: e.target.value as Form["natFamily"],
                      natInterface: "",
                    })
                  }
                >
                  <option value="ipv4">IPv4</option>
                  <option value="ipv6">IPv6</option>
                </select>
              </label>
              {k === "nat" && (
                <>
                  <label className="network-field-wide">
                    STUN 服务
                    <input
                      value={form.natServers}
                      onChange={(e) => update({ natServers: e.target.value })}
                    />
                  </label>
                  <label>
                    <input
                      type="checkbox"
                      checked={form.natAdvanced}
                      onChange={(e) =>
                        update({ natAdvanced: e.target.checked })
                      }
                    />
                    类型识别
                  </label>
                </>
              )}
              <p className="muted network-field-wide">
                NAT 检测通过手选的物理网卡直连 STUN
                服务，服务会看到该网络的公网出口 IP。
              </p>
            </>
          )}
          {k === "fake_ip" && (
            <label className="network-field-wide">
              IP 地址
              <textarea
                rows={2}
                value={fakeIpInput}
                placeholder="最多 64 个 IP，空格或逗号分隔"
                onChange={(e) => setFakeIpInput(e.target.value)}
              />
            </label>
          )}
        </div>
        <div className="network-actions">
          {k === "fake_ip" ? (
            <button
              type="button"
              className="btn primary"
              disabled={!online || !instance || fakeIpBusy}
              onClick={() => void queryFakeIp()}
            >
              {fakeIpBusy ? "查询中…" : "查询映射"}
            </button>
          ) : active ? (
            <button
              type="button"
              className="btn"
              disabled={cancelling}
              onClick={() => void cancel()}
            >
              {cancelling ? "正在取消…" : "取消"}
            </button>
          ) : (
            <button
              type="button"
              className="btn primary"
              disabled={!canRun}
              onClick={() => void run()}
            >
              {busy ? "提交中…" : "运行"}
            </button>
          )}
          <button
            type="button"
            className="btn"
            disabled={!items.length}
            onClick={exportReport}
          >
            导出 JSON
          </button>
        </div>
      </div>
      {error && (
        <p className="network-tone-error" role="alert">
          {error}
        </p>
      )}
      {stale && (
        <p className="network-tone-warning">
          实例选择已变化；本次结果仍对应启动时的出口。
        </p>
      )}
      {k === "fake_ip"
        ? fakeIpResult && (
            <section className="card card-pad" aria-label="Fake-IP 结果">
              <p
                className={
                  fakeIpResult.status === "ok"
                    ? "network-tone-success"
                    : "network-tone-warning"
                }
              >
                {coreLabels[fakeIpResult.status]}
                {fakeIpResult.message ? ` · ${fakeIpResult.message}` : ""}
              </p>
              {fakeIpResult.mappings.map((m, i) => (
                <p
                  key={i}
                  className={
                    m.domain ? "network-tone-success" : "network-tone-muted"
                  }
                >
                  {m.address} → {m.domain ?? "未查到映射，无法判断用途"}
                </p>
              ))}
            </section>
          )
        : (report || items.length > 0) && (
            <section className="card card-pad network-result" aria-label="结果">
              {report && (
                <p
                  className={
                    report.state === "complete"
                      ? "network-tone-info"
                      : terminal(report.state)
                        ? "network-tone-warning"
                        : "network-tone-info"
                  }
                >
                  {states[report.state]}
                  {terminal(report.state) && report.message
                    ? ` · ${report.message}`
                    : ""}
                  {(report.truncated || report.expired) && " · 报告不完整"}
                </p>
              )}
              <ProbePathView
                analysis={analysis}
                items={items}
                operation={operation}
              />
              {natReport && <NatTypeTable report={natReport} />}
              <ResultLines lines={lines} />
            </section>
          )}
    </div>
  );
}
