import { useState, type ReactNode } from "react";
import { natConclusion } from "./NetworkConclusions";
type Row = Record<string, any>;
const value = (x: unknown) =>
  x == null ? "未知" : typeof x === "object" ? JSON.stringify(x) : String(x);
export function Facts({ rows }: { rows: [string, unknown][] }) {
  return (
    <dl className="network-facts">
      {rows.map(([name, item], i) => (
        <div key={i}>
          <dt>{name}</dt>
          <dd>{value(item)}</dd>
        </div>
      ))}
    </dl>
  );
}
function Table({
  label,
  headers,
  rows,
}: {
  label: string;
  headers: string[];
  rows: ReactNode[][];
}) {
  return (
    <div className="table-scroll" role="region" aria-label={label} tabIndex={0}>
      <table>
        <thead>
          <tr>
            {headers.map((h) => (
              <th key={h}>{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={i}>
              {row.map((cell, j) => (
                <td key={j}>{cell ?? "未知"}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
const outcome: Record<string, string> = {
  connected: "已连接",
  refused: "明确拒绝",
  timed_out: "连接超时",
  unreachable: "不可达",
  failed: "失败",
  reply: "收到应答",
  time_exceeded: "TTL / 跳数到期",
  no_response: "未响应（原因未知）",
  permission_denied: "权限不足",
  unavailable: "不可测",
  path_changed: "路径变化，已停止",
  tun_local: "TUN 本地处理，不代表目标可达",
  unconfirmed: "无法确认",
};
const behavior: Record<string, string> = {
  unknown: "未知（证据不足）",
  endpoint_independent: "端点无关",
  address_dependent: "地址相关",
  address_and_port_dependent: "地址与端口相关",
};
const comparison: Record<string, string> = {
  insufficient: "证据不足",
  consistent: "多个目标映射一致",
  different: "不同目标映射不同（不单独分类）",
  unstable: "同一目标映射变化，不稳定",
};
function Observation({ o }: { o: Row }) {
  return (
    <Table
      label="STUN 采样"
      headers={[
        "阶段 / 服务",
        "本地端点",
        "映射端点",
        "应答来源",
        "重试 / 丢弃",
        "耗时 ms",
        "结果",
      ]}
      rows={[
        [
          `${o.phase} / ${o.server}`,
          o.local_endpoint,
          o.mapped_endpoint,
          o.source,
          `${o.attempts} / ${o.discarded_packets}`,
          String(o.elapsed_ms),
          o.error ?? (o.response_received ? "收到有效响应" : "无有效响应"),
        ],
      ]}
    />
  );
}
export function ToolDetails({ event }: { event: Row }) {
  const d = event.detail ?? {};
  switch (event.event) {
    case "connection":
      return (
        <Facts
          rows={[
            [
              "连接对象",
              (
                {
                  business_target: "业务目标",
                  proxy_entry: "代理入口",
                  dns_service: "DNS 服务",
                } as Row
              )[event.purpose] ?? event.purpose,
            ],
            ["主机名", event.host],
            ["实际端点", event.address ?? "不可见，保持未知"],
            ["地址来源", event.address_source],
            ["网卡", event.interface],
            ["地址族", event.family],
          ]}
        />
      );
    case "context":
      return (
        <>
          <Facts
            rows={[
              ["实际出口", event.actual_egress],
              ["核心启动标识", event.running_core?.boot_id],
              ["配置摘要", event.config_digest],
              ["配置证据", event.config_observation],
              ["有效网卡", event.effective_interface],
              ["网卡来源", event.interface_source],
            ]}
          />
          <Table
            label="固定选择链"
            headers={["组", "模式", "固定成员", "来源", "网卡"]}
            rows={(event.selection_chain ?? []).map((hop: Row) => [
              hop.group,
              hop.mode,
              hop.selected,
              hop.source,
              hop.interface,
            ])}
          />
          <p>上下文是探测时配置证据，不单独证明已经发包或经过物理路径。</p>
        </>
      );
    case "tcp": {
      const r = event.result ?? {};
      return (
        <Table
          label="TCP 连接结果"
          headers={[
            "主机",
            "端口",
            "地址族",
            "结果",
            "连接 / 握手耗时 ms",
            "说明",
          ]}
          rows={[
            [
              r.host,
              String(r.port),
              r.family,
              outcome[r.outcome] ?? r.outcome,
              value(r.connect_ms),
              r.message,
            ],
          ]}
        />
      );
    }
    case "tls":
      if (d.kind === "trust")
        return (
          <>
            <Facts
              rows={[
                ["信任来源", d.source],
                ["已加载根证书", d.loaded_roots],
                ["验证策略", d.policy],
              ]}
            />
            <p>系统根证书快照验证，不等同于系统完整证书策略；不联网补链。</p>
            {d.load_errors?.length > 0 && (
              <p role="status">
                根证书加载问题：{d.load_errors.join("；")}
                {d.load_errors_truncated && "（已截断）"}
              </p>
            )}
          </>
        );
      if (d.kind === "handshake")
        return (
          <>
            <Facts
              rows={[
                ["TLS 名称", d.server_name],
                [
                  "完整 TLS 验证",
                  d.verified
                    ? "通过"
                    : d.verification_attempted
                      ? "失败"
                      : "未完成验证",
                ],
                [
                  "系统根证书链验证",
                  d.system_chain_verified === true
                    ? "通过"
                    : d.system_chain_verified === false
                      ? "失败"
                      : "未单独记录",
                ],
                ["验证错误", d.verification_error],
                ["握手错误", d.handshake_error],
                [
                  "协议 / 密码套件",
                  `${d.protocol ?? "未知"} / ${d.cipher ?? "未知"}`,
                ],
                ["ALPN", d.alpn],
                ["握手耗时 ms", d.handshake_ms],
                [
                  "收到 / 保留证书",
                  `${d.received_certificates} / ${d.retained_certificates}`,
                ],
              ]}
            />
            {!d.received_certificates && <p>握手尚未收到证书，无证书证据。</p>}
            {d.truncated && <p role="status">证书证据已截断。</p>}
            {d.bundled_comparison && (
              <p>
                同一链内置根证书离线对照：
                {d.bundled_comparison.verified ? "通过" : "失败"}；
                {d.bundled_comparison.error ?? "无额外网络请求"}
              </p>
            )}
          </>
        );
      if (d.kind === "certificate")
        return (
          <>
            <h3>证书 #{d.index}</h3>
            <Facts
              rows={[
                ["SHA-256 指纹", d.sha256],
                ["主题", d.subject],
                ["颁发者", d.issuer],
                ["有效期开始", d.not_before],
                ["有效期结束", d.not_after],
                ["名称 SAN", d.subject_alt_names],
                ["DER 字节数", d.der_bytes],
                ["解析错误", d.parse_error],
              ]}
            />
            {d.metadata_truncated && <p>证书元数据已截断。</p>}
          </>
        );
      if (d.kind === "certificate_der")
        return (
          <p>
            证书 #{d.index} 原始证据分块：偏移 {d.offset} / {d.total_bytes}{" "}
            字节。展开原始证据或导出报告查看。
          </p>
        );
      return null;
    case "http":
      if (d.kind === "response")
        return (
          <>
            <Facts
              rows={[
                ["服务", d.service],
                ["请求方法", d.method],
                ["状态码", d.status],
                ["协议", d.version],
                ["跳转序号", d.hop],
                ["头 / 预览字节数", `${d.header_bytes} / ${d.preview_bytes}`],
              ]}
            />
            <Table
              label="HTTP 阶段耗时"
              headers={[
                "解析",
                "传输连接",
                "代理握手",
                "TLS",
                "响应头",
                "正文",
                "总计",
              ]}
              rows={[
                [
                  d.timings?.dns_ms,
                  d.timings?.transport_connect_ms,
                  d.timings?.proxy_handshake_ms,
                  d.timings?.tls_ms,
                  d.timings?.response_headers_ms,
                  d.timings?.body_ms,
                  d.timings?.total_ms,
                ].map(value),
              ]}
            />
            <p>
              耗时单位 ms；代理握手可能包含远端解析。
              {d.body_truncated && "正文已截断。"}
              {d.headers_redacted && "敏感响应头已过滤。"}
              {d.preview_redacted && "预览已脱敏。"}
            </p>
          </>
        );
      if (d.kind === "headers")
        return (
          <Table
            label="HTTP 响应头"
            headers={["名称", "值"]}
            rows={(d.headers ?? []).map((h: Row) => [h.name, h.value])}
          />
        );
      if (d.kind === "preview")
        return (
          <div className="network-record" tabIndex={0}>
            <pre>{d.text}</pre>
          </div>
        );
      if (d.kind === "redirect")
        return (
          <Facts
            rows={[
              ["跳转", `${d.from} → ${d.to}`],
              ["是否跟随", d.followed ? "是" : "已停止"],
              ["原因", d.reason],
            ]}
          />
        );
      if (d.kind === "failure")
        return (
          <p role="status">
            HTTP {d.phase} 失败：{d.message}
          </p>
        );
      return null;
    case "public_ip":
      return (
        <Facts
          rows={[
            ["观察到的出口地址", event.address],
            ["观测协议 / 来源", event.source],
            ["测试服务", event.service],
          ]}
        />
      );
    case "stun":
      return (
        <>
          <p>{event.note ?? "STUN 单服务映射观察，不进行 NAT 分类。"}</p>
          <Facts
            rows={[
              ["探测出口", event.egress],
              ["代理独立会话", event.proxy_session],
            ]}
          />
          <Observation o={event.observation ?? {}} />
        </>
      );
    case "nat": {
      if (event.kind === "observation")
        return <Observation o={event.observation ?? {}} />;
      const r = event.report ?? {};
      return (
        <>
          <h3>NAT 分级检测</h3>
          <Facts
            rows={[
              ["物理网卡", `${r.interface_name} (${r.interface_id})`],
              ["地址族", r.family],
              ["本地端点", r.local_endpoint],
              [
                "物理绑定核验",
                r.physical_path?.binding_verified === true
                  ? "已核验"
                  : "未确认 / 已失效",
              ],
              [
                "到目标路由核验",
                r.physical_path?.route_verified === true
                  ? "已核验"
                  : "未确认 / 已失效",
              ],
              ["物理路径采样时间", r.physical_path?.sampled_at_ms],
              ["基础映射对照", comparison[r.comparison] ?? r.comparison],
              ["映射行为", behavior[r.mapping] ?? r.mapping],
              ["过滤行为", behavior[r.filtering] ?? r.filtering],
              ["类型识别结论", natConclusion(r).text],
              ["高级服务能力", r.advanced_capable ? "已确认" : "未确认"],
              ["总耗时 ms", r.elapsed_ms],
            ]}
          />
          {r.partial && <p role="status">部分结果，不能视为完整 NAT 分类。</p>}
          {r.unstable && (
            <p role="status">同一目标映射不稳定，分类结论不适用。</p>
          )}
          {(r.notes ?? []).map((n: string, i: number) => (
            <p key={i}>{n}</p>
          ))}
          <p>该结果附着于所选路径，不推断 NAT 设备数量。</p>
        </>
      );
    }
    case "trace": {
      const s = event.sample ?? {};
      return (
        <Table
          label="逐跳采样"
          headers={["跳数", "次数", "地址", "往返耗时 ms", "结果", "说明"]}
          rows={[
            [
              String(s.hop),
              String(s.probe),
              s.address ?? "*（未知）",
              s.outcome === "no_response" ? "—" : value(s.rtt_ms),
              outcome[s.outcome] ?? s.outcome,
              s.message,
            ],
          ]}
        />
      );
    }
    case "trace_summary":
      return (
        <>
          <Facts
            rows={[
              ["目标 / 地址族", `${event.target} / ${event.family}`],
              ["平台方法", event.capability?.method],
              ["测量能力", event.capability?.supported ? "可测" : "不可测"],
              ["权限要求", event.capability?.permission_required],
              ["发出探测数", event.probes_sent],
              ["到达目标", event.reached ? "已观察到" : "未确认"],
            ]}
          />
          <p>{event.message}</p>
          <p>{event.capability?.reason}</p>
          <p>
            未响应只表示没有收到可验证回应，不推断设备不存在、故障或被监听。
          </p>
        </>
      );
    case "echo": {
      const o = event.observation ?? {};
      return (
        <>
          <Facts
            rows={[
              ["目标 / 地址族", `${o.target} / ${o.family}`],
              ["结果", outcome[o.outcome] ?? o.outcome],
              ["应答来源", o.from],
              [
                "往返耗时 ms",
                o.tun_local || !o.proves_reachability ? "不计入统计" : o.rtt_ms,
              ],
              [
                "目标可达性",
                o.proves_reachability && !o.tun_local
                  ? "本次应答已确认"
                  : "无法据此确认",
              ],
            ]}
          />
          <p>{o.tun_local ? "TUN 本地处理，不代表目标可达。" : o.message}</p>
        </>
      );
    }
    case "project_status":
      return (
        <Facts
          rows={[
            ["项目", event.project_id],
            ["路径", event.path_id],
            ["轮次", event.round],
            ["状态", event.state],
            ["说明", event.message],
          ]}
        />
      );
    case "analysis":
      return <Analysis report={event.report ?? event.analysis ?? event} />;
    default:
      return null;
  }
}
function Evidence({ entries }: { entries: Row[] }) {
  return (
    <Table
      label="关联证据"
      headers={["项目", "路径", "轮次 / 序号", "采样时间", "来源"]}
      rows={entries.map((e) => [
        e.project_id,
        e.path_id,
        `${e.round} / ${e.event_index}`,
        new Date(Number(e.sampled_at_ms)).toLocaleTimeString(),
        e.source,
      ])}
    />
  );
}
export function Analysis({ report }: { report: Row }) {
  const [selected, setSelected] = useState<Row | null>(null);
  return (
    <>
      <h3>固定规则诊断 · 版本 {report.rules_version}</h3>
      <p>{report.comparison_status}</p>
      {(report.incomplete || report.truncated) && (
        <p role="status">证据不完整或已截断，保留部分结果。</p>
      )}
      <p>可信程度表示证据支持程度，严重程度表示影响；两者都不是被监控概率。</p>
      {(report.notes ?? []).map((n: string, i: number) => (
        <p key={i}>{n}</p>
      ))}
      {(report.findings ?? []).map((f: Row, i: number) => (
        <section
          key={i}
          className={`network-tone-${f.severity === "risk" ? "error" : f.severity === "needs_verification" ? "warning" : "info"}`}
        >
          <h4>{f.title}</h4>
          <p>
            可信程度：
            {({ high: "高", medium: "中" } as Row)[f.confidence] ??
              f.confidence}{" "}
            · 严重程度：
            {(
              {
                risk: "风险",
                needs_verification: "待核实",
                information: "信息",
              } as Row
            )[f.severity] ?? f.severity}
          </p>
          <p>{f.explanation}</p>
          <Evidence entries={f.evidence ?? []} />
          <p>其他解释：{f.alternatives?.join("；") || "未提供"}</p>
          <p>建议：{f.recommendation}</p>
        </section>
      ))}
      {(report.topology ?? []).map((path: Row, i: number) => (
        <section key={i}>
          <h4>路径 {path.path_id}</h4>
          {path.truncated && <p>路径证据已截断。</p>}
          {["dns", "connection", "trace"].map((section) => (
            <div key={section}>
              <h5>
                {
                  (
                    {
                      dns: "DNS 路径",
                      connection: "连接路径",
                      trace: "逐跳采样",
                    } as Row
                  )[section]
                }
              </h5>
              <div
                className="table-scroll"
                role="region"
                aria-label={`${path.path_id} ${section} 拓扑`}
                tabIndex={0}
              >
                <div
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: 12,
                    minWidth: "max-content",
                    padding: 8,
                  }}
                >
                  {(path.edges ?? [])
                    .filter((e: Row) => e.section === section)
                    .map((edge: Row, j: number) => (
                      <button
                        key={j}
                        onClick={() => setSelected(edge)}
                        title={edge.description}
                      >
                        {path.nodes?.find((n: Row) => n.id === edge.from)
                          ?.label ?? edge.from}{" "}
                        →{" "}
                        {path.nodes?.find((n: Row) => n.id === edge.to)
                          ?.label ?? edge.to}
                        <br />
                        {(
                          {
                            measured: "实测",
                            configuration: "配置推断",
                            unknown: "未知",
                          } as Row
                        )[edge.level] ?? "未知"}
                      </button>
                    ))}
                </div>
              </div>
            </div>
          ))}
          {(path.attachments ?? []).map((a: Row, j: number) => (
            <details key={j}>
              <summary>{a.label}</summary>
              <Evidence entries={a.evidence ?? []} />
            </details>
          ))}
        </section>
      ))}
      {selected && (
        <section aria-label="路径证据详情">
          <h4>路径证据详情</h4>
          <p>{selected.description}</p>
          <Evidence entries={selected.evidence ?? []} />
          <button onClick={() => setSelected(null)}>关闭详情</button>
        </section>
      )}
      {(report.visibility ?? []).length > 0 && (
        <Table
          label="潜在观察点"
          headers={["位置", "可见信息"]}
          rows={report.visibility.map((v: Row) => [v.location, v.information])}
        />
      )}
    </>
  );
}
