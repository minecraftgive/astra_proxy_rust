# Windows TUI 连接与日志栈溢出修复

日期：2026-11-03。

用户报告在 TUI 按 `c` 连接或打开日志时出现 `thread 'main' has overflowed its stack`。在 Windows x86_64 MSVC Debug 实际终端中，两条路径均已复现；旧代码为 `060fdfa`，其程序实现与 `1e3be94` 相同。

## 原因与修改

实际 EXE 的主线程栈预留为 1 MiB。CDB 捕获 `0xc00000fd`，调用链没有递归：大型异步动作分发函数在 Debug 模式下生成的大栈帧，与入口、LocalSet、任务和响应反序列化的栈帧叠加后耗尽主栈。普通 Cargo 测试线程未覆盖这条完整调用链的栈限制。

- `protocol::call` 先同步转换动作，再执行少量异步分支；保存和回滚保留各自的事务步骤。相同未优化汇编探针测得该函数 poll 栈帧从 304,688 B 降到 29,872 B，减少约 90%。此数值是该函数的栈帧，不是整个程序的峰值栈占用。
- 后台操作同步选择独立的更新、运行控制、日志或配置处理函数，避免把所有操作的异步临时变量集中在同一 poll 栈帧。每个任务只创建所需的处理 future，仍使用原 LocalSet 和任务取消机制。
- 没有提高 EXE 栈预留、设置 `RUST_MIN_STACK` 或依靠 Release 优化绕过问题。

实际服务端终端复验还发现并修复了两处适配遗漏：有效 Server 配置预检原先返回路径，被 TUI 当成配置正文打开；现在返回正确的空准备结果，继续启动。Server 支持读日志，但不支持动态修改录制级别；日志页现在按目标实例类型跳过该查询并禁用调整入口，管理 API 对不支持的查询和修改返回 `Unsupported`，显示过滤仍可使用。

## 回归证据

新增 `ui::stack_tests::connect_and_logs_fit_background_stack_budget`，在子进程内用 512 KiB 线程执行 LocalSet、Jobs、操作处理、真实 AMV2 请求/响应和日志消费，为实际终端入口留出额外栈空间。父测试检查子测试确实完成，栈溢出不会终止其余测试。

隔离副本仅恢复旧 `protocol.rs` 和 `operations.rs`，测试源码与当前完全相同。旧实现的 Connect 和 Logs 均以 `0xC00000FD` 退出；新实现两条路径均通过。Windows GitHub Actions 现在显式运行该测试，Linux/macOS 的常规 TUI 单元测试也会覆盖它。

本机已执行：

- TUI 库：201 通过，8 个原有/子进程入口测试按约定 ignored。
- Manager 库：46 通过，9 个需单独调用的在线测试或子进程入口 ignored；包含真实更新事务子进程。
- 统一 `live` runner：TUI 6 项和 Manager 8 项全部通过；两 crate 的 all-targets Clippy（`-D warnings`）、全工作区 fmt 和 diff 检查通过。
- 新增 Server 回归：稳定 ID 下日志可读、不支持的录制级别返回正确错误；真实核心完成预检、启动、SOCKS 鉴权协商、日志读取与停止。
- 实际 `astra-tui.exe`：初始日志页、按 `c` 启动、SOCKS 回环传输、运行中日志、按 `c` 停止和 `q` 退出。
- 实际 `astra-server-tui.exe`：初始日志页、按 `c` 启动、认证 SOCKS 回环传输、运行中日志、按 `c` 停止和 `q` 退出。

实际终端测试使用独立 state/config/runtime 和回环监听，不改用户配置、系统代理、TUN 或提权状态。测试管理器和核心在每轮结束后关闭。

本机诊断原件保存在 `.test-tools/tui-stack-repro/`，包含原始 CDB 栈、dump、栈帧测量及实际终端复验报告；旧实现失败证据在 `.test-tools/tui-stack-red/result.json`。这些临时诊断产物不进入安装包。

之前 `dist/2026-11-03_11-19-12_403702/` 的验证包包含旧 TUI，保留为历史产物。应使用本次修复后重新构建的客户端和服务端包；旧 CI 通过记录不能代替本次栈回归结果。
