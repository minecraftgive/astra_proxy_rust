# Windows 原生网络验证（2026-11-09）

此记录只声明实际执行过的样本，不代表完整三平台联合验收。源码与二进制放在当前用户临时目录 `astra-network-20261109-1742`，没有覆盖生产工作目录、修改既有网络/防火墙/系统根库或自动提权。经明确授权，TUN 场景只短时创建独立接口及专用测试路由，结束后核验清理。环境为 Windows NT 10.0.26340、AMD64、Rust 1.99，目标 `x86_64-pc-windows-msvc`。

## 已执行

| 项目 | 实际结果与边界 |
|---|---|
| 系统状态 | `network_snapshot` 8 项通过；实际 Agent 快照采到 26 个接口、16 个 DNS resolver、66 条路由，`issues=[]`；30 秒 DNS 缓存保持原始采样时间。未创建非空 NRPT 策略。 |
| 原生路径 | `diagnostic_path` 4 项通过，含真实 Windows 接口 socket option 设置/读回与 IPv6 回环 ICMP。IPv4 正向响应未验收，见下文。 |
| 物理绑定 | 明确选择在线物理 Ethernet 2（index 27），IPv4 `10.42.0.233` 与该接口 IPv6 link-local 均创建未连接 UDP socket，源 IP 与设备绑定读回成功。没有把仅绑定成功当作路由成功：该样本 `route_verified=false`。 |
| 实际物理 STUN | 后续一次显式授权的公共基础采样经 Ethernet 2 成功：接口 DNS 有约束解析 Cloudflare，绑定/路由证明均为 true，同一未连接 socket 的两次请求均获合法映射响应。只作现场样本，不是自动测试的公网成功依赖。 |
| TLS | 原生系统根加载测试与本地 TLS/HTTP fixture 已执行；未向系统证书库导入测试根。 |
| Core 工具回归 | 最新 `nettool` 62 项通过；另一个已实测失败的旧 IPv4 正向 ICMP 用例显式跳过。测试包含 DNS/DoT/DoH、TLS/HTTP、STUN/NAT、IPv6 与完整流结束事件。 |
| 真实远程管理 | Mac 共享客户端→私有 IP 上临时 TLS Controller→Windows Agent→实际 Core 子进程完成 DNS、TCP v4/v6、HTTP、STUN 五项；22 页、92,688 字节报告可按原 ID 恢复，取消后的 DNS 收包数稳定。详见 [验收清单](network-plan-acceptance.md)。 |
| Agent 启动修复 | 实际 debug Agent 在远程重连的嵌套异步分派中发生主线程栈溢出。将 `AgentService::handle` 的内部状态机置于堆上后，原 main/service/controller 调用结构无需改变；真实启动、Hello、Instances、Shutdown 均通过。1 MiB 线程上的 TLS 握手/转发回归通过。 |
| 实际 TUN | 独立 Wintun 接口与双族 host routes 上，普通系统 ping 均收到本地 TUN echo；诊断 System ICMP 双族均保留 `tun_local`、无 RTT/来源/目标可达性结论，System Trace 为 0 次探测，虚拟接口 NAT 拒绝，物理接口对本机 TUN 地址亦在发包前拒绝。详见下节。 |

启动回归实际测得 future 大小：`run=12144`、`run_config=11256`、公开 `handle=328`、内部 dispatch `22368` 字节。没有增加主线程栈来隐藏问题。测试代码还修复 Windows 专用 NativeClient fixture 漏初始化新 `probes` 字段的问题。

日志：`/tmp/astra-network-windows-platform.log`、`/tmp/astra-network-windows-smoke.log`、`/tmp/astra-network-windows-final-tests.log`、`/tmp/astra-remote-client-test.log`。具体失败和跳过项不能从汇总中抹除。

## 系统环境限制

