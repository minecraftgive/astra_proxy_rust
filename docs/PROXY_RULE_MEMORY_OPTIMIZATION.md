# Proxy / Rule Core 内存优化

2026-11-08，macOS arm64，12 逻辑处理器，36 GiB 内存，Rust 1.98.0。按 `proxy-core-rule-core-shimmying-crescent.md` 计划实施。修改前基线从提交 `b45fd95d5fdf16d382aac0367a039326bae33bbd` 的独立源码副本重新编译；前后均使用仓库原有 release 配置（fat LTO、一个 codegen unit）。IPC、持久化格式、代理组选择及故障切换规则保持。

## 已实施

- GEOSITE 逐组、逐条解码，只保存引用分类，仍验证未引用组的结构、名称、重复组、条目类型、长度和总条数。保留条目使用 `u8` 类型、`Box<str>` 值和仅包含 key 的紧凑属性。资源加载直接返回校验后的数据库；rule-set 同样只解析一次。
- 索引 Entry 从逐项动作变为两个 `u32`（8 字节），动作由表去重。哈希键、keyword、pattern 标签改为紧凑字符串，各集合构建后收缩容量；展开使用 Kind 枚举和动作 ID，provider 缓存按索引读取，GEOSITE 使用回调直接展开。
- 代理组流直接包装上游，取消 future 同时唤醒读写两侧；取消后读 EOF、写 BrokenPipe。移除每连接 duplex、中继任务及两个 copy buffer。Lease 独立持有取消 epoch，避免释放 App 后误断连接。
- TUN 保持两个 256 KiB TCP 窗口；两个暂存缓冲各为 64 KiB。四块缓冲预算从每流 1 MiB 降至 640 KiB（下降 37.5%），256 流从 256 MiB 降至 160 MiB。这是容量预算，不是 RSS 实测；不包含 socket 元数据和转发缓冲。vendor 的新 builder 选项保持默认向后兼容，记录在 `vendor/netstack-smoltcp/ASTRA-PATCHES.md`。
- 嗅探 prefix 在发送后立即释放；StableWrite 备用缓冲排空后释放容量；路由缓存命中使用借用键，容量从 65,536 降到 8,192。
- 相同订阅合并文本跳过 apply；普通模式不保留 effective_text。TCP 和每目标 UDP 建立后释放旧 App，只保留流所需取消信号、流量状态及租约。SOCKS UDP 关联仍需为后续新目标路由，因此显式保留共享 RoutingContext，释放旧 App 的文本和决策缓存，保留原代际的路由与组选择语义。
- 两个可执行文件使用 mimalloc。Rule runtime 为 2 个工作线程、最多 4 个阻塞线程；Proxy runtime 为 min(CPU, 4) 个工作线程、最多 32 个阻塞线程。

## 计划外的行为改动

以下两项不在原计划内，与本次内存改动一同实施并经过验收后保留。它们改变了运行行为，下面的实测结果也包含它们的影响。

- **TCP 半关闭超时**（`crates/astra-proxy/src/proxy/relay.rs`）：SOCKS5、HTTP CONNECT、透明代理、TUN、hosted 转发及 hosted DNS 中继，均从 `copy_bidirectional` 改为 `relay`。任一方向结束（读到 EOF 或已向对端发送 shutdown）后，另一方向若连续 30 秒（`HALF_CLOSE_TIMEOUT`）没有数据，就结束整条转发。两个方向都未关闭时不设空闲超时。这样客户端已离开、远端却始终不关闭时，两侧描述符不会一直占用。代价是：先关闭写方向、再等待超过 30 秒才开始响应的合法半关闭用法，会被提前断开。
- **文件描述符上限**（`crates/astra-proxy/src/platform/file_limit.rs`）：Unix 下，代理核心和 hosted 工作进程启动时尝试把 `RLIMIT_NOFILE` 软上限提高到 65536。实际值受继承的硬上限约束，macOS 上还受 `kern.maxfilesperproc` 约束；已有的较高上限不会被降低，也不修改系统全局设置。未能达到目标时启动日志会给出警告。说明见 README 的连接限制一节。

## Rule Core 实测

