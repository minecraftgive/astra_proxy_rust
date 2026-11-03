# macOS 原生重构验收记录

日期：2026-11-03。第三轮 GitHub Actions 的 **10 个 job 全部通过**，包含原生 macOS 15 ARM64 与 Intel 的完整构建、workspace 测试、真实在线更新、核心回归、管理浏览器和四包验收。首轮路径断言及第二轮 IPC 取消清理失败均已修复并由第三轮验证；下文保留前两轮失败记录。真实系统代理、TUN、提权等未执行边界仍保留。

## 环境与执行范围

工作流为 [Checks](../.github/workflows/ci.yml)，两个架构各自从检出的源码构建和运行，没有使用交叉编译结果替代原生执行：

| Runner | 本机构建目标 | 安装包 |
| --- | --- | --- |
| `macos-15`，ARM64 | `aarch64-apple-darwin` | client、server、controller、web |
| `macos-15-intel`，Intel | `x86_64-apple-darwin` | client、server、controller、web |

每个 job 校验 Rust host 与预期目标一致，安装 Python 3.13、Node 22、稳定版 Rust、CMake/Ninja/NASM/LLVM/pkg-config/OpenSSL 3，并安装项目锁定版本对应的 Playwright Chromium。Cargo/npm 下载缓存仅用于提速，不要求预先存在本地二进制、浏览器或参考协议服务器。

验证使用 **Debug 包**，关闭调试符号以适应 runner 的内存和磁盘。临时目录采用权限为 `0700` 的短路径，测试可执行文件复制到 `/tmp/astra-bin`，避免 macOS Unix socket 路径长度限制；实际包验收仍使用本轮 `artifacts.json` 指向并校验摘要的四个归档。

主要命令如下；两个变量由构建产物索引设置，不能指向其他构建批次：

```sh
python scripts/build.py --role all --profile debug --package --non-interactive
cargo run --locked -p astra-management --bin export-contracts -- --check
npm --prefix apps/web test
python scripts/test.py check unit live client server --debug \
  --bin-dir "$ASTRA_CI_BIN_DIR" --keep-going --report .test-tools/macos-core.json
python scripts/test.py package management --debug \
  --artifact-dir "$ASTRA_CI_ARTIFACT_DIR" --browser --keep-going \
  --report .test-tools/macos-management.json
```

`unit` 同时执行 Python 工具测试。第二轮的 Rust workspace 测试增加 `--no-fail-fast`；`live` 除 TUI 外，新增执行已迁往 manager 的六个 `live_` 测试，第三轮再增加一项真实 AMV2 持久任务回归，共七项。独立三平台 tools job 执行 `tools architecture`。

## 首轮：已结束，整体失败

