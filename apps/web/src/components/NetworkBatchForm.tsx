import { useEffect, useState } from "react";
import type { NetworkBatchRequest, Snapshot } from "../generated/management";
import {
  buildRequest,
  physicalInterfaces,
  stunPresets,
  type Form,
  type ToolKind,
} from "./networkToolsModel";
import { EgressField } from "./NetworkToolFields";
const choices: [ToolKind, string][] = [
  ["dns", "DNS"],
  ["tcp", "TCP"],
  ["tls", "TLS / 证书"],
  ["http", "HTTP"],
  ["https_ip", "HTTPS 公网 IP"],
  ["stun", "UDP / STUN"],
  ["ipv6", "IPv6"],
  ["trace", "逐跳"],
  ["icmp", "ICMP"],
];
export default function NetworkBatchForm({
  agent,
  instance,
  network,
  outbounds,
  base,
  disabled,
  submitted,
  onRun,
}: {
  agent: string;
  instance: string;
  network: Snapshot | null;
  outbounds: string[];
  base: Form;
  disabled: boolean;
  submitted: number;
  onRun: (request: NetworkBatchRequest) => void;
}) {
  const [selected, setSelected] = useState<ToolKind[]>([
    "dns",
    "tcp",
    "tls",
    "http",
    "https_ip",
    "stun",
    "ipv6",
  ]);
  const [paths, setPaths] = useState([
    { egress: "system", resolver: "", family: "" as Form["family"] },
  ]);
  const [comparison, setComparison] = useState(false);
  const [offline, setOffline] = useState(false);
  const [nat, setNat] = useState(false);
  const [natInterface, setNatInterface] = useState("");
  const [natFamily, setNatFamily] = useState<"ipv4" | "ipv6">("ipv4");
  const [natAdvanced, setNatAdvanced] = useState(false);
  const [natServers, setNatServers] = useState(stunPresets);
  const [presets, setPresets] = useState({
    dns: "example.com",
    server: "udp://1.1.1.1:53",
    tcp: "example.com:443",
    tls: "example.com:443",
    http: "https://example.com/",
    https_ip: "https://api.ipify.org",
    stun: "stun.cloudflare.com:3478",
    ipv6: "example.com:443",
    trace: "example.com",
    icmp: "example.com",
  });
  const [error, setError] = useState("");
  useEffect(() => {
    setNatInterface("");
  }, [agent, instance, submitted]);
  useEffect(() => {
    setPaths([{ egress: "system", resolver: "", family: "" }]);
    setError("");
  }, [agent, instance]);
  const submit = () => {
    try {
      if (paths.length + (nat ? 1 : 0) > 3)
        throw new Error("每批最多 3 条路径；NAT 独立物理路径也占一条");
      if (!selected.length && !nat) throw new Error("请至少选择一个诊断项目");
      const projects = paths.flatMap((path, index) =>
        selected.map((kind) => ({
          id: `path${index + 1}-${kind}`,
          path_id: `path${index + 1}`,
          request: buildRequest(
            {
              ...base,
              ...path,
              kind,
              count: 1,
              timeout: kind === "trace" || kind === "icmp" ? 1000 : 3000,
              target: presets[kind as keyof typeof presets],
              server: presets.server,
              stunServer: presets.stun,
              record: "A",
              serverName: "",
              method: "HEAD",
              compareBundled: offline,
              maxHops: 30,
              probesPerHop: 3,
            },
            agent,
            network,
          ),
        })),
      );
      if (nat)
        projects.push({
          id: "nat",
          path_id: "nat",
          request: buildRequest(
            {
              ...base,
              kind: "nat",
              natInterface,
              natServers,
              natAdvanced,
              family: natFamily,
              resolver: paths[0]?.resolver ?? "",
            },
            agent,
            network,
          ),
        });
      setError("");
      onRun({ projects, comparison });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <section className="card card-pad">
      <h2>完整诊断</h2>
      <p>
        点击后才发送探测。整批最长 180 秒，包含排队；普通项目最多并发 4
        项。每个项目失败独立保留证据，恢复任务只查询原标识。
      </p>
      <fieldset>
        <legend>诊断项目</legend>
        <div className="network-form">
          {choices.map(([kind, label]) => (
            <div key={kind}>
              <label>
                <input
                  type="checkbox"
                  checked={selected.includes(kind)}
                  onChange={(e) =>
                    setSelected((old) =>
                      e.target.checked
                        ? [...old, kind]
                        : old.filter((x) => x !== kind),
                    )
                  }
                />
                {label}
              </label>
              <label>
                {label} 预设
                <input
                  aria-label={`${label} 预设`}
                  value={presets[kind as keyof typeof presets]}
                  onChange={(e) =>
                    setPresets((old) => ({ ...old, [kind]: e.target.value }))
                  }
                />
              </label>
            </div>
          ))}
          <label>
            DNS 服务预设
            <input
              value={presets.server}
              onChange={(e) =>
                setPresets((old) => ({ ...old, server: e.target.value }))
              }
            />
          </label>
        </div>
      </fieldset>
      <fieldset>
        <legend>测试路径（最多 3 条，含 NAT）</legend>
        {paths.map((path, index) => (
          <div className="network-form" key={index}>
            <EgressField
              label={`路径 ${index + 1} 出口`}
              value={path.egress}
              onChange={(egress) =>
                setPaths((old) =>
                  old.map((p, i) => (i === index ? { ...p, egress } : p)),
                )
              }
              network={network}
              outbounds={outbounds}
            />
            <label>
              路径 {index + 1} 解析器
              <input
                value={path.resolver}
                placeholder="留空采用可确认的出口 DNS"
                onChange={(e) =>
                  setPaths((old) =>
                    old.map((p, i) =>
                      i === index ? { ...p, resolver: e.target.value } : p,
                    ),
                  )
                }
              />
            </label>
            <label>
              路径 {index + 1} 地址族
              <select
                value={path.family}
                onChange={(e) =>
                  setPaths((old) =>
                    old.map((p, i) =>
                      i === index
                        ? { ...p, family: e.target.value as Form["family"] }
                        : p,
                    ),
                  )
                }
              >
                <option value="">自动</option>
                <option value="ipv4">IPv4</option>
                <option value="ipv6">IPv6</option>
              </select>
            </label>
            {paths.length > 1 && (
              <button
                onClick={() =>
                  setPaths((old) => old.filter((_, i) => i !== index))
                }
              >
                删除路径 {index + 1}
              </button>
            )}
          </div>
        ))}
        <button
          disabled={paths.length + (nat ? 1 : 0) >= 3}
          onClick={() =>
            setPaths((old) => [
              ...old,
              { egress: "system", resolver: "", family: "" },
            ])
          }
        >
          添加路径
        </button>
      </fieldset>
      <label>
        <input
          type="checkbox"
          checked={nat}
          onChange={(e) => {
            setNat(e.target.checked);
            setNatInterface("");
          }}
        />
        包含 NAT 检测（独立手选物理网卡，可取消）
      </label>
      {nat && (
        <>
          <div className="network-form">
            <label>
              批量 NAT 地址族
              <select
                value={natFamily}
                onChange={(e) => {
                  setNatFamily(e.target.value as "ipv4" | "ipv6");
                  setNatInterface("");
                }}
              >
                <option value="ipv4">IPv4</option>
                <option value="ipv6">IPv6</option>
              </select>
            </label>
            <label>
              批量 NAT 物理网卡
              <select
                value={natInterface}
                onChange={(e) => setNatInterface(e.target.value)}
              >
                <option value="">请选择物理网卡</option>
                {physicalInterfaces(network, natFamily).map((i) => (
                  <option key={i.id} value={i.id}>
                    {i.name} ({i.id})
                  </option>
                ))}
              </select>
            </label>
            <label>
              批量 NAT 服务
              <textarea
                rows={3}
                value={natServers}
                onChange={(e) => setNatServers(e.target.value)}
              />
            </label>
            <label>
              <input
                type="checkbox"
                checked={natAdvanced}
                onChange={(e) => setNatAdvanced(e.target.checked)}
              />
              批量 RFC 5780 高级检测
            </label>
          </div>
          <p>
            将通过所选物理网卡直接访问测试服务，服务会看到该网络的公网出口
            IP。NAT 最长 30 秒并独占 Agent 诊断调度，其他项目等待 NAT
            检测完成。不会继承通用出口网卡；提交后清空选择。
          </p>
        </>
      )}
      <p>
        <label>
          <input
            type="checkbox"
            checked={comparison}
            onChange={(e) => setComparison(e.target.checked)}
          />
          对照检测：为 DNS 和 TLS 增加第二轮外部请求
        </label>
      </p>
      <p>
        <label>
          <input
            type="checkbox"
            checked={offline}
            onChange={(e) => setOffline(e.target.checked)}
          />
          对同一证书链离线对照内置根证书（不增加外部请求）
        </label>
      </p>
      <button disabled={disabled} onClick={submit}>
        运行完整诊断
      </button>
      {error && <p role="alert">{error}</p>}
    </section>
  );
}
