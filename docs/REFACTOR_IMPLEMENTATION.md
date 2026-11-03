# 全项目公共库与独立管理系统实施记录

日期：2026-11-03。起点为 `aad07fc`（`CP_Before_rework`），开始时工作区干净。本记录区分已实现代码、已执行验证和仍需外部环境的验收；没有把未执行的平台验证记为通过。

## 已落地的代码

| 范围 | 实现 |
| --- | --- |
| 公共模型与基础能力 | 新增 model、platform、resource；配置声明、纯校验、网络/控制 DTO、日志、私有端点、原子文件与资源加载归位；配置准备和实际应用留在核心。 |
| 依赖方向 | protocols/dns/rule-data/convert 消费 model；manager、management、Web、TUI 不链接代理或服务端实现；核心不依赖界面。架构检查覆盖正常依赖和脚本入口互导。 |
| 管理服务 | 独立 Controller/Agent、管理 v2、稳定实例 ID、SQLite WAL 专用线程、操作幂等和恢复、共享变更串行入口、持久草稿；原 TUI 管理器迁入 manager，生产不再绑定旧 manager.sock。 |
| 安全连接 | Agent 主动 WSS、TLS 身份校验、独立节点凭据、轮换/撤销、连接代际、心跳/退避和重连快照；网页独立 Argon2id 管理员、内存会话、限速、同源检查与脱敏审计。 |
| 公共应用与终端 | app 文档/字段/引用/导入历史能力；tui-kit 面板、真实列表几何、Unicode 输入与终端生命周期；两个薄 TUI 入口共用应用，后台任务不再构造第二个 Ui。 |
| 正式网页 | apps/web 的 React/TypeScript 页面消费真实节点、实例、运行状态、日志、任务、草稿/schema 与更新接口；独立 astra-web 嵌入资源，核心构建不依赖 Node。 |
| 保存与更新 | 保存结果区分已应用、下次启动、拒绝、恢复和未知；迟到保存不覆盖新编辑。制品按块和整体摘要校验，上传、导入、激活分离；失败更新不会被任务索引记为完成。 |
| 工具和安装包 | astra_tools 统一构建布局、角色、产物索引、进程、网络 fixtures 和 runner；client/server/controller/web 四包，all 构建全部角色。 |
| 清理与迁移 | 删除旧 server-api/server-tui 和模拟 GUI 实现；原型样本只留测试 fixtures；shared_* 和多余 path 模块归位。旧状态保留/备份，未知状态和未来版本拒绝覆盖，身份库丢失拒绝静默重建。 |

部署说明见 [MANAGEMENT](MANAGEMENT.md)、[INSTALL](INSTALL.md)、[SERVER](SERVER.md)；前端开发说明见 [apps/web](../apps/web/README.md)。

## 已执行验证

| 验证 | 结果与证据 |
| --- | --- |
| Windows 工作区 | Cargo workspace unit/integration、真实在线更新连接保持、严格 workspace/all-targets Clippy、fmt、契约生成检查与 diff 检查通过。日志位于 `.test-tools/final-core-regressions.log`、`final-static-checks.log`。 |
| 客户端/服务端回归 | 统一 runner 13 步通过，包括订阅完整定时刷新周期、DNS、规则、代理组、Windows 普通代理路径、19 种服务端协议组合和生命周期。见 `.test-tools/final-core-regressions.json`；初次失败和有依据的 fixture 修正记录另外保留。 |
| 真实 TUI | 6 个需真实核心的 live 测试通过，使用 Agent v2，覆盖表单、导入、路由解释、网络工具事件和错误恢复。 |
| 管理与浏览器 | 最新四包解压后，17 项真实场景通过；Playwright 实际登录/双节点切换/字段编辑/草稿恢复/服务端启停/窄屏检查通过。见 `.test-tools/management-scenario.json`、`final-acceptance.json`。 |
| 网页单元与组件 | Rust Web 认证/Origin/会话/权限边界 3 项通过；TypeScript 检查、生产资源构建和 7 项 API/组件测试通过。 |
| 公共工具 | Python 15 项测试与依赖架构检查通过。 |
| Linux | WSL2 中全 workspace/all-targets check 通过；8 个库共 112 通过，7 个原有 ignored，1 个需完整 Linux 程序的升级测试明确过滤。详见 [Linux 记录](LINUX_REFACTOR_VERIFICATION.md)。 |
| Linux GitHub Actions | 第三轮完整 workspace unit/integration 通过，含真实在线更新和 ICMP loopback；严格 workspace/all-targets Clippy、真实管理 17 项、浏览器与四包验收通过。此项为新增证据，不覆盖上面的 WSL 历史结果。 |
| macOS | 第三轮原生 macOS 15 ARM64 与 Intel 全部通过：四包构建、完整 workspace 检查/测试（含真实在线更新）、IPC 20 项、TUI 6 项 live、manager 7 项 live、客户端 7 组、服务端 19 种协议及生命周期、17 项管理与浏览器场景和四包验收。每架构核心 runner 14 步、管理/包 runner 5 步全部通过；详见 [macOS 记录](MACOS_REFACTOR_VERIFICATION.md)。 |
| 额外参考协议矩阵 | 21 组中 20 组通过。TCP 矩阵保留 Xray Windows EOF 超时限制，未改为成功；见下文。 |

