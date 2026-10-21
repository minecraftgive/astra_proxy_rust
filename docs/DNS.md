# DNS 与 Fake IP

DNS 核心位于独立的 `astra-dns` crate，负责报文、TTL 缓存、上游选择和域名映射；
`astra-proxy/src/dns` 负责监听和代理出站，网卡绑定继续复用平台适配层。
Windows、Linux、macOS 共用实现。

## 使用

在客户端配置中加入以下内容，或在 TUI 配置编辑器的 **DNS / Fake IP** 中设置：

```toml
[dns]
enable = true
listen = ["127.0.0.1:1053"] # 同时监听 UDP 和 TCP；空列表仅提供 TUN 内部接管
nameserver = ["https://1.1.1.1/dns-query", "tls://1.0.0.1:853"]
fallback = ["udp://8.8.8.8:53"]
bootstrap = ["1.1.1.1", "8.8.8.8"]
respect_rules = false
ipv6 = true
hijack = true
fake_ip = true
fake_ip_range = "198.18.0.0/16"
# fake_ip_range6 = "fdab::/64" # 配合 TUN 时还需配置 tun.ipv6_address
fake_ip_filter_mode = "blacklist"
fake_ip_filter = ["+.lan", "+.local", "localhost"]
cache_capacity = 4096
cache_max_ttl_secs = 3600
negative_ttl_secs = 60
query_timeout_ms = 3000
max_concurrent = 128
mapping_capacity = 8192
mapping_ttl_secs = 86400
fake_ip_ttl = 60

[dns.nameserver_policy]
"+.example.org" = ["udp://8.8.8.8:53"]
```

上游支持普通 IP / IP:端口、`udp://`、`tcp://`、`tls://`（DoT）和 `https://`（DoH）。
DoH 使用 DNS wire-format POST，支持 HTTP/2 ALPN 与 HTTP/1.1；TLS 校验证书和服务器名称，
不跳过验证。上游域名先通过 bootstrap 中的 IP 使用 UDP/53 引导解析，不依赖系统 DNS。
目前引导解析采用 A 记录，需要至少一个可达的 IPv4 引导解析器。

nameserver 按列表顺序尝试；超时、连接失败、无效响应、SERVFAIL 和 REFUSED 会切换下一个，
随后尝试 fallback。每个上游有 query_timeout_ms 的超时，整个失败切换有两倍该时间的截止。
UDP 响应标记 TC 时自动改用 TCP 重试。TTL 缓存包含有 SOA 的 NXDOMAIN / NODATA 负缓存；
返回缓存时更新事务 ID 并扣减 TTL。缓存还受固定 16 MiB 报文预算约束。

nameserver_policy 优先选择最长匹配：普通域名精确匹配，`+.example.org` 包含根域名与子域，
`*.example.org` 仅匹配子域。blacklist 表示匹配项使用真实解析，whitelist 表示只有匹配项使用 Fake IP。
节点服务器、订阅服务器及 DNS 上游域名自动排除在 Fake IP 之外。

respect_rules=false 时 DNS 上游使用物理直连出口；true 时上游连接按 DNS 服务器的域名/IP
参与现有路由和代理组选择。引导解析始终直连。TUN 接管全局路由时，节点和引导 DNS 的绕行仍需
按原有 TUN 配置正确设置，避免系统 DNS/节点解析的循环。

## TUN 行为

未配置 DNS 时，TUN DNS 仍作为普通 TCP/UDP 流量按规则转发。
启用 DNS 且 hijack=true 后，TUN 内目的端口 53 的 TCP/UDP 请求由本地 DNS 模块处理，
UDP 回包保持原 DNS 服务器地址。hijack=false 可继续采用普通转发。
应用自身的 DoH/DoT 流量不会被 53 端口接管识别。

Fake IP 的 A/AAAA 回答分别来自配置的地址池。IPv4 地址池限定在 198.18.0.0/15 内；
IPv6 地址池限定 ULA。保留地址池的网络地址、首个主机、末端地址及 TUN 的地址和子网端点。
没有配置 IPv6 地址池时，Fake IP 域名的 AAAA 返回空答案；HTTPS/SVCB 返回空答案，
避免实际 IP hints 绕过域名还原。

