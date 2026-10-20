# Mihomo 配置转换工具

`astra-convert` 是独立的原生 Rust 命令行工具和 Rust library，将本地 mihomo YAML 转为当前 Astra 的 `proxy.toml`、`rule.toml`。运行时不需要 Python、mihomo 或代理核心；转换阶段不会下载订阅、规则集或连接节点；规则 core 启动时加载文件/HTTP(S) 资源并按 interval 更新。只编译此工具也不需要代理协议库、CMake 或 libclang。

```sh
cargo build --release -p astra-convert
target/release/astra-convert configs/mihomo-rulesets-example.yaml -o converted
# 指定代理组成员；多个代理组可重复 --select：
target/release/astra-convert your-mihomo.yaml -o your-config --select 'Proxy=节点名称'
# 只解析检查，不创建输出目录：
target/release/astra-convert your-mihomo.yaml --check
# 标准输入：
cat your-mihomo.yaml | target/release/astra-convert - -o converted-stdin
```

未指定 `-o/--output-dir` 时，`proxy.toml` 和 `rule.toml` 输出到输入 YAML 所在目录；输入为 `-`（stdin）时输出到当前工作目录。`--base-dir` 仅影响资源路径解析，不改变输出位置。已有文件不会覆盖，Unix 下新文件权限为 `0600`。两份配置默认共用 `/tmp/astra-rule.sock`，可用 `--rule-socket /path/to/rule.sock` 修改。多实例应使用不同的 socket 和监听端口。

转换后用核心检查实际协议参数，再启动：

```sh
target/release/astra-rule --check converted/rule.toml
target/release/astra-proxy --check converted/proxy.toml
# 两个终端分别运行：
target/release/astra-rule converted/rule.toml
target/release/astra-proxy converted/proxy.toml
```

转换器的 `--check` 只检查解析和映射，并输出跳过摘要；核心的 `--check` 进一步验证协议和加密参数。这些检查均不能证明节点在线、凭据有效或服务端互通。

跳过节点、代理组、规则和规则集的提示附带输入 YAML 中条目的原始行号。CLI 以 `── 完整输入路径 ──` 作为分隔，只显示一次源文件路径；下面逐条显示 `提示: [第 42 行] 跳过节点 example: 不支持的协议 ssr`。行号从 1 开始，按原文计算，保留注释、空行、CRLF、行内列表及锚点引用的位置；多行字符串定位到内容起始行。独立条目使用别名时定位到引用行，整个列表通过合并/别名继承时定位到定义中的条目。标准输入以 `── <stdin> ──` 分隔。`--strict` 的规则/规则集错误也带行号；TUI 导入使用同样的提示。

## 转换范围

| Mihomo | Astra |
|---|---|
| `socks-port` | `socks5_listen` |
| `port` | `http_listen`，仅 HTTP CONNECT |
| `mixed-port` | 没有 `socks-port` 时作为 SOCKS5 端口；不生成混合监听器 |
| `allow-lan`、`bind-address` | IPv4/IPv6 监听地址；默认 loopback |
| `ss` | native shadowsocks；SS plugin 不支持，跳过该节点 |
| `vmess` | native vmess；仅 alterId 0，支持 auto/aes-128-gcm/chacha20-poly1305 |
| `vless` | native vless；UUID、flow、encryption |
| `trojan` | native trojan，启用 TLS |
| `hysteria2` | native hysteria2；password、Salamander、证书 pin |
| `tuic` | native TUIC v5；UUID、password、UDP 模式、拥塞控制 |
| `socks5`、`http` | 对应上游；可选用户名/密码；上游 TLS 不支持，跳过该节点 |
| `tls`、`servername`/`sni` | TLS 和服务器名称 |
| `client-fingerprint` | TLS ClientHello fingerprint |
| `fingerprint` | 证书 SHA-256 pin；Hysteria2 使用 `pin_sha256`，普通 TLS 使用 `cert_pin_sha256`；TUIC 忽略 |
| `reality-opts` | public-key、short-id |
| `network` | tcp、ws、grpc、httpupgrade/http-upgrade、xhttp |
| `ws-opts`、`grpc-opts`、`xhttp-opts` | path、host、headers、grpc-service-name、mode 等已有字段 |
| `ws-opts.v2ray-http-upgrade` | http_upgrade |
| `smux` | VLESS/VMess 的 enabled、protocol |
| `DOMAIN`、`DOMAIN-SUFFIX`、`DOMAIN-KEYWORD` | domain_exact、domain_suffix、domain_keyword |
| `GEOSITE` | geosite（标准 geosite.dat，支持 @属性/@!属性） |
| `DOMAIN-REGEX` | domain_regex（Rust regex 支持的语法） |
| `GEOIP` | geoip（MMDB / geoip.dat，支持国家、LAN 和 telegram 等分类） |
| `PROCESS-NAME`、`PROCESS-PATH` | process_name、process_path |
| `RULE-SET`、`rule-providers` | 保留命名引用和 inline/file/http 规则集定义 |
| `IP-CIDR`、`IP-CIDR6` | ip_cidr；接受 no-resolve |
| `DST-PORT`、`NETWORK` | 单个目标端口、tcp/udp |
| `MATCH` | default_action；其后的规则不再转换 |
| `DIRECT`、`REJECT`、`REJECT-DROP` | direct、block；不保留 REJECT-DROP 的静默丢包差异 |

