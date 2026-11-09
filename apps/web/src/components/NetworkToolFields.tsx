import type { Snapshot } from "../generated/management";
import {
  defaultTarget,
  physicalInterfaces,
  tools,
  type Form,
  type ToolKind,
} from "./networkToolsModel";

export function EgressField({
  value,
  onChange,
  network,
  outbounds,
  disabled = false,
  label = "本次探测出口",
}: {
  value: string;
  onChange: (value: string) => void;
  network: Snapshot | null;
  outbounds: string[];
  disabled?: boolean;
  label?: string;
}) {
  return (
    <label>
      {label}
      <select value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="system">目标 Agent 系统路由</option>
        {network?.interfaces
          .filter((i) => i.connected && i.kind !== "loopback")
          .map((i) => (
            <option key={i.id} value={`iface:${i.id}`} disabled={disabled}>
              {i.name} ({i.id})
            </option>
          ))}
        {outbounds.length > 0 && (
          <optgroup label="配置出口 / 运行组">
            {outbounds.map((name) => (
              <option key={name} value={`outbound:${name}`}>
                {name}
              </option>
            ))}
          </optgroup>
        )}
      </select>
    </label>
  );
}
export default function NetworkToolFields({
  form,
  update,
  network,
  outbounds,
  supported,
  interfaceSupported,
}: {
  form: Form;
  update: (patch: Partial<Form>) => void;
  network: Snapshot | null;
  outbounds: string[];
  supported: boolean;
  interfaceSupported: boolean;
}) {
  const nat = form.kind === "nat";
  const trust = ["tls", "http", "https_ip"].includes(form.kind);
  return (
    <>
      <div className="network-form">
        <label>
          工具
          <select
            value={form.kind}
            onChange={(e) => {
              const kind = e.target.value as ToolKind;
              update({
                kind,
                target: defaultTarget(kind),
                natInterface: "",
                timeout: ["trace", "icmp"].includes(kind) ? 1000 : 3000,
                ...(kind === "ipv6" ? { family: "ipv6" as const } : {}),
              });
            }}
          >
            {tools.map(([kind, label]) => (
              <option key={kind} value={kind}>
                {label}
              </option>
            ))}
          </select>
        </label>
        {!nat && (
          <EgressField
            value={form.egress}
            onChange={(egress) => update({ egress })}
            network={network}
            outbounds={supported ? outbounds : []}
            disabled={supported && !interfaceSupported}
          />
        )}
        {supported && (
          <label>
            解析器（可选）
            <input
              value={form.resolver}
              onChange={(e) => update({ resolver: e.target.value })}
              placeholder="留空采用可确认的出口 DNS"
            />
          </label>
        )}
        {supported && (
          <label>
            地址族
            <select
              value={
                nat
                  ? form.family || "ipv4"
                  : form.kind === "ipv6"
                    ? "ipv6"
                    : form.family
              }
              disabled={form.kind === "ipv6"}
              onChange={(e) =>
                update({
                  family: e.target.value as Form["family"],
                  natInterface: "",
                })
              }
            >
              {!nat && <option value="">自动</option>}
              <option value="ipv4">IPv4</option>
              <option value="ipv6">IPv6</option>
            </select>
          </label>
        )}
        {!["nat", "stun"].includes(form.kind) && (
          <label>
            目标
            <input
              value={form.target}
              onChange={(e) => update({ target: e.target.value })}
            />
          </label>
        )}
        {form.kind === "tcp_ports" && (
          <label>
            端口（最多 16 个）
            <input
              value={form.ports}
              onChange={(e) => update({ ports: e.target.value })}
              placeholder="80,443,8080"
            />
          </label>
        )}
        {form.kind === "dns" && (
          <>
            <label>
              查询类型
              <select
                value={form.record}
                onChange={(e) => update({ record: e.target.value })}
              >
                {["A", "AAAA", "CNAME", "MX", "TXT", "NS", "HTTPS", "SOA"].map(
                  (r) => (
                    <option key={r}>{r}</option>
                  ),
                )}
              </select>
            </label>
            <label>
              DNS 解析器
              <input
                value={form.server}
                onChange={(e) => update({ server: e.target.value })}
                placeholder="udp:// / tcp:// / tls:// / https://"
              />
            </label>
          </>
        )}
        {form.kind === "tls" && (
          <label>
            TLS 名称（可选）
            <input
              value={form.serverName}
              onChange={(e) => update({ serverName: e.target.value })}
              placeholder="留空使用目标主机名"
            />
          </label>
        )}
        {form.kind === "http" && (
          <label>
            HTTP 方法
            <select
              value={form.method}
              onChange={(e) =>
                update({ method: e.target.value as Form["method"] })
              }
            >
              <option>HEAD</option>
              <option>GET</option>
            </select>
          </label>
        )}
        {form.kind === "stun" && (
          <label>
            STUN 服务
            <input
              value={form.stunServer}
              onChange={(e) => update({ stunServer: e.target.value })}
            />
          </label>
        )}
        {nat && (
          <>
            <label>
              NAT 物理网卡
              <select
                value={form.natInterface}
                onChange={(e) => update({ natInterface: e.target.value })}
              >
                <option value="">请选择物理网卡</option>
                {physicalInterfaces(network, form.family || "ipv4").map((i) => (
                  <option key={i.id} value={i.id}>
                    {i.name} ({i.id})
                  </option>
                ))}
              </select>
            </label>
            <label>
              NAT 测试服务（1–4 个）
              <textarea
                rows={3}
                value={form.natServers}
                onChange={(e) => update({ natServers: e.target.value })}
              />
            </label>
            <label>
              <input
                type="checkbox"
                checked={form.natAdvanced}
                onChange={(e) => update({ natAdvanced: e.target.checked })}
              />
              识别 NAT 类型（需要 RFC 5780 服务支持）
            </label>
            <p className="network-tone-info">
              类型识别需要支持 RFC 5780
              的服务；默认公共服务可能仅返回出口映射，证据不足时会显示“未能确定”。
            </p>
          </>
        )}
        {form.kind === "trace" && (
          <>
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
            <label>
              每跳次数
              <input
                type="number"
                min={1}
                max={3}
                value={form.probesPerHop}
                onChange={(e) =>
                  update({ probesPerHop: Number(e.target.value) })
                }
              />
            </label>
          </>
        )}
        {!["nat", "trace"].includes(form.kind) && (
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
        {!nat && (
          <label>
            每次超时（毫秒）
            <input
              type="number"
              min={100}
              max={30000}
              value={form.timeout}
              onChange={(e) => update({ timeout: Number(e.target.value) })}
            />
          </label>
        )}
      </div>
      {trust && (
        <label>
          <input
            type="checkbox"
            checked={form.compareBundled}
            onChange={(e) => update({ compareBundled: e.target.checked })}
          />
          同一证书链离线对照内置根证书（不额外联网）
        </label>
      )}
      {nat && (
        <>
          <p>
            将通过所选物理网卡直接访问测试服务，服务会看到该网络的公网出口
            IP。每次运行都需重新手选网卡；无法确认物理绑定时停止，不切换出口。
          </p>
          <p className="muted">
            总时限 30 秒，独占本 Agent
            的诊断调度器，其他主动诊断等待；不暂停用户流量。公共服务不保证支持高级检测，基础映射差异不直接判定
            NAT 类型。
          </p>
        </>
      )}
      {form.kind === "stun" && (
        <p>
          每次 STUN 是独立出口会话；显示单服务观察到的映射，不做 NAT
          分类或多服务器映射对照。UDP 代理仅支持已验证的 SOCKS5 组合。
        </p>
      )}
      {["trace", "icmp"].includes(form.kind) && (
        <p>
          无法排除 TUN 影响时，逐跳不可测；TUN 本地 ICMP
          应答不代表目标可达。需要物理路径时请主动选择物理网卡或已绑定的
          DIRECT。权限不足不自动提权，未响应不代表设备不存在。
        </p>
      )}
    </>
  );
}
