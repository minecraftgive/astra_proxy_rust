# Rule Core GEOIP 内存优化与后续审查

2026-10-20，Windows x64，32 逻辑处理器，release 构建。此次完成 GEOIP 优化；其余项目为源码审查结果，尚未实施或测定收益。

## 已完成的改动

- Rule Core 在规则集展开后收集实际引用的 GEOIP 分类，大小写归一化，排除内置 LAN。DAT 只为这些分类建立 IPv4/IPv6 索引；MMDB 保留原有匹配路径。
- GEOIP DAT 按分类、按网段逐条解码，复用单条 CIDR 的缓冲，避免一次建立全部网段的 protobuf 对象。仍校验所有分类的名称、重复项、IP 长度、前缀、消息边界与总条数限制；未引用分类的坏数据也会拒绝。
- 资源加载可返回已经校验的解析结果，GEOIP 不再“校验时建索引，正式使用时再建一次”，DAT 也不再为校验复制整份文件。
- 缺失分类仍报错；下载、校验或落盘失败仍回退到有效缓存。原始规则优先级、来源解释、IPv4-mapped IPv6 与 inverse_match 语义保留。

实现位于 `crates/astra-rule-data/src/geoip.rs`、`crates/astra-rule-data/src/lib.rs` 和 `crates/astra-rule/src/engine.rs`。

## 新旧版本实测

基线为本次修改前从工作区编译的程序，优化版使用同一构建配置与依赖。没有使用上一轮运行中旧发行版的内存值作对比。

每个场景交替启动 before/after 各三次，启动后等待 1 秒读 Windows 进程计数器，报告中位数。启动时间从创建进程计至观察到 `Rule Core ready`，日志轮询间隔 10 ms，受文件缓存、杀毒扫描和系统调度影响。峰值来自 Windows 进程峰值计数器，不依赖轮询捕捉瞬时峰值。

| 场景 | 指标 | 修改前 | 修改后 | 降幅 |
|---|---|---:|---:|---:|
| 仅 GEOIP：CN、TELEGRAM | 私有提交内存 | 25.63 MiB | 3.06 MiB | 88.0% |
| 仅 GEOIP：CN、TELEGRAM | 工作集 | 29.53 MiB | 8.13 MiB | 72.5% |
| 仅 GEOIP：CN、TELEGRAM | 峰值私有提交 | 120.68 MiB | 27.83 MiB | 76.9% |
| 仅 GEOIP：CN、TELEGRAM | 就绪时间 | 762 ms | 230 ms | 69.9% |
| 当前完整规则 | 私有提交内存 | 53.14 MiB | 31.29 MiB | 41.1% |
| 当前完整规则 | 工作集 | 56.99 MiB | 35.99 MiB | 36.8% |
| 当前完整规则 | 峰值私有提交 | 152.84 MiB | 77.13 MiB | 49.5% |
| 当前完整规则 | 就绪时间 | 1,155 ms | 607 ms | 47.5% |

完整配置包含 114 条源规则、17 个 GEOSITE 引用、2 个 GEOIP 引用。GEOIP 数据库大小 16,587,403 字节，260 类、1,060,863 条 CIDR；使用的 CN 为 10,009 条、TELEGRAM 为 12 条。GEOSITE 文件为 4,274,537 字节，包含 1,557 类、220,777 条域名记录。

这里只测规则加载与启动后空闲占用，没有外加请求负载，没有测路由吞吐提升，也不是长期泄漏测试。在线更新地址在测试副本中移除；测试实例使用独立端点，原有实例和配置未修改。

原始测量与二进制、数据库 SHA-256 见 [rule-memory-results.json](rule-memory-results.json)。

## 其他内存与性能优化项

以下优先级代表建议验证顺序，不是已证实的缺陷严重程度；容量推算不代表当前实际占用。