支持 YAML anchors 和合并键。数字形式的端口、密码和 short-id 按解析后的标量转换；带前导零的 short-id 等必须在源 YAML 中加引号。

`skip-cert-verify` 忽略且输出提示，Astra 仍验证证书。不转换 CA、客户端证书、ECH、自定义 ALPN 等未映射 TLS 项；这些节点可能需要手工调整。入站认证无法映射，存在 `authentication` 时将监听限制为 `127.0.0.1`。

## 代理组和跳过策略

`select` 和 `url-test` 保留为原生 `proxy_groups`，对应手动模式 `select` 和自动测速 `auto`。保留成员、有效的嵌套引用、url、interval、timeout、tolerance；`include-all-proxies`、`filter`、`exclude-filter` 映射为动态节点规则。`--select GROUP=MEMBER` 必须指定直接成员，并将该组改为手动选择此成员；不存在或不可转换的选择报错。原生组上限 128 个、总计 4096 个成员、嵌套最多 32 层，不允许循环。无法保留的组会带源行号提示；`fallback`、`load-balance` 延用首个可转换成员的固定动作，并提示没有保留其故障转移或负载均衡语义。链式 `relay` 组忽略。

转换结果始终定义 `PROXY` 组，源配置未定义同名组时，默认动态包含所有已转换节点。所有代理动作（包括 MATCH/默认代理动作及 RULE-SET 的代理动作）统一写成 `proxy:PROXY`；明确的 DIRECT、REJECT 保持原动作，规则顺序不变。保留下来的其他组可在 PROXY 的 members 中引用，或手动调整原生规则指向这些组。`--select` 修改对应保留组的选择；该组被 PROXY 引用时才参与统一代理路径。节点使用保留名称 PROXY 时转换报错，需先改名。

`mode: rule` 按源顺序转换规则；未知策略、不支持的规则、引用被跳过节点的规则会跳过。没有可用 `MATCH` 时默认 `block`。`mode: direct` 使用默认直连并忽略规则；`mode: global` 使用源配置中显式声明的 `GLOBAL` 组，无可用组时默认 `block`。

没有对应实现的字段忽略，包括 DNS/Fake-IP、sniffer、proxy-provider、面板及控制 API。TUN、重定向、透明代理及自定义 listeners 也不自动映射：双方的路由和系统设置语义不同。没有可转换监听器时补充 `127.0.0.1:1080` SOCKS5 并提示。远程代理 provider 不展开，节点必须已经位于 `proxies` 中。规则 provider 由规则 core 在运行时加载。

跳过信息输出到标准错误；摘要不保证枚举每个未映射字段，首条提示说明其他字段被忽略。语法错误、顶层数组格式错误、重复名称、端口冲突、错误的 `--select` 会失败；单个节点缺少必要字段会跳过该节点。转换结果是功能子集，忽略规则及字段会改变源配置的路由行为，应检查摘要及输出后再使用。

