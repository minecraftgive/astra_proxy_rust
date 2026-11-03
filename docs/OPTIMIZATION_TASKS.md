# astra-dual-core 精简与性能优化任务书

本文件交给执行方（ChatGPT Work）逐项实施。所有结论来自对当前分支 `codex/shared-management-macos` 的只读审查，行号以审查时为准，执行前请重新定位。

## 通用约束

- 不修改 `vendor/` 下的任何代码（可以阅读、复用）。
- 不改变对外协议与持久化格式（bincode IPC、`instances.toml`、`runtime.json`、`ui-session.json`），除非任务明确说明。
- 每个任务单独一个提交，提交信息说明动机与验证方式。
- 每个任务完成后至少运行：
  - `cargo fmt --all`
  - `cargo clippy --workspace --all-targets`
  - `cargo test --workspace`（涉及平台代码时注明未覆盖的平台）
  - 前端任务额外运行 `cd apps/web && npm run typecheck && npm test`
- 工作区当前有大量未提交改动，开始前先确认它们已提交或另开分支，避免混入。

---

## 第一批：低风险清理

### T1 删除未使用的 Cargo 依赖
| crate | 依赖 |
|---|---|
| `crates/astra-dns/Cargo.toml` | `url`（`uuid` 在 T7 中一并移除） |
| `crates/astra-protocols/Cargo.toml` | `futures-util` |
| `crates/astra-proxy/Cargo.toml` | `toml_edit` |
| `crates/astra-rule-data/Cargo.toml` | `serde_yaml`、`ipnet` |
| `crates/astra-server-core/Cargo.toml` | `ipnet` |

验收：`cargo check --workspace --all-targets` 在 Windows/Linux/macOS 目标下通过（至少 `cargo check --target` 交叉检查，注意 `cfg` 下的引用）。

### T2 合并完全重复的模块
- `crates/astra-manager/src/tui_log.rs` 与 `crates/astra-tui/src/tui_log.rs` 内容完全相同。
- `crates/astra-manager/src/subscription_view.rs` 与 `crates/astra-tui/src/subscription_view.rs` 几乎相同（manager 版多一个 `read_in(root, path)`）。

做法：移到两者共同依赖的 crate（建议 `astra-platform` 或 `astra-model`，以依赖方向不成环为准），保留带 `root` 参数的 `read_in`，`read` 作为其便捷封装；删除两份副本并更新引用。

### T3 合并文件系统 helper
- `crates/astra-manager/src/manager.rs` 的 `private_dir` / `atomic_write`（约 496–512 行）
- `crates/astra-tui/src/manager.rs` 的 `private_dir` / `atomic_write`（约 26–38 行）

二者都只是包装 `astra_platform::storage::atomic_write(.., Default)`。在 `astra-platform::storage` 提供 `private_dir` 与默认策略的写入函数，删除两层包装。`astra-tui/src/manager.rs` 剩余内容仅测试使用，移入 `#[cfg(test)]`。

### T4 重命名 `legacy` 模块
`crates/astra-management/src/legacy.rs` 实为当前仍在用的 bincode 协议（`Action`/`Reply`/`call`/`subscribe`），通过 `pub use legacy::*` 导出。重命名为 `wire`（或 `v1`），模块头部注释写明与 `v2` 的关系。纯重命名，不改类型。

### T5 拆出内联测试
- `crates/astra-tui/src/editor.rs`（约 5061 行，3050 行起为测试）→ `editor/tests.rs`
- `crates/astra-manager/src/manager.rs`（约 3324 行）→ `manager/tests.rs`

参照已有的 `crates/astra-manager/src/agent/tests.rs` 写法。纯移动。

### T6 格式化
运行 `cargo fmt --all`，重点是被压成一行的代码：`crates/astra-proxy/src/dns/server.rs:74`、`crates/astra-proxy/src/platform/tun.rs:710`、`crates/astra-server-core/src/proxy/net.rs:285`、`crates/astra-web/src/assets.rs:30`。在 CI 中加入 `cargo fmt --check`。单独提交。

---

## 第二批：性能热点

### T7 DNS 缓存改用 moka（最高优先级）
位置：`crates/astra-dns/src/resolver.rs`，`cache: Mutex<HashMap<Vec<u8>, Cached>>`，插入逻辑约 355–390 行。

问题：每次插入先 `retain` 全表；`while` 每淘汰一条都重新累加所有条目 `bytes` 并 `min_by_key` 全表扫描，O(n²) 且持锁；为计算大小额外 `message.to_vec()`；同一查询并发时全部打到上游。

