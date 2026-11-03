# 网络整合验证（2026-11-03）

代码基准：`d76db8f`。本记录覆盖已整合的 hosted DNS 中继缓冲和 Linux 进程归属缓存；T7 的 Moka DNS 实现没有合入。

## 实现范围

- hosted DNS 中继引用 `COPY_BUFFER_SIZE = 32 * 1024`，上传和下载各使用 32 KiB。`nettool/mod.rs` 的独立中继仍使用默认大小，不将 T8 的全部位置标为完成。
- Linux `/proc/net` socket 表和 inode→唯一 PID 映射使用 100 ms 缓存；缺失时刷新，保留 TCP 精确匹配、UDP 通配地址及多 owner 返回 `None` 的语义。
- 同一 PID 的重复 fd 不制造歧义；不同 PID 共享 inode 时不猜测归属。超出映射容量时不返回不完整结果。
- 当前 ArcSwap worker 排除集合、注册/注销及异常清理逻辑保持不变。

## 验证

| 检查 | 结果 |
|---|---|
| Windows workspace Clippy、构建、全量测试 | 通过；677 项测试通过，22 项既有忽略 |
| Windows DNS / ruleset 回归 | 使用整合版本构建的程序，通过 |
| Linux workspace Clippy、构建 | 通过 |
| Linux `platform::process` 测试 | 6 项通过 |
| Linux `process_routing_regression.py` | 通过：进程名/路径、SOCKS/HTTP/CONNECT/UDP、跨应用缓存隔离、DIRECT、reload/opt-out |
| Linux `ruleset_regression.py` | 使用最终整合版本的 proxy/rule/convert，再次通过 |

Linux 环境为 WSL CachyOS，内核 `6.18.33.2`，Rust 1.99。进程路由脚本使用真实解释器 `/usr/bin/python3.14`：若通过 `python3` 符号链接启动，`sys.executable` 与 `/proc/PID/exe` 的程序名可能不同，影响按名称匹配的测试。

## WSL 大报文限制

Linux 未过滤的全量测试有两项超时：UDP 大包接收缓冲复用测试和 padded-smux 大包测试。验证过程如下：

1. 单独重跑整合版本的两个测试，仍分别在 5 秒和 10 秒超时。
2. 从精确基线 `21c625f` 提取源码重跑，得到相同超时。对应的 `proxy/udp.rs` 和 `protocols/multiplex.rs` 在本轮没有修改。
3. 使用不导入项目的标准库 socket loopback 对照：TCP/UDP 的 32、1024 字节传输通过；8192 字节及以上接收超时。有界对照正常结束。
4. 回到整合版本，重新构建所有程序，再运行最终全量测试和进程路由、规则集回归，避免混用基线构建产物。

最终 Linux 全量结果为 **671 通过、0 失败、21 项既有忽略、2 项明确排除**，不重复计入更新事务子进程的 1 项通过。排除项未计作通过；Windows 全量测试没有排除它们。

在设置好 Linux Rust/C 构建环境及 `ASTRA_UPDATE_TEST_BIN`（指向本次构建的可执行程序目录）后，最终测试命令为：

```sh
cargo test --workspace --no-fail-fast --locked --offline -- \
  --test-threads=1 \
  --skip proxy::udp::group_policy_tests::association_preserves_datagrams_across_receive_buffer_reuse \
  --skip protocols::multiplex::tests::server_accepts_padded_smux_without_client_configuration_changes
```

移除两个 `--skip` 即可复跑未过滤版本。应在大报文 loopback 正常的 Linux 环境中重新验收这两项。

本地证据位于整合工作树 `.test-tools/integration-remaining/`：`linux-test.log`、`linux-udp-retest.log`、`linux-smux-retest.log`、`linux-baseline21-udp.log`、`linux-baseline21-smux.log`、`linux-standalone-loopback-bounded.log`、`linux-final-test.log`、`linux-summary.json`。这些产物未纳入 Git，主要结果已汇总在本文件。

本轮没有改动系统网络设置，也没有重跑吞吐基准。旧分支的小包样本对应不同代码和构建产物，不作为本次 32 KiB 中继的提速证明。
