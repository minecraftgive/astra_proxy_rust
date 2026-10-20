# Windows 系统代理与 TUN 验证

2026-10-19，Windows AMD64、Rust MSVC 原生开发构建。本记录针对客户端 `astra-proxy`；服务端没有系统代理/TUN 接管功能，也不参与修改本机网络设置。

## 系统代理

以下 7 项本机集成检查通过：

- 读取真实 WinINet LAN 设置并接管 HTTP/HTTPS，保存原始代理、PAC、绕过列表和标志。
- Windows WinINet PRECONFIG 请求实际读取系统代理，通过测试上游回传 HTTP 64 KiB。
- 同样通过系统代理完成 HTTPS CONNECT 与 TLS 内 64 KiB 回传。仅测试请求接受临时自签证书，不改变系统证书信任或客户端证书校验设置。
- 单独验证 CONNECT 隧道；第二实例拒绝争抢接管权。
- 热重载关闭系统代理时恢复原设置，再开启时重新接管。
- 正常退出后原代理/PAC/绕过/标志逐项相等，恢复文件删除。
- 强制结束进程后保留恢复文件，执行恢复命令后设置逐项还原。

测试使用保留的 `.invalid` 域名与只连接回环目标的测试上游，确认请求确实经过 Astra，无需外部站点。

## TUN

通过管理员进程加载官方 Wintun，创建独立测试设备。仅接管两个专用测试地址的主机路由，不改默认路由：`198.19.253.2/32`、`fd00:198:19:253::2/128`。

- 实际创建设备及配置 IPv4/IPv6 地址、MTU、两条拥有明确归属的路由。
- IPv4 与 IPv6 原生 TCP 各回传 1、4096、65536、1048576 字节，并验证上传半关闭后的 EOF。
- IPv4 与 IPv6 原生 UDP 各回传 0、1、64、1200 字节，验证源地址、源端口与内容。
- 同设备热重载保留两条路由，重载后继续成功转发。
- 停止后测试设备消失、两条测试路由均删除。

首轮实际设备测试发现 UDP 回包被重新送入协议栈，造成回包超时。已改为将完整 UDP 回包直接写到 TUN 设备；修复后上述双栈测试全部通过。另有 3 项 TUN 协议栈/封包单元测试通过，既有 Windows IPC、SOCKS/HTTP、UDP、重载回滚回归通过。

本次范围不含全局默认路由接管、真实公网节点、DNS 接管、跨网络切换、超 MTU UDP 分片和长时间负载；不能将专用地址转发结果当作这些场景的验收。

## 复现

同名 Wintun 复用已通过独立管理员测试（`scripts/windows_tun_reuse_regression.py`）：外部先创建空闲网卡，Astra 按名称绑定后接口索引不变，IPv4/IPv6 地址按配置生效，同设备重载成功；另一个实例争用该设备时在设置地址/MTU 前失败，原地址与 MTU 保持不变。Astra 停止后释放会话，外部仍可开启会话，网卡保留至其创建者关闭句柄。该测试不添加接管路由。

系统代理在普通当前用户终端运行：

```powershell
python scripts/windows_modes_regression.py --mode system-proxy --output-dir .test-tools/windows-system-proxy
```

TUN 在管理员终端运行，`--interface` 填实际物理出口的接口索引（用 `Get-NetAdapter` 查询），不要填 TUN 接口：

```powershell
python scripts/windows_modes_regression.py --mode tun --interface 27 --output-dir .test-tools/windows-tun
```

默认程序目录为 `target/debug`，可用 `--bin-dir` 更改。系统代理 HTTPS 测试可用 `--test-cert-dir` 指定已有临时证书；未提供时借助 `astra-server-core --generate-cert` 离线生成临时证书，该命令只生成文件，不启动服务端或外部协议核心。结果分别写入输出目录的 `result.json`，运行日志留在同目录。脚本在退出路径停止所属进程、恢复代理；TUN 正常退出删除所属设备与路由。

## 分发包再次验证

2026-10-19 在 Windows 11（10.0.26340）解压最新客户端包，并直接运行包内 `client/astra-proxy.exe`，重新完成系统代理 7 项和 Wintun 5 项检查。使用已有临时证书，未复制或运行服务端程序。

- 压缩包：`astra-client-0.1.0-preview-windows-AMD64.tar.gz`。
- 包 SHA-256：`9bde516790b8dc06fd856d332630e899fffab21aab7f7681b790e8357cbb2aa2`。
- 已测程序 SHA-256：`ddaa3e0f127c2229870741326dabf8199bb80ced1e3f2f2be2c744ec17f064b6`，两类报告一致。
- 新增检查：TUN 热重载前已建立的 TCP 连接继续传输；错误 MTU 配置重载被拒绝，旧连接继续传输。
- 完成后没有测试设备或测试路由残留；系统代理逐项恢复到测试前状态，原先运行的 TUI 与代理进程保持运行。
- 精简包文件集合与解压运行回归再次通过；运行报告位于 `.test-tools/windows-package-validation/system-proxy/result.json`、`tun/result.json` 和 `archive-result.json`。
