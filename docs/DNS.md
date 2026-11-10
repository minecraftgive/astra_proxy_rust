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
fallback = ["tls://8.8.8.8:853"]
plaintext_bypass = false          # 默认：仅 DoH/DoT，绝不降级到明文
plaintext_bypass_domains = ["+.lan"] # 仅这些域名的 nameserver_policy 可使用明文上游
bootstrap = ["1.1.1.1", "8.8.8.8"]  # 仅 plaintext_bypass = true 时用于 UDP/53 引导
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
"+.example.org" = ["https://dns.google/dns-query"]
"+.lan" = ["udp://192.168.1.1:53"] # 已被 plaintext_bypass_domains 覆盖
```

## 加密强制（默认开启）

默认 `plaintext_bypass = false`，DNS 只使用 DoT（`tls://`）和 DoH（`https://`）：

- `nameserver` / `fallback` 中出现 `udp://`、`tcp://` 或裸 IP 时配置校验失败，错误信息指明
  对应条目和放行开关。
- `nameserver_policy` 的明文上游只有在其域名模式被 `plaintext_bypass_domains` 完整覆盖时才允许
  （`+.lan` 覆盖 `+.lan`、`*.x.lan`、`router.lan`；`*.lan` 不覆盖 `+.lan`，因为后者含 `lan` 本身）。
- 上游域名（如 `https://dns.google/dns-query`）通过 nameserver/fallback 中 **IP 直连的 DoH/DoT**
  上游加密引导解析，不再经 `bootstrap` 的明文 UDP/53；因此使用域名上游时至少需要一个 IP 直连
  加密上游（如 `https://1.1.1.1/dns-query`）。引导结果按 TTL 缓存，最长 5 分钟。
- 上游失败、超时或证书校验失败只会切换到下一个加密上游，不会回退到明文；运行时对明文上游另有
  兜底拦截（计数器 `dns.upstream.plaintext_blocked`）。
- `plaintext_bypass = true` 关闭以上检查，恢复明文上游和 `bootstrap` UDP/53 引导（不安全）。

### 本机解析（直连目标与节点域名）

代理核心自身的域名解析同样默认加密，不走系统解析器（getaddrinfo）：

- 直连出站的目标域名、节点服务器域名（Socks5/HTTP/Trojan/VLESS/TUIC 以及 meow 原生协议）
  都经核心安装的解析钩子交给 DNS 模块解析，结果进入同一 TTL 缓存。
- 未配置 `[dns]` 时使用默认加密上游建立一个只解析、不监听、不接管、不做 Fake IP 的内部解析器；
  `enable = false` 时同样按该表中的上游（及加密检查）解析，只是不提供 DNS 服务。
- `localhost` 与 `*.localhost` 直接返回回环地址，IP 字面量不查询。
- 命中 `plaintext_bypass_domains` 且没有对应 `nameserver_policy` 的域名交给系统解析器
  （可用于 `+.local` mDNS、公司内网分离 DNS 等）；有策略时按策略上游解析。
- `respect_rules = true` 时，若 DNS 上游经代理节点连接而解析该节点域名又需要 DNS，嵌套解析
  改为直连加密上游，不会回退到系统解析器。
- 只有 `plaintext_bypass = true` 且未启用 DNS 服务时，才恢复使用系统解析器。

注意：系统 hosts 文件与 mDNS 不经过加密解析；需要时把相应域名加入 `plaintext_bypass_domains`。
`--nettool` 诊断工具和服务端（astra-server-core）的出站解析不受此设置影响。

上游支持 `tls://`（DoT）和 `https://`（DoH）；放行明文后还支持普通 IP / IP:端口、`udp://`、`tcp://`。
DoH 使用 DNS wire-format POST，支持 HTTP/2 ALPN 与 HTTP/1.1；TLS 校验证书和服务器名称，
不跳过验证。上游域名默认经 IP 直连的加密上游引导解析（见上节），不依赖系统 DNS；
仅 plaintext_bypass=true 时改用 bootstrap 中的 IP 走 UDP/53。引导解析采用 A 记录，需要 IPv4 可达。

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

## 系统 DNS 接管（TUN 期间，默认开启）

启用 TUN、`[dns]` 的 `enable` 与 `hijack` 均为 true 时，核心在 TUN 启动后接管系统 DNS，
TUN 停止、配置改动或正常退出时恢复；开启实验性网络恢复时，异常退出后由恢复日志还原。

```toml
[dns]
system_takeover = true   # A：系统 DNS 指向 TUN 子网中的地址（默认 198.18.0.2）
route_system_dns = true  # B：把接管前的系统 DNS 服务器以 /32、/128 路由进 TUN
```