做法：
- 使用 `moka::future::Cache`（workspace 中 astra-proxy 已依赖 moka 0.12，加 `future` feature）。
- `weigher` 实现 16 MiB 总量上限，`max_capacity` 对应 `config.cache_capacity`。
- 实现 `Expiry`，按每条记录 TTL 过期；命中时仍按已过时长扣减记录 TTL（保留现有行为）。
- 用 `try_get_with` 合并同 key 并发请求；仅缓存 `NoError`/`NXDomain` 且 `ttl > 0` 的结果，其它结果不得写入缓存。
- `set_mapping_store` 中的清空改为 `invalidate_all`。

附带：
- `crates/astra-dns/src/wire.rs` 的 `validate_query` 不再 `to_vec()` 计算长度，改由调用方传入原始字节长度。
- `wire.rs:10` 用 `uuid` 生成随机 id，改为 `rand::random::<u16>()`（或 `getrandom`），并删除 `uuid` 依赖。

验收：原有 DNS 测试通过；新增测试覆盖 TTL 过期、容量淘汰、并发同查询只触发一次上游；`scripts/dns_regression.py` 通过。

### T8 加大中继缓冲区
`tokio::io::copy_bidirectional` 默认 8 KiB。以下位置改为 `copy_bidirectional_with_sizes(.., 64 * 1024, 64 * 1024)`（tokio 1.53 支持）：
- `crates/astra-proxy/src/inbound/http.rs:97`
- `crates/astra-proxy/src/inbound/socks5.rs:70`
- `crates/astra-proxy/src/platform/tun.rs:573`
- `crates/astra-proxy/src/platform/transparent.rs:54`
- `crates/astra-proxy/src/hosted.rs:884`、`:1226`
- `crates/astra-proxy/src/hosted/dns.rs:86`
- `crates/astra-proxy/src/proxy/group_sessions.rs:98`

把缓冲区大小提成一个常量。验收：`scripts/benchmark.py` / `scripts/hosted_benchmark.py` 前后对比吞吐并记录结果。

### T9 规则匹配优化
位置：`crates/astra-rule/src/engine.rs`，`RuleEngine::decision`（约 585–722 行）。
- `keywords`：逐个 `domain.contains` → 编译期构建 `aho_corasick::AhoCorasick`，一次扫描得到所有命中，再按 `Entry.index` 取最小。
- `patterns`：循环内每个 pattern 分配两个 `Vec`。编译时预拆分 pattern 为标签列表并按标签数分组；请求时只拆一次 domain。
- `req.target.host.parse::<IpAddr>()` 调用两次（约 608、654 行），合并为一次。
- `process.name.to_lowercase()` 等每请求分配：在不改变语义的前提下尽量避免（如 ASCII 快速路径）。
- 保持"最小 index 获胜"语义不变。

`crates/astra-rule-data/src/geoip.rs`（约 138 行）：每个前缀长度一个 `HashSet<u32>/HashSet<u128>`，查询最多 33/129 次哈希。改为加载时合并为有序、去重、合并相邻的区间 `Vec<(start, end)>`，查询用 `partition_point` 二分；`inverse` 语义保持。

验收：现有 engine/geoip 测试全部通过；新增 `criterion` 基准（`crates/astra-rule/benches/`）覆盖大量 keyword/pattern/GeoIP 的场景，记录前后数据；`scripts/ruleset_regression.py` 通过。

### T10 i18n 查找改为哈希表
位置：`crates/astra-tui/src/i18n.rs:101`，`translate` 对 839 条 `CATALOG` 线性 `find`，约 439 处调用，多在每帧渲染中。

做法（任选其一，优先不加依赖）：
- `static INDEX: OnceLock<HashMap<&'static str, usize>>`；或
- 使用 `phf` 生成编译期表。

加测试保证 `CATALOG` 无重复 key。`message` / `tr!` 语义不变。

### T11 Linux 进程归属加缓存
位置：`crates/astra-proxy/src/platform/process.rs:365-436`。每个连接都重新读解析 `/proc/net/tcp{,6}`（每行分配 `Vec`），并遍历所有进程所有 fd `read_link`。

参考已 vendor 的 `vendor/meow-common/src/process_lookup.rs`（`sock_table` 约 216 行、`find_pid_by_inode_cached` 约 281 行），astra-proxy 已依赖 meow-common：
- 方案 A：直接复用 meow-common 的查找函数（确认 UDP 未指定地址匹配、多 owner 返回 None 等现有语义一致）。
- 方案 B：照其思路加入短期 socket 表缓存 + inode→pid 缓存，行解析改用 `split_whitespace().nth()` 不分配。
- 可选进阶：netlink `SOCK_DIAG`（`netlink-packet-sock-diag`）按五元组直接取 inode/uid。

