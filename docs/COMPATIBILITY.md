# 首版支持范围与兼容性

验证环境为 macOS arm64；参考服务端为 Xray v26.3.27、sing-box v1.14.3。参考服务端只用于开发测试，运行包不包含它们。协议在 Astra 的 Rust 进程内实现；TLS/QUIC 使用链接的密码库。

## 已验证能力

| 能力 | 验证范围 |
|---|---|
| 本地入站 | SOCKS5 CONNECT/UDP ASSOCIATE；HTTP CONNECT |
| 常见出站 | Direct；SOCKS5 TCP/UDP，可选用户名密码；HTTP CONNECT，可选 Basic 认证 |
| Shadowsocks | AES-128-GCM、SS2022 AES-128-GCM 的 TCP/UDP |
| VMess | 原生 AEAD TCP/UDP、TLS、AES-GCM/ChaCha20-Poly1305 编解码测试 |
| Trojan | TLS TCP/UDP |
| VLESS | TCP/UDP；TLS、WebSocket、gRPC、HTTPUpgrade |
| XHTTP | HTTP/2 的 stream-one、stream-up、packet-up；TLS 与 REALITY 组合 |
| REALITY / Vision | TCP/gRPC/XHTTP；错误公钥拒绝；Vision 内层 TLS 1.3 的 8 MiB 传输与双向 raw 切换 |
| Hysteria2 | TCP/UDP、证书指纹验证、Salamander；原生 QUIC 分片重组 |
| TUIC v5 | TCP、原生 datagram/单向 stream UDP、共享连接、断线重连、证书与凭据失败测试 |
| 连接复用 | VLESS/VMess 的 smux、yamux、h2mux、muxcool；每组合 65 TCP 流及三类地址 UDP |
| 路由与资源 | 有序规则、缓存、IPC 总期限、规则失败关闭、TCP/UDP 分开缓存、UDP 队列/目标数/全局额度、空闲与关闭清理 |

配置允许的其他密码套件或协议组合不自动等同于已通过上述互通测试。

## 已确认的边界

| 场景 | 当前行为与选择 |
|---|---|
| XHTTP packet-up 上传结束 | 该模式没有独立上传 EOF；确认已排队 POST 后关闭整个隧道。需要上传结束后继续接收响应的应用应使用适合该行为的其他路径。 |
| Xray 上的半关闭 | 最小 VLESS 客户端绕过 Astra 仍收不到 EOF 后的尾部响应；同一诊断对 sing-box 正常。Astra VLESS/VMess 经 sing-box 的 8 MiB 尾部响应测试通过。不要把普通回显通过理解为任意半关闭兼容。 |
| Xray UDP 空包/大包 | SOCKS5、Trojan、VLESS、VMess 路径的空包探针超时；部分大包也失败。SS IPv4 的应用载荷 8153 字节通过、8154 超时，对应加上地址、salt、tag 后的 8192 字节边界。域名/IPv6 头长度不同。 |
| sing-box UDP | SOCKS5（含认证）、Trojan、VLESS、VMess、SS/SS2022、TUIC 的 IPv4/IPv6/域名空包至 8192 字节通过。 |
| Hysteria2 大包 | 原生 QUIC 测试端往返 32768 字节通过；sing-box 能把 8192 字节完整发到目标，但返回只剩 4096。该参考路径验证范围为 0..4096 字节；未声称更大报文端到端可用。 |
| Mux.Cool 域名 UDP | 使用独立原生 UDP 连接，保持远端域名解析；IP UDP 使用复用池。不是本地 DNS 回退。 |
| 认证无显式确认的协议 | SOCKS CONNECT 成功不一定等于远端凭据已被接受。Xray SS 错误密码会静默等待；15 秒探针未收到应用数据。其他已测错误凭据/证书路径关闭或返回失败。 |
| 规则更新 | 规则进程启动时编译配置；代理默认缓存最多保留 5 秒，既有 TCP/UDP 流保持已选路由。需要每次新连接查规则时设 TTL 为 0。 |
| 平台与入站 | 本次可运行二进制为 macOS arm64，未签名/公证；默认入站只监听本机且没有用户认证。 |

TUN、Fake-IP、DNS 嗅探、规则热加载和 Windows IPC 不在这个首版的实现范围。HTTP CONNECT 不承载 UDP；Hysteria2 暂不支持自定义 CA 文件，使用系统信任或证书指纹。XHTTP HTTP/1.1/HTTP/3、所有 CDN 和所有服务端配置组合没有经过本版验收。

## 复现与证据

- 完整功能检查：`python3 scripts/release_check.py --tls-python /path/to/python-with-tls13`。
- 原始检查命令、退出状态和输出：`release-verification.json`；本机性能条件与二进制 SHA-256：`performance-baseline.json`。
- `--udp-edge` 和 `--trailer` 是边界诊断，选择上表已确认不支持的参考路径时仍会报告失败；不会把失败伪装成互通成功。
- 功能测试与性能基线不能替代具体公网部署的延迟、丢包、负载和服务端版本验证。

本次新增：VLESS Encryption（mlkem768x25519plus）、TLS cert_pin_sha256、XHTTP auto，及通过原生 Encryption 层执行 Vision DIRECT 的 HTTP 隧道组合。通用配置见 NATIVE_PROTOCOLS.md；私人节点的配置与验证记录保存在本地 `Local/` 目录。此项更新需要重新构建的可执行文件，原始 0.1.0 包并不自动获得新字段。