- **A（系统解析器指向 TUN）**：选取 TUN 子网中除 TUN 自身外的第一个地址（默认 `/30` 下为
  `198.18.0.2`），为它加一条进入 TUN 的主机路由，并从 Fake IP 地址池保留该地址。
  - macOS：对每个启用的网络服务执行 `networksetup -setdnsservers <服务> 198.18.0.2`，
    恢复原有手动 DNS 或 `Empty`（回到 DHCP），随后刷新 mDNSResponder 缓存。
  - Linux：要求 systemd-resolved，在 TUN 链路上设置 `resolvectl dns/domain ~./default-route`，
    恢复时 `resolvectl revert`；没有 resolvectl 时仅记录警告。
  - Windows：把 TUN 网卡的 IPv4 DNS 设为该地址（TUN 网卡跃点数最低），恢复为 DHCP。
    Windows 仍可能并行查询其他网卡，B 负责兜住发往原 DNS 服务器的查询。
- **B（原系统 DNS 服务器路由进 TUN）**：接管前读取系统 DNS 服务器（`scutil --dns`、
  systemd-resolved、`Get-DnsClientServerAddress`），为每个服务器加一条进入 TUN 的主机路由，
  即使应用直接向局域网 DNS（如 `192.168.1.53`）发包也会被 53 端口接管。以下地址跳过：
  物理网关（`tun.gateway`/`gateway6`，路由网关会破坏默认路由）、回环、链路本地、组播、
  TUN 子网、198.18.0.0/15、`tun.bypass` 中的网段，以及 TUN 没有 IPv6 地址时的 IPv6 服务器。
  **路由器本身就是 DNS 时（最常见）B 会跳过它**，此时依靠 A。
- **D（IPv6）**：配置了 `tun.ipv6_address` 且 `tun.routes` 中没有任何 IPv6 路由时，自动追加
  `::/1` 与 `8000::/1`，IPv6 流量（包括 IPv6 DNS）也进入 TUN；B 同时路由 IPv6 DNS 服务器。
  自定义了 IPv6 路由时保持不变。

### 不开 TUN 时接管（macOS，需显式开启）

```toml
[dns]
system_takeover_without_tun = true
```

TUN 关闭时，核心额外在 `0.0.0.0:53` 监听 DNS（普通用户在 macOS 上可以绑定通配地址的 53 端口，
但不能绑定 `127.0.0.1:53`），并把每个启用网络服务的 DNS 设为 `127.0.0.1`，退出或开启 TUN 时
恢复原值。这个隐式监听只应答本机回环来源，不受 `allow_lan` 影响；若在 `listen` 中显式写了
`0.0.0.0:53`，则按普通监听处理并遵循 LAN ACL。开启 TUN 时改用上面的 TUN 接管。
53 端口被占用时配置应用失败并报错；Linux/Windows 上该选项只记录警告，请改用 TUN。
默认关闭，因为它会占用 53 端口并修改系统设置。

接管失败（例如缺少 resolvectl、路由已存在）只记录警告，不影响 TUN 和 53 端口接管本身。
两个开关都设为 false 时完全不修改系统 DNS 和额外路由。应用自带的 DoH/DoT 无法改写，
可用规则阻断后让应用回退到系统 DNS。

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
未启用 TUN 且未开启 `system_takeover_without_tun` 时不修改系统/网卡 DNS。

仅运行 DNS 服务时可以直接使用 `configs/dns.example.toml`，不需要 Rule Core；
设置 respect_rules=true 或同时运行代理入站时，按正常客户端配置连接规则核心/独立代理组。

## Mihomo 导入

使用 `astra-convert --include-config` 完整转换时支持 dns：enable、listen、ipv6、respect-rules、nameserver、
fallback、default-nameserver（普通 IP UDP/53）、enhanced-mode、fake-ip-range、fake-ip-range6、
fake-ip-ttl、fake-ip-filter、blacklist/whitelist filter-mode、cache-size 和域名 nameserver-policy。
导入时明文上游（裸 IP、udp://、tcp://）会被丢弃并给出警告；nameserver 为空时使用默认加密上游，
仅剩明文上游的 nameserver-policy 条目整条移除；如需保留明文请导入后手动设置 plaintext_bypass。
converter 默认只转换节点，不导入 DNS；订阅更新同样不导入 DNS，即使设置了 `import_groups=true`。
本地已有的 dns 表（包括之前导入后保存的设置）保留不变；需要 DNS 时请手工配置或使用完整转换。

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