代理核心自动给 TUN 加入 Fake IP 地址池路由；TCP/UDP 在规则查询和出站连接前还原成域名。
代理出站保留域名，由远端解析；直连出站通过真实上游查询真实地址，绕开 Fake IP 合成。
UDP 回包恢复客户端原先使用的虚拟源地址。未知/过期 Fake IP 拒绝连接，不作为公网 IP 转发。

普通 DNS 的真实 A/AAAA 答案也记录 TTL 域名映射。共享 IP 对应多个域名时保留歧义，
不随意选择其中一个。TUN 对 80/443/8080/8443 端口的 TCP 可通过 HTTP Host 或 TLS SNI
补充域名（`tun.sniff=true`，默认开启）；读取上限 16 KiB，等待上限 200 ms，读取内容原样转发。
真实目标 IP 保持不变，域名和 IP/CIDR/GeoIP 规则按原有先后顺序共同匹配，避免重新解析后
改变 CDN 或地址族。Fake IP 则还原为域名连接；未知 Fake IP 可尝试从上述握手中还原。
未命中映射且未识别出域名的真实 IP 仍按 IP 规则处理，不能仅凭 IP 猜测域名。
QUIC、加密 ClientHello 和非上述端口不会通过此嗅探恢复域名，应使用本 DNS 的映射。

Fake IP 表有容量上限，满时返回 SERVFAIL，不提前回收仍有效的映射。配置热重载共享映射，
防止旧 DNS 缓存指向另一个域名。更改/关闭正在使用的 Fake IP 地址池需重启。
映射目前不跨进程重启持久化；重启后客户端应重新解析，必要时清理系统或浏览器 DNS 缓存。
本功能不自动更改系统/网卡 DNS；TUN 的 53 端口接管或显式设置本地监听地址均可使用。

仅运行 DNS 服务时可以直接使用 `configs/dns.example.toml`，不需要 Rule Core；
设置 respect_rules=true 或同时运行代理入站时，按正常客户端配置连接规则核心/独立代理组。

## Mihomo 导入

本地 YAML 和订阅节点导入均转换 dns：enable、listen、ipv6、respect-rules、nameserver、
fallback、default-nameserver（普通 IP UDP/53）、enhanced-mode、fake-ip-range、fake-ip-range6、
fake-ip-ttl、fake-ip-filter、blacklist/whitelist filter-mode、cache-size 和域名 nameserver-policy。
订阅首次合并仅在本地没有 dns 表时采用远端设置；以后保留本地设置，不覆盖手工调整。
删除本地 dns 表后再次更新可重新采用订阅 DNS。多个订阅按先成功导入的 DNS 设置为准。

不支持的字段明确提示；GeoSite/规则集 DNS policy、rule 模式 fake-ip-filter、带 #PROXY
等后缀的上游地址不能等价转换，会报告转换错误。fallback-filter 的 GeoIP/污染筛选尚未实现，
导入时提示 fallback 仅在上游失败时使用。hosts、Mihomo sniffer 配置导入和系统 DNS
自动设置不由此功能提供；TUN 域名识别使用本地 `tun.sniff` 设置。

DoH 报文语义参照 [RFC 8484](https://www.rfc-editor.org/rfc/rfc8484.html)；
Mihomo 字段参照[官方 DNS 配置](https://wiki.metacubex.one/config/dns/)。

## 验证范围

回归覆盖 Fake IP 稳定性、IPv4/IPv6、容量耗尽、热重载、过期/未知地址、缓存命中/过期、
域名策略、失败切换、错误事务 ID、过滤和节点域名保护；内存中 TUN TCP 完整握手和负载、
TUN DNS/UDP 回包还原、TCP/UDP 域名规则 IPC；本地 DoH/DoT TLS 交换及不可信证书拒绝。
这些验证不创建或修改用户当前的系统 TUN/网卡设置。
