// 字段 Schema：对应设计中的 astra-app::document，TUI 与 GUI 共用同一份定义。
export const SCHEMA = {
  general: [
    { k: "general.standalone", l: "独立模式", t: "bool", h: "开启后跳过路由引擎，所有流量使用下方的默认出站。" },
    { k: "general.standalone_outbound", l: "默认出站", t: "ref", h: "独立模式下使用的节点或代理组。", show: (d) => d.general.standalone },
    { k: "general.log_level", l: "日志级别", t: "choice", c: ["error", "warn", "info", "route", "debug"], h: "ROUTE 记录每条连接的路由与字节数。" },
    { k: "general.watch_config", l: "监测配置文件", t: "bool", h: "文件在外部被修改时自动热重载。" },
  ],
  inbound: [
    { k: "inbound.listen", l: "混合监听（SOCKS5/HTTP）", t: "addr", h: "留空表示禁用。默认只监听本机。" },
    { k: "inbound.auth_user", l: "认证用户名", t: "text", h: "留空表示不需要认证。" },
    { k: "inbound.auth_pass", l: "认证密码", t: "secret" },
  ],
  takeover: [
    { k: "takeover.tun_stack", l: "TUN 协议栈", t: "choice", c: ["system", "gvisor", "mixed"] },
    { k: "takeover.tun_auto_route", l: "自动写入路由", t: "bool", h: "开启 TUN 时添加默认路由，关闭时恢复。" },
    { k: "takeover.egress", l: "出口网卡", t: "choice", c: ["自动选择", "以太网", "WLAN", "vEthernet (WSL)"], h: "选择具体网卡后不再自动跟随出口变化。" },
    { k: "takeover.bypass", l: "系统代理例外", t: "text", h: "以分号分隔，这些地址不经过系统代理。" },
  ],
  dns: [
    { k: "dns.listen", l: "DNS 监听", t: "addr" },
    { k: "dns.upstream", l: "上游", t: "text", h: "支持 UDP、TCP、DoH（https://）与 DoT（tls://）。" },
    { k: "dns.fake_ip", l: "Fake IP", t: "bool", h: "TUN 模式下用虚拟地址加速域名分流。" },
  ],
  node: [
    { k: "name", l: "名称", t: "text", req: true },
    { k: "type", l: "协议", t: "choice", c: ["vless", "vmess", "trojan", "shadowsocks", "hysteria2", "tuic", "socks5", "http"] },
    { k: "server", l: "服务器", t: "text", req: true },
    { k: "port", l: "端口", t: "port", req: true },
    { k: "password", l: "凭据（密码 / UUID）", t: "secret", req: true },
    { k: "security", l: "安全层", t: "choice", c: ["none", "tls", "reality"] },
    { k: "sni", l: "服务器名称（SNI）", t: "text", show: (n) => n.security !== "none" },
    { k: "transport", l: "传输", t: "choice", c: ["tcp", "ws", "grpc", "httpupgrade", "xhttp", "quic"] },
  ],
  sub: [
    { k: "name", l: "名称", t: "text", req: true },
    { k: "url", l: "订阅链接", t: "secret", req: true, h: "链接含凭据，默认隐藏；日志中也不会记录完整链接。" },
    { k: "interval", l: "更新间隔（秒）", t: "num", min: 600, max: 604800 },
    { k: "join_proxy", l: "加入 PROXY 组", t: "bool", h: "订阅的节点组成同名代理组；关闭后不出现在默认组中。" },
  ],
};

export const SECTIONS = [
  ["general", "常规"], ["inbound", "入站"], ["takeover", "系统接管"], ["nodes", "节点"], ["groups", "代理组"],
  ["subs", "订阅"], ["dns", "DNS"], ["rules", "规则"], ["advanced", "高级"],
];

export const RULE_TYPES = ["domain", "domain_suffix", "domain_keyword", "geosite", "ip_cidr", "geoip", "process_name", "rule_set", "network", "port"];

export const getP = (o, path) => path.split(".").reduce((a, k) => a?.[k], o);
export function setP(o, path, v) {
  const ks = path.split(".");
  const last = ks.pop();
  ks.reduce((a, k) => a[k], o)[last] = v;
}

export function specFor(path) {
  const parts = path.split(".");
  if (parts.length === 3) return SCHEMA[parts[0] === "nodes" ? "node" : "sub"].find((s) => s.k === parts[2]);
  return Object.values(SCHEMA).flat().find((s) => s.k === path);
}

export function fieldError(spec, v, draft, scope) {
  if (spec.req && (v === "" || v == null)) return "必填";
  if (spec.t === "port" && !(Number.isInteger(+v) && +v >= 1 && +v <= 65535)) return "端口需为 1–65535 的整数";
  if (spec.t === "num" && (v === "" || isNaN(+v) || +v < spec.min || +v > spec.max)) return `需为 ${spec.min}–${spec.max}`;
  if (spec.t === "addr" && v && !/^[\w.\-[\]:]+:\d{1,5}$/.test(v)) return "格式为 地址:端口，例如 127.0.0.1:7890";
  if (spec.k === "name" && scope === "nodes") {
    if (draft.nodes.filter((n) => n.name === v).length > 1) return "名称与其他节点重复";
    if (["PROXY", "DIRECT", "REJECT"].includes(v)) return "保留名称不能用于节点";
  }
  return "";
}

/** 草稿中所有错误：[分区, 字段路径]。 */
export function allErrors(draft) {
  const out = [];
  for (const sec of ["general", "inbound", "takeover", "dns"]) {
    for (const s of SCHEMA[sec]) {
      if (s.show && !s.show(draft)) continue;
      if (fieldError(s, getP(draft, s.k), draft)) out.push([sec, s.k]);
    }
  }
  draft.nodes.forEach((n, i) =>
    SCHEMA.node.forEach((s) => {
      if ((!s.show || s.show(n)) && fieldError(s, n[s.k], draft, "nodes")) out.push(["nodes", `nodes.${i}.${s.k}`]);
    }),
  );
  draft.subs.forEach((n, i) => SCHEMA.sub.forEach((s) => fieldError(s, n[s.k], draft) && out.push(["subs", `subs.${i}.${s.k}`])));
  return out;
}
