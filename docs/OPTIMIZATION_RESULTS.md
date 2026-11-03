# 优化整合结果（2026-11-03）

本记录对应已验证的代码提交 `d76db8f`，整合分支为 `codex/integration-selected`。从 `optimization-tasks` 按功能挑选提交，以 `996fab5` 的较新架构、监控和恢复实现为基础；本地 `main` 已包含这些代码，没有推送远端。本轮补齐文档不改变该代码状态。

## 已整合范围

| 任务 | 当前结果 |
|---|---|
| T1 | 删除未使用的直接依赖；检查了 Unix/macOS 条件代码中的引用 |
| T2 | 日志共享到 `astra-platform`，订阅视图共享到 `astra-management`，保留 `read` / `read_in` |
| T3 | 统一目录权限和默认原子写入 helper；调用方已适配返回类型 |
| T4 | `legacy` 重命名为 `wire`，公开导出及 bincode 类型、线格式不变 |
| T5 | 编辑器和管理器内联测试移入各自的 `tests.rs` |
| T8 的已选补齐项 | hosted DNS 中继使用共享常量，两个方向各 **32 KiB**；`nettool/mod.rs` 的中继仍待补齐 |
| T9 | 关键词索引、预编译域名 pattern、单次 IP 解析及 GeoIP 合并区间查找 |
| T10 | 翻译目录使用一次初始化的哈希索引，保留未知文本回退 |
| T11 | Linux socket 表和唯一 inode owner 使用 100 ms 缓存，保留当前 ArcSwap worker 排除集合 |
| T12 | 拆分 manager action、Ui key/render、Editor key/render/commit_field 六个函数，保留操作和绘制顺序 |
| T13 | 共享 POSIX 引号函数，修正 fish 中反斜杠和单引号转义 |
| T14 | 可取消的串行轮询、页面隐藏时暂停、事件触发刷新及断线兜底 |

第一批四项整合到 `21c625f`，第二批八项整合到 `d76db8f`。T12 按函数分别提交；解决模块声明冲突后，仅对两个受影响的声明文件另做了格式整理。

## 来源分支的剩余差异

- `4164250`、`37010ca`、`0e41821`、`58252fa` 的监控、SQLite 文件头预检及测试夹具修复已被较新基线覆盖，无需重复应用。保留了当前采样时间字段和更完整的身份断言。
- `git cherry` 中部分 `+` 来自解决冲突后补丁上下文不同；已逐项核对共享模块、shell 和 Ui 绘制功能，不代表遗漏。
- `optimization-network`、`optimization-tui`、`optimization-rules-web` 的已接受功能也已覆盖，不再重复合并它们的旧快照。
- 未合入旧工作区快照、全 workspace 格式化及其 CI 改动、Moka DNS 缓存；未修改 `vendor/`、host/worker 分层、每条流的 generation 租约、TUN 路由方式或代理组故障切换语义。

T6 的限定文件格式化、按当前要求重做的 T7、T8 的 nettool 补齐，以及 T15–T21 尚未完成的子项，仍以[任务书](OPTIMIZATION_TASKS.md)为准。这些不是可以继续直接挑选的已完成来源提交。

## 验证结果

| 检查 | 结果及范围 |
|---|---|
| Windows workspace Clippy / 构建 | 通过；Clippy 包含全部 targets，仍有现有告警 |
| Windows workspace 测试 | **677 通过、0 失败、22 项既有忽略**，没有额外排除；不重复统计更新事务子进程的 1 项通过 |
| Linux workspace Clippy / 构建 | 在 WSL CachyOS 中通过 |
| Linux workspace 测试 | **671 通过、0 失败、21 项既有忽略、2 项环境排除**；排除依据见下文，不计作通过 |
| Linux 进程归属 | 6 项通过，含真实 IPv4/IPv6 socket、歧义归属、UDP 通配及 worker 排除 |
| 脚本回归 | Windows DNS / ruleset 通过；Linux process routing / ruleset 使用最终重新构建的程序再次通过 |
| 工具及架构 | 20 项工具测试、架构边界检查通过 |
| 管理接口契约 | `export-contracts --check` 通过 |
| 格式 | 仅检查本轮改动的 22 个 Rust 文件，通过；未运行全 workspace 格式化 |
| 前端 | 第一批的类型检查、构建、53 项单元测试通过；监控 E2E 1 项通过，真实 TLS 场景因缺少地址和凭据跳过 1 项。第二批未修改前端源码 |

本次没有 macOS 实机或交叉编译通过的验证结果。Windows 和 Linux 的通过不等同于 macOS 已验收。

Linux 未过滤的首次全量测试为 671 通过、2 失败、21 忽略。两个大报文超时在精确基线 `21c625f` 中同样复现，且不导入项目的独立 TCP/UDP loopback 对照也出现相同大小限制。因此最终测试只显式排除以下两项：

- `proxy::udp::group_policy_tests::association_preserves_datagrams_across_receive_buffer_reuse`
- `protocols::multiplex::tests::server_accepts_padded_smux_without_client_configuration_changes`

这两个测试在 Windows 全量测试中通过。没有通过修改业务代码、测试断言或系统网络设置来掩盖 WSL 限制。详细证据和复测方法见[网络验证记录](optimization-results/NETWORK.md)。

## 记录和性能数据

整合工作树内的本地记录为 `.test-tools/integration/summary.json` 和 `.test-tools/integration-remaining/summary.json`；Linux 原始失败、基线对照和最终排除记录位于后者同目录的 `linux-summary.json` 及日志文件。这些本地验证产物未纳入 Git。

[T9](optimization/T9.md) 保留开发阶段的 Criterion 前后数据及适用范围；本轮没有重新跑吞吐基准，也没有声称所有规则分布或实际网络吞吐获得同等提升。[T14](optimization/T14.md) 记录最终轮询行为及前端验证。旧分支的 64 KiB / Moka 网络样本不作为当前整合版本的性能数据。
