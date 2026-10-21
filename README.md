# Astra Dual Core

原生 Rust 代理与规则双进程项目，当前版本为 0.1.0。流量处理由 `astra-proxy` 与 `astra-rule` 承担，可使用原生 `astra-tui` 管理；协议在代理进程内实现，不依赖 Xray/sing-box 等外部协议核心。Rust 协议库与 Quinn 直接链接；TLS 的 AWS-LC/BoringSSL 属于加密依赖，并非代理核心。

首版提供常见 TCP 代理、SOCKS5 UDP 入站及 SS/SS2022、VMess、Trojan、VLESS、Hysteria2、TUIC 原生出站。macOS arm64 release 的 24 组 TCP 矩阵、UDP、Vision、mux、认证失败与生命周期检查已通过，并提供可解压运行的安装包和本地性能基线。实际支持范围及已确认的服务端限制见 [兼容性说明](docs/COMPATIBILITY.md)，验证证据见 [验收记录](docs/V1_ACCEPTANCE.md)，节点字段见 [原生协议配置](docs/NATIVE_PROTOCOLS.md)。

## 架构

配套服务端已单独提供 `astra-server-core` 和 `astra-server-tui`，覆盖客户端全部协议类型（SS/SS2022、VMess、Trojan、VLESS、Hysteria2、TUIC，以及认证 SOCKS5/HTTP CONNECT）。服务端原生 Rust 实现，独立配置、管理接口和安装包，无需客户端规则核心；REALITY、Vision、VLESS 解密、Mux/XUDP 等入站扩展见 [服务器端扩展](docs/SERVER_EXTENSIONS.md)，启动与独立打包方法见 [服务端说明](docs/SERVER.md)。客户端使用出站配置，服务器端使用独立的入站配置，两端程序与安装包分开。

- `astra-proxy`：入站握手、路由缓存、原生协议出站、TCP relay 与 UDP 会话管理。
- `astra-rule`：启动时编译规则索引，按配置顺序返回第一条匹配规则。
- `astra-ipc`：版本化请求与响应、bincode 长度帧，路由通道只传元数据；管理通道独立。
- `astra-tui`：原生 Rust 终端界面与后台实例管理服务，通过内部 IPC 分别管理两个 core。

```text
客户端 -> SOCKS5 / HTTP CONNECT -> 决策缓存
                                     | 未命中
                                     v
                              UDS 连接池 -> Rule Core
                                     | 路由结果
                                     v
                           Direct / 原生协议出站 -> 目标
```

网络载荷始终留在 Proxy Core。TCP 连接建立时决定路由；UDP 按关联内的目标创建流并决定路由。既有流不会逐包查询规则，也不会因为 Rule Core 重启而重新选择出口。配置可通过 TUI、SIGHUP、管理 IPC 或文件监测热重载；规则版本通知使旧决策缓存失效。

## 系统接管与热重载

