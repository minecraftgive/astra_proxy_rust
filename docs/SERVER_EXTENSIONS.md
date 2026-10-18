# 服务器端扩展

本文只配置 `astra-server-core` 的入站。客户端由 `astra-proxy`、`astra-rule`、`astra-tui` 及离线转换工具 `astra-convert` 组成，服务端使用独立的 `astra-server-core`、`astra-server-tui` 和本地管理端点。两端不会互相启动、加载对方配置或调用外部协议核心。

| 内容 | 客户端 | 服务器端 |
|---|---|---|
| 配置入口 | `[[outbounds]]` | `[[listeners]]` |
| 完整示例 | `configs/client.example.toml` | `configs/server.example.toml` |
| REALITY 密钥 | `reality_public_key` | `[listeners.reality].private_key` |
| VLESS 加解密 | `encryption` 公钥字符串 | `[listeners.decryption].private_keys` |
| Mux | `mux = "h2mux"` 等 | `multiplex = true` |
| 构建 | `scripts/build.py` / `build.sh` | `scripts/build_server.py` |
| 打包 | `scripts/package_release.py` | `scripts/package_server.py` |

服务器端包只包含服务器核心与服务器 TUI，不携带客户端程序和客户端示例。客户端与服务端归档根目录分别为 `client/` 和 `server/`，每端仅包含自己的程序与一份完整示例配置。客户端运行所需 Wintun DLL 若存在，也作为运行二进制随包提供。说明和验证记录仅留在仓库。

## REALITY

```powershell
astra-server-core --generate-reality-keys
```

输出中的 `server_private_key` 只放入服务端；`client_public_key` 交给客户端。不要把整个密钥输出粘贴到客户端配置中。完整的两端示例分别在 `configs/server/reality.example.toml`、`configs/client/server-reality.example.toml`。

服务端使用 `[listeners.reality]` 替代 `[listeners.tls]`，配置私钥、允许的 SNI、十六进制 short ID，以及固定伪装目标 `target`。仅 VLESS 监听可启用。认证检查包含 X25519、AEAD、SNI、short ID、时间窗口和握手重放；普通 TLS 或认证未通过的完整 ClientHello 会转发到固定目标，代理用户仍须通过 VLESS UUID 认证。

本版提供 X25519 REALITY 入站；不提供 REALITY 的 ML-DSA 额外签名或混合 TLS 密钥交换。客户端浏览器指纹用于其出站 ClientHello，服务端不使用客户端的 `fingerprint` 字段。双方系统时间应同步。

## Vision

服务端监听设置 `flow = "xtls-rprx-vision"`，客户端同样设置 `flow`；要求 VLESS + TLS/REALITY。服务端解析 Vision 填充，按方向在精确记录边界执行 DIRECT 切换。普通 VLESS 的 TCP 请求不能绕过配置要求的 flow；UDP 使用普通 VLESS UDP 封装。

现有客户端允许 Vision 配合 TCP TLS/REALITY；HTTP 传输需要 native/xorpub VLESS Encryption。客户端当前不支持 Vision + Mux，这一客户端限制未在本次服务端工作中改变。

## VLESS 解密

```powershell
astra-server-core --generate-decryption-keys x25519
# 可选：mlkem768 或 hybrid
```

输出 `server_private_keys` 和 `client_public_keys`，分别用于两端。服务端配置：

```toml
[listeners.decryption]
private_keys = ["SERVER_PRIVATE_KEY"]
mode = "native" # native / xorpub / random
ticket_lifetime_secs = 0 # 0 禁用恢复；1..3600 启用限时 ticket
```

客户端使用 `encryption = "mlkem768x25519plus.native.1rtt.CLIENT_PUBLIC_KEY"`；多个公钥按服务端私钥链顺序用点号连接。启用票据时客户端可使用 `0rtt`。服务端票据、握手重放记录均设有期限和容量；相同解密配置重载时保留票据状态。此层在传输内部、VLESS 请求之前运行，与 TLS/REALITY 是不同配置。

## Mux / XUDP

服务端设置 `multiplex = true`，在认证后处理复用。支持 sing-mux 的 smux、yamux、h2mux 和可选会话填充；VLESS CommandMux 支持 Mux.Cool 的 TCP 子流及 UDP/XUDP 数据报。XUDP 关联限于当前认证会话，不提供跨连接 GlobalID 恢复或 full-cone。子流共用全局额度，仍执行目的地址策略，关闭单个流不会结束整个物理会话。

当前客户端配置可选 VLESS/VMess 的 `mux = "smux"`、`"yamux"`、`"h2mux"`；VLESS 还支持 `"muxcool"`。客户端暂未暴露会话 padding 配置，且不支持 Trojan/SS 的 Mux 配置；服务端能力不通过修改这些客户端选项实现。

现有客户端 Mux.Cool 接收路径会忽略空 UDP 回复。服务端保留空包支持，并单独使用原生封装检查；这不影响其他模式的空 UDP 数据报。

## XHTTP 与 Salamander

XHTTP 监听设置 `transport = "xhttp"` 和 `path`，服务端按请求路径区分 stream-one、stream-up、packet-up，不使用客户端 `mode` 字段。上传/下载可使用同一个或不同 HTTP/2 连接；会话按来源 IP 和随机会话 ID 关联，限制会话、子请求、数据块和排序窗口。packet-up 没有上传 EOF 信号，断开下载或空闲超时回收。

Hysteria2 监听设置 `obfs_password = "..."` 启用 Salamander；客户端配置相同密码。服务器端在 QUIC socket 中执行收发变换，不启动额外 UDP 转发进程。混淆不替代 Hysteria2 密码或 TLS 身份验证。

## 校验

```powershell
python scripts/server_extensions_regression.py --bin-dir target/debug
python scripts/server_extensions_regression.py --bin-dir target/debug --bad-auth
```

测试中的服务器私钥与客户端公钥配置保存在不同目录，并检查两端拒绝对方配置。测试只运行原生 Astra 进程；不会启用 Windows TUN 或系统代理。
