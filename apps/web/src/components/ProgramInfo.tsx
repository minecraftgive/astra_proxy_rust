import { useEffect, useState } from "react";
import * as API from "../api";
import type {
  FeatureSupport,
  ProgramInfo as ProgramInformation,
} from "../generated/management";

const unknown = "未知";
const featureText: Record<string, [string, string]> = {
  instances: ["实例管理", "管理代理、规则和服务端实例；核心程序需单独安装。"],
  documents: ["配置编辑", "支持配置字段、草稿和校验。"],
  monitor: ["监控", "进程指标、连接与日志；实时数据需要运行中的核心。"],
  diagnostics: ["网络诊断", "由核心提供诊断与路由解释。"],
  subscriptions: ["订阅管理", "管理订阅与规则数据。"],
  updates: ["更新管理", "提供更新计划与状态，执行受更新策略及安装方式约束。"],
  remote_management: ["远程管理", "控制器与节点通信需要配置凭据和网络连接。"],
  tun: ["TUN", "需要核心配置、权限及可用的 TUN 设备或驱动。"],
  system_proxy: ["系统代理", "支持情况取决于桌面环境及代理配置。"],
  tproxy: ["TProxy", "仅 Linux 支持，需要权限及防火墙路由规则。"],
  redirect: ["Redirect", "仅 Linux 支持，需要权限及防火墙重定向规则。"],
  local_ipc: ["本地通信", "私有本地管理通信通道。"],
};

function Indicator({
  label,
  value,
  enabled,
  detail,
}: {
  label: string;
  value: string;
  enabled?: boolean;
  detail?: string;
}) {
  return (
    <span
      className={`pill program-indicator ${enabled ? "ok" : ""}`}
      title={detail}
    >
      <span className={`dot ${enabled ? "ok" : ""}`} aria-hidden="true" />
      {label}
      <span className="program-indicator-value">{value}</span>
    </span>
  );
}

function Features({
  title,
  items,
}: {
  title: string;
  items: FeatureSupport[];
}) {
  return (
    <div className="program-feature-row">
      <h3>{title}</h3>
      <div className="program-indicators">
        {items.map((feature) => (
          <Indicator
            key={feature.id}
            label={featureText[feature.id]?.[0] ?? feature.label}
            value={feature.supported ? "支持" : "不支持"}
            enabled={feature.supported}
            detail={featureText[feature.id]?.[1] ?? feature.detail}
          />
        ))}
        {items.length === 0 && <span className="muted">未报告</span>}
      </div>
    </div>
  );
}