[Run 41560051267](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41560051267)，源码为 [`b772c630e7cd21f33a26c072cee49d287f1c622a`](https://github.com/minecraftgive/astra_proxy_rust/commit/b772c630e7cd21f33a26c072cee49d287f1c622a)。[ARM64 job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41560051267/job/125668617342) 和 [Intel job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41560051267/job/125668617203) 均已完成。两者结果一致：

| 验证项 | ARM64 | Intel | 实际覆盖与限制 |
| --- | --- | --- | --- |
| 四角色原生构建和打包 | 通过 | 通过 | Web 真实前端资源随包构建；产物索引与归档摘要校验通过。 |
| Rust workspace/all-targets check | 通过 | 通过 | 全工作区编译检查。 |
| TypeScript 契约及前端测试 | 通过 | 通过 | 契约检查通过，前端 7 项测试通过。 |
| Python 工具测试 | 14 通过、1 失败 | 14 通过、1 失败 | 归档路径断言未处理 `/tmp` 与 `/private/tmp` 别名。 |
| Rust workspace unit/integration | 未完整通过 | 未完整通过 | 转换器诊断路径断言失败后，Cargo 默认 fail-fast 停止后续目标；不能把未执行 crate 记为通过。 |
| TUI 真实核心 live | 6 项通过 | 6 项通过 | 表单校验、导入、编辑保存、恢复和规则查询等真实核心流程。 |
| manager 独立 live | 未执行 | 未执行 | 当时 runner 尚未登记迁入 manager 的六个 ignored live 测试，第二轮补齐。 |
| 客户端脚本 | 7 组通过 | 7 组通过 | 订阅诊断、DNS、路由日志、规则集、代理组、定时订阅、多订阅。Windows 专属场景不在 macOS 执行。 |
| 服务端脚本 | 2 组通过 | 2 组通过 | 19 种协议组合的 TCP/适用 UDP，以及启动回滚、重载、连接保持和停止清理。 |
| 真实管理与浏览器 | 17 项通过 | 17 项通过 | TLS Controller、双 Agent、独立 Web 和 Playwright 实际浏览器。 |
| 解压后四包验收 | 全部通过 | 全部通过 | client 实际 SOCKS 回显/TUI IPC；server 独立生命周期；controller/web 可执行文件和包内容校验。 |

管理场景实际验证 HTTPS 会话与 Origin/Host 边界、节点选择与实例隔离、客户端/服务端启停、双端草稿冲突、操作去重、分块上传与完整摘要、按真实 release ID 暂存且不激活、SSE 游标恢复、Web 崩溃监督重启、禁用 Web 不产生子进程、在线凭据轮换/撤销、Controller 断开后的核心流量和本地管理、重连与 Agent 重启后任务持久性、离线变更拒绝、退出登录和审计脱敏。

首轮不能证明完整 workspace 测试或真实在线更新连接保持父测试已经在 macOS 通过；制品上传暂存通过也不等于已执行激活。两项路径断言失败保留为首轮失败，没有改变生产路径规范化语义来迎合测试。

## 修复与补齐

| 提交 | 修改 |
| --- | --- |
| [`5022b85`](https://github.com/minecraftgive/astra_proxy_rust/commit/5022b850c505387670e674c86d5589ba3fe799c1) | Python 工具测试比较 `archive.resolve()`，并主动使用 `alias/..` 覆盖非规范路径。该问题也曾在 Windows runner 的 `RUNNER~1` 与规范用户名目录之间出现。同批补齐 manager live 登记及跨平台 fixture 适配。 |
| [`6070208`](https://github.com/minecraftgive/astra_proxy_rust/commit/6070208509da8ea9202ab8e0d1e4cd4f1c01206e) | 转换器测试解析诊断中的绝对路径，规范化后比较文件身份，仍检查单一来源分组和原始行号；Rust workspace 测试增加 `--no-fail-fast`，让后续 crate 也产生结果。 |

## 第二轮：已结束，唯一 macOS 失败为 manager 取消清理

[Run 41562660905](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41562660905)，源码为 [`6070208509da8ea9202ab8e0d1e4cd4f1c01206e`](https://github.com/minecraftgive/astra_proxy_rust/commit/6070208509da8ea9202ab8e0d1e4cd4f1c01206e)。[ARM64 job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41562660905/job/125676645945) 和 [Intel job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41562660905/job/125676645708) 均已结束，最终结论均为失败；两者核心 runner 都只报告 `Failed: manager live`。

| 验证项 | ARM64 | Intel |
| --- | --- | --- |
| 四角色原生构建和打包 | 通过 | 通过 |
| TypeScript 契约及前端测试 | 通过，前端 7 项 | 通过，前端 7 项 |
| Python 工具测试 | 15 项通过 | 15 项通过 |
| Rust workspace/all-targets check | 通过 | 通过 |
| Rust 完整 workspace unit/integration | 通过 | 通过 |
| 真实在线更新连接保持父测试 | 通过 | 通过 |
| TUI 真实核心 live | 6 项通过 | 6 项通过 |
| manager 真实核心 live | 5 项通过、1 项失败 | 5 项通过、1 项失败 |
| 客户端脚本 | 7 组通过 | 7 组通过 |
| 服务端脚本 | 19 种协议组合及生命周期通过 | 19 种协议组合及生命周期通过 |
| 真实管理与浏览器 | 17 项通过 | 17 项通过 |
| 解压后四包验收 | 全部通过 | 全部通过 |
| Job 最终结论 | 失败 | 失败 |

首轮的两处路径断言在第二轮均已通过。`--no-fail-fast` 使完整 workspace 测试执行结束，其中 `real_update_transaction_preserves_live_connections` 使用本轮真实程序完成在线更新与连接保持；独立 `live` 步骤另行执行 TUI 和 manager 的 ignored 场景，不把 workspace 中未启用的 ignored 项算作通过。

两个架构唯一失败的测试均为 `manager::tests::live_diagnostics_use_running_snapshot_keep_manager_responsive_and_cancel_child`。日志定位到当轮 `crates/astra-manager/src/manager.rs:2444`：取消诊断请求后，等待上游连接 EOF 的 3 秒期限超时。该失败不是路径断言，也没有通过放宽超时或删除断言处理。

代码排查已确认当轮 `astra-ipc` 的 `Peer` worker 仅在开始处理请求前检查 `reply.is_closed()`，进行中的请求没有持续监听调用方取消，因此取消可能未及时释放上游请求。该问题已在 `2050f8a` 修复，通过本地验证及第三轮双架构 macOS CI。

同一 run 的三平台 tools/architecture、Linux 与 Windows 编译检查、Linux management 和 Web job 均通过。Linux Rust unit 当轮剩余失败是 ICMP ping socket 的组权限配置；提交 `6ca3cfa` 调整 CI 权限配置并清理三处 Unix 严格 lint，第三轮真实 Linux CI 已通过。原 Linux 历史报告保持不变。

## 第三轮：已结束，全部通过

[Run 41567249779](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779)，源码为 [`1e3be94cdd470fa460800472cdc0da69c135effd`](https://github.com/minecraftgive/astra_proxy_rust/commit/1e3be94cdd470fa460800472cdc0da69c135effd)，最终状态为 `completed / success`，10 个 job 全部成功。运行创建于 `2026-11-03 02:37:15 UTC`，最终更新于 `03:10:19 UTC`。第三轮包含以下修复与回归测试：

| 提交 | 修改与已经执行的本地验证 |
| --- | --- |
| [`6ca3cfa`](https://github.com/minecraftgive/astra_proxy_rust/commit/6ca3cfad4c3a517b8f12f4f5b631daafd127a59a) | 配置 Linux CI 的 ICMP ping socket 组权限，清理三处 Unix 严格 lint；Unix lint 已经 WSL 验证。此项不修改第二轮的失败结果。 |
| [`2050f8a`](https://github.com/minecraftgive/astra_proxy_rust/commit/2050f8adff6d22d4f3dbd0e6855df87f8de00fb7) | `Peer` 在请求进行中监听调用方取消，关闭可能存在部分帧的连接，下次调用重新连接且不自动重放请求。新增三个确定性 IPC 回归，分别覆盖握手期间取消、发送后取消、排队请求取消不影响活动连接。Windows/Linux 的 IPC 20 项及严格 Clippy 通过，原有 manager 6 项 live 通过。 |
| [`1e3be94`](https://github.com/minecraftgive/astra_proxy_rust/commit/1e3be94cdd470fa460800472cdc0da69c135effd) | 新增真实 AMV2 服务 live 回归：持久任务已受理并进入 Running 后，前端断开仍完成；重新提交相同 operation ID 返回原结果且不重复执行。使用真实核心 HTTP 订阅及服务查询验证，Windows 本地测试和严格 Clippy 通过。manager live 总数增至 7 项。 |

临时连接请求的取消与已经受理的持久任务分别验证：前者及时释放连接，后者保留原任务结果和幂等语义。没有通过放宽失败断言代替实现修复。以下结果来自第三轮实际运行及下载后的 JSON 报告，不是沿用第二轮或本地验证结果。[ARM64 job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/job/125691607052) 与 [Intel job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/job/125691607189) 均成功。

| 验证项 | ARM64 | Intel |
| --- | --- | --- |
| 四角色原生构建、契约及前端测试 | 全部通过，前端 7 项 | 全部通过，前端 7 项 |
| Python 工具测试 | 15 项通过 | 15 项通过 |
| Rust workspace/all-targets check 与完整 unit/integration | 全部通过 | 全部通过 |
| 真实在线更新连接保持父测试 | 通过 | 通过 |
| IPC 库测试，含新增确定性取消回归 | 20 项通过 | 20 项通过 |
| TUI 真实核心 live | 6 项通过 | 6 项通过 |
| manager 真实核心 live，含取消清理及 AMV2 持久任务断开观察 | 7 项通过 | 7 项通过 |
| 客户端脚本 | 7 组通过 | 7 组通过 |
| 服务端脚本 | 19 种协议组合及生命周期通过 | 19 种协议组合及生命周期通过 |
| 核心 runner 总结果 | 14 步全部通过 | 14 步全部通过 |
| 真实管理与浏览器 | 17 项通过 | 17 项通过 |
| 解压后 client/server/controller/web 四包 | 全部通过 | 全部通过 |
| 管理与包 runner 总结果 | 5 步全部通过 | 5 步全部通过 |
| Job 最终结论 | 成功 | 成功 |

同一 run 的其余八个 job 也全部成功：三平台 tools/architecture、Linux/Windows workspace 编译检查、Web、Linux Rust 和 Linux management。[Linux Rust job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/job/125691606862) 完整 workspace unit/integration（含真实在线更新和 ICMP loopback）及严格 workspace/all-targets Clippy 通过；[Linux management job](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/job/125691607018) 的真实管理 17 项、浏览器及四包验收通过。这是新增 CI 证据，不覆盖原 WSL 历史记录。

## 证据与验收边界

每轮 Actions 的 `macos-acceptance-arm64`、`macos-acceptance-intel` artifacts 保存 `macos-build.log`、`macos-core.json/log`、`macos-management.json/log`、`management-scenario.json`、包验证报告、`artifacts.json`、角色 manifest，以及管理场景报告、浏览器截图和 runner 日志。保留期为 14 天，后续复核应以相应 run 的源码提交和产物索引区分批次。部署凭据和包含认证请求的浏览器 trace 不在上传范围。

第三轮证据已下载至本地 `.test-tools/github/41567249779/`。两份 macOS 证据归档均已核对 SHA-256：

| 证据 artifact | ID | 下载 ZIP 的 SHA-256 |
| --- | --- | --- |
| [macos-acceptance-arm64](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/artifacts/13200960715) | `13200960715` | `6549537036281a088a7d3fd6198bb7dfb07b54c0c20b6192d2bd8a13201baf84` |
| [macos-acceptance-intel](https://github.com/minecraftgive/astra_proxy_rust/actions/runs/41567249779/artifacts/13200498938) | `13200498938` | `13cc7d6395530e207b2b09296fab28f378f72d3e1ecbe41e07fc22b3644ca975` |

上述摘要对应验收证据 ZIP，不是角色安装包摘要；各角色摘要另见归档内 `artifacts.json`。双架构 `macos-core.json` 各 14 步、`macos-management.json` 各 5 步，均为 `passed=true`；两份 macOS 与一份 Linux `management-scenario.json` 均为 17 checks、`passed=true`。本地还保留 `run.json`、产物索引和截图；Intel compact overview 截图已作视觉复核，未见横向溢出。

本轮没有执行 macOS 真实系统代理变更、TUN/默认路由接管或交互提权。测试使用同一 runner 上的独立进程和 loopback TLS，不代表跨真实主机部署验收；Debug 验证不替代优化 Release 包、签名或公证验收。Xray/sing-box 参考协议完整矩阵也不属于上述 macOS job。