管理联测包含：Controller 同时连接两个真实 Agent；客户端与服务端分别运行；中央/本地编辑冲突；断开 Controller 时核心继续传输且本地可管理；重连恢复任务；重复请求与实际 Agent 重启保持幂等；凭据在线轮换/撤销；网页崩溃独立重启；关闭 Web 不产生子进程；真实版本包断点分块上传、校验、暂存且不隐式激活。

最终 Windows 四包及浏览器复验报告为 `.test-tools/final-windows-post-ci.json`（5 步全部通过）和 `.test-tools/final-windows-post-ci-management.json`（17 项全部通过）。浏览器截图保存在本机 `C:/Users/minecraftgive/AppData/Local/Temp/astra-management-xutxmcji/browser/`。测试中的进程均已清理，临时工具和报告保留供复核；早期截图和报告继续保留。

源码 `1e3be94cdd470fa460800472cdc0da69c135effd` 的 [第三轮 CI 41567249779](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779) 最终为 `completed / success`，10 个 job 全部通过；三平台工具/架构、Linux/Windows 编译、Web 和上述原生 macOS/Linux 验收均包含在内。证据已下载至 `.test-tools/github/41567249779/`，macOS 两份 ZIP 摘要已核验，核心/包报告及双 macOS、Linux 的 17 项管理场景 JSON 均为 `passed=true`。前两轮的路径断言与 IPC 取消失败及修复记录保留在 [macOS 记录](MACOS_REFACTOR_VERIFICATION.md)。

## 本次验证安装包

最终 Windows x86_64 MSVC **Debug 验证包**位于 `dist/2026-11-03_11-19-12_403702/`，索引为 `artifacts.json`，`source_revision` 为已通过 CI 的 `1e3be94cdd470fa460800472cdc0da69c135effd`。这批包包含最终 IPC 取消修复，已按该索引独立解压并完成真实浏览器管理复验。早期 `dist/2026-11-03_08-40-08_678845/` 仅作为历史验证产物保留，不代表最终代码。

| 角色 | tar.gz SHA-256 |
| --- | --- |
| client | `a095986db3bf683e4fe9750a2a1ccf9a07a7e4f64f4c8865c4907a5930dd7a8b` |
| server | `bf9074553284f05631d22be1eaaf13985875b20032910d35f5edcd725d8f6ecb` |
| controller | `73770c0c9db444d0a18f0ac9668fcdfd6b689265a899f6201b5a38470bbe1ab7` |
| web | `a2977c467dc6e7b9be6af0f9e1aed5c5cbf7a7c2b0e2daeb9e6e146cb1bba684` |

四包均独立解压启动验收通过；服务器包不含 Proxy/Rule 客户端核心。更新库接受新清单的 client role 和未带 role 的旧客户端包；其他服务包明确要求相应服务的维护重启，不冒充 worker 热更新。

## 尚未关闭的验收边界

- **未执行真实系统代理变更、TUN/默认路由接管或交互提权。** Linux WSL 验证也不替代原生平台网络接管验收。
- **Windows Xray 参考端关闭限制仍保留失败。** 原始 TCP 矩阵 24 例中 4 例在完整回显后等待 EOF 超时；部分复跑通过。无 Astra 的 Python TLS/VLESS/HTTPUpgrade 对照，在精确 close_notify + TCP FIN 后仍复现 4/8 次 EOF 超时，全部 8 次完整回显成功。UDP、认证/TLS/Reality 拒绝、Vision 8 MiB、sing-box 严格半关闭、TUIC 640 并发复用和 Mux 均通过。完整证据见 `.test-tools/final-protocol/report.json` 与 `reference-tls-fin.json`；总报告保持 `all_passed=false`。
- 此处没有声称已完成优化 Release 包、跨真实主机部署或所有平台生产验收，也没有扩大既有历史报告的结论。

按原计划“所有平台与真实系统接管均验收才算完成”的标准，以上边界关闭前，不将本次标记为全部验收完成。
