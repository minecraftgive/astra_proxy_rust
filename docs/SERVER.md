# Astra 原生服务端

`astra-server-core` 处理认证与转发；独立 Agent 管理它的生命周期，统一 TUI 的 `astra-server-tui` 入口默认打开 Server 配置档，Web 也可操作同一 Agent。退出界面后核心继续运行；这些程序均为 Rust，不调用 Xray、sing-box 或其他外部协议核心，也不依赖客户端 Rule Core。

## 构建与启动

```powershell
python scripts/build.py --role server
./target/x86_64-pc-windows-msvc/release/astra-server-core --init server.toml
./target/x86_64-pc-windows-msvc/release/astra-server-tui
```

产物位于 `target/<本机 Rust target>/release/`，调试构建使用 `python scripts/build.py --role server --debug`。Windows 原生编译所需工具与客户端相同，见 INSTALL.md；服务端运行不需要 Wintun 或管理员权限。Linux/macOS 使用相同 Python 构建入口，二进制没有 `.exe` 后缀。首次生成的配置只监听回环地址，包含随机 UUID。公网部署请先配置域名、TLS 证书及实际监听地址。完整字段示例见仓库 `configs/server.example.toml`。

统一 TUI 的配置档类型包含 Proxy、Rule、Server；在 Server 配置档创建实例或绑定已有 server.toml，然后使用启动、停止、重启、校验与编辑命令。客户端和服务端共用终端输入、表单与布局，不再使用旧专用服务器 TUI 的快捷键或 `--config` 参数。多主机管理、权限与部署配置见 [MANAGEMENT.md](MANAGEMENT.md)，通用操作见 [TUI.md](TUI.md)。

直接运行核心：

```powershell
./target/<本机 Rust target>/release/astra-server-core --config server.toml
./target/<本机 Rust target>/release/astra-server-core --config server.toml --check
./target/<本机 Rust target>/release/astra-server-core --status
./target/<本机 Rust target>/release/astra-server-core --reload
./target/<本机 Rust target>/release/astra-server-core --stop
```

核心在前台运行时支持 Ctrl+C。Agent 启动的实例分别使用自己的控制端点和状态目录日志。独立运行 Core 的默认控制目录仍为：Windows `%LOCALAPPDATA%/.astra-server`，Unix `$HOME/.astra-server`。独立 Core 命令可用 `--control PATH` 管理其他实例，每个实例必须使用独立路径和监听端口。控制接口使用当前用户可访问的本地 IPC，不监听公网管理端口。不要在核心运行时删除控制文件；异常退出后的残留文件应在确认旧进程已经结束后手动清理。

## 协议和传输

| 协议 | 认证 | TCP | UDP |
|---|---|---|---|
| SOCKS5 | 用户名/密码，强制 RFC1929 | CONNECT | UDP ASSOCIATE |
| HTTP | Basic Proxy-Authorization | CONNECT | 无 |
| Shadowsocks | 监听级密码/密钥 | AEAD / SS2022 | AEAD / SS2022 |
| VMess | 多用户 UUID，AEAD 请求头 | AES-GCM / ChaCha20-Poly1305 | 原生逐记录数据报 |
| VLESS | 多用户 UUID | 普通 VLESS | 长度分帧 UDP |
| Trojan | 多用户密码 | TLS | TLS 数据报封装 |
| Hysteria2 | 多用户密码，HTTP/3 认证 | QUIC 流 | QUIC DATAGRAM / 分片 |
| TUIC v5 | UUID + 密码，TLS exporter | QUIC 流 | native / quic 两种模式及分片 |

Shadowsocks 接受 `aes-128-gcm`、`aes-256-gcm`、`chacha20-ietf-poly1305`、`2022-blake3-aes-128-gcm`、`2022-blake3-aes-256-gcm`、`2022-blake3-chacha20-poly1305`。SS2022 密钥为标准 Base64 编码的 16 或 32 个随机字节，不是任意文本密码。一个 SS 监听对应一把密钥。

