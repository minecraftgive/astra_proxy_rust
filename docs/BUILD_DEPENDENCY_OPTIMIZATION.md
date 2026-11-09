# 编译、产物体积与运行时优化方案

2026-11-09。以实施前的完整工作区（包括既有未提交改动）为基线。保持现有功能、协议、密码套件、配置格式、管理接口与 dev/release 配置；release 保留 opt-level=3、fat LTO、单 codegen unit 和 panic unwind。允许维护局部第三方补丁；发布副本剥离符号，同时保存匹配的原始二进制。

## 实施顺序

1. 构建：将元数据监听从整个 crates 收窄到实际本地依赖；保留版本、构建时间、目标、Git 与 SOURCE_DATE_EPOCH 信息。脚本只链接一次 manager，再复制普通文件作为 host；直接 Cargo 构建仍支持两个入口。为普通 CI Rust 任务增加依赖缓存，保留干净构建验收；删除重复 Web 类型检查。
2. 发布：原始构建产物不变。先归档原件，再剥离发布副本并处理签名，最后计算 manifest/校验值、打包。按构建身份、平台和剥离前后 SHA-256 关联排错文件。debug 不剥离；不改变更新包的普通文件规则。
3. 依赖：将 management 的 ts-rs 和关联 bindings 隔离为非默认功能，导出工具显式启用；ZIP 保留 Stored/Deflate，去掉 Zopfli；管理器 WebSocket 统一到 0.29。原拟裁剪代理的 Hyper HTTP/2，经复核 DoH 生产代码仍直接调用它，取消此项裁剪，保留显式功能声明。
4. 运行时：StableWrite 用消费位置替代反复移动剩余字节，排空仍释放缓冲；AMV2 管理消息合并头部/正文写入，不改帧格式、上限、flush 或重放语义；Web 审计移入阻塞任务并等待完成，保留轮转与错误处理。原拟合并规则 IPC 的 bincode 帧、仅在半关闭后更新时间，两项经十对性能对照未获采用，恢复实施前代码。
5. 第三方：meow-proxy 拆分 ss-core 与插件能力，原 ss 保持完整能力，Astra 保留全部现有密码套件。gpui-component 的 chart/dock/diff/time/questionnaire/carousel/table 及初始化移入默认开启的 extra-components，Astra 关闭；保留基础输入、编辑器、文本、主题及其依赖。记录来源、版本、许可证、补丁与回归方法。

## 验证和采用条件

- 保存基线源码身份、参数、依赖功能、构建报告和产物；分别测冷构建、无改动重建和模块增量构建。冷构建必须使用空构建缓存，不能将已有缓存的结果标作冷构建。
- 验证无关包修改不再使其他程序的元数据失效；普通构建不带 ts-rs/Zopfli/旧 WebSocket，单角色及组合构建都检查功能合并。
- 回归协议矩阵、SS 密码套件、TLS/WS、更新解压与 manifest、类型合同、GUI 输入和编辑、部分写入、半关闭、组切换、IPC 异常、审计轮转。
- 运行性能按相同 release 配置前后交替至少 5 轮，观察 ordinary/hosted、direct/group、缓存命中与未命中的吞吐、p50/p95/p99、CPU/内存。吞吐下降超过 3% 或尾延迟上升超过 5% 时扩展到 10 轮；持续回退则撤回对应改动。
- 各阶段分开记录结果；Linux/Windows/macOS 分开验收。本机结果不能代替其他平台，也不将多个改动的收益归因于某一项。
- 工作区已有改动不 reset/stash、不整体格式化。只提交本任务可明确隔离的变化，无法安全隔离时保留工作区供审查。

## 已有分析证据与边界

既有 macOS ARM64 release 六个客户端程序临时副本剥离后合计 71.75 → 60.53 MiB（约 -15.6%）；逐文件 gzip 合计约 -5.9%，不等于完整安装包结果。convert/proxy/manager 的代码和数据节相同且签名校验通过，convert 帮助输出一致。原件因 debug=false 不含完整源码调试信息，另存原件不能恢复本来不存在的行号。