验收：`process.rs` 现有测试（如 `ipv6_tcp_and_udp_socket_owners_are_attributed`）在 Linux 通过；`scripts/process_routing_regression.py` 通过。

---

## 第三批：结构与前端

### T12 拆分超长函数（每个函数一个提交，行为不变）
| 位置 | 约行数 | 拆分方式 |
|---|---|---|
| `crates/astra-tui/src/editor.rs:1838` `Editor::key` | 670 | 按 `Mode` 拆成 `key_browse`/`key_input`/`key_picker` 等，外层只分发 |
| `crates/astra-tui/src/ui.rs:1171` `Ui::key` | 510 | 按焦点/页面拆分 |
| `crates/astra-tui/src/editor.rs:2630` `render` | 420 | 按区域拆函数 |
| `crates/astra-tui/src/ui.rs` `render` | 336 | 同上 |
| `crates/astra-manager/src/manager.rs` `action` | 356 | 每个 `Action` 分支一个方法 |
| `crates/astra-tui/src/editor.rs:1126` `commit_field` | 250 | 按字段类型拆 |

### T13 shell 引号统一并修正 fish
- `crates/astra-tui/src/export.rs:416` 与 `crates/astra-tui/src/windows.rs:265` 两份 `shell_quote` 合并；POSIX 引号可用 `shlex::try_quote`。
- `export.rs:420` `fish_quote` 直接复用 POSIX 规则，但 fish 单引号内 `\\`、`\'` 会被解释，含反斜杠的值输出错误。改为 fish 规则：单引号内将 `\` → `\\`、`'` → `\'`。补测试。

### T14 前端轮询改为 hook + 事件驱动
`apps/web/src/App.tsx` 103、162、215、243 行有 4 段相同的 `useEffect` + `setInterval`（1.5–5 秒）。
- 抽出 `usePolling(fn, intervalMs, deps)`：使用 `setTimeout` 链（上次请求结束后再排下一次，避免叠加），`AbortController` 取消，`document.hidden` 时暂停。
- 已有 `/api/v2/events` 流（`apps/web/src/api.ts` 约 280–330 行），改为收到相关事件时触发刷新，轮询作为断线兜底并拉长间隔。
- 评估用浏览器原生 `EventSource` 替换手写 SSE 解析（需确认鉴权是否走 cookie；若需自定义 header 则保持现状或用 `@microsoft/fetch-event-source`）。

验收：`npm run typecheck`、`npm test`、`npm run test:e2e` 通过。

---

## 第四批：深度性能问题（第二轮审查新增）

> 状态说明：审查时工作区里 T8 已基本完成（`COPY_BUFFER_SIZE` 已引入）。还剩 `crates/astra-proxy/src/hosted/dns.rs:86` 和 `crates/astra-proxy/src/nettool/mod.rs:786` 两处没改，请顺手补上。

### T15 TUN 读取循环里的串行进程查找（最高优先级）
位置：`crates/astra-proxy/src/platform/tun.rs` 读取任务（约 488–512 行）→ `tun_ingress::SelfExclusion::excludes` → `process::is_self`。

问题：
- 所有 TUN 入站包都由一个任务串行读取。遇到每个 TCP SYN、每个缓存未命中的 UDP 流，它都会 `await process::is_self`，最长等 250 ms。
- 在非 Windows 平台上，`is_self` 走的是 `resolve`，也就是全量扫描 `/proc/*/fd`。
- 查找期间，**包括已建立连接在内的所有 TUN 流量都被阻塞**。浏览器同时打开几十个连接时，延迟会叠加到秒级。
- `bounded_lookup`（约 75 行）每次查找 UDP 都会执行 `std::net::UdpSocket::bind(..)` 来判断地址是否是本机地址，多了一次 socket 创建和关闭。

做法：
- Linux：给自身出站 socket 设置 `SO_MARK`，再配合策略路由，让自身流量根本不进 TUN。这样 `is_self` 可以退化为只在 Windows 和 macOS 上使用。
- 通用：把自排除判断移出读取循环。读取循环只做无阻塞的缓存查询；未命中时先放行，交给 TCP/UDP 分发器在单个流的任务里判断（文档注释说"routing then rejects that recaptured connection"，这套兜底机制已经存在）。
- 用本机地址集合（启动时枚举网卡地址，网络变化时刷新）替代每次的 `bind` 测试。

验收：`scripts/windows_tun_reuse_regression.py`、`scripts/process_routing_regression.py` 通过；建 TUN 后用 `curl` 并发 100 个连接，记录改造前后的 p99 建连延迟。