VMess、VLESS、Trojan 可组合 `tcp`、`ws`、`http_upgrade`、`grpc` 或 `xhttp` 传输；XHTTP 支持 `stream-one`、`stream-up`、`packet-up`。WS/HTTP Upgrade/XHTTP 使用 `path`，gRPC 使用 `service_name`。QUIC 和 Shadowsocks 使用各自原生传输，配置中保持 `transport="tcp"`。

服务器端扩展现已提供 REALITY、Vision、VLESS 解密、Mux/XUDP、XHTTP 分离上传以及 Hysteria2 Salamander。服务端入站字段和客户端出站字段严格分开，配置方法与组合边界见仓库 `docs/SERVER_EXTENSIONS.md`。HTTP 入站提供 CONNECT 隧道，不提供普通 HTTP 页面转发。

## TLS、访问策略和配置更新

Trojan、Hysteria2、TUIC 强制 TLS。REALITY 使用自己的入站认证配置替代证书 TLS；其余非 TLS 公网监听必须显式设置 `allow_plaintext=true`；SOCKS5/HTTP 回环监听例外。VLESS UUID 和 SOCKS/HTTP 密码在普通 TCP 上没有加密，请在公网使用 TLS。服务端不会自动签发或续期公网证书。

开发用自签名证书：

```powershell
./target/<本机 Rust target>/release/astra-server-core --generate-cert local-tls localhost
```

该命令要求目录尚不存在。客户端 TLS 的 `server_name` 必须与证书一致；VLESS/VMess/Trojan/TUIC 可配置 `ca_file`，Hysteria2 使用 `pin_sha256`。生产环境使用实际域名和可信证书。

默认禁止访问私网、回环、链路本地、组播等目的地址。域名先解析，再连接经过检查的具体 IP。内网转发和本地互通测试需设置 `allow_private_targets=true`；`deny_cidrs` 始终优先。全部入站必须认证；不提供匿名开放代理。

`--check` 解析配置、执行纯结构校验并准备证书，不绑定端口。管理服务使用 `--check-stdin PATH` 从 stdin 校验候选，PATH 是原配置身份/相对证书基目录，不写入源文件。启动时先准备全部监听，任何一个失败都会撤回本次启动。重载失败保留旧配置，既有连接继续使用原配置。TCP 监听可重载认证、证书和传输参数；监听地址、顺序、协议、UDP 开关、会话额度，以及 QUIC/SS 监听设置的改变要求重启。QUIC 证书更新也应重启。修改 UDP 出站策略时重启以应用到全部关联。

TCP、QUIC、UDP 流和缓冲均有限额；UDP 超额丢弃，空闲流回收。SS2022 和 VMess 入站具有重放检查。关闭时停止接受新连接，等待 `shutdown_timeout_secs`，再终止剩余会话。

## 验证与独立打包

```powershell
python scripts/server_regression.py --bin-dir target/debug
python scripts/server_regression.py --bin-dir target/debug --bad-auth
python scripts/server_regression.py --bin-dir target/debug --deny-private
python scripts/server_lifecycle.py --bin-dir target/debug
python scripts/server_extensions_regression.py --bin-dir target/debug
python scripts/build.py --role server --package
python scripts/test.py server package      # 回归 + 解压 dist/ 中的服务端包并验证
```

互通脚本只启动 Astra 原生客户端和服务端，使用本地 TCP/UDP 回声目标与临时证书；不会启用 TUN 或系统代理。独立压缩包根目录为 `server/`，包含 `astra-server-core`、`astra-server-tui`、`astra-manager`、`astra-host`、版本清单和一份完整 `server.example.toml`。Windows 程序带 `.exe` 后缀，并附带静默自启动辅助程序 `astra-tui-startup.exe`，开关位于 TUI 设置页且默认关闭。说明、源码、许可材料与验证记录留在仓库，不放入运行包；验证记录见 `docs/SERVER_VERIFICATION.md`。