SS 部分插件符号仍存在于已有 release 样本，值得裁剪；Zopfli 和类型生成的首要收益是编译成本。Tokio full、加密后端替换、QUIC 合并、QPACK 单独提取、图像格式和平台功能裁剪不在本轮实施范围。hosted TCP 已合并数据帧头正文，IPC 优化仅针对尚未合并的控制消息。

## 执行记录

### 实施内容

构建、发布、依赖与第三方补丁已落入代码，运行时按下述验收结果采用部分候选。构建监听表由 `scripts/build_metadata_inputs.py` 生成，架构检查会阻止遗漏更新；额外精确监听平台安装逻辑嵌入的两张 GUI 图标，不恢复整个 GUI 目录监听。`astra-host` 在构建脚本中复用 manager 的普通文件副本。第三方补丁的来源、许可证、完整/裁剪功能组合与复验命令保存在两个 vendor 目录的 `ASTRA_PATCHES.md`。

普通客户端、服务端、Web、GUI 的 normal/build 依赖检查均不再包含 ts-rs、Zopfli 或 0.26 版 WebSocket。代理的 DoH 与 Web 均保留 Hyper HTTP/2；SS 的原有 stream/AEAD/AEAD-2022 密码套件和 h2 传输保持启用。完整 SS 插件组合与完整 GPUI 组件组合另行编译验证。单包依赖声明不能依赖组合构建碰巧合并其他包的功能，后续检查须覆盖代理单包。

### 产物验收（macOS ARM64）

以下为撤回两项运行时候选后重新生成的最终原生及 GUI 发布包，比较每次构建的原始二进制与发布副本，减幅只归因于符号分离。GUI 行统计该角色包内全部二进制，包含配套核心；目录大小包含资源、配置和 manifest，安装包不含符号归档。

| 角色 | 发布目录 MiB | 安装包 MiB | 符号包 MiB | 二进制减幅 |
| --- | ---: | ---: | ---: | ---: |
| client | 63.949 | 29.590 | 31.388 | 15.61% |
| server | 44.919 | 20.626 | 21.800 | 15.33% |
| controller | 14.649 | 6.677 | 7.043 | 15.15% |
| web | 6.561 | 2.917 | 3.109 | 18.57% |
| astra-gui | 87.014 | 39.979 | 42.442 | 16.24% |
| astra-server-gui | 67.985 | 31.016 | 32.856 | 16.23% |

最终四个原生角色共 194 项完整性与行为一致性检查通过：归档及文件摘要、manifest、符号恢复件、Mach-O 可加载节、ad-hoc 签名与入口行为一致。manager/host 构建原件逐字节相同且不是硬链接；发布副本会因各自签名标识而有不同摘要。客户端安装包实际启动、管理通信、64 KiB 转发与退出清理通过；服务端包的配置、认证热更新、失败回滚和清理通过。

最终 GUI 两套归档的 12 条二进制记录和 106 项检查通过。每套 GUI 包内二进制合计分别为 108,716,736 → 91,064,136 字节、84,957,360 → 71,166,264 字节，约 -16.24% 和 -16.23%。最终包曾在锁屏期间等待首帧 30.91 秒后超时，该失败及环境证据保留。2026-11-09 解锁后的补验直接使用相同最终归档：客户端、服务端各通过信号退出和系统原生退出，共 4/4 项；窗口绘制、隔离后台连接、会话保存及后台清理均通过。信号退出使用实际解压的发布 bundle；原生退出沿用测试脚本的临时独立 app 封装，执行的是同一发布二进制。两个前端共 12 个 manifest 文件校验通过，5 个 GUI 归档及 11 个冻结二进制摘要均未改变，**最终窗口验收已完成**。另一个独立的既有边界是：旧、新 `.app` 均未完成资源封装层面的整体签名，严格 bundle 验签失败；独立 Mach-O 验签通过。本轮未扩大为正式签名/公证改造，该既有签名边界未计为通过。

### 功能回归

