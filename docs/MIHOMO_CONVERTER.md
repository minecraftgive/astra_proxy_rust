# Mihomo 配置转换工具

`astra-convert` 是独立的原生 Rust 命令行工具和 Rust library，将本地 mihomo YAML 转为当前 Astra 的 `proxy.toml`、`rule.toml`。运行时不需要 Python、mihomo 或代理核心；不会下载订阅、解析远程 provider 或连接节点。只编译此工具也不需要代理协议库、CMake 或 libclang。

```sh
cargo build --release -p astra-convert
target/release/astra-convert configs/mihomo-example.yaml -o converted
# 指定代理组成员；多个代理组可重复 --select：
target/release/astra-convert your-mihomo.yaml -o your-config --select 'Proxy=节点名称'
# 只解析检查，不创建输出目录：
target/release/astra-convert your-mihomo.yaml --check
# 标准输入：
cat your-mihomo.yaml | target/release/astra-convert - -o converted-stdin
```

默认输出到工作目录下的 `converted/`。已有文件不会覆盖，Unix 下新文件权限为 `0600`。两份配置默认共用 `/tmp/astra-rule.sock`，可用 `--rule-socket /path/to/rule.sock` 修改。多实例应使用不同的 socket 和监听端口。

转换后用核心检查实际协议参数，再启动：

```sh
target/release/astra-rule --check converted/rule.toml
target/release/astra-proxy --check converted/proxy.toml
# 两个终端分别运行：
target/release/astra-rule converted/rule.toml
target/release/astra-proxy converted/proxy.toml
```

转换器的 `--check` 只检查解析和映射，并输出跳过摘要；核心的 `--check` 进一步验证协议和加密参数。这些检查均不能证明节点在线、凭据有效或服务端互通。

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
| `DOMAIN`、`DOMAIN-SUFFIX` | domain_exact、domain_suffix |
| `IP-CIDR`、`IP-CIDR6` | ip_cidr；接受 no-resolve |
| `DST-PORT`、`NETWORK` | 单个目标端口、tcp/udp |
| `MATCH` | default_action；其后的规则不再转换 |
| `DIRECT`、`REJECT`、`REJECT-DROP` | direct、block；不保留 REJECT-DROP 的静默丢包差异 |

支持 YAML anchors 和合并键。数字形式的端口、密码和 short-id 按解析后的标量转换；带前导零的 short-id 等必须在源 YAML 中加引号。

`skip-cert-verify` 忽略且输出提示，Astra 仍验证证书。不转换 CA、客户端证书、ECH、自定义 ALPN 等未映射 TLS 项；这些节点可能需要手工调整。入站认证无法映射，存在 `authentication` 时将监听限制为 `127.0.0.1`。

## 代理组和跳过策略

`select`、`url-test`、`fallback`、`load-balance` 固定为 `proxies` 列表中第一个可转换成员，可递归解析嵌套组，最多 64 层；摘要列出每组的最终动作。`--select GROUP=MEMBER` 必须指定该组的直接成员，成员不可转换、循环或组不存在时返回错误。不会保留动态选择、健康检查、故障转移和负载均衡。链式 `relay` 组忽略。

`mode: rule` 按源顺序转换规则；未知策略、不支持的规则、引用被跳过节点的规则会跳过。没有可用 `MATCH` 时默认 `block`。`mode: direct` 使用默认直连并忽略规则；`mode: global` 使用源配置中显式声明的 `GLOBAL` 组，无可用组时默认 `block`。

没有对应实现的字段忽略，包括 DNS/Fake-IP、sniffer、GeoIP/GeoSite、RULE-SET、正则、进程规则、provider、面板及控制 API。TUN、重定向、透明代理及自定义 listeners 也不自动映射：双方的路由和系统设置语义不同。没有可转换监听器时补充 `127.0.0.1:1080` SOCKS5 并提示。远程 provider 不展开，节点必须已经位于 `proxies` 中。

跳过信息输出到标准错误；摘要不保证枚举每个未映射字段，首条提示说明其他字段被忽略。语法错误、顶层数组格式错误、重复名称、端口冲突、错误的 `--select` 会失败；单个节点缺少必要字段会跳过该节点。转换结果是功能子集，忽略规则及字段会改变源配置的路由行为，应检查摘要及输出后再使用。

源字段参考：[Mihomo TLS](https://wiki.metacubex.one/config/proxies/tls/)、[传输层](https://wiki.metacubex.one/config/proxies/transport/)、[TUIC](https://wiki.metacubex.one/config/proxies/tuic/)。

## 开发验证

```sh
cargo test -p astra-convert
```

测试覆盖节点和传输映射、规则顺序、未知项跳过、代理组循环/选择、YAML 合并、模式、认证监听限制、错误输入、文件权限及拒绝覆盖。