将本机 geosite.dat 复制到独立临时目录（4,125,536 字节、1,438 类）；移除在线资源更新，使用独立 socket/runtime。每个场景前后各三次，表中为中位数。峰值来自 `/usr/bin/time -l astra-rule --check <cfg>`；空闲 RSS 来自另一个正常运行的进程，IPC socket 出现后等待 1 秒，再读取 `ps -o rss`。两者是独立进程，空闲值可能高于 check 峰值。先测基线，再测优化版，没有交替启动。

| 场景 | 指标 | 修改前 | 修改后 | 降幅 |
|---|---|---:|---:|---:|
| google、telegram | check 峰值 RSS | 40.61 MiB | 22.42 MiB | 44.8% |
| google、telegram | 空闲 RSS | 40.59 MiB | 24.14 MiB | 40.5% |
| 15 分类 | check 峰值 RSS | 93.88 MiB | 50.50 MiB | 46.2% |
| 15 分类 | 空闲 RSS | 96.58 MiB | 50.48 MiB | 47.7% |

15 分类包含 cn、google、youtube、github、apple、microsoft、netflix、telegram、twitter、facebook、amazon、openai、spotify、onedrive、geolocation-!cn。默认动作 direct，GEOSITE 动作统一 proxy:edge。不同平台、数据版本和分配器不能与旧 Windows 表直接比较。

原始样本、数据库及二进制 SHA-256、Proxy 测量参数见 [完整测量记录](proxy-rule-memory-results.json)。

## Proxy Core 实测

扩展 `scripts/hosted_benchmark.py`，在 ordinary / hosted 两种模式分别测 direct 和 `select → DIRECT` 代理组。显式禁用 local bypass，确认代理组解析路径实际执行；每轮保持 200 条经过 4 字节 echo 校验的空闲 SOCKS5 CONNECT，连接保持期间取 5 次 RSS 的中位数，计算 `(连接后 RSS − 基线 RSS) / 200`。每场景 3 轮；RSS 包含 Rule Core、Proxy Core 及其 hosted worker，不含 Python 客户端/echo 服务。

| 模式 / 路由 | 修改前每连接 RSS | 修改后每连接 RSS | 降幅 | echo 吞吐（前 → 后） |
|---|---:|---:|---:|---:|
| ordinary / direct | 132.08 KiB | 43.76 KiB | 66.9% | 881.4 → 866.4 MiB/s（-1.7%） |
| ordinary / group | 132.08 KiB | 43.84 KiB | 66.8% | 893.8 → 860.6 MiB/s（-3.7%） |
| hosted / direct | 234.96 KiB | 122.40 KiB | 47.9% | 489.0 → 456.3 MiB/s（-6.7%） |
| hosted / group | 235.20 KiB | 121.36 KiB | 48.4% | 457.5 → 462.7 MiB/s（+1.1%） |

吞吐使用 4 条流、64 KiB payload、每轮 3 秒；按回显确认的有效载荷计一次，不叠加上传下载。完整前后两批串行运行，期间无编译或其他回归负载。普通模式空闲进程基线约 19.1 → 19.8 MiB，hosted 约 30.4 → 31.7 MiB；200 连接后的总 RSS 则分别约 44.9 → 28.3 MiB 和 76.3 → 55.6 MiB。

小流量空闲场景不会触碰全部缓冲页，因此基线 direct/group 的 RSS 接近，并不能据此认为原中转缓冲没有容量成本。每连接的 RSS 收益包含分配器、线程和流包装等所有改动，不能单独归因于某一项。

吞吐中位数变化为 −6.7% 至 +1.1%。hosted/direct 三次值的范围重叠（前 449.5–491.6、后 445.7–468.3 MiB/s）；仅三轮、前后整批顺序运行，不足以排除温度/调度噪声，亦不能证明线程限额没有性能代价。保留这个观测下降，不将其解释为确定的线程回归。

复现（psutil 可用时才记录 RSS）：

```sh
TMPDIR=/tmp python3 scripts/hosted_benchmark.py --bin-dir <release目录> --routes direct group --idle-connections 200 --connections 100 --concurrency 10 --rounds 3 --seconds 3 --streams 4 --settle-seconds 1 --output <结果.json>
TMPDIR=/tmp python3 scripts/benchmark.py --bin-dir <release目录> --rules 1000 10000 --concurrency 1 32 --connections 300 --rounds 3 --output <结果.json>
```