- Rust 各包最近结果去重合计 1108 通过、0 失败、28 项原有忽略；这是分批补验汇总，不是单次全工作区全绿。最初强制 `TMPDIR=/tmp` 引起 1 项目录组归属和 5 项路径符号链接拒绝；基线复现同一客户端失败。改用解析后的短私有临时目录、正确主组归属后，client/platform 281 项通过、1 项原有忽略，未改测试或生产逻辑。2026-11-09 已补跑 client 文档测试入口并通过，当前实际有 0 个文档用例，不增加覆盖数量。
- 最终 Python 构建、归档与对照驱动测试 81 项通过；`scripts/test.py` 和各级文档均补齐 `bindings`，移除重复类型检查但保留 build 中的 tsc。Web 75 项通过，构建成功；类型合同校验通过。
- 裁剪 SS 组合 23 项、完整 SS 插件组合 39 项通过；裁剪与完整 GPUI 组件组合均编译通过。
- 协议 TCP 矩阵 23/24 通过；支持路径的严格 8 MiB 半关闭尾响应 3/3、协议 UDP 6/6、UDP 关联生命周期 1/1 通过。Xray `ss-plain` 和额外 sing-box AES128GCM TCP 试验在前后各两次均超时，SS2022 TCP 和三种 SS UDP 路径通过。保留这些共同失败，不能把结果写成全部协议通过。
- 元数据生成表、架构边界和差异空白检查通过。只有既有第三方 AEAD 弃用警告；不据本机结果宣称 Linux/Windows 或 TUN 实机已验收。
- 恢复 Hyper HTTP/2 后，代理单包 lib/bin 检查通过；manager 编译检查确认实际构建脚本包含两张图标监听。撤回两项运行时候选后，同五库最终 225 项通过、0 失败、2 项原有忽略；规则 IPC 生产代码和 relay 全文与基线逐字一致。

### 编译实测

| 场景 | 实施前 | 实施后 |
| --- | ---: | ---: |
| management 库，最终源码，空 target 的 dev 构建（单次） | 14.16 s | 11.78 s |
| manager release，无改动重建（三次中位数） | 0.23 s | 0.24 s |
| manager release，触碰无关 GUI 源码 | 150.83 s | 0.23 s |
| manager release，触碰 management 源码 | 153.73 s | 151.69 s |

最终管理库冷构建单次样本约缩短 16.8%（macOS ARM64、Rust 1.98.0），不可外推为全项目收益或统计显著提速。无关 GUI 源码变更从重编 manager 一个包变为零重编；相关 management 变更两边都重编两个包，耗时相近。增量数据来自运行时候选撤回及图标监听补充之前的阶段；最终保留同一监听算法，测试所触碰的 GUI lib.rs 不在新增资源路径中。最终库冷构建已在所有构建/测试停止后另开两个空 target 重测。

### 测量口径与复验材料

实施前包含既有未提交修改的源码、清单和二进制保存在 `target/optimization-baseline/`。原始五包 release 构建 184.18 秒；实施后的 all-role 构建与打包 223.42 秒，其中 Cargo 201.24 秒；GUI 两入口构建 238.73 秒。最终重新构建 all-role 并打包 197.53 秒，其中 Cargo 175.87 秒；GUI 两入口重新构建 176.07 秒、打包 22.91 秒。以上均使用已有缓存且构建范围/缓存状态不同，**不能据此声称冷构建加速或退化**。

对照构建另用独立空目录测 management 库的冷构建，并对 manager release 记录无改动、无关 GUI 文件和相关 management 文件的时间戳变更；不外推为全工作区冷构建结果。增量对照的前后源码使用隔离 target，复用第三方缓存前先备份并失效本地包指纹，再核对实际构建脚本输出。一次跨源码共用 target 的尝试复用了错误构建脚本，已弃用并保留在 `builds-shared-cache-rejected/`，不参与结论。最初的基线二进制在改代码之前生成并固定保存，不受这次后续计时尝试影响。

运行性能使用同一五包 release 命令生成前后输入，停止其他编译与测试后交替采样。每场景 1000 次连接，吞吐至少 3 秒；5 对超过阈值扩展到 10 对，统计每轮指标的中位数，而非合并所有延迟样本的分位数。

### 运行时采用结论