- 系统 `ping -4 127.0.0.1` 实际超时，`ping -6 ::1` 成功。平台测试只有在明确设置 Windows 专用 `ASTRA_TEST_ICMP_V4_BLOCKED=1` 时，允许 IPv4 `NoResponse`，并严格要求来源/延迟为空；IPv6 仍须真实 Reply。旧 `icmp_echo_reaches_loopback` 正向 IPv4 测试已执行并失败，与系统行为一致，随后定点回归显式跳过该一项。没有放宽产品输出或改防火墙。
- 独立 Python socket 对刚关闭的 IPv4/IPv6 回环端口，以同样 1 秒超时连接均返回 TimeoutError（约 1001/1000 ms），因此不能把此环境的“关闭端口”硬编码为 Refused。仅 Windows 测试可显式设置 `ASTRA_TEST_TCP_CLOSED_TIMED_OUT=1`，且还须独立 OS socket 再确认超时；单端口和多端口输出必须同为 TimedOut、无连接耗时或 RTT。其他平台仍严格断言 Refused。可控 SOCKS5 显式拒绝响应的 typed outcome 测试保持不变并通过。
- 当前可到 Mac `192.168.137.152` 的 Windows `192.168.137.1` 是虚拟接口路径；在线物理口实际为 `10.42.0.233/16`，另一个物理 Wi-Fi 未连接。没有强行把此虚拟路径算作 Windows 物理 NAT 正例，也未修改路由。受控跨机正例反向在 Mac `en0` 上完成；后续 Windows 物理口经其已有默认路由到公共 STUN 的成功样本另列。
- Windows OpenSSH 会话退出会回收其子进程，因此短时 fixture/Agent 均在保留的前台 SSH 会话中运行；初次后台进程退出不是防火墙丢包证据。
- 此 SSH 管理员令牌新对象默认 owner 是 Administrators，现有安全校验正确拒绝。测试只把独立启动进程的 TokenOwner 设为当前用户 SID，影响其测试子进程新建对象；不持久修改账户、不提权、不放宽生产 ownership 校验。所有 state/config/runtime 路径显式位于任务临时目录。

## 既有实例只读核对

既有生产代理 `t1` 的管理实例 ID 为 `5b958809-0631-4a24-b1d5-52b31e4de187`。Runtime 采样 `1794220146321` ms：PID `42936`、`tun=false`、`mode=rule`、`system_proxy=false`。旧管理 Hello 没有新 Core 能力，因此只调用既有稳定 CoreIdentity，采样 `1794220276259` ms：同 PID、boot ID `890b4342-165c-46e0-9516-0d3df68b2a8a`、进程 start `134386302222687657`、applied digest `14790617473851169423`。未读取或输出配置内容，没有修改该实例。

虚拟网卡存在不能证明 Astra TUN 开启。后续独立 TUN 场景另经真实 Runtime 验证，不能反推此生产实例启用了 TUN。服务账户、非空 NRPT、企业根策略、实物热拔插和物理目标 IPv4 正向 ICMP 仍未覆盖。

## 独立 TUN 场景

`scripts/windows_network_diagnostics_regression.py` 在任务目录启动新 Agent 与 `tun-fixture`，只使用新 `AstraDiagTest<PID>` 接口、`198.18.253.1/30` 与 `fd00:198:18:253::1/126` 地址及 `198.19.253.2/32`、`fd00:198:19:253::2/128` 两条专用 host routes。运行前拒绝地址、接口名称及已有非默认路由冲突，不添加默认路由。使用现有可信 Wintun DLL；启动默认 TUN 关闭后，只对新实例以其 PID 调用明确的本地 EnableTun。

本轮通过证据 `/tmp/astra-windows-tun-evidence.json`，执行日志 `/tmp/astra-windows-tun-diagnostics.log`：

- Runtime 采样 `1794222022438` ms 为 `tun=true`，PID `47700`；新 CoreIdentity 的 PID 相同，boot ID `22395d06-38bf-4815-a7dc-8fbf81a28fd6`、applied digest `2267954856287394705`。两个 ICMP 诊断的 Context 与该身份/摘要一致。
- 系统普通 `ping` 两个专用 IPv4/IPv6 目标均成功，证明真实 TUN echo 路径存在。网络诊断两族均明确 `tun_local=true`、`proves_reachability=false`，`rtt_ms`/`from` 为空，没有解析/连接/Attempt 事件；System Trace 报 `probes_sent=0`，没有逐跳或解析事件。
- 选择测试虚拟网卡的 NAT 请求在入队时拒绝。明确选择物理 Ethernet 2（index 27）但目标为本机新 TUN 地址时，报告 Partial、无 observations，原因是本机目标不是可证明的物理路径。**这是拒绝正例，不是物理 NAT 出口成功证明**；物理成功来自下节独立公共基础采样，不能混同这两个场景。
- Finally 通过管理协议 Shutdown 并等待进程退出，专用路由与新适配器均消失，整个既有路由快照前后一致。生产 t1 本轮前后同为 PID `42936`、`tun=false`、`system_proxy=true`、`mode=rule`；这个 system_proxy 值不同于较早只读样本，只以每轮前后相同验证隔离，没有改回历史值。回环代理 fixture 收到 0 个请求。

