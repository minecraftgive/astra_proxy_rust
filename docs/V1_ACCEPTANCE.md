# Astra 0.1.0 — 首版验收

日期：2026-10-18。交付目标是可实际运行的原生 Rust 代理首版，覆盖常见与加密代理及 XHTTP 等传输能力。运行时仅有 Astra Proxy/Rule 两个进程，不调用外部协议核心。

## 要求与实际证据

| 要求 | 验证结果 |
|---|---|
| 原生协议运行时 | 源码运行路径无外部核心启动；release 代理只动态链接 macOS 系统库；安装包不含参考服务端。TLS 密码库不等同于代理核心。 |
| 常见代理 | Direct、SOCKS5 TCP/UDP、HTTP CONNECT；SOCKS5/HTTP 上游认证与错误凭据通过。 |
| 加密代理 | SS/SS2022、VMess、Trojan、VLESS、Hysteria2、TUIC v5；24 组 release TCP 基础矩阵通过。 |
| 额外传输 | WS、gRPC、HTTPUpgrade；XHTTP HTTP/2 三种模式；REALITY TCP/gRPC/XHTTP；Vision 8 MiB 内层 TLS 1.3 与读写 raw 切换通过。 |
| UDP | 16 组基础矩阵通过；兼容服务端上的 SOCKS5/Trojan/VLESS/VMess/SS/SS2022/TUIC 空包及 8192 字节通过；Hysteria2 空包至 4096 字节互通及原生 QUIC 32768 字节测试通过。 |
| 连接复用 | VLESS/VMess × smux/yamux/h2mux/muxcool 共 8 组，每组 65 条 TCP 流和三类地址 UDP；验证物理连接池及域名 UDP 独立连接行为。 |
| 错误与资源回收 | 错误 UUID/密码/SNI/CA/REALITY 公钥/Hysteria2 pin；IPC 总期限与取消、XHTTP 拒绝响应和任务清理、UDP 来源隔离/额度/空闲回收、规则重启、TUIC 并发/重连通过。 |
| 原生补丁 | 工作区 18 项测试通过（1 项手动基准忽略）；VMess 14 项、Hysteria2 37 项专项测试通过；工作区 Clippy 通过。依赖仍有两条已知弃用警告。 |
| Hysteria2 混淆 | Salamander TCP 及 IPv4/IPv6/域名空包/4096 字节 UDP 通过。 |
| 性能基线 | 1000/10000 混合规则、缓存关闭/预热、并发 1/32，三轮共 7200 个连接；条件、分位数、二进制 SHA-256 已存档。未据此宣称生产吞吐或调整默认参数。 |
| 可运行交付 | macOS arm64 release 二进制、示例配置、启动器、安装说明、285 项依赖许可记录、压缩包校验与文件清单；解压后检查和 64 KiB 回显及退出清理。 |

## 可复现记录

- `release-verification.json`：21 项主检查的命令、退出状态、耗时和原始输出，全部成功。
- `release-supplement.json`：VMess/Hysteria2 原生补丁和 Salamander 共 4 项补充检查，全部成功。
- `scripts/release_check.py` 已整合上述 25 项，后续可一次运行：

```sh
cargo build --release --workspace
python3 scripts/release_check.py --tls-python /path/to/python-with-tls13
python3 scripts/package_release.py
python3 scripts/package_regression.py dist/astra-0.1.0-darwin-arm64.tar.gz
```

最后一项解压测试的交付记录位于压缩包旁的 `.verification.json`。需要 CMake/Clang 的开发测试应在具备构建依赖的环境运行。普通运行包不需要这些工具，也不需要参考服务端。

## 已确认边界，不能误写为通过

详见 [兼容性说明](COMPATIBILITY.md)。这些是已验证的服务端/模式限制，而不是尚未接入的协议：

- XHTTP packet-up 没有独立上传 EOF；不能承诺该模式支持任意 TCP 半关闭。
- 不运行 Astra、只使用最小 VLESS 客户端时，Xray 仍不返回上传 EOF 后的尾部数据，sing-box 返回正常。Astra 经 sing-box 的 VLESS/VMess 8 MiB 严格半关闭通过。
- Xray 对应 UDP 路径的空包及部分大包失败；同样的 Astra 实现经 sing-box 通过。SS IPv4 的 8153/8154 字节边界已测，不能把不同地址形式当成同一个最大载荷。
- Hysteria2/sing-box 的 8192 字节请求完整到达目标，但回复截成 4096 字节；较大报文端到端未通过，保留为明确限制。
- Xray SS 错误密码为静默等待：15 秒探针没有应用数据，不能声称立即收到认证错误。

边界诊断仍会报告失败；验收没有把这些结果改写成互通成功。首版支持范围与选择适用路径的说明已随包交付。

## 首版范围以外

TUN、Fake-IP、DNS 嗅探、热加载、Windows IPC、XHTTP HTTP/1.1/HTTP/3，以及任意公网/CDN/服务端版本组合不在本次交付范围。加密长流的生产容量、CPU/RSS 全量压测及更细的性能调优仍是后续工作，不是本版已作出的性能承诺。