function Facts({ entries }: { entries: [string, string][] }) {
  return (
    <dl className="program-facts">
      {entries.map(([label, value]) => (
        <div key={label}>
          <dt>{label}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}

function compiledDate(value: string) {
  if (value === "unknown") return unknown;
  const date = new Date(value);
  if (Number.isNaN(date.valueOf())) return value;
  const utc = date.toLocaleString("zh-CN", { timeZone: "UTC", hour12: false });
  const local = date.toLocaleString("zh-CN", { hour12: false });
  const minutes = -date.getTimezoneOffset();
  const offset = `UTC${minutes < 0 ? "-" : "+"}${String(Math.floor(Math.abs(minutes) / 60)).padStart(2, "0")}:${String(Math.abs(minutes) % 60).padStart(2, "0")}`;
  return `${utc} UTC [${local} ${offset}]`;
}

export default function ProgramInfo({
  agent,
  nodeName,
  online,
  instance,
  runtime,
  onError,
}: {
  agent: string;
  nodeName?: string;
  online: boolean;
  instance?: API.Instance;
  runtime: Record<string, unknown> | null;
  onError: (error: unknown) => void;
}) {
  const [info, setInfo] = useState<ProgramInformation | null>(null);
  const [loading, setLoading] = useState(online);
  const [message, setMessage] = useState("");
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    if (!agent || !online) {
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    setLoading(true);
    setMessage("");
    void API.query<ProgramInformation>(
      agent,
      "program_info",
      undefined,
      controller.signal,
    )
      .then((reply) => {
        if (controller.signal.aborted) return;
        if (reply.type !== "program_info")
          throw new API.ApiError("unsupported", "节点未提供程序信息。");
        setInfo(reply.data);
      })
      .catch((error: unknown) => {
        if (controller.signal.aborted) return;
        if (error instanceof API.ApiError && error.code === "unauthorized") {
          onError(error);
        } else if (
          error instanceof API.ApiError &&
          ["unsupported", "invalid_request", "version_mismatch"].includes(
            error.code,
          )
        ) {
          setMessage("当前节点版本尚未提供程序信息，升级节点后可查看。");
        } else {
          setMessage(
            error instanceof Error
              ? error.message
              : "无法读取程序信息，请重试。",
          );
        }
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false);
      });
    return () => controller.abort();
  }, [agent, online, onError, revision]);

  // A retained runtime sample cannot describe a stopped or restarted instance.
  const running =
    online && instance?.state === "Running" && instance.pid != null;
  const currentRuntime =
    running && Number(runtime?.pid) === instance.pid ? runtime : null;
  const runtimeState = (key: string) => {
    const value = currentRuntime?.[key];
    return typeof value === "boolean"
      ? { value: value ? "已开启" : "已关闭", enabled: value }
      : {
          value: !online
            ? unknown
            : instance?.state === "Stopped"
              ? "未运行"
              : "未读取",
        };
  };
  const instanceState = !online
    ? unknown
    : ((
        {
          Running: "运行中",
          Stopped: "未运行",
          Unresponsive: "无响应",
          Unreachable: "不可达",
        } as Record<string, string>
      )[instance?.state ?? ""] ??
      instance?.state ??
      unknown);
  const unavailable = !agent
    ? "选择一个节点后查看程序信息。"
    : !online
      ? `节点离线。${info ? "以下为上次读取的程序信息。" : "连接后可读取程序信息。"}`
      : message
        ? `${message}${info ? " 以下为上次读取的程序信息。" : ""}`
        : loading && info
          ? "正在刷新，以下为上次读取的程序信息。"
          : "";

  return (
    <section
      className="card card-pad program-info"
      aria-labelledby="program-info-title"
    >
      <div className="program-info-heading">
        <div>
          <h2 id="program-info-title">程序信息</h2>
          <p className="muted">
            {(nodeName ?? agent) || "未选择节点"} · 受控节点的软件与系统信息
          </p>
        </div>
        <div className="program-info-actions">
          <Indicator
            label="节点"
            value={online ? "在线" : "离线"}
            enabled={online}
          />
          <button
            className="btn sm"
            disabled={!agent || !online || loading}
            onClick={() => setRevision((value) => value + 1)}
          >
            {loading ? "读取中…" : "刷新信息"}
          </button>
        </div>
      </div>
      {unavailable && (
        <p className="program-info-message muted" role="status">
          {unavailable}
        </p>
      )}
      {loading && !info && (
        <p className="muted" role="status">
          正在读取程序与系统信息…
        </p>
      )}
      {info && (
        <>
          <div className="program-info-columns">
            <div>
              <h3>软件与编译</h3>
              <Facts
                entries={[
                  ["程序", info.build.name],
                  ["版本", info.build.version],
                  ["编译日期", compiledDate(info.build.compiled_at)],
                  ["构建配置", info.build.profile],
                  ["编译目标", info.build.target],
                  ["编译器", info.build.rustc],
                  ["源码版本", info.build.git_revision ?? "未记录"],
                  [
                    "调试断言",
                    info.build.debug_assertions ? "已启用" : "未启用",
                  ],
                ]}
              />
            </div>
            <div>
              <h3>节点系统</h3>
              <Facts
                entries={[
                  [
                    "操作系统",
                    [info.system.os, info.system.os_version]
                      .filter(Boolean)
                      .join(" · "),
                  ],
                  ["架构", info.system.architecture],
                  ["系统家族", info.system.family],
                  [
                    "可用逻辑 CPU",
                    info.system.logical_cpus?.toString() ?? unknown,
                  ],
                  ["管理进程 PID", info.system.process_id.toString()],
                  [
                    "进程权限",
                    info.system.elevated == null
                      ? unknown
                      : info.system.elevated
                        ? "管理员 / root"
                        : "普通用户",
                  ],
                  [
                    "本地通信",
                    info.system.ipc_transport === "Named pipes"
                      ? "命名管道"
                      : info.system.ipc_transport,
                  ],
                  ["程序路径", info.system.executable ?? unknown],
                ]}
              />
            </div>
          </div>
          <div className="program-support">
            <Features title="编译功能" items={info.features} />
            <Features title="平台特性" items={info.platform_features} />
            <p className="muted">
              支持状态表示程序及目标平台具备该功能；实际启用状态见下方当前实例。
            </p>
          </div>
        </>
      )}
      <div className="program-runtime">
        <h3>当前实例{instance ? ` · ${instance.name}` : ""}</h3>
        {!instance ? (
          <p className="muted">选择实例后查看功能运行状态。</p>
        ) : (
          <div className="program-indicators">
            <Indicator label="核心" value={instanceState} enabled={running} />
            {instance.kind === "proxy" && (
              <>
                <Indicator label="系统代理" {...runtimeState("system_proxy")} />
                <Indicator label="TUN" {...runtimeState("tun")} />
                <Indicator label="允许局域网" {...runtimeState("allow_lan")} />
              </>
            )}
            {instance.pid != null && online && (
              <span className="pill mono">PID {instance.pid}</span>
            )}
          </div>
        )}
      </div>
    </section>
  );
}