| 优先级 | 项目与源码依据 | 建议与验证重点 |
|---|---|---|
| 高 | GEOSITE 在 `engine.rs` 的资源校验和规则展开阶段各解码一次；`geosite.rs` 一次物化全库域名、属性。 | 复用已校验结果，并逐组读取、仅保留被引用组。属性筛选、重复组检测、坏更新回退及首条匹配顺序必须保持。当前完整配置优化后仍约 31 MiB，可以先对这部分做单变量测量。 |
| 高 | `Entry` 为每个索引项保存 `RouteAction`，代理动作内含独立 `String`；GEOSITE 展开还复制规则的 kind/action 字符串。 | 用共享动作或动作 ID，并减少展开阶段临时副本；记录编译峰值与持有字节数，避免为了省内存增加每次查询分配。不能把剩余 31 MiB 全部归因于这些字符串。 |
| 高 | `App::route_connection` 在规则模式下进行进程归属查询；Windows `process::table` 每次查询分配系统连接表，再复制到新 Vec。当前规则没有进程规则。 | 让 Rule Core 按配置版本发布“是否需要进程身份”的能力，再决定查询。需要兼顾 route 日志用途、规则热更新及无法识别身份的行为。查询路径本身可避免整表二次复制；不要简单长期缓存 PID，连接复用与 PID 重用会造成误判。 |
| 中 | 代理组 `Lease::wrap` 为每条相关 TCP 连接创建双向 duplex（每方向上限 32 KiB）、额外 copy relay 与 task。 | 考虑在实际转发处监听取消，减少一层中转；验证半关闭、背压、分组切换、丢弃清理。缓冲按流量增长，不代表每条空闲连接立即占满 64 KiB。 |
| 中 | TUN TCP 栈和读写适配层各有收发缓冲，共四块 256 KiB，约 1 MiB/流；UDP 每流有 64 KiB 接收缓冲，默认上限 512 流、空闲期限 120 秒。 | 先测并发、流量及空闲回收。512 个活跃 UDP 接收缓冲本身可达 32 MiB，另有队列和协议开销；TCP 缩小窗口可能降低高延迟链路吞吐，宜配置化并做延迟/吞吐对比。四块缓冲是在后续 [TUN 回环排查](TUN_RECAPTURE_FIX.md) 中沿依赖实现核实的。 |
| 中低 | 域名通配匹配每个 pattern、每次请求都创建两个标签 Vec；regex/keyword 等也可能扫描已无法胜过更早匹配的规则。 | 使用迭代器或预编译标签，并按已命中的最小规则序号剪枝。当前配置没有 domain_pattern，这项不是当前内存主因；需维持跨类型首条匹配。 |
| 低 | Rule Core 默认 Tokio 多线程运行时，本机约 32 个工作线程。 | 用 2/4/默认线程对比 IPC 并发延迟和吞吐再选默认值。优化后的 GEOIP-only 整个进程仅约 3 MiB 私有内存，说明线程不是此次主要占用源；虚拟栈预留也不能当作实际 RAM。 |

有界机制也已检查：路由缓存最多 65,536 项并有 TTL；UDP 流数、每流待发队列有限制；旧 `Arc<App>` 会被既有连接持有到结束。因此“热重载后占用没有立即下降”不能单独判定泄漏。若要继续排查，应增加当前缓存条目、活动流、持有的配置代数等指标，与私有内存一起采样。

## 验证与复现

- `cargo test -p astra-rule-data -p astra-rule -p astra-convert`：58 项通过，1 项手动性能测试按原设置忽略。
- 新增测试覆盖按需索引、大小写/重复选择、IPv4/IPv6/mapped/inverse 一致性、未引用分类的坏数据、缺失分类、未知字段/字段乱序/截断，以及解析结果复用和坏更新回退。
- `scripts/ruleset_regression.py`：通过规则集在线更新、失效下载缓存保留、MMDB/GeoIP DAT/GEOSITE 查询、缺失分类拒绝、失败重载保留与离线缓存启动等端到端检查。测试组合为新 Rule Core、新构建的 converter 和发行目录中的 Proxy Core。
- `cargo build --release -p astra-rule` 已完成；产物在 `target/release/astra-rule.exe`。本次未替换发行目录或重启正在运行的核心。

本机留存了修改前的基线程序，可以重复测试：

```powershell
python scripts/windows_rule_memory.py --before .test-tools/rule-memory/astra-rule-before.exe --after target/release/astra-rule.exe --config "$env:LOCALAPPDATA/.astra/tui/configs/t2.toml" --output .test-tools/rule-memory/results.json
```

测试脚本针对包含 GEOIP、GEOSITE 数据库路径的内联规则配置；有外部 `rule_sets` 时会明确拒绝，避免改变相对路径或误触远端更新。