### T16 Shadowsocks/QUIC 服务端 UDP：全局 64 流上限与每包 O(n) 遍历
位置：`crates/astra-server-core/src/protocols/shadowsocks.rs:17-21`、`crates/astra-server-core/src/proxy/net.rs:297-320`（`Hub`）、`crates/astra-server-core/src/proxy/quic.rs:89,92`。

问题：
- `ss_udp` 在**每个监听 socket 上只创建一个 `Hub`**，而 `Hub` 的硬上限是 `flows.len() >= 64`（net.rs:310）。结果是同一端口下所有客户端加起来只能有 64 个 UDP 目标，超出的包直接丢弃。这是容量 bug。
- 每收到一个包都会：`clients.retain(..)` 全表扫描；在 aead2022 下再做一次 `clients.keys().any(..)` 全表扫描。回程每个包还要 `clients.iter_mut().find(|v| v.0 == association)` 线性查找。
- quic.rs 的 89 和 92 行同样每包都 `native.retain(..)`。
- `Hub` 内每包要做：`flows.retain`、`packet.target.host.clone()`、`socket.peer_addr()` 系统调用（connected socket 的对端地址不会变，算一次即可）、`peer.ip().to_string()`、`buf[..n].to_vec()`，还要每轮重新创建 `tokio::time::sleep` 计时器。
- server-core 的 socks5.rs（约 34–38 行）同样每轮新建 `sleep`，每个回程包分配一个 `out` Vec。

做法：
- 64 改为配置项（默认与 `max_udp_flows` 一致），或者按客户端会话分别建 `Hub`。
- 过期清理改为由已有的 `cleanup` interval（或单独的 1 s 定时器）周期执行，不再每包执行。
- 增加辅助索引：`association -> key`、`client_session_id -> peer`，把 any/find 变成 O(1)。
- 把 `clients` 的元组 `(u32, UdpSocketControlData, Instant, u64, u64)` 改成具名结构体，抗重放窗口抽成 `ReplayWindow` 类型并补单测。
- 计时器改为 `pin!(sleep)` 后调用 `reset()`；`peer_addr` 移到循环外计算。
- `server_session_id` 用 `rand::random::<u64>()` 生成，不再用 uuid。
- 先 `cargo fmt`（T6），再改这几处被压成一行的代码。

验收：新增测试，验证超过 64 个目标的 UDP 流全部能转发；`scripts/udp_regression.py`、`scripts/tuic_regression.py` 通过；压测一万个包/秒时 CPU 占用前后对比。

### T17 Windows 命名管道：只有一个待连接实例，忙时 10 ms 轮询且没有超时
位置：`crates/astra-platform/src/platform.rs` 的 `LocalListener::accept`（约 576 行）、`LocalStream::connect`（约 618 行）。

问题：
- 服务端任何时刻只保留一个待连接的管道实例，`accept` 被 `Mutex` 串行化。
- 突发连接时客户端会拿到 `ERROR_PIPE_BUSY`(231)，然后每次 `sleep(10ms)` 重试，**没有上限也没有超时**。
- hosted 模式下每条 TCP/UDP 流都要新建一个管道连接（hosted.rs:1057 `connect_trusted`），所以建连延迟会被拉高到 10 ms 的整数倍。

做法：
- 短期：服务端同时挂起 N 个（例如 8 个）待连接实例，接受一个就补一个；客户端重试加总时限（例如 2 s），并用 `WaitNamedPipeW` 代替固定的 sleep。
- 长期：host 和 worker 之间维持一条持久连接，在上面复用多路流（例如 `yamux`，或者项目已有的 mux 实现），免去每条流的连接、鉴权和 `ready` 往返。在 Unix 上也可以评估用 `SCM_RIGHTS` 把已接受的 socket 直接交给 worker，让 host 退出数据路径，少一次用户态拷贝。

验收：`scripts/windows_load.py`、`scripts/hosted_benchmark.py` 改造前后对比建连速率和 p99。

### T18 `connect_trusted` 每次调用都创建一对 UnixStream
位置：`crates/astra-platform/src/platform.rs:214-231`。

问题：为了拿到自身 uid，每次连接都执行 `tokio::net::UnixStream::pair()` 再 `peer_cred()`，多出 2 个 fd 和若干系统调用。hosted 模式下每条流都会走到这里。

做法：改用 `libc::geteuid()`（或 `rustix::process::geteuid()`），结果可以用 `OnceLock` 缓存。注释里提到 macOS 下 `getpeereid` 的问题只和对端有关，不影响取自身 uid。