复现须显式选择已获授权的测试机器、空输出目录、物理接口和生产只读端点。先构建 `cargo build -p astra-management --example network_fixture_query`；脚本的 `--rpc-helper` 指向该示例二进制，它使用共享管理契约/可信 IPC，只处理一次显式输入请求。脚本还要求 `--bin-dir`、`--output-dir`、`--wintun`、`--physical-interface`、`--production-endpoint`、`--production-instance`。它不安装服务、不自动提权、不导入根证书。此场景不覆盖企业策略、物理热拔插或所有第三方 TUN 实现。

## 一次公共物理 STUN 基础采样

在原有路由不变的情况下，只读预检（`1794222651058` ms）确认 Ethernet 2/index 27 为 Up、HardwareInterface=true、Virtual=false，地址 `10.42.0.233/16`，该接口 IPv4 默认路由经 `10.42.0.1`，接口 DNS 为 `223.6.6.6`、`1.1.1.1`。没有默认 IPv6 路由，因此此项只测 IPv4。

新任务专用 Agent 接受一次 NAT Basic：手选 interface 27、显式 resolver `udp://223.6.6.6:53`、服务 `stun.cloudflare.com:3478`、`advanced=false`。解析记录明确经过 Ethernet 2 和指定 resolver，得到 `162.159.207.0:3478`，没有系统 DNS 回退。核心保留 30 秒总界限，本轮 NAT 用时 463 ms。

真实任务最终 Complete，`partial=false`、`Done` 完整；物理源 `10.42.0.233`，`binding_verified=true`、`route_verified=true`。同一未连接 socket 的 `basic_binding` 与 `same_target_repeat` 各发 1 次、均获合法映射响应、丢弃包 0。只用一个服务，所以 `comparison=insufficient`、`advanced_capable=false`，不据此生成完整 NAT 类型。

证据 `/tmp/astra-windows-public-nat-evidence.json`，日志 `/tmp/astra-windows-public-nat.log`。这是显式授权的一次现场结果，自动测试仍使用可控 fixture，不以公共服务可用作为通过条件。任务未新增 TUN 或修改路由/防火墙，临时 Agent 已通过协议 Shutdown 并退出；前一节独立 TUN 拒绝场景与本次成功样本分别保留。

## 为 macOS 隔离 TUN 提供的短时对端

Windows 最终只在私有地址 `192.168.137.1:59739` 开启任务专用 STUN fixture（PID `21080`）。真实 UDP endpoint/owner 已查询核对；日志记录 ready `1794224821216` ms、原定 deadline `1794226621216` ms。服务未绑定所有接口，未改防火墙或路由。

实际收到并回复 4 个合法事务，与 macOS 完整通过记录 `/private/tmp/astra-macos-isolated-tun-oxq6unte.json` 逐项匹配：

| 场景 | Mac 源端点 | 事务 ID | Windows 收到时间（ms） |
|---|---|---|---|
| TUN 前基础采样 | `192.168.137.152:59187` | `075c57a7c96ab564b36315ec` | `1794224912451` |
| TUN 前同端点重复 | `192.168.137.152:59187` | `cbc426551d90da8394981728` | `1794224912552` |
| TUN 开启后物理采样 | `192.168.137.152:58387` | `62cfa80d736ef7b6fea3e2a9` | `1794224912798` |
| TUN 开启后同端点重复 | `192.168.137.152:58387` | `a76eaad5bebed34169df8666` | `1794224912901` |

逐包日志保存在 `/tmp/astra-windows-stun-session-21080.jsonl`。主测试确认通过后，于 `1794225356441` ms 提前停止该已核验身份的自有进程，端口消失、临时 Python/Agent/Core 进程列表为空；核验记录 `/tmp/astra-windows-stun-cleanup-21080.json`。没有关闭用户的 root Terminal。此前过期 fixture 或脚本对明确拒绝结果的断言失败另留记录，不能冒充产品成功或网络故障证据。