首轮因压测端的临时 TCP 端口耗尽中断，不计入结果；随后规则专用回显端在完整回显 1024 字节后主动关闭，客户端记录延迟后等待 EOF，两侧使用完全相同的修正 fixture。规则完成速率包含完整关闭等待，不能直接与旧 fixture 的速率对比；未调整系统网络参数。

正式 `performance-v2` 共十对、40 个子运行、759.1 秒采样。门槛未通过：1000 规则/暖缓存/并发 1 的完成速率 -6.59%，10000 规则/暖缓存/并发 32 为 -11.47%；部分尾延迟越线。hosted 四场景吞吐 -1.11%～+0.96%、CPU -0.81%～+0.17%，未触发吞吐门槛。逐轮结果明显波动，不能证明某项代码导致回退，也不能改称门槛通过。

按预设采用条件，撤回与这些直连路径有关的规则 IPC bincode 合帧和 relay 时钟条件修改，恢复基线实现并保留协议/半关闭回归测试。DIRECT 和 group→DIRECT 不经过 StableWrite；AMV2 管理帧及 Web 审计也不在此次规则压测直接路径，保留其功能验证通过的改动，**不宣称已证明运行时提速**。失败候选的源码身份、二进制与原始数据保留，最终发布包另建，避免把淘汰候选的结果当成最终版本的性能证明。回退后的最终版本未再次执行性能对照，故不提供其运行时提速结论。

### 最终交付与剩余边界

- 方案与代码已保存；没有提交或覆盖既有未提交修改。
- 最终原生安装包与符号包：`target/optimization-final-dist/2026-11-09_02-56-26_927822/`。
- 最终 GUI 安装包、客户端更新包与符号包：`target/optimization-final-gui/`。
- 11 个最终未剥离二进制固定保存在 `target/optimization-final/bin/`；失败性能候选保存在 `target/optimization-after/bin/`，两者不混用。
- 最终验收汇总：`target/optimization-results/final-verification.json`；四角色/GUI 明细为 `final-artifacts.json`、`final-gui-artifacts.json`，最终运行时回归为 `final-runtime-tests.json`，Python 日志为 `final-python-tests.log`，依赖检查为 `dependencies-final.json`。
- 编译资料：`final-cold-builds/comparison.json` 与 `builds/comparison.json`；未采用的性能候选：`performance-v2/comparison.json`、`performance-v2/diagnostics.json`。路径均相对 `target/optimization-results/`。

这些材料不进入安装包。本机待补的最终 GUI 窗口与 client 文档测试入口均已完成。Linux/Windows/TUN 实机、既有 AES128GCM TCP 失败与 `.app` 资源层签名边界仍不被本轮结果覆盖；保留这些限制，不宣称所有平台或全部诊断通过。

### 补验记录（2026-11-09）

| 最终发布前端 | 信号退出 | 系统原生退出 |
| --- | ---: | ---: |
| 客户端 GUI | 通过，4.71 s | 通过，4.82 s |
| 服务端 GUI | 通过，5.43 s | 通过，4.81 s |

对应记录为 `target/optimization-results/final-gui-resumed-smoke.json`、`final-server-gui-resumed-smoke.json`；文档测试记录为同目录 `resumed-client-doctests.json`。`final-verification.json` 已补入成功结果，同时保留锁屏时失败的原始步骤和独立的签名边界。汇总字段 `current_host_pending_checks_passed=true` 表示本次待补项已通过；广义 `all_requested_checks_passed` 因既有 bundle 严格签名诊断失败仍为 false，不把它解释为本次窗口补验失败。

验收脚本增加 `--frontend astra-server-gui`，默认客户端入口保持不变。可使用冻结目录复验程序行为（本次实际测试从归档解压目录启动）：

```sh
python3.12 scripts/gui_smoke.py --bin-dir target/optimization-final/bin --termination signal
python3.12 scripts/gui_smoke.py --bin-dir target/optimization-final/bin --termination native
python3.12 scripts/gui_smoke.py --bin-dir target/optimization-final/bin --frontend astra-server-gui --termination signal
python3.12 scripts/gui_smoke.py --bin-dir target/optimization-final/bin --frontend astra-server-gui --termination native
```