提供 macOS/Linux 原生 TUN、Windows Wintun、Linux TPROXY TCP/UDP 和 REDIRECT TCP，以及 macOS/GNOME 系统 SOCKS 代理和 Windows 当前用户 HTTP/HTTPS 系统代理。Windows 系统代理和 Wintun 已通过本机有限范围集成验证，见 [验证记录](docs/WINDOWS_MODES_VERIFICATION.md)。配置表可独立开启或关闭；TUI 保存/切换配置时可热应用，`R` 手动热重载，顶层 `watch_config = true` 开启文件监测。候选失败保留旧配置；节点切换默认保留既有连接，代理组可按手动、自动优选、故障、恢复和成员/模式变更分别配置是否断开旧连接，见 [代理组设置](docs/TUI.md#配置可选项与切换后的连接)。

精简配置见 [configs/modes.toml](configs/modes.toml)，平台权限、TUN 防自环、透明代理路由、设置恢复及实测边界见 [模式与热重载说明](docs/PROXY_MODES.md)。设备和防火墙权限相关路径尚需目标机器验收，不能仅凭配置校验视为已接管系统流量。

## 独立代理模式

在代理配置顶层设置 `standalone = true`，所有 SOCKS5 / HTTP CONNECT 入站流量都使用固定出站，跳过规则查询与决策缓存，无需启动 Rule Core。只有一个出站时自动选用；多个出站时必须填写 `standalone_outbound = "节点标签"`。节点故障时请求失败，不回退直连。TCP、UDP 均遵循此选择，UDP 能力取决于所选协议。

将本地配置的 `standalone` 设置为 `true` 后，可直接启动（替换下列配置路径）：

```sh
./target/release/astra-proxy /path/to/proxy-standalone.toml
# 启动器也会根据 standalone 自动只启动 Proxy Core：
python3 scripts/astra.py --bin-dir target/release --proxy /path/to/proxy-standalone.toml
```

独立模式可省略 `rule_socket`；`ipc_connections`、`decision_timeout_ms` 和决策缓存设置在此模式不参与流量路由。默认 `standalone = false`，保留双核心行为。

## 已验证范围与限制

本地参考服务端为 Xray v26.3.27、sing-box v1.14.3，仅由测试脚本启动。

| 能力 | 当前范围 |
|---|---|
| 入站 | SOCKS5 CONNECT / UDP ASSOCIATE；HTTP CONNECT |
| 常见出站 | Direct；SOCKS5 TCP/UDP（可选用户名密码）；HTTP CONNECT（可选 Basic 认证） |
| 加密 TCP 出站 | Shadowsocks/SS2022、VMess、Trojan、VLESS、Hysteria2、TUIC v5 |
| VLESS 传输 | TCP/TLS、WebSocket、gRPC、HTTPUpgrade、XHTTP HTTP/2 三种模式 |
| REALITY / Vision | TCP 基础互通通过；Vision 内层 TLS 1.3 的 8 MiB 传输与双向 raw-switch 通过 |
| 加密 UDP 出站 | VMess、Trojan、SS/SS2022、Hysteria2、TUIC；VLESS 的 TLS/REALITY/WS/gRPC/HTTPUpgrade/XHTTP 路径通过基础互通 |
| UDP 未完项 | 部分协议的大包/空包边界；Hysteria2/sing-box 返回方向超过 4096 字节仍有截断 |
| XHTTP | stream-one、stream-up、packet-up；当前范围为 HTTP/2 |
| TCP 半关闭 | relay 使用 `copy_bidirectional`，但最终行为取决于协议和服务端；不能据此保证所有路径支持半关闭 |
| DNS / Fake IP | UDP/TCP、DoH/DoT、TTL 缓存、TUN DNS 接管、IPv4/IPv6 Fake IP 与域名还原；详见 [DNS](docs/DNS.md) |

XHTTP packet-up 关闭上传时不能发送独立的上传 EOF，当前实现确认已排队 POST 后结束整个隧道；需要上传结束后继续接收响应的应用不能依赖该模式。普通互通测试是在收到回显后才关闭上传；严格半关闭测试使用 `--trailer`，当前 VLESS/Xray 路径仍有失败。

`kind = "trojan_tls"` / `"vless_tls"` 是旧 TCP 配置，功能有限。新节点使用 `kind = "native"`，见 [配置说明](docs/NATIVE_PROTOCOLS.md)。入站无用户认证，默认只监听 loopback。

## 安装包

分发包的解压和启动步骤见 [安装说明](docs/INSTALL.md)。客户端与服务端分别打包为 `client/` 与 `server/`；每包仅包含自己的执行文件与一份完整示例配置。客户端还包含离线转换工具及可选 Wintun 运行库。本机初始性能数据见 [性能基线](docs/PERFORMANCE.md)。

VLESS Encryption、Vision 与 XHTTP 的配置字段见 [原生协议说明](docs/NATIVE_PROTOCOLS.md)。私人节点配置及验证记录保存在 Git 忽略的 `Local/` 目录，不随项目发布。

## 完整配置示例

### 转换 Mihomo 配置

独立原生 Rust 工具 `astra-convert` 可将 mihomo YAML 转为 Astra 的代理与规则 TOML，不支持的配置项会跳过并输出提示，无需运行 mihomo 或 Python：

```sh
cargo build --release -p astra-convert
target/release/astra-convert your-mihomo.yaml -o converted
# 可选：--select '代理组=成员'、--check、--rule-socket /tmp/custom.sock
target/release/astra-rule --check converted/rule.toml
target/release/astra-proxy --check converted/proxy.toml
```

代理组固定为一个可用成员，不保留动态测速或故障转移；未知规则会跳过。详见 [转换范围和使用说明](docs/MIHOMO_CONVERTER.md)，本地示例见 [mihomo-example.yaml](configs/mihomo-example.yaml)。

### Astra 配置

项目根目录的 [example_proxy.toml](example_proxy.toml) 覆盖全部设置项、协议、传输与变体；[example_rule.toml](example_rule.toml) 覆盖全部规则类型和动作。两者配套使用，节点值均为占位数据，运行前替换。配置检查：

```sh
python3 scripts/astra.py --check --proxy example_proxy.toml --rules example_rule.toml
```

## 构建与启动

目标平台：Linux/macOS/Windows。Windows 本地 IPC 使用仅当前用户可访问的命名管道，支持双核心、独立代理及 TUI；已加入 Wintun 与当前用户系统 HTTP/HTTPS 代理实现，已通过系统 HTTP/HTTPS、恢复和 TUN 双栈专用地址转发检查，全局路由接管尚未验收。透明代理 TPROXY/REDIRECT 仍仅限 Linux。加密协议矩阵的既有验收环境为 macOS arm64，Windows 本地验证范围见 [安装说明](docs/INSTALL.md)。需要 Rust 工具链、CMake、Clang/C++ 与 libclang；Windows x64 另需 MSVC C++ 工具和 NASM。Python 3 用于构建、启动和测试脚本。

编译、测试并分发客户端原生程序：

```sh
./scripts/build.sh
```

Windows PowerShell 使用 `./scripts/build.ps1`，参数与上面的脚本相同；也可执行 `python scripts/build.py`。构建后运行 `./target/x86_64-pc-windows-msvc/release/astra-tui.exe`。Windows TUI 默认状态目录为 `%LOCALAPPDATA%/.astra/tui`，支持 `--state-dir` 自定义路径。

安装包输出到 `dist/`，包含 `astra-proxy`、`astra-rule`、`astra-tui`、配置、文档与依赖许可，并附 SHA-256 和验证报告。支持 `--debug`、`--build-only`、`--label`、`--output-dir`；详见 [构建与打包说明](docs/INSTALL.md)。

```sh
cargo build --release
cargo test --workspace
```

原生 Rust TUI（无需 Python）：

```sh
./target/release/astra-tui
```

按 Tab 切换 core，n 添加实例，s/x/r 启动、停止、重启，e 编辑配置。q 退出界面后实例继续运行；Q 确认后停止全部实例。规则模式首次请添加并启动 Rule Core，再添加 Proxy Core；独立模式只需添加并启动 Proxy Core。详见 [TUI 管理说明](docs/TUI.md)。客户端打包脚本会包含 `client/astra-tui`；旧安装包需重新构建。

本地直连示例：

```sh
python3 scripts/astra.py --check
python3 scripts/astra.py
```

规则模式启动器先校验两个配置，再启动两个 Astra 可执行文件；任一进程退出时停止另一进程。收到停止信号后，先让代理停止接受连接并等待现有会话，再停止规则进程。使用 debug 构建时添加 `--debug`。

连接自有节点时，先填写 `configs/native.toml` 或 `configs/tuic.toml` 的服务器、凭据与 TLS 配置，然后选择代理规则：

```sh
python3 scripts/astra.py --check --proxy configs/native.toml --rules configs/proxy-all-rule.toml
python3 scripts/astra.py --proxy configs/native.toml --rules configs/proxy-all-rule.toml
```

**原始 `configs/rule.toml` 默认直连；`configs/proxy-all-rule.toml` 才将流量交给 `edge`。** TLS CA 文件路径相对代理配置文件，默认验证证书和服务器名。

也可以分别启动：

```sh
target/release/astra-rule configs/rule.toml
target/release/astra-proxy configs/proxy.toml
```

若 UDS 路径已存在，程序拒绝覆盖。确认没有运行中的规则进程后再删除遗留 socket；正常 SIGINT/SIGTERM 会清理它。

## 规则与缓存

```toml
socket = "/tmp/astra-rule.sock"
default_action = "direct"

[[rules]]
kind = "domain_suffix"
value = "example.com"
action = "proxy:edge"

[[rules]]
kind = "ip_cidr"
value = "10.0.0.0/8"
action = "block"
```

- 支持 `domain_exact`、`domain_suffix`、`ip_cidr`、`port`、`network`（`tcp` / `udp`）、`process_name`、`process_path`、`domain_keyword`、`domain_regex`、`geoip`、`geosite` 和 `rule_set`。不同规则类型仍严格按文件顺序，第一条匹配生效。
- 本地 IP 段默认 DIRECT（`direct_local = true`）：私有、回环、链路本地 IPv4/IPv6；显式规则优先，可在规则表单关闭。进程规则由 Windows/Linux 本机连接归属提供身份，无法识别时继续匹配其他规则。
- GeoIP 使用本地或 HTTP(S) 缓存 MMDB / geoip.dat，支持国家与 telegram 等自定义分类；GEOSITE 使用标准 geosite.dat 并支持属性筛选；命名规则集支持 inline/file/http、classical/domain/ipcidr 和 YAML/text。Mihomo converter 保留 provider 引用和顺序，使用方式见 [转换说明](docs/MIHOMO_CONVERTER.md)。
- `proxy:TAG` 对应代理配置中的出站标签；未知标签阻止连接，不会回退直连。
- 后缀按域名标签边界匹配；CIDR 只匹配请求中已有的 IP，不会为了规则匹配主动解析域名。
- 缓存键区分目标、TCP/UDP、入站和进程身份；最大 65536 项，默认 TTL 5 秒，`decision_cache_ttl_secs = 0` 可禁用。
- Rule Core 不可用时，缓存未命中的新请求失败关闭；缓存命中仍可能沿用旧策略至 TTL 到期。重启规则进程不会使代理缓存立即失效。

IPC 默认 8 个持久通道，每通道一条在途请求。一次决策的总超时包含排队、连接和重试；取消、超时或不完整响应会关闭该通道。帧上限 8192 字节，响应校验版本及 request ID。

## 资源与排障

默认 `max_connections = 4096`；空闲监听器不占用会话额度。UDP 默认全局最多 512 个流、每个关联最多 32 个目标，每流发送队列 4 个包，空闲 120 秒回收。UDP 关联随 TCP 控制连接关闭，禁止 SOCKS 分片并固定客户端来源；超限丢包，不无限积压。

`connect_timeout_secs` 默认 8 秒，`decision_timeout_ms` 默认 500 毫秒，`shutdown_timeout_secs` 默认 10 秒。运行时可用 `RUST_LOG=debug` 查看会话错误；`--check` 只验证配置，不能证明节点可连接或凭据有效。

## 验证

```sh
cargo build
python3 scripts/install_test_servers.py
python3 scripts/protocol_matrix.py
python3 scripts/protocol_matrix.py --udp --case vless-xhttp:packet-up
python3 scripts/core_regression.py
python3 scripts/udp_regression.py
python3 scripts/tuic_regression.py
```

参考服务端安装器固定版本和 SHA-256；Astra 本身不会下载或启动它们。完整 TCP 矩阵覆盖 24 组配置（含 sing-box VMess、REALITY/gRPC 和 REALITY/XHTTP 三种模式）；UDP 应按已实现的配置选择测试，HTTP CONNECT 本身不提供 UDP。`--udp-edge` 检查空包和 8192 字节，`--trailer` 检查上传 EOF 后的响应，二者包含尚未通过的路径。

## 性能改进原则

双进程的价值是规则与连接生命周期隔离，尚无数据证明它比单进程更快。索引、缓存及持久 IPC 已减少每个新连接的工作；后续先测缓存命中/未命中的建连 p50/p95/p99、吞吐、CPU 和峰值内存，再决定调整 IPC 池或 relay 缓冲。当前单规则微基准不能代表大量规则或整机代理性能。

优先补协议边界、取消清理与可定位错误，再做有测量依据的优化。架构审查及验收方案见 [架构评价](docs/ARCHITECTURE_REVIEW.md)。

## 代码分层

共享协议、proxy 核心和操作系统适配的目录及依赖规则见 [模块架构](docs/ARCHITECTURE.md)。运行 `python scripts/check_architecture.py` 检查模块边界。
