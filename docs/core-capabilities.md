# 核心能力与只读 Fake-IP 查询

目标核心通过 `control::identity()` 返回可选的 `capabilities`，身份格式版本仍为 `1`。代理核心声明 `fake_ip_lookup_v1`、`network_diagnostics_stream_v1`、`network_diagnostics_pinned_v1` 和 `network_diagnostics_resolution_v1`；未注册这些功能的核心返回明确的空列表。列表缺失表示旧核心能力无法确认，不能据此推断支持或不支持。

实现时核对发现：计划所称的“既有 Identity JSON 响应”在当前仓库中实际分为两种。落盘身份和外层管理数据使用 JSON，而 control socket 的 `Identity` 响应使用有长度前缀的 **bincode**。直接给其结构追加字段会使新客户端无法读取旧核心，所以本次采用以下兼容策略：

- `Identity` 的 JSON 序列化包含可选能力字段；旧 JSON 读取器忽略新增字段，缺少字段的旧 JSON 仍可读取。
- `Identity` 的 bincode 序列化始终保留原有四字段布局。既有 Runtime、ConfigSnapshot 等二进制快照没有更改。
- 单次 `Identity` control 响应在原 bincode 数据后附加 `ASTRA-CAPS\0` 标记和版本为 `1` 的 JSON 扩展，扩展最多 4 KiB，整个身份帧最多 16 KiB。当前旧读取器的 `bincode::deserialize` 会忽略尾随数据；已有实际旧结构和旧 `receive_management` 读取器测试验证此行为。
- 新读取器显式解析该尾部扩展。没有尾部是“能力无法确认”；未知标记、未知扩展版本、损坏 JSON、过大的列表都作为协议错误处理。不会把损坏尾部当成旧核心。
- 能力只通过单次身份 helper 读取；兼容性的会话 `Response::Identity` 仍保持旧二进制布局，不承载能力扩展。

`astra_ipc::control::fake_ip_lookup(path, addresses)` 每次重新读取目标核心身份，只有明确声明 `fake_ip_lookup_v1` 才发送新命令，因此不存在跨重启复用能力缓存的问题。命令携带刚读取的 `expected_boot_id`，响应带独立版本和核心启动标识，核心切换后必须重新读取身份再重试。新命令和响应均追加在原枚举末尾，旧枚举编号保持不变。

客户端与核心各自检查一次最多 64 个地址。返回结果保持请求顺序，每项为地址和可选域名。查询只读取尚未过期的 Fake-IP 映射表，不分配地址、不刷新 TTL、不将普通反向映射当成 Fake-IP，也不因地址属于常见 Fake-IP 网段就作判断。未找到映射的 `domain` 为 `null`。

错误分类包括：明确不支持、通信失败、协议错误、能力无法确认。只有明确不支持可触发兼容降级。合法响应中的核心执行失败另保留为 `Core`，调用参数越界保留为 `InvalidRequest`，避免把它们伪装成协议解码错误或不支持。

本地回归覆盖旧 JSON、旧 bincode 与真实旧读取器兼容、原命令/响应编号、缺少及明确否定能力时不发送新命令、64 地址双端校验、启动标识变化、错误响应类型、损坏帧、连接错误，以及未知/过期地址不会被误认。相关命令：

```sh
cargo test -p astra-ipc --lib control::fake_ip
cargo test -p astra-dns --lib diagnostic_tests
```


管理 v2 的只读桥接由 Agent 的 `core_network_v1` 能力声明：

- `Query::NetworkCore { instance }` 返回 `NetworkCoreReply`，包含状态、采样时间和可选核心身份（启动标识、PID、原身份版本、可选能力列表、配置摘要）。缺失能力时状态为 `unconfirmed`，仍保留合法身份字段。
- `Query::FakeIpLookup { instance, addresses }` 返回 `FakeIpLookupReply`，保留相同错误分类和最多 64 项映射。Agent 也在联系核心前校验数量。
- 目标先按 Agent 实例注册表解析，必须是正在运行的代理实例。不会把服务端/规则核心地址猜成代理控制 socket。身份查询还核对当前管理记录的 PID。
- 两类查询均不创建任务，也不进入配置变更锁；客户端不能把 `core_network_v1` 当作目标核心已经支持 Fake-IP 的证据。

诊断子进程的能力由实际可执行文件的 `--program-info` 独立采样，不能沿用仍在运行的核心声明。该入口在配置、提权和工作线程初始化前返回程序身份；代理程序声明 `network_diagnostics_stream_v1`。Manager 的查询有 5 秒期限、16 KiB 输出上限，查询阶段与探测阶段均在取消或失败时终止并回收子进程。

有效元信息明确缺少新流能力时，只有 system TCP 和 UDP/TCP DNS 可以使用旧 `--nettool-stream`，报告标记 `legacy_details=true`，不补造详细 DNS 数据。指定接口、DoT、DoH 或显式 bootstrap 不降级。旧程序不认识元信息入口时能力无法确认；损坏输出、读取失败及异常退出分别保留实际失败，均不会当作明确不支持而继续探测。

