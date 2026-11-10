# 网络工具计划逐项追踪


> **2026-11-10 界面重新设计：** 三端网络工具已改为单页（常驻链路条 + 工具表单 + 单栏结果 + 探测路径条），保留的工具与行为见 [网络诊断](network-diagnostics.md#三端页面单页)。下文中关于四分区、结论/原始报告双栏、Ctrl+D/L/P/R/O 快捷键、可编辑诊断计划与预设、“恢复后台任务”工具、IPv6 可达性工具和前端旧 Agent 准入矩阵（49 个共享样例）的条目是重新设计前的历史验收记录，已不再适用。后端任务/报告、批量任务、分析规则与拓扑、NAT、固定出口与调度器保持不变；批量任务在界面上以固定的“完整检查”提交。

依据用户提供的 `/Users/Nothing/Downloads/PLAN.md`，原文件未修改。这里区分代码落地、自动测试和真实运行验收；不能将已写测试、交叉编译或模拟服务当作三平台真机验收通过。代码仍在同一工作区集成，本文不宣称已经分阶段合入发布。

截至 2026-11-09，阶段 0–3 功能已实现，逐条实现位置与验证证据如下。明确已运行的本轮证据包括：macOS 便携式 NAT 8 项、STUN 3 项，Linux ARM64 独立 VM 九种内核 NAT 映射/过滤组合，Web 67 项网络页面测试与全套 143 项测试、类型检查及 Chromium 桌面/390px 检查。macOS 普通账户平台路径 7 项、代理路径 4 项通过；Linux VM 平台路径 5 项及代理 nettool 60 项通过，另一个 ignored 用例由 namespace 矩阵单独执行。Windows 已完成原生平台路径 4 项、系统状态 8 项、Core 工具 62 项、真实跨机批量/分页/取消、实际 TUN 与物理公网 STUN 样本；macOS 完成真实物理网卡跨机 STUN 与普通/root 权限差异。三组真实新旧版本组合与 GUI 可点击拓扑也已验证。

**macOS 独立 TUN 场景也已通过，本轮计划范围内的实现与联合验收已完成。** 三平台各自的实际运行样本分别记录；已知环境限制及补充覆盖需求单列，不把这些样本外推为所有操作系统、账户、企业策略、远程组合与人工视觉的完整交叉矩阵。具体样本、Windows IPv4 ICMP 限制与日志见下文和 [Windows 原生验证](network-windows-validation.md)。

## 实现位置索引

下表路径均相对仓库根目录；“已有测试”只说明测试覆盖入口，除上述明确结果外，以实际运行日志为准。

| 标记 | 实现与对应测试 |
|---|---|
| 任务 | `astra-management/src/v2/network.rs`、`astra-manager/src/network.rs`、`agent.rs`、`agent/network_batch.rs`；`test/network_scheduler_test.rs`、`network_batch_test.rs`、`network_controller_test.rs` |
| 存储 | `astra-manager/src/store/network.rs`；`test/network_store_test.rs`、`network_storage_failure_test.rs` |
| 子进程 | `astra-manager/src/manager/network.rs`；`test/network_process_test.rs` |
| 系统状态 | `astra-platform/src/network_snapshot/`、`astra-model/src/network_state.rs`、Agent `Query::NetworkState`；平台 parser/cache 测试及 Manager `network_state_test.rs` |
| 核心 | `astra-ipc/src/control/identity_wire.rs`、`control/fake_ip.rs`、`astra-manager/src/agent/core_network.rs`；`network_core_bridge_test.rs`、IPC `control_fake_ip_test.rs` |
| 固定路径 | `astra-manager/src/manager/network/{pinned,selection}.rs`；`network_pin_state_test.rs`、`network_selection_test.rs` |
| DNS/解析 | `astra-proxy/src/nettool/detailed.rs`、`detailed/path.rs`；`test/nettool_detailed_test.rs`、`nettool_routes_test.rs` |
| 常用工具 | `astra-proxy/src/nettool/{basic,tls,http}.rs`、`astra-model/src/network_tls.rs`；`test/nettool_tls_test.rs`、`nettool_http_test.rs` |
| STUN/NAT | `astra-proxy/src/nettool/{stun,nat}.rs`、`astra-model/src/network_nat.rs`；`test/network_{stun,nat}_test.rs`、`network_nat_fixture.rs`；`scripts/nat_test_service.py`、`test_nat_namespaces.py` |
| 物理路径 | `astra-platform/src/diagnostic_path/`、`astra-proxy/src/nettool/path_probe.rs`；平台 `diagnostic_path/tests.rs` 与 probe 单测；Agent `network_admission.rs` |
| 规则/拓扑 | `astra-model/src/network_analysis.rs`、`astra-manager/src/network/analysis.rs`、`analysis/{rules,topology}.rs`；`test/network_analysis_test.rs` |
| 客户端/原生界面 | `astra-client/src/nettools/{chain,form,full_check,jobs,lookup,nat,result}.rs`、`network_diagnostics.rs`；GUI `views/nettools.rs`、`views/nettools_path.rs`、TUI `nettools.rs` 及各自测试 |
| Web | `apps/web/src/components/NetworkTools.tsx`、`NetworkPath.tsx`、`NetworkResult.tsx`、`networkChain.ts`、`networkNat.ts`、`networkToolsModel.ts`、生成管理契约；`NetworkTools.test.tsx`、`networkChain.test.ts`、`networkNat.test.ts`、`tests/e2e/network-tools.spec.ts` |

`astra-*` 路径均位于 `crates/` 下。

## 阶段 0：任务、取消与存储

| 计划条目 | 实施位置与行为 | 已有验证 / 尚缺验证 |
|---|---|---|
| 持久化任务与请求指纹后返回；只读诊断绕过 mutation，配置仍串行 | 任务、存储、Agent 分派 | 调度/Controller/服务并发测试；真实长时人工压力矩阵属于补充测试 |
| NetworkReport 游标；复用 Task/Events/Cancel；区分接受与完成 | 任务、存储、客户端、Web | 回执丢失、分页、终态仍有页、原 ID 恢复测试 |
| 2 运行、4 排队、超限繁忙 | 任务 | Scheduler 容量测试；NAT 独占并占两份许可 |
| 入队/启动维护检查；更新前取消并等待 | Agent maintenance、任务、子进程 | 维护/受控关闭测试、macOS 实际更新事务保活回归通过；其他平台更新场景为补充测试 |
| 取消贯穿排队/解析/连接/读取/重试，等待子进程回收 | 任务、子进程、工具有界 Future | 排队取消、重试取消、进程终止与实际取消停包测试；强杀 Agent 后跨平台残留进程场景为补充测试 |
| 断线不取消；相同 ID 防重放；重启标中断 | 存储、客户端、Web | 身份指纹/中断/停止观察/恢复测试 |
| Task 严格结构与旧状态枚举不扩展，报告独立状态 | 管理契约、存储 | 契约、状态映射测试 |
| 旧 NetTool 和文本保留，Hello 能力门槛 | Agent、客户端、Web | 旧能力 fixture；真实旧 Agent/旧 Core、新 Agent/旧 Core、旧 Agent/新 Core 三组通过，旧 TCP/DNS 接口继续工作，不支持的新请求零探测 |
| 持久事件每条 2 KiB、每任务 256，仅阶段/结果 ID | 存储 | 事件预算/游标测试；子进程 16,000 条上限与此分开 |
| 报告 2 MiB、分页 64 KiB、显式截断 | 存储、规则报告适配 | 预算/分页测试；截断不能作为完整诊断 |
| DNS 256×8 KiB、HTTP 32/64 KiB、证书 16 张/256 KiB | DNS、常用工具 | 本地 DNS/HTTP/TLS 超限 fixture |
| 最多 7 天/32 任务/64 MiB，只淘汰终态；保留身份 | 存储、周期清理 | 到期、超预算、报告过期后防重放、存储失败测试 |

## 阶段 1：来源、固定路径与解析

| 计划条目 | 实施位置与行为 | 已有验证 / 尚缺验证 |
|---|---|---|
| 网卡/地址/路由/DNS 必须来自目标 Agent | 系统状态、三端页面 | Agent 身份匹配与切换测试、三平台采集；真实 Mac Client→Windows Agent 已验证，其他远程平台组合为补充测试 |
| Runtime 复用模式/系统代理/TUN/组；不改旧快照布局 | 核心、三端 Runtime 查询 | 兼容/身份测试；不将系统状态冒充核心状态 |
| Fake-IP 独立只读查询，最多 64 IP，不按网段猜测 | 核心、客户端、Web | 64/65、非法 IP、未命中、切换核心/Agent、类型错配测试 |
| 隔离子进程；配置摘要/核心身份/采样/实际出站 | 子进程、固定路径、Context | program-info/配置版本固定测试 |
| Group.selected 仅输入；不冒充有效网卡/完整链 | 固定路径、Context、三端路径页 | 多层链、网卡优先级测试 |
| load-balance 下一连接不确定，需选具体节点 | 固定路径拒绝分支、界面说明 | 拒绝测试；真实负载均衡环境人工验证为补充测试 |
| select/url-test/fallback 同版本逐层固定，外层网卡优先 | 固定路径 | 嵌套组、默认网卡、链路来源测试 |
| 截断/缺层/未知成员/重载/版本不符不自动固定 | 固定路径 | 不匹配、重启/摘要变化、快照失效测试 |
| 无额外健康检查/订阅刷新 | 诊断专用固定叶构造 | 仅重建受控叶节点；真实订阅配置未确认时拒绝 |
| 当前选择与探测选择分开；变化标过时而不改探测 | 客户端/原生界面、Web | Web boot_id/applied_digest/Runtime 变化测试 |
| 新响应独立版本；Identity 可选 capabilities，不升级 version | 核心、管理 DTO | 旧身份解码/可选字段兼容测试 |
| fake_ip_lookup_v1 必须由 Core 自己声明；缓存绑身份 | 核心、三端 lookup | 缺失/未包含/重启/未知能力不发命令测试 |
| 不支持、通信失败、协议错误、能力无法确认分开 | 核心、三端 | 四类响应 fixture；仅明确不支持可兼容降级 |
| macOS scutil 有界解析接口/作用域/域/优先级 | 系统状态/macOS | parser fixture 与本机采样；企业复杂分域环境未覆盖，保留为补充测试 |
| Windows 网卡 DNS 与有效 NRPT 结构化；失败非无策略 | 系统状态/Windows | 原生采集 8 项、真实快照含网卡 DNS/路由且 issues=[]；非空 NRPT 与其他服务账户未覆盖，保留为补充测试 |
| Linux resolved 全局/每链路，失败显式 resolv.conf 全局降级 | 系统状态/Linux | parser/交叉编译与 Linux 环境测试；复杂 resolved 多链路策略环境未覆盖，保留为补充测试 |
| 不声称复现完整 OS DNS 策略 | 系统状态、DNS候选选择、文案 | 未知/失败/截断策略保守拒绝自动候选 |
| 系统解析仅系统路径允许；内部路径不可见注明 | DNS/解析 | 系统调用边界计数回归 |
| 网卡/绑定 DIRECT 无隐式系统回退；显式或可靠接口 resolver | DNS/解析、Agent 候选准备 | 本地受约束解析/无候选拒绝/作用域变更测试 |
| DNS/DoH/DoT 服务引导 IP 保留 TLS 名称 | DNS/解析、TLS | 本地 bootstrap/协议测试；URL 令牌输出脱敏 |
| 代理业务目标与入口解析分开记证据 | DNS/解析、Connection/Resolution | SOCKS5/HTTP CONNECT fixture；其他未经验证组合拒绝 |
| TCP/UDP/DNS 适配共用约束入口，不审计业务流量 | DNS/解析、STUN/NAT | 受约束路径系统解析调用拦截测试；新适配必须先测试再声明 |
| DNS UDP/TCP/DoT/DoH、RCODE/flags/EDNS/区段/TTL/CNAME | DNS、三端详情 | 本地 fixture、Web 记录/截断渲染；三端共享证据及详情回归已执行；额外人工对照为补充测试 |
| 区分响应/解析/NXDOMAIN/无记录/失败/超时；同路 TCP 重试 | DNS | 详细状态与 TCP 回退 fixture |
| 不补造系统未提供信息；AD 非本机 DNSSEC | DTO 与说明 | 展示语义/规则测试 |
| 三端四区；Web 新页面、进度/结果表/详情 | 原生界面、Web | Rust/Web 测试入口；Web 桌面/390px 浏览器已测，原生 GPUI 窗口/指针/键盘及 TUI 窄终端回归通过；完整人工矩阵为补充测试 |

## 阶段 2：工具、证书与 NAT

| 计划条目 | 实施位置与行为 | 已有验证 / 尚缺验证 |
|---|---|---|
| TCP 单端口/最多 16 端口；区分连接/拒绝/超时/不可达 | basic、客户端表单、Web builder/详情 | 端口边界/结果语义测试；代理耗时明确含解析握手 |
| ICMP/IPv6/公网 IP 分别记录地址/路径/可达性 | basic、path_probe、HTTP IP、Connection | 平台 fixture、本地 HTTPS IP、真实 IPv6 回环与 Windows 双族 TUN；当前环境缺少物理公网 IPv6 多出口样本，属于明确的环境覆盖限制 |
| TUN 本地 echo 不证明目标可达，不计延迟或 IPv6 结论 | path_probe、三端结果 | TUN 语义测试、Web 不计统计；Linux 真实 TUN 下系统 ICMP 无 RTT/可达性结论；Windows 真实 Runtime.tun=true、普通双族 echo 成功，诊断双族无 RTT/可达性结论；macOS 实际 TUN 场景亦通过，详见下文 |
| HTTP GET/HEAD、状态/头/阶段耗时、最多五跳、拒绝降级 | http | 本地 HTTP/HTTPS 重定向与降级测试 |
| HTTP 认证信息/敏感头/预览/URL脱敏 | http、报告预算、Web 导出 | 本地敏感值 fixture；Web JSON 不含原表单令牌实测 |
| 代理 UDP/STUN 逐协议验证 | stun 自持 SOCKS5 control/relay | 本地代理会话独立/控制释放/HTTP UDP 拒绝测试；其他组合不宣称支持 |
| rustls-native-certs 系统根快照与加载错误，不静默换 roots | tls、依赖与构建 | macOS 普通账户实际core加载158根、0错误；本地证书失败保留证据；三平台原生 TLS 样本与企业链 fixture 已覆盖；全部服务账户/企业策略组合为补充测试 |
| 系统快照非完整系统策略；不联网补链；同链离线对照 | tls、三端 checkbox | 企业证书及 bundled 差异 fixture；不额外发连接 |
| 验证前有界留证、原签名/链校验委托，失败不继续 HTTP | tls/http | 两种签名委托、证书失败无应用数据、无证书和取消 fixture |
| 企业根差异非监听结论 | 共享规则、三端 | 规则证据和 UI 语义检查 |
| NAT 独立手选，默认空/不继承/不记住上次 | 三端表单、Web builder | Web 无网卡不 execute、提交清空、切 Agent/族清空测试 |
| 仅已确认 physical/connected/所选族；拒绝 unknown/virtual/TUN/loopback | 系统状态、admission、PhysicalGuard、三端 | 平台拒绝/Agent身份测试；Linux 实际接口失效后停止并恢复原状态，实物热拔插矩阵为补充测试 |
| 请求携 Agent+网卡 ID；接收/排队启动再次验证 | admission、NAT、PhysicalGuard | 接收/身份/排队失效测试入口；真实远程网卡热更换为补充测试 |
| 绑定失败/无物理证明停止，无系统/代理 fallback | PhysicalGuard、NAT | Linux 首包后实际接口身份变化，约 514ms 停止、无第二包/回退，清除 binding/route 证明；恢复后地址/路由/规则等价 |
| 基础 STUN 显示网卡/本地端点/映射/响应/多服务对照 | NAT 报告、三端 | 本地响应/显示测试；macOS en0 到 Windows 临时服务两次真实响应；Windows 物理 27 经显式接口 DNS 到 Cloudflare 两次真实响应，绑定/路由均核验；自动测试不依赖公网成功 |
| Cloudflare/Google 预设可改删，不假定 RFC5780 | 三端表单 | 官方地址端口核对；服务可用性不做保证 |
| 普通 STUN 只能称所测映射一致/不同 | NAT | 目标间差异非不稳定/非完整类型测试 |
| RFC5780 替代端点能力验证，完整证据才分类 | NAT advanced | 双 IP/双端口 fixture与 Linux 九组合真实内核验证通过 |
| 映射同未连接 socket/端口；过滤全新会话 | NAT Engine | socket peer 未连接/本地端口隔离断言与内核矩阵 |
| txid/source/length/FINGERPRINT 检查 | STUN codec | IPv4/IPv6 XOR、错误来源/事务/截断、包预算测试 |
| NAT 全 Agent 诊断独占，不暂停业务；旧工具也受许可 | 任务、legacy gate | 独占与排队测试；公平性/维护并发由整仓测试复核 |
| 同目标重复映射变化标不稳定；不同目标差异正常 | NAT Engine | 两类专用回归通过 |
| 所有请求/替代端点/辅助解析同物理口，必要时显式 DNS | PhysicalGuard、resolve_physical_bound | 每次 resolver 发包核验；辅助 DNS 另一族同接口守卫 |
| 运行前公网 IP 可见说明 | 三端 NAT 表单 | Web 文案和手动执行检查 |
| TUN 开启只有证明物理绕行才允许，否则不发 | PhysicalGuard、NAT | Linux 实际 TUN 接收未绑定控制包，选定 eth0 的 NAT 两次成功且 TUN 收到 0 个 NAT 包；Windows 真 TUN 的虚拟接口/本机 TUN 目标拒绝通过，另有不新增 TUN 的公共物理 STUN 成功样本，两者分别记录；macOS 实际 TUN 场景亦通过，详见下文 |
| 独立 STUN 代理观察不做 NAT 分类/多服务对照 | stun、独立 Probe/UI | 本地 SOCKS5 两独立会话；输出无 NAT 分类 |
| 30 秒、重传有界；无寿命/Hairpin/ALG/服务部署 | NAT timeout/retry，测试服务隔离 | 超时/取消测试；产品没有新 STUN 监听入口 |
| 本地双 IP/双端口测试服务及 NAT 映射过滤仿真 | test fixture、两个 Python scripts | Linux 九组合全部实测；设施只在测试程序/脚本，不随产品监听发布 |

## 阶段 3：逐跳、拓扑与规则

| 计划条目 | 实施位置与行为 | 已有验证 / 尚缺验证 |
|---|---|---|
| 已知/无法排除 TUN 时逐跳不可测；ICMP 不证明逐跳 | path_probe | 拒绝/unknown 语义测试；Linux、Windows、macOS 真实 TUN 下 System Trace 拒绝且零探测 |
| 物理网卡或绑定 DIRECT 才测物理路径，不自动绕行 | PhysicalGuard、path_probe | 接口约束/拒绝测试；macOS root 选定 en0 单跳到 Windows 成功，Windows IPv4/IPv6 真实物理绑定读回通过 |
| Linux UDP错误队列、macOS UDP+ICMP、Windows ICMP API | 平台 probes | Linux 原生 UDP 错误队列、macOS root 单跳、Windows IPv6 原生 ICMP 已运行；Windows IPv4 正向回包受系统环境限制 |
| 按运行权限，不自动提权；mac root与普通用户分验 | 平台 probes、Capability 报告 | macOS 普通账户拒绝与用户认证 root 单跳已验证；Windows 当前账户已验证，其他服务账户为补充测试 |
| 默认30×3×1000ms，未响应只标未知 | path_probe、三端表单 | 边界/默认请求测试、Web no-response显示 |
| DNS/连接/逐跳分区、实测/配置/未知、NAT附着不推设备数 | 共享 topology、三端渲染；GUI/Web 节点及边可点击，TUI 文字表格 | 拓扑不补造路由器测试；Web 未知边点击看证据；GPUI 420×460 鼠标点击节点/边，显示项目/来源/时间与原始事件，缺失引用不补造，切任务清选择 |
| 固定 FindingKind/规则版本/来源/可信度/严重度 | network_analysis 模型与后端 | 前端只渲染报告；无“被监控概率”字段 |
| 明文 DNS：实际 UDP/TCP 及路径 | analysis/rules | Collector/规则测试入口；高/风险，范围限明文链路 |
| 未加密代理段：有效配置+实际协议 | analysis/rules、Policy/Connection | 同版本和实际协议核对；高/风险，不外推全路径 |
| 关闭证书验证：当前生效配置明确关闭 | analysis/rules、Policy | 缺失/未生效证据不推断；高/风险 |
| TLS失败：验证器明确错误 | analysis/rules、TLS evidence | 无证书/网络超时不算验证失败测试；高/待核实 |
| 信任库差异：同链两 roots 不同结果 | analysis/rules | 离线企业根 fixture；高/信息 |
| DNS差异：同名/类型、30秒、两轮均不同 | analysis/rules、batch comparison | 对照/时间/身份回归由后端统一；中/待核实 |
| 证书差异：同名/目标 IP、30秒、指纹不同 | analysis/rules、Connection/cert evidence | 相同目标及两轮约束；中/待核实，不将轮换当监听 |
| 解析路径不符：实际/声明冲突 | analysis/rules、Resolution/Context | 无实际记录不推断；高/风险 |
| v4/v6出口差异：双族成功且路径不同 | analysis/rules | 不以单族失败/ICMP本地应答触发；中/信息 |
| HTTPS/STUN差异：同轮/族/出口/固定成员且均成功 | analysis/rules | 不比较跨族、跨出口或不同固定成员；中/信息 |
| 第二轮需明确勾选，未做不产生差异或无差异 | Batch comparison flag、三端 | Web 默认false/主动勾选true测试；后端额外请求边界测试 |
| 观察点分网关/接入/DNS/代理/目标 | analysis/topology visibility | 报告字段与三端渲染；不是实际监听判断 |
| 每条发现有证据/时间/路径/替代解释/建议；截断不增强 | Analysis/EvidenceRef、三端 | 预算/证据缺失规则测试；Web截断提示及详情 |

## 联合验收与发布边界

| 计划条目 | 当前状态与缺口 |
|---|---|
| 阶段分别合入验收、混合版本能力展示 | 代码按层分模块，未在本清单声称已发布或合并；Agent最新Hello与实际binary独立校验，Core单独能力校验 |
| 共享客户端模型、Web生成契约、规则不重复 | 已接；请求 Probe/Egress 已直接采用生成契约；报告规则仅后端实现 |
| 默认只读、5秒状态/30秒DNS缓存、采样时间 | 有缓存/轮询/手动点击测试；公网探测未隐式随渲染启动 |
| 批量NAT独立必选，可取消该项 | 三端表单/后端校验；Web无选择不发请求、取消NAT后普通项目可运行测试 |
| 4并发项目/3路径/180秒含NAT排队，超时部分报告 | 后端 batch/scheduler/全局项目许可；端到端与排队竞态测试由整仓验证确认 |
| DNS/TLS/HTTP/IP/STUN可编辑预设，单项目失败隔离 | 原生 Ctrl+R/按钮加入五项可编辑预设，Ctrl+P 加入当前工具；共享计划支持三路径/删除/移回工具箱编辑/显式两轮对照，最新项目状态含等待NAT；共享客户端/GPUI/TUI回归通过，本地项目失败 fixture |
| 摘要复制与有界JSON，敏感输入不混入共享报告 | 三端输出；原生计划摘要隐藏DNS/HTTP/解析器URL，DER只进有界JSON；共享客户端与实际本地HTTPS令牌/URL路径脱敏断言通过，Web仅导出report/items并有浏览器断言 |
| 本地DNS/TLS/HTTP/STUN、丢包/截断/错误来源/取消 | 已有本地fixture；不依赖公共服务成功；Linux内核NAT九组合已通过 |
| 三平台TUN/物理绑定/网卡移除/Agent切换/IPv6/权限/DNS/根库/远程 | 三平台原生样本与各自实际 TUN 场景均已执行；物理绑定、Linux 接口身份失效停止与恢复、Agent 切换/真实远程链路、IPv6、macOS 普通/root 权限差异、DNS 与根证书样本均有对应证据；具体覆盖范围及环境限制见各平台记录 |
| 旧客户端/新身份、无能力/明确不支持/损坏/通信错误 | 四类协议 fixture 覆盖；真实旧 Agent 读取新 Core 身份并成功持久化，version 仍为 1；真实旧 Core 缺少 program-info 时新 API 显示能力无法确认且不发包，旧接口仍正常 |
| Rust/Web/契约/架构检查与窄窗口/键盘/滚动/三端语义 | Web桌面/390px真实浏览器已执行；原生GPUI 420×460窗口的12工具输入同步/批量键盘流程与TUI 44×14、22×10、5×3布局及CJK长记录滚动自动检查通过；三平台完整人工视觉矩阵为补充测试 |
| 无新增产品服务器监听/长期抓包/MTR/PMTU/历史数据库 | 未新增这些产品功能；STUN服务只存在独立测试设施，报告保留受明确预算限制 |

公共 STUN 地址来源、运行本地/namespace 测试的方法见 [NAT 测试说明](network-nat-testing.md)。API、限制与恢复语义见 [网络诊断实现说明](network-diagnostics.md)。

## 2026-11-09 原生客户端与 macOS 普通账户证据

- 使用仓库 `scripts/astra_tools/workspace.py::build_environment()` 执行：共享客户端库测试 262 项通过，TUI 库测试 222 项通过、7 项既有现场/子进程入口测试忽略，GUI 库测试 74 项通过；GUI `--all-targets` 检查通过。原始会话日志：`/tmp/astra-native-full-tests.log`；随后不带 `--lib` 的三包完整测试（含二进制、集成目标与文档测试）也通过，日志 `/tmp/astra-native-all-target-tests.log`。忽略项不计作通过，其中旧 live 网络工具仍要求完整构建后的 Manager/Core 集成环境。
- 新原生回归覆盖所有 12 工具的字段与请求、最多三路径计划、DNS/TLS 明确开启后两轮、NAT 禁止复制及每次重新手选同 Agent 网卡、编辑参数往返、Project 包裹的完整 DNS 文本、证书 DER 仅导出、后端 Findings 全字段及 DNS/连接/逐跳三个独立视图、来源与替代解释、关闭页面不取消、Stop 等终态和丢失回执不重新执行。GPUI 使用测试窗口实际布局/键盘分派，不代表人工视觉验收。
- 实际进程 `astra-proxy --network-diagnostics-stream`（`aarch64-apple-darwin`，构建时间 `2026-11-09T09:42:44Z`）以 UID 502、`elevated=false` 运行。选择物理接口 `en0`、域名目标与仅监听回环的显式 DNS fixture：macOS raw ICMP 返回 `Operation not permitted`，`TraceSummary.capability.supported=false`、`probes_sent=0`、`completed=false`，没有 Resolution/Trace 事件，DNS fixture 收到 0 包；没有提权或修改宿主 TUN/路由。
- 同一实际进程访问本地可控 HTTPS fixture：`rustls-native-certs: macos account-visible system root snapshot` 加载 158 个根，加载错误为空。正常验证器拒绝 fixture 证书，仍保存 1 张证书、803 字节 DER；没有发送 HTTP 应用数据。报告/错误均不含测试 URL 的私有路径和令牌。测试证书未导入系统根库。原始会话日志：`/tmp/astra-macos-native-core-evidence.log`。
- 以上实际进程样本验证普通账户的拒绝与系统根加载行为；后续 root 单跳、远程 Agent 和 Windows 原生样本另列，仍不代替 TUN、网卡热移除、企业根策略完整矩阵。

### 真实新版 Client → Agent → Core 本地闭环

`astra-client/src/test/network_live_test.rs` 提供需预先构建 `astra-proxy` 的独立现场测试，默认忽略以避免普通库测试误用陈旧二进制。本轮用 `build_environment()` 重建当前核心后显式运行通过，所有服务仅监听回环；日志 `/tmp/astra-live-client-test.log`。

- 八个项目通过真实管理协议入队、持久化、子进程执行与报告分页：两个 DNS 服务、单端口 TCP、IPv6 TCP、TLS、HTTP、失败的 HTTPS 和 STUN。DNS 两轮查询计数各为 2，并由实际事件产生后端 `dns_difference`。
- HTTP 63 KiB 预览连同其他证据使报告超过 64 KiB，真实分页无缺失；从原任务 ID 重新读取终态报告，事件序列完全相同。测试 URL 路径/令牌和 Set-Cookie 内容不在报告中。
- 正常 TLS 验证失败后保留证书与内置根离线对照，服务端收到的 HTTP 应用数据为 0；STUN 返回有效绑定结果并正确结束项目；IPv6 回环连接报告实际族及成功状态。
- 另起 50 次 DNS 请求，客户端等待取消确认后计数不再增长。未修改宿主路由、TUN、系统根证书或启动被诊断实例。
- 原生分页额外拒绝游标倒退/停滞、重复乱序项目和超过 64 KiB 的页，观察器累计保留上限 2 MiB；不把坏页重试当作新诊断，也不无限累积重复证据。

### 当前 macOS 生产实例只读状态核对

通过既有管理端点只读查询 Hello、Instances 和 Runtime，采样 `1794219391601` ms：实例 `m1` 的 Instances PID 与 Runtime PID 同为 `99006`，Runtime `tun=false`、`system_proxy=true`、`mode=rule`、`truncated=false`。另一个同账户 proxy 进程为探测辅助进程，不能当作启用 TUN 的用户实例。没有据此声称 macOS TUN-on 已覆盖，也没有修改用户配置、实例状态、TUN 或路由。仅保留必要状态的日志为 `/tmp/astra-user-runtime-audit.json`。

### macOS 实际物理网卡与 root 单跳

- macOS 明确绑定物理 `en0`，源地址 `192.168.137.152`，访问 Windows 任务专用 STUN fixture `192.168.137.1:52220`。两次请求均收到严格校验的响应，`partial=false`，`binding_verified=true`、`route_verified=true`，真实子进程流含完整 `Done`。日志 `/tmp/astra-macos-physical-nat-evidence.log`。该样本证明物理出口绑定和基本映射采集；fixture 未提供 RFC5780 双 IP/双端口能力，不据此声称高级 NAT 分类完成。
- 用户在本机终端认证后，root 一次 UDP+raw ICMP Trace 经 `en0` 到 `192.168.137.1`：`probes_sent=1`、`reached=true`、`completed=true`，物理绑定及路由证明均为 true。日志 `/tmp/astra-macos-root-path-fj1abgnf.json`；未修改宿主网络/TUN。
- Windows 的现有生产代理亦经 Runtime 与稳定 CoreIdentity 只读核对为 `tun=false`，不能用虚拟适配器存在推断 Astra TUN 已启用。具体身份与采样时间见 [Windows 原生验证](network-windows-validation.md)。

### 真实 macOS Client → TLS Controller → Windows Agent

使用仅监听本机私有局域网 IP 的临时 Controller、独立一日测试 CA、独立 Agent 凭据和 Windows 临时状态目录，未修改生产实例、系统根、TUN、路由或防火墙。共享客户端测试 `remote_controller_routes_shared_client_tools_pages_and_cancel_to_target_agent` 于本轮显式执行通过，日志 `/tmp/astra-remote-client-test.log`。

- 目标 `windows-network-test` 的网络快照采样为 `1794220733827` ms，共 26 个接口；接口来源于 Windows Agent，未误用 Mac 客户端网卡。
- 实际批量任务 `remote-live-1794220733313` 完成 Windows 回环 DNS、TCP IPv4、TCP IPv6、HTTP 和 STUN 五项，通过 Controller 转发、真实 Agent 排队及新版 Core 子进程返回 22 页、92,688 字节证据；终态任务与项目均完成，原任务 ID 恢复的报告一致，测试 URL 令牌没有进入导出。
- 50 次 DNS 的任务 `remote-cancel-1794220733313` 在首次探测后取消；取消确认后 700 ms 报告和游标不变。Windows fixture 的独立计数样本 `1794220776232` / `1794220777782` ms 均为 DNS 2、TCP IPv6 1、HTTP 1、STUN 1，确认取消后 DNS 不再发包。
- 测试结束后通过协议关闭 Windows 临时 Agent，并终止专用 Mac Controller；其临时令牌和两个私钥已删除。上述验证覆盖真实 TLS 远程链路与分页/取消，不等于企业根策略或 macOS TUN-on 验收。

### 原生可点击路径证据

GUI 路径区直接渲染后端 `AnalysisReport`，按 DNS、连接、逐跳分组；节点与标为实测、配置推断或未知的连接均可点击。详情包含项目、路径、轮次、采样时间、来源及精确匹配的原始事件；事件没有保留时明确说明，不根据引用补造数据。NAT 仅为可点击旁注，不作逐跳设备。长标签可通过提示与详情读取，证书 DER 仍仅保留在 JSON 导出，切换任务清除旧选择。TUI 保留相同语义的文字表格。

新增 GPUI 420×460 窗口的真实指针点击节点/边测试和证据等级/缺失引用回归通过。补完后 `cargo test -p astra-gui` 全部 76 项通过（含二进制/集成目标与文档目标），`cargo clippy -p astra-gui --all-targets -- -D warnings` 通过，均使用 `build_environment()`；日志 `/tmp/astra-gui-topology-test.log`、`/tmp/astra-gui-topology-final.log`。这些测试验证可点击行为及布局执行，不代替三平台人工视觉检查。

### 真实混版本兼容

使用仓库现有 `dist/2026-11-08_18-44-07_970252/release/client` 中的真实旧 `astra-manager` / `astra-proxy`，与当前共享客户端、新 Agent 及重建 Core 组合，执行 `real_mixed_versions_keep_legacy_tools_and_reject_unsupported_probes` 通过。三个组合各使用独立临时 state/config/runtime/IPC，服务只监听回环。日志 `/tmp/astra-mixed-version-live-test.log`。

- **新客户端 → 旧 Agent → 旧 Core**：Hello 正确归类 Legacy；共享 API 的 TLS 提交和原生模型的 TLS/ICMP 均明确拒绝。透明 IPC 转发器计数确认这些操作没有向真实旧 Agent 发出 Execute。原 `NetTool` TCP/DNS 成功，仍为旧 Started/Attempt/Summary/Done 结果，没有新事件混入。
- **新客户端 → 新 Agent → 旧 Core**：这个旧发行版连 `--program-info` 都不支持，因此新诊断 API 的 TLS、TCP、DNS 返回“能力无法确认”，不偷偷降级。旧 `NetTool` 入口 TCP/DNS 仍成功；监听和计数证据确认 TLS 连接为 0、TCP 与 DNS 仅旧入口各发生 1 次，没有新 API 的探测流量。
- **新客户端 → 旧 Agent → 新 Core**：旧 TCP/DNS 入口继续成功且保留旧事件。真实旧 Agent 启动新版 Core，使用旧 bincode 解码器完成身份持久化与服务提交，并成功读取 Runtime。旧持久记录不含 capabilities；新版身份读取可见 4 项能力，连续新连接的完整 Identity 相同，PID/boot_id 与旧 Agent 记录一致，`Identity.version` 仍为 1。
- 每个组合结束均由管理协议 Shutdown 并等待进程退出，临时目录自动移除；没有改动生产配置、TUN、路由或系统根。该现场测试默认忽略，必须显式提供旧版目录后执行，避免普通测试依赖遗留二进制。

混版本测试最终断言通过后，`cargo clippy -p astra-client --all-targets -- -D warnings` 也通过（`/tmp/astra-client-mixed-final-check.log`）；进程列表复核没有本轮临时 Agent/Core 残留，跨机 Controller 的临时目录也已删除，保留的仅为上述脱敏验收日志。


### 本轮最终检查与清理

- 管理端在 Windows 栈修复后的全库回归为 166 项通过、11 项既有现场入口忽略，含 1 MiB 线程握手/转发和真实更新事务：`/tmp/astra-final-manager-after-windows-fix.log`。最新 TCP 分类、NAT/STUN 流结束回归 5 项通过：`/tmp/astra-final-typed-tcp-tests.log`。
- 11 个改动 Rust crate 的 `--all-targets` Clippy（包含 management bindings，`-D warnings`）通过：`/tmp/astra-final-clippy.log`；第三方 vendored `meow-proxy` 的两个既有弃用警告另列，不冒充本次新增错误。随后混版本客户端与测试桥示例的定点检查也通过。
- 格式、构建元数据、架构边界、差异格式与生成契约检查全部通过：`/tmp/astra-final-static-checks.log`。最新 Web 全套 143 项与类型检查通过：`/tmp/astra-final-web-tests.log`、`/tmp/astra-final-web-typecheck.log`；真实浏览器最后结果文件为 `apps/web/test-results/.last-run.json`，状态 passed。
- Linux 专用 `astra-network-test` VM 已停止，未触碰其他 Colima profile。Windows 最终清理证据 `/tmp/astra-windows-final-cleanup.json` 中临时 Agent/Core、Python fixture、测试适配器和路由均为空；测试 token/CA 不存在。
- `scripts/test_macos_isolated_tun.py` 已由用户认证后实际运行通过，证据 `/private/tmp/astra-macos-isolated-tun-oxq6unte.json`，`passed=true`、`cleanup_verified=true`。独立检查也确认 `utun200` 与专用路由消失、原实例状态一致。最后一个 Windows STUN fixture 已提前停止，端口/临时进程为空（`/tmp/astra-windows-stun-cleanup-21080.json`）。按用户要求保留已提权的 Terminal 会话，不关闭该终端。


### macOS 独立 TUN 的最终实测

用户认证后，脚本在独立临时目录启动 PID `62474` 的测试 Core，Runtime 明确为 `tun=true`、`system_proxy=false`、`allow_lan=false`。只创建 `utun200`、`198.19.254.1/30` 及专用 `198.19.254.2/32` 路由，不添加默认或半默认路由。最终证据为 `/private/tmp/astra-macos-isolated-tun-oxq6unte.json`。

- 开启前，显式绑定 `en0` 对 Windows 私有 fixture 的两次 STUN 均成功；开启后新 socket 的两次请求也成功，两个阶段的 binding/route 证明均为 true。服务端 `/tmp/astra-windows-stun-session-21080.jsonl` 独立记录四个匹配事务；仍只作基础映射采样，不推断 RFC5780 分类。
- 普通系统 ping 对测试 TUN 目标收到真实本地 echo；新诊断仍返回 `tun_local=true`、`proves_reachability=false`，来源/RTT 为空，没有解析或连接事件。
- System Trace 明确拒绝：真实 `TraceSummary` 为 `probes_sent=0`、`supported=false`、`reached=false`、`completed=false`，无 Resolution/Trace/Connection/Attempt 事件。直接 Core 命令用退出码 1 表达拒绝；验收脚本仅在这一个预期拒绝分支接受该退出码，其余探测必须退出 0 且收到 Done，未放宽产品行为。
- 选择 `utun200` 的 NAT 报告 Partial、无 observations；选择 `en0` 的 NAT 两次响应均成功。普通 TUN echo 不能冒充物理路径证明。
- Finally 关闭自己的 TUN、停止并回收临时 Core，核验接口与路由消失、默认/半默认路由和系统代理相同。生产 m1 前后均为 PID `99006`、`tun=false`、`system_proxy=true`、`mode=rule`、`allow_lan=true`，未重启或改写。

此前过期 fixture 的预检失败均在创建 TUN 前停止；一次测试脚本误要求被拒绝的 System Trace 也输出 Done，已按实际协议修正后重跑。保留失败记录，不将这些轮次算作通过。最终 PASS 的完整流程和独立服务端收包/清理证据才用于本轮验收。

## P2：旧接口 DNS 限制与类型化错误收口

GUI/TUI 与 Web 统一旧 Agent 的准入规则：保留系统 TCP、UDP/TCP DNS 和不需要主机名解析的网卡端点；拒绝配置出口（含 DIRECT）、网卡域名、自定义解析器、bootstrap 及新版工具选项。Rust/Web 共用 49 个准入样例，包含 IPv6、合法末尾斜杠、非法 authority 和 bootstrap。旧 RPC/CLI 仍保留，系统 DNS 的已知限制及新 Agent 对旧 Core 的严格降级条件见 [诊断文档](network-diagnostics.md)。

Agent、进程内 NativeClient、Controller、存储和客户端的相关错误分类改用类型。任务完成和取消通知不再读取文字关键词；SQL 故障与资源缺失、队列满与通道关闭、未入队失败与写入回执丢失分别分类。保存详情保留原始证据，未知结果不会宣称未写入；外层错误不能掩盖内层的结果不确定性。公开错误码、管理协议和数据库格式未改。

本轮验证：

- Manager 库 186 项通过、11 项既有入口忽略；最终分类优先级调整后，错误分类、保存详情和关闭不确定性回归再次通过。日志 `/tmp/astra-p2-manager-tests.log`、`/tmp/astra-p2-final-error-check.log`。
- Client 270、GUI 76、Management 22、TUI 222 项通过；Client/TUI 分别有 3/7 项既有入口忽略。日志 `/tmp/astra-p2-native-tests.log`。Manager 使用短临时目录避免 macOS socket 路径长度限制，客户端权限测试保留默认账户临时目录的所属组语义。
- Web 全套 201 项通过，类型检查通过。日志 `/tmp/astra-p2-web-tests.log`。
- 重建当前 Core 后显式运行混版本现场测试：旧 Agent/旧 Core、新 Agent/旧 Core、旧 Agent/新 Core 均通过，旧 TCP/DNS 成功且不支持的探测不发包；各组合正常 Shutdown。日志 `/tmp/astra-p2-mixed-versions.log`。
- 五个相关 Rust crate 的 all-targets Clippy（含 management bindings、`-D warnings`）、生成契约、格式、构建元数据、架构边界和差异格式检查通过。日志 `/tmp/astra-p2-clippy-contract.log`。

本轮未重跑三平台完整网络/TUN 验收；此前现场证据仍按各自范围保留。用户要求保留的 root Terminal 会话保持打开。

## NetTool 环境隔离：准确性优先

用户确认采用应用运行环境隔离，保留真实主机网络。Agent 启动的新详细诊断、能力查询和旧 System/Interface 探测使用每次独立的私有 runtime/临时目录，默认隔离工作目录并去除继承的 Astra 启动控制变量。真实账户权限、路由、TUN、DNS 和系统信任来源保持原义；相对证书/DNS 文件和 PATH 在切换工作目录前固定，证书目录空项不变成额外信任来源。动态加载环境或路径列表无法无损改写时保留原 cwd 和完整路径输入，仍隔离 runtime/临时目录。旧 Outbound 为保持订阅缓存及相对资源的准确性保留原 runtime/cwd。独立 `--nettool` CLI 保留调用者环境，System/Interface 即使提供配置也不初始化完整 App。完整边界见 [环境与准确性说明](network-diagnostics.md#诊断进程环境与准确性边界)。

验证记录：

- 管理端全库 195 项通过、11 项既有现场入口忽略；NetTool 核心 66 项通过。日志 `/tmp/astra-nettool-isolation-regression.log`。
- 最终环境回归 11 项及实际 metadata/probe 同环境启动回归通过，覆盖并行目录独立、原环境保留、相对信任文件、空证书变量、取消后清理、失败退出、未确认回收保留、旧 Outbound 例外，以及路径列表无法改写时的原子回退。日志 `/tmp/astra-nettool-environment-final.log`。
- 两项实际 Core 入口测试通过：System CLI/旧 stream 不加载错误配置且保留原缓存；旧 Outbound 仍要求配置。日志 `/tmp/astra-nettool-isolation-proxy.log`。
- 重建实际 Core 后，真实 Agent/Core 的工具批次、报告分页和取消测试通过；旧 Agent/旧 Core、新 Agent/旧 Core、旧 Agent/新 Core 三组组合再次通过。全部网络目的地均为本地受控 fixture，日志 `/tmp/astra-nettool-isolation-live.log`。
- Manager/Proxy 全目标 Clippy `-D warnings`、格式、架构、构建元数据和差异格式检查通过。第三方 `meow-proxy` 保留两个既有弃用警告。日志 `/tmp/astra-nettool-environment-final.log`、`/tmp/astra-nettool-isolation-clippy.log`。

本轮执行环境为 macOS；Windows drive-relative 路径回归已添加，但未在 Windows 实机运行，也未重复 Linux/Windows 或 TUN 现场矩阵。未修改生产网络设置，按要求保留现有 root Terminal。

## 结论视图、结果颜色与 NAT 类型

GUI/TUI/Web 默认显示结论，Ctrl+D 或可见按钮展开高级信息。运行快照、原始证据和拓扑默认收起，错误、未确认状态、截断与不完整提示保持可见。展示开关不改变请求或原报告，JSON 仍保留完整有界证据。成功、注意、失败/风险、普通信息与详细证据使用不同颜色和文字标识；通用收发计数只作信息，不能覆盖 NAT 未分类、STUN 拒绝或逐跳实测结果。项目状态默认只显示每个项目/路径/轮次的最新状态。

NetTool 的 F1–F12 分派已移除，GUI/TUI 同时处理了全局快捷键抢占。Ctrl+L 切换分区、Ctrl+P 加入项目、Ctrl+R 加预设、Ctrl+O 停止观察、Ctrl+Y 复制文本、Ctrl+E 复制 JSON；TUI 支持 Ctrl+↑/↓ 滚动。上层菜单继续使用其自身快捷键。

NAT 默认请求类型识别，中文展示完全锥型、受限锥型、端口受限锥型、对称型及未观察到地址转换。能力、稳定性、完整性或物理路径证据不足时显示未能确定。公共基础 STUN 预设未保证 RFC 5780 能力，表单明确提示服务要求。后端优先选择声明完整类型探测能力的候选服务，并实际验证；明确 STUN 拒绝/无效报文不能冒充过滤超时，无地址转换但有过滤不能误标为锥型。

验证记录（本次改造）：

- Client 全库 279 项通过，3 项既有现场入口忽略；Management 22 项通过；GUI 78 项通过；TUI 最终全库 229 项通过，7 项既有现场入口忽略。覆盖默认折叠、颜色与换行滚动、输入框内切换、真实全局按键分派、状态历史保留、NAT 未知与收发计数、STUN 拒绝、截断 DNS 等。日志 `/tmp/astra-nettool-conclusions-regression.log`、`/tmp/astra-nettool-ui-native-final-full.log`、`/tmp/astra-nettool-final-pass.log`。
- Core NetTool 70 项通过，其中 NAT 12 项包含九种映射/过滤组合、传统类型、无翻译但有过滤、多个服务候选和拒绝/无效应答。日志 `/tmp/astra-nat-type-regression.log`。
- Web 232 项测试和类型检查通过；真实 Edge 浏览器完成桌面与 390px 窄屏回归，检查按键切换、结论配色、长 DNS 摘要和完整证据。日志 `/tmp/astra-nettool-ui-web.log`、`/tmp/astra-nettool-ui-web-typecheck.log`、`/tmp/astra-nettool-ui-e2e.log`。
- Client/GUI/TUI/Proxy 全目标 Clippy、生成协议契约、格式、架构边界、构建元数据和差异检查通过。协议版本、公开错误码及原有执行/取消机制未改动。未重跑三平台完整网络与 TUN 现场矩阵；未操作终端会话或生产网络设置。

### 结论与原始报告左右双栏

GUI/TUI/Web 展开报告后保留左侧结论，完整报告放在右侧，不再切换或混排。默认继续只显示结论，Ctrl+D 和按钮控制右栏。GUI 两栏独立横向/纵向滚动，路径拓扑放在右栏；TUI 按 40/60 左右分栏并分别换行；Web 两栏独立纵向滚动，窄屏仅在结果区域横向滚动，保持左右排列。展示调整不改变任务、原始事件或 JSON 导出。

本次 Client NetTool 35 项、GUI 相关 7 项、TUI 相关 8 项通过（1 项既有 live 入口忽略）；Web 233 项、类型检查和真实 Edge 桌面/窄屏回归通过。新增几何断言验证左右位置、长报告不扩张窗口、右栏滚动不移动左栏。Client/GUI/TUI 全目标 Clippy 及本次修改文件的格式检查通过；全仓库格式检查发现其他并行 DNS 改动尚未格式化，未修改这些无关文件。日志 `/tmp/astra-nettool-columns-{client,native,web,typecheck,e2e,checks}.log`。
