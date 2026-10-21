# TUN 域名与目标地址

TUN 接收到的是目标 IP。直接用纯 IP 查询规则，不能命中 `geosite cn` 等域名规则，
即使用域名进行手工查询时结果为 DIRECT，实际连接仍可能落到默认代理。

现在先使用本地 DNS 映射及 TCP HTTP Host/TLS SNI 补充域名，再携带真实目标 IP 一起
查询规则。域名与 IP/CIDR/GeoIP 规则保持原有的全局先后顺序，不强行改变用户规则优先级。
真实 IP 连接仍拨号到应用选中的原始 IP，避免域名还原后重新解析到不同 CDN。
TCP 与 UDP 使用相同的目标选择模块；Fake IP 连接则使用还原的域名拨号。

`tun.sniff` 默认 true，可在 TUI 的 TUN 设置中关闭。仅检查 TCP 80、443、8080、8443；
最多读取 16 KiB、等待 200 ms，随后把读取过的数据完整转发。未知协议及时结束识别。
QUIC、ECH 或其他端口需依赖本地 DNS 映射；未恢复域名时仍使用 IP 规则和默认动作。

SOCKS5/HTTP CONNECT 同样支持已知 Fake IP 的域名还原。未知/过期虚拟地址会明确报错，
不再直接转发到公网。多个代理同时运行或切换核心后，如应用保留旧 Fake IP，需重新解析。

路由日志记录 `destination_ip`；TUN 的 `phase="destination"` 另记录 `original`、`host`、
`dial`，可区分原始 IP、规则域名与实际连接地址。节点握手失败与分流结果分别诊断。

IPC 版本升级到 4。更新时请一起替换 astra-proxy、astra-rule、astra-tui、astra-convert，
避免新旧核心混用。回归覆盖规则顺序、GeoIP、显式代理本地监听地址隔离、Fake IP、
跨记录 TLS SNI、HTTP 读取/回放及内存 TUN TCP 完整握手；不会更改系统网卡或路由。