这与“新客户端连接旧 Agent”是两个不同分支：旧 Agent 的合法 Hello 未声明作业能力时，GUI/TUI 与 Web 提示 Agent 版本过旧，不再调用原 `Command::NetTool`（该命令仍保留给旧调用者）。新 Agent 连接旧探测程序仍先执行上述程序能力查询，不能因 Agent 自身支持作业而跳过校验，也不能因查询失败而偷偷调用旧工具。

旧流内部的 `nettool/mod.rs::resolve` 仍会对域名执行系统 `lookup_host`，不受后续 socket 网卡绑定约束；允许的系统路由兼容请求保留这一已知限制，不宣称复现实例 DNS/代理路径。旧管理命令、二进制布局和文本入口尚有实际兼容调用，本轮继续保留；完全迁移调用者后再单独评估废弃。详见[网络诊断的解析边界](network-diagnostics.md#dns解析边界与探测上下文)。

由 Agent/Manager 启动的诊断子进程将应用状态与真实网络上下文分开：新详细诊断及旧 System/Interface 基础请求默认使用每次私有的 runtime、临时和工作目录，删除继承的 `ASTRA_*` 控制变量并显式设置本次 runtime；旧 System/Interface 即使收到非空配置也不加载完整 App。Agent 启动的 `--program-info` 使用相同诊断环境，仍须检查实际执行文件。用户独立运行 `--nettool` 时继续继承调用者环境，不自动创建这组私有目录；System/Interface 仍忽略所给 profile，Outbound 仍读取原配置与缓存。环境隔离不新增能力声明，不改变程序身份、旧 bincode、管理请求或报告布局。

诊断保留目标 Agent 的当前账户/权限、网络命名空间、接口、路由、TUN、系统 DNS 和账户可见根证书；这不是完整文件系统访问沙箱。`SSL_CERT_FILE` / `SSL_CERT_DIR` 等实际信任输入不能被清空，相对证书路径、文件型系统 DNS 环境路径和 `PATH` 搜索目录在改工作目录前固定。非空 `LD_*` / `DYLD_*` 保留原 cwd，不重写加载器语法；路径列表无法无损表示时（例如 Unix cwd 含 `:`）整体放弃改写并保留原 cwd 与全部原路径变量。这两种例外仍隔离 runtime/临时目录。旧 Outbound 兼容请求因依赖实例订阅缓存、配置侧车与相对资源，保留原 runtime/cwd，仍执行旧接口本身的组选路语义，可能触发额外健康探测；新三端不会向旧 Agent 提交该组合。既有节点测速与订阅刷新启动器不在本次变更范围内。逐工具的保留项和例外见[诊断进程环境与准确性边界](network-diagnostics.md#诊断进程环境与准确性边界)。

新作业报告含 Manager 添加的 `context` 项，记录采样时间、运行核心身份、配置摘要、探测程序身份与能力。`probe_binary.capability_process_id` 是短暂的 `--program-info` 查询进程 PID，后续真正探测进程的 PID 未包含在该快照中。运行配置采样由前后两次身份夹取，只有启动/进程/应用摘要均保持一致且文本摘要匹配时才标记 `matched_running`；离线文件标记 `saved_not_applied`，读不到或变化中标记 `unconfirmed`。system/interface 探测的 `config_used=false`、`selection_chain=[]`、`egress_source=requested`；固定出站为 `config_used=true`、`egress_source=pinned_request`，表示使用从确认配置中提取的单节点或 DIRECT 参数。两类都不是已经发包或绑定成功的证据，实际连接结果由探测项提供。子进程不能伪造 Manager 的 `context`。

固定出站使用独立 `DetailedRequest`，保留旧 `Request` 与旧 NetTool 管理命令。Manager 以 Identity、配置快照、Runtime、Identity 配对重建每层组选择，手动组直接选择后代节点时仍保留中间组及其绑定来源；外层首个网卡优先，随后是运行时 TUN 开关下适用的配置默认网卡。发送前重新读取 Identity 和 Runtime，重启、重载或整条路径变化均拒绝启动。报告只含选择链、具体节点标签、`effective_interface` 和 `interface_source`；含凭据的单节点配置仅经 stdin 交给子进程，不进入报告，也不创建整份 App、健康检查或订阅刷新。

负载均衡、缺层、未知成员、截断、成员/模式不符、无法确认的自动 TUN 网卡、订阅缓存与源配置不能证明相同版本的节点，均拒绝自动固定。用户选择已确认的具体本地节点可以绕过组选择，但仍必须保证实际绑定。配置解析错误不包含可能带凭据的 TOML 原文。

实际探测程序须另外声明 `network_diagnostics_pinned_v1` 才能接收固定节点；显式解析器、接口域名解析、固定路径以及 DoT/DoH 请求须声明 `network_diagnostics_resolution_v1`，防止旧程序忽略新增字段或回显敏感服务 URL。自动解析器只来自目标 Agent 本次采样中可确认的具体网卡。DNS/NRPT 查询失败、截断或无法绑定接口的分域策略会阻断自动选择，要求用户指定解析器；不会过滤掉未知策略后悄悄采用普通网卡 DNS。