### T19 代理组每次连接都遍历依赖图并加锁
位置：`crates/astra-proxy/src/proxy/groups.rs` 的 `resolve`（约 319–354 行）、`refresh`（约 355 行起）。

问题：
- 每个连接都重新用 `HashSet` 和 `Vec` 遍历组的依赖关系，计算可达组集合，但这个集合在配置加载后就固定了。
- 对每个可达的自动组都会调用 `refresh`，后者**先** `self.locks[name].lock().await`（tokio Mutex）**再**检查 `checked` 是否过期。也就是说，即使结果是新鲜的，每个连接也要拿一次各组的锁。
- 选择结果过期时，测速（`probe`，每个成员最长 `timeout_ms`）会在**当前连接的路径上同步执行**，触发它的那个连接要等测速做完才能建立。

做法：
- 在 `Groups::new` 里预先算出每个入口组的可达集合（`HashMap<String, Vec<usize>>`）。
- `refresh` 先在读锁下检查是否新鲜，新鲜就直接返回，不碰 tokio 锁。
- 已有可用选择时一律直接用旧结果，把刷新交给后台 `test_task`（stale-while-revalidate）；只有完全没有可用选择时才同步等待。
- `selections` 改用 `ArcSwap<HashMap<..>>`，读路径无锁。

验收：`scripts/proxy_groups_regression.py` 通过；现有 groups 测试通过；新增测试，验证选择过期时连接不等待测速。

### T20 SOCKS5 UDP 关联：每个包占住 64 KiB 内存，队列过小
位置：`crates/astra-proxy/src/proxy/udp.rs:77-130`。

问题：
- 循环里先 `packet.split_to(len).freeze()`，再 `packet.reserve(65536)`。冻结的 `Bytes` 和原缓冲区共享同一块分配，`reserve` 没法回收，只能重新分配 64 KiB。结果是每个包都新分配 64 KiB，并且在排队期间一直占着；一个 100 字节的 DNS 包也会占住 64 KiB。
- 新目标的处理路径对 `flows` 做了 3 次哈希查找（`get`、`contains_key`、`[]`），还有一次 `retain` 全表扫描。
- `QUEUE_PACKETS = 4`：每个流只能排 4 个包，QUIC 或视频的突发流量很容易被 `try_send` 丢弃。

做法：
- 复用一个固定的接收缓冲区，向下游发送时用 `Bytes::copy_from_slice(body)`（小包拷贝的成本远低于分配 64 KiB）。
- 用 `entry` API 合并查找；`retain` 只在 `workers.join_next()` 分支里执行（已有）。
- `QUEUE_PACKETS` 提高到 32～64，或者改为可配置。

验收：`scripts/udp_regression.py` 通过；用 QUIC 下载测试比较丢包率和吞吐。

### T21 全局流量计数器的缓存行竞争（低优先级）
位置：`crates/astra-proxy/src/proxy/traffic.rs:23-30` 的 `Metrics`。

问题：`upload`、`download`、`proxy_upload`、`proxy_download` 四个全局 `AtomicU64` 挨在一起，所有流在每次读写时都会对它们 `fetch_add`，多核下会有伪共享。现在 TCP 缓冲区已经是 64 KiB，影响有限；UDP 是每包计数，影响更明显。

做法：全局计数只在 `Traffic::drop` 时累加已结束流的总量；`snapshot` 用"已结束总量 + 活跃流各自的计数"算出结果（`snapshot` 本来就要遍历 `flows`）。如果保留全局计数，至少用 `crossbeam_utils::CachePadded` 把它们隔开。

---

## 暂缓（需维护者决策，不要执行）

- 删除旧格式迁移代码：`editor.rs:82` 旧草稿与 `"__legacy__"` 键、`ui.rs:315` `session_version == 0` 迁移、`manager.rs:660` 旧 `instances.toml` 路径、`agent.rs:91` 旧 `manager.sock` 检查。取决于对老用户数据的承诺。
- bincode 1.3 → `postcard`：改变 IPC 线格式，需配合版本协商。
- `std::sync::Mutex` → `parking_lot`：收益小，可在触及相关文件时顺手处理。
- 本地清理（未被 git 跟踪，不涉及提交）：空目录 `crates/astra-server-api`、`crates/astra-server-tui`；`prototype/gui` 残留 `node_modules`/`dist`；`dist/` 下 12 个时间戳目录约 2.7 GB。

## 建议执行顺序
T1 → T6 → **T15 → T16 → T18 → T7 → T17 → T19 → T20** → T9 → T10 → T11 → T2 → T3 → T4 → T5 → T13 → T14 → T21 → T12

（T6 格式化要提前做，因为 T16 涉及的代码被压成了一行，不格式化很难改。）