源字段参考：[Mihomo TLS](https://wiki.metacubex.one/config/proxies/tls/)、[传输层](https://wiki.metacubex.one/config/proxies/transport/)、[TUIC](https://wiki.metacubex.one/config/proxies/tuic/)。

## 开发验证

```sh
cargo test -p astra-convert
```

测试覆盖节点和传输映射、规则顺序、未知项跳过、代理组循环/选择、YAML 合并、模式、认证监听限制、错误输入、文件权限及拒绝覆盖。

## GeoIP 与规则集

```sh
astra-convert mihomo.yaml -o converted --geoip-db /path/to/Country.mmdb --strict
```

`GEOIP` 支持 MMDB 和标准 V2Ray `geoip.dat`，按文件内容识别格式。MMDB 支持 GeoIP2/GeoLite2 国家记录、分类字符串和分类列表；`geoip.dat` 支持 CN、telegram 等命名分类、IPv4/IPv6 CIDR 和数据库的 inverse_match。分类不区分大小写，必须存在于所用数据库；DAT 缺少引用分类会拒绝加载，并在更新时保留有效缓存和当前规则。

`--geoip-db` 指定本地数据库并禁用源配置的 GeoIP 下载地址。未指定时，`geodata-mode: true` 使用 `geox-url.geoip`（DAT），否则使用 `geox-url.mmdb`；缓存放在输出目录中的 `geoip/`。没有提供下载地址的自定义分类规则会优先读取源 YAML 旁的 `geoip.dat`，缺少文件时使用 MetaCubeX 默认 DAT 下载源；普通国家规则继续使用源 YAML 旁的 `Country.mmdb`。转换离线进行，数据库在规则核心校验或启动时加载。MRS 规则集暂不支持。

支持 `rule-providers` 的 `inline`、`file`、`http`；`behavior` 为 `classical`、`domain`、`ipcidr`，`format` 为 `yaml`、`text`。本地文件路径按输入 YAML 的目录解析并输出绝对路径；HTTP 缓存放在输出规则文件旁的 `rule-cache/`，保留 URL 和更新间隔。stdin 可用 `--base-dir DIR` 指定本地资源目录。规则集文件不会在转换时下载或复制；迁移设备时需同时迁移本地文件和数据库并调整路径。

`domain` 支持精确域名、`+.example.com`（含根域）、`.example.com`（仅子域）以及按域名层级的 `*`。`classical` 支持当前核心可表示的 DOMAIN/DOMAIN-SUFFIX/DOMAIN-KEYWORD、IP-CIDR/IP-CIDR6、DST-PORT、NETWORK、GEOIP、PROCESS-NAME/PROCESS-PATH；未知条目会使该规则集加载失败，保留运行中的旧规则。RULE-SET 引用按原位置和动作展开，可被前面的规则覆盖。

引用缺失或不支持的 provider 会导致转换失败。`--strict` 还会拒绝其他不支持的规则/规则集，默认模式保留原有的跳过提示。provider 的自定义 header、proxy 和 path-in-bundle 暂不映射，会给出提示；下载直连并验证 TLS，不应用源配置的下载代理。

转换后的规则集可在 TUI 中展开编辑、重命名并同步引用、启停；实际命中可在运行中的规则实例上按 `t` 查询。classical 内的无效正则在转换内联列表或加载文件时即拒绝，文件错误保留条目序号，text 文件还显示原始行号。

转换结果设置 `direct_local=false`，使 Mihomo 的显式规则与 MATCH 默认动作顺序保持一致；需要本地兜底直连时可手动开启。所有支持的规则（包括 DOMAIN、DOMAIN-SUFFIX 和 classical 规则集条目）均接受末尾的 `no-resolve`，不会因此跳过。核心匹配仅使用请求已有的域名或 IP，不主动进行 DNS 查询，因此该标记无需写入生成的 TOML；IP/GEOIP 仅匹配已有目标 IP，未携带 no-resolve 的源规则可能需要调整。未知附加参数仍报错或按原文件行号提示跳过。

格式参考：[Mihomo 规则集](https://wiki.metacubex.one/config/rule-providers/)、[规则集内容](https://wiki.metacubex.one/config/rule-providers/content/)。

## GEOSITE

`GEOSITE,google,策略` 和 classical provider 中的 `GEOSITE,google` 会转换为原位置的 geosite 匹配，保留动作与分组/属性选择器。支持 `google@cn`、`google@!cn` 以及多个属性的交集。读取标准 V2Ray protobuf `geosite.dat`，支持关键词、正则、域名后缀及精确域名。分组或数据库缺失时拒绝候选配置，不静默跳过。正则使用 Rust regex 语法及大小限制，不支持回溯引用或环视。

可用 `--geosite-db /path/geosite.dat` 指定本地数据（优先于源 URL）；否则保留源 `geox-url.geosite`，或使用输入目录的现有 geosite.dat，再无本地数据时预设 MetaCubeX 下载地址。远程缓存路径为输出目录下的 `geosite/geosite.dat`。原生配置使用 `geosite_database`、`geosite_url`、`geosite_interval`，默认模板已提供下载地址；首次使用 GEOSITE 时加载，后续更新保留有效缓存与当前规则。


## Proxy Core 管理 Clash 订阅

订阅链接保存在代理实例自己的 `proxy.toml`。Proxy Core 负责获取、定时更新以及持久化上次成功的节点列表，无需 converter 常驻。TUI 的代理配置「订阅」中可填写链接、更新间隔和 User-Agent。

```toml
[subscription]
url = "https://example.com/clash?token=YOUR_TOKEN"
interval = 3600
user_agent = "clash.meta/astra"
# 默认 false：只导入节点，保留本地代理组。
import_groups = false
```

可以直接为现有代理实例添加该段配置；原有手动节点会保留。订阅尚未下载时，动态 PROXY 组可以暂时为空，连接会明确失败，不会自动直连。首次下载成功后即可使用。链接和周期随实例配置保存，节点归属和成功结果由核心缓存管理。

converter 也能生成包含订阅链接的代理配置：

```powershell
astra-convert "https://example.com/clash?token=YOUR_TOKEN" -o configs/subscribed --interval 3600
```

订阅默认只生成 `proxy.toml`，导入节点和订阅信息；不生成 `rule.toml`，不采用订阅的组、规则、监听、TUN 或 DNS 设置。默认 PROXY 仍是本地动态节点组。普通本地 YAML 转换仍默认输出两份完整配置；使用 `--subscribe URL` 绑定订阅后也默认仅节点。

需要首次转换时额外采用远程配置的组与规则，可显式使用 `--include-config`。这会生成两份配置并设置 `import_groups = true`，后续核心更新节点及订阅管理的组；定时更新始终不会改变 Rule Core 的规则和本地入站设置。只有设置 `import_groups = true` 时，订阅里的组定义才参与周期转换与校验；默认节点模式会忽略无效或不支持的远程规则、组和监听设置。

`--user-agent` 控制下载请求的 User-Agent，默认 `clash.meta/astra`。URL 输入未指定输出目录时使用当前目录；本地 YAML 仍默认写到输入文件旁。已有输出文件不会覆盖，`--check` 不写配置。

支持完整 Clash/Mihomo YAML 中的节点、仅含 `proxies` 的节点订阅、YAML 锚点/合并键，以及 HTTP、file、inline 类型的 `proxy-providers`。provider 的 `filter`、`exclude-filter`、`exclude-type` 会过滤节点；暂不支持 provider `override`，遇到时明确失败。file 路径相对于本地输入目录；URL 输入的 file 路径相对于 `--base-dir` 或当前配置目录。普通文件转换保留原始行号；provider 展开或节点提取后的诊断会标明展开后的内容。

更新周期允许 60–604800 秒，默认 3600 秒。Proxy Core 启动后检查一次，之后定时重试。无需启用 `watch_config`。空节点列表、名称冲突、无效节点配置或下载失败，均保留上次成功的运行状态。订阅只替换自身管理的节点；与手动节点同名时拒绝更新，避免覆盖手动配置。converter 自动填写的 `managed_nodes`/`managed_groups` 用于兼容初始导出的条目，TUI 不要求用户编辑这些内部列表。

默认保留全部本地代理组，包括断开连接策略和选择。若更新移除了本地静态组正在引用的节点，此次更新会被拒绝；推荐使用动态成员筛选。开启导入订阅组后，本地同名组优先，订阅组会保留仍有效的手动选择及断开连接策略，连接切换遵守 `disconnect_on_*`。

成功结果写入配置旁的 `*.subscription-cache.json`，保存节点、归属及对应配置。重启时先校验并恢复缓存，再检查订阅，断网时仍可使用最新成功节点。缓存损坏或本地配置发生改变时忽略旧缓存，重新从当前配置和链接建立状态。缓存文件包含节点凭据，权限与本地配置同样需要妥善保管。保存缓存失败会记录提示，不丢弃已经可用的运行节点。

每份 YAML/响应上限为 8 MiB，最多展开 32 个 providers、4096 个节点；核心仍校验自身节点与组的容量限制。HTTP 请求有超时、证书校验和有限重定向；更新日志不输出订阅链接或凭据。订阅的 interval 统一控制其中 providers 的检查周期。


## 多订阅与手动刷新

推荐使用具名订阅，最多 32 个，每个来源分别保存链接、周期和节点归属：

```toml
[subscriptions.home]
url = "https://example.com/home-clash"
interval = 3600

[subscriptions.backup]
url = "https://example.net/backup-clash"
interval = 7200
```

旧的 `[subscription]` 继续兼容，也可以与具名订阅同时使用；`__legacy__` 为内部保留名称。默认仍仅导入节点。本地节点、其他订阅的节点与新节点重名时，本次来源更新被拒绝，不覆盖已有条目。各来源按自己的周期检查，单个来源失败时保留其成功列表并继续更新其他来源。

TUI 编辑代理配置中的「订阅列表」，选中后按 `n` 输入订阅名称，再填写 URL；选中订阅后按 `d` 可删除，Ctrl+S 保存。保存运行中实例会热重载，并触发新增或修改链接的首次检查；停止的实例在下次启动时检查。外部直接修改文件时，启用 `watch_config = true` 可自动检测，否则手动热重载。

运行中代理实例的操作菜单提供「立即更新订阅」，后台排队检查全部来源，无需重新启动核心，也不会重载本地监听。命令行可调用：

```powershell
astra-proxy --update-subscriptions "实例的管理 socket 路径"
```

触发成功表示已排队，实际下载/校验结果见核心日志。刷新期间核心管理接口保持可响应；重复触发合并为待处理轮次，各来源不同时执行重叠更新。保存配置后当前实现会重新检查全部来源，而不只检查改变的链接。


## VLESS 分享链接导入

代理配置编辑页按 Ctrl+O 后粘贴 `vless://...`，也可在浏览配置时直接粘贴链接。支持每行一个节点的链接列表。节点追加到当前草稿，现有节点、组、订阅和监听设置保留；Ctrl+S 校验并保存。名称冲突会报错，不覆盖原节点。规则核心不接受节点链接。

converter 支持直接将链接作为输入（包含 `&` 时须加引号），也可通过标准输入读取，每行一个链接。Proxy Core 的节点订阅同样支持返回 VLESS 链接列表。

```powershell
astra-convert "vless://UUID@server.example:443?security=tls&sni=server.example&type=xhttp&path=%2Fproxy&mode=auto#my-node" -o imported
```

支持 UUID、百分号编码的节点名称、IPv6、VLESS encryption、Vision flow、TLS/REALITY、SNI、客户端指纹 `fp`、证书 SHA-256 指纹 `pcs`、REALITY `pbk`/`sid`，以及 TCP、WebSocket、gRPC、HTTP Upgrade、XHTTP。`pcs` 映射为 `cert_pin_sha256`，与客户端指纹 `fp` 不同。XHTTP path/host/mode 按原值导入。未知参数、重复参数、不支持的 TLS 设置和不安全证书跳过选项会明确拒绝，避免静默改变节点行为。导入不发起节点连通性测试；解析错误不输出原始链接。


## 更多协议分享链接

导入入口现支持 `vmess://`、`trojan://`、`ss://`、`tuic://`、`hysteria2://`/`hy2://`、`socks://`/`socks5://`，以及已有的 `vless://`。在代理配置编辑页 Ctrl+O 粘贴，或直接粘贴到浏览状态，追加到草稿；也可导入包含每行一个链接的文本文件。converter 支持直接输入、文件和 stdin；Proxy Core 节点订阅使用相同解析器。支持标准和 URL-safe Base64（有/无 padding）的混合协议订阅列表。

| 协议 | 支持格式与主要参数 |
| --- | --- |
| VMess | Base64 JSON v2，字符串/数字端口，UUID、AEAD cipher、TLS、SNI、WebSocket/gRPC 等已有传输 |
| Trojan | URI password、TLS/SNI、客户端指纹、证书 pin、已有 TCP/WS/gRPC/XHTTP/HTTP Upgrade 传输 |
| Shadowsocks | SIP002 Base64 用户信息或明文 method:password；兼容旧版整个地址 Base64；AEAD-2022 明文用户信息；密码中的冒号和百分号编码保留 |
| TUIC | UUID:password、SNI、congestion_control、udp_relay_mode |
| Hysteria2 | hy2 别名、auth/user:pass、默认 443、SNI、salamander obfs/obfs-password、pinSHA256 |
| SOCKS5 | 无认证、明文 username:password 或 Base64 用户信息、IPv4/IPv6 |

导入支持不代表核心新增了协议能力：旧版 VMess alterId、SS 插件、Hysteria1、Hysteria2 Gecko/端口跳跃/ECH、AnyTLS/WireGuard、TLS 不安全跳过和自定义 ALPN 等当前无法正确执行的选项会拒绝，不会默默丢弃。有效导入不会自动联网测试节点；未知查询参数、混合列表中的错误和重名节点使本次导入失败，不留下半份草稿。

格式核对来源：[Shadowsocks SIP002](https://shadowsocks.org/doc/sip002.html)、[Hysteria2 URI Scheme](https://hysteria.network/docs/developers/URI-Scheme/)、[v2rayN VMess 分享格式](https://github.com/2dust/v2rayN/wiki/Description-of-VMess-share-link)、[TUIC 导入实现](https://github.com/2dust/v2rayN/blob/master/v2rayN/ServiceLib/Handler/Fmt/TuicFmt.cs)、[Trojan 导入实现](https://github.com/2dust/v2rayN/blob/master/v2rayN/ServiceLib/Handler/Fmt/TrojanFmt.cs)。