### 短连接吞吐

另外按计划运行 `scripts/benchmark.py`，每轮 300 连接、3 轮，测 SOCKS 建连加 1 KiB echo（Python 客户端/服务端也在计时路径上）。表中为每秒完成连接数的中位数。连续测试曾耗尽 macOS 临时端口，失败的 after 批次没有结果文件；等待端口回收后重新完成整批，没有调整系统参数。

| 规则数 | 缓存 | 并发 | 修改前 conn/s | 修改后 conn/s | 变化 |
|---|---|---:|---:|---:|---:|
| 1,000 | disabled | 1 | 1566.8 | 1546.0 | -1.3% |
| 1,000 | disabled | 32 | 1529.5 | 1711.8 | +11.9% |
| 1,000 | warm | 1 | 1511.1 | 1604.6 | +6.2% |
| 1,000 | warm | 32 | 1540.9 | 1947.2 | +26.4% |
| 10,000 | disabled | 1 | 1510.6 | 1603.5 | +6.1% |
| 10,000 | disabled | 32 | 1340.4 | 1533.4 | +14.4% |
| 10,000 | warm | 1 | 1533.8 | 1602.0 | +4.4% |
| 10,000 | warm | 32 | 1840.6 | 1446.9 | -21.4% |

由于 10,000 规则 / warm / 并发 32 首测下降 21.4%，在端口回收后按相反顺序（先 after，再 before）对该场景复测，每组增加到 5 轮 × 300 连接。warm 吞吐为 2747.3 → 2837.0 conn/s（+3.3%），禁用缓存为 2720.3 → 2949.4（+8.4%）；首测的下降没有复现。warm 建连 p50 为 7.214 → 6.830 ms，但 p95 为 15.328 → 30.206 ms，尾延迟仍有波动。全部原始结果均保留，不据此宣称普遍提速或没有尾延迟代价。这些 loopback/Python 基准只能作为本机回归观察，不能代替真实负载的吞吐和延迟验证。

## 验证

- `cargo test -p astra-rule-data -p astra-rule -p astra-proxy -p astra-protocols --no-fail-fast`：207 通过、3 按原设置忽略；使用短 `TMPDIR=/tmp`。包括组切换取消、半关闭、分离读写背压、重复订阅跳过、旧 App 释放、UDP 后续目标、跨代际配额和 HTTP 转发。
- `cargo clippy --workspace --all-targets`：通过；保留原有 astra-platform 的 needless_return 和 meow-proxy 的两项弃用警告，没有新增警告。
- `git diff --check`：通过。最初交付时 `cargo fmt --check` 并未通过，有 6 个 relay 相关文件未格式化（`hosted.rs`、`hosted/dns.rs`、`inbound/http.rs`、`inbound/socks5.rs`、`platform/transparent.rs`、`test/proxy_relay_test.rs`）。验收时只对这 6 个文件运行了 rustfmt，之后对 astra-proxy、astra-rule、astra-rule-data、astra-protocols 运行 `cargo fmt --check` 通过。
- vendor 独立 `cargo test --manifest-path vendor/netstack-smoltcp/Cargo.toml --lib`：4 通过，实际 SYN 建立后检查 staging 和 TCP 窗口容量，覆盖默认兼容行为；单独运行是因为该依赖不属于 workspace。
- `python3 -m unittest discover -s scripts/tests -p 'test_*benchmark.py'`：8 通过，覆盖测量期间连接存活、失败清理、缺失 RSS、组路由配置等。
- `scripts/ruleset_regression.py`、`proxy_groups_regression.py`、`reload_regression.py`、`udp_regression.py`：全部通过。重载脚本原先遗漏 `direct_local=false`，回环地址绕过默认 block，基线和优化版在同一断言失败；修正测试配置后两版均完整通过，未改变产品路由语义。
- Release 构建完成：astra-proxy、astra-rule、astra-convert。构建环境需在 PATH 提供 CMake。

未进行真实 TUN 设备、高延迟网络或跨平台实测；这些场景不能从本机 loopback 结果推导。没有部署或重启用户正在使用的核心。
