// 示例数据：节点、规则与配置文档模板。全部为占位值，不对应真实服务。
export const clone = (o) => JSON.parse(JSON.stringify(o));

export const NODES = [
  { name: "hk-01", type: "vless", server: "hk1.example.net", port: 443, password: "b831381d-6324-4d53-ad4f-8cda48b30811", sni: "www.microsoft.com", transport: "tcp", security: "reality" },
  { name: "hk-02", type: "trojan", server: "hk2.example.net", port: 443, password: "correct-horse-battery", sni: "hk2.example.net", transport: "tcp", security: "tls" },
  { name: "hk-03", type: "hysteria2", server: "hk3.example.net", port: 8443, password: "hy2-secret-sample", sni: "hk3.example.net", transport: "quic", security: "tls" },
  { name: "jp-01", type: "vmess", server: "jp1.example.net", port: 443, password: "a3482e88-686a-4a58-8126-99c9df64b7bf", sni: "jp1.example.net", transport: "ws", security: "tls" },
  { name: "jp-02", type: "tuic", server: "jp2.example.net", port: 443, password: "tuic-sample-pass", sni: "jp2.example.net", transport: "quic", security: "tls" },
  { name: "sg-01", type: "shadowsocks", server: "sg1.example.net", port: 8388, password: "ss2022-sample-key", sni: "", transport: "tcp", security: "none" },
  { name: "us-01", type: "vless", server: "us1.example.net", port: 443, password: "5f0e7c1a-1f0f-4b2a-9c1e-3c7c2f0d9a11", sni: "us1.example.net", transport: "xhttp", security: "tls" },
];

export const RULES = [
  { type: "domain_suffix", value: "ads.example.net", action: "block", sec: "域名" },
  { type: "process_name", value: "steam.exe", action: "direct", sec: "进程" },
  { type: "rule_set", value: "ads", action: "block", sec: "规则集引用", note: "12 条" },
  { type: "geosite", value: "cn", action: "direct", sec: "GEOSITE" },
  { type: "domain_keyword", value: "github", action: "proxy:自动-香港", sec: "域名" },
  { type: "geosite", value: "google", action: "proxy:PROXY", sec: "GEOSITE" },
  { type: "ip_cidr", value: "10.0.0.0/8", action: "direct", sec: "IP" },
  { type: "geoip", value: "CN", action: "direct", sec: "IP" },
];

export const GEOSITE = {
  cn: ["bilibili.com", "hdslb.com", "baidu.com", "qq.com", "163.com", ".cn"],
  google: ["google.com", "youtube.com", "ytimg.com", "gstatic.com", "googleapis.com", "googlevideo.com"],
};
export const ADSET = ["doubleclick.net", "adservice.google.com", "ads.example.org"];

export const HOSTS = [
  ["www.youtube.com", 443, "tcp", "chrome.exe"], ["i.ytimg.com", 443, "tcp", "chrome.exe"], ["rr3---sn-a5mekn6r.googlevideo.com", 443, "udp", "chrome.exe"],
  ["github.com", 443, "tcp", "git.exe"], ["api.github.com", 443, "tcp", "Code.exe"], ["www.bilibili.com", 443, "tcp", "msedge.exe"],
  ["i0.hdslb.com", 443, "tcp", "msedge.exe"], ["update.steampowered.com", 443, "tcp", "steam.exe"], ["open.spotify.com", 443, "tcp", "Spotify.exe"],
  ["fonts.gstatic.com", 443, "tcp", "chrome.exe"], ["cdn.jsdelivr.net", 443, "tcp", "chrome.exe"], ["doubleclick.net", 443, "tcp", "chrome.exe"],
  ["10.0.3.21", 445, "tcp", "explorer.exe"], ["www.baidu.com", 443, "tcp", "msedge.exe"], ["discord.com", 443, "tcp", "Discord.exe"],
];

export function makeDoc(over = {}) {
  return {
    general: { standalone: false, standalone_outbound: "", log_level: "info", watch_config: true },
    inbound: { listen: "127.0.0.1:7890", auth_user: "", auth_pass: "" },
    takeover: { tun_stack: "system", tun_auto_route: true, egress: "自动选择", bypass: "localhost;127.*;10.*" },
    nodes: clone(NODES),
    groups: [
      { name: "PROXY", mode: "select", members: ["自动-香港", "手动-日本", "main", "DIRECT"], builtin: true },
      { name: "自动-香港", mode: "auto", members: ["hk-01", "hk-02", "hk-03"] },
      { name: "手动-日本", mode: "select", members: ["jp-01", "jp-02"] },
      { name: "main", mode: "select", members: NODES.map((n) => n.name), sub: true },
    ],
    subs: [{ name: "main", url: "https://sub.example.net/api/v1/client/9f2c…", interval: 86400, join_proxy: true }],
    dns: { listen: "127.0.0.1:1053", upstream: "https://1.1.1.1/dns-query", fake_ip: true },
    ...over,
  };
}

export const BUILTIN_TAIL = [
  { name: "DIRECT", mode: "select", members: ["DIRECT"], builtin: true },
  { name: "REJECT", mode: "select", members: ["REJECT"], builtin: true },
];
