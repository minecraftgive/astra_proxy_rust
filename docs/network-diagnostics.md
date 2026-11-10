# 网络诊断：实现与验收状态

阶段 0 的任务、取消、报告存储与兼容机制已经实现；阶段 1–3 的系统状态、受约束解析、常用工具、系统证书快照、STUN/NAT、逐跳、批量任务、证据拓扑和共享规则均已有实现。本轮已完成三平台各自的原生与 TUN 样本、macOS 普通/root 权限差异、真实 Mac→Windows 远程链路和三组新旧版本组合验收。样本覆盖范围与环境限制分别记录，不把本地测试或单个样本外推为全部环境通过。逐条映射见 [计划验收清单](network-plan-acceptance.md)。

新后台诊断支持系统路由、指定网卡、配置 DIRECT，以及可确认的 SOCKS5 / HTTP CONNECT 固定代理路径。TCP、TLS、HTTP、DNS 共用受约束解析；代理 DNS 只支持 TCP/DoT/DoH，代理 UDP/STUN 只声明已验证的 SOCKS5 UDP ASSOCIATE。其他协议组合明确拒绝，不静默改走系统路由或 DIRECT。单项任务和完整诊断批次均可恢复查询、取消及保留部分结果。

新诊断不初始化完整代理 profile，不额外启动健康检查、组测速或订阅刷新。配置出口只重建一个固定叶节点与选择链，将必要节点配置私下传给诊断子进程；报告不包含节点凭据。系统路由或指定网卡仍是独立探测，不能解释为实例当前代理出口。旧 `NetTool` 接口及文本结果继续保留给旧调用者；三端不再回退到旧工具路径，目标 Agent 未声明 `network_diagnostics_jobs_v1` 时直接提示更新。

## 能力与目标

提交新命令前读取目标 Agent 的 `Query::Hello`，检查 `Hello.capabilities`：

- `network_diagnostics_jobs_v1`：支持本文的异步任务和报告接口。
- `network_diagnostics_system_tcp_dns_v1`：系统路由 TCP、UDP/TCP DNS 基础工具。
- `network_diagnostics_dns_detail_v1`：新 DNS 详情请求与报告接口。
- `network_diagnostics_interface_ip_v1`：显式网卡、目标 IP 或 DNS 引导 IP 的受约束请求。
- `network_diagnostics_resolution_v1`：显式解析器、加密 DNS 与网卡域名等新版解析请求。
- `network_diagnostics_pinned_v1`：配置 DIRECT/节点/代理组的固定路径请求。
- `network_diagnostics_tools_v1`：新常用工具、地址族与系统信任库语义。
- `network_diagnostics_nat_v1`：独立物理网卡 NAT 请求。
- `network_diagnostics_path_v1`：TUN 感知的 ICMP / 逐跳请求。
- `network_diagnostics_batch_v1`：最多 32 项、3 路径的完整诊断批次。
- `network_state_v1`：目标 Agent 网卡、地址、路由与系统 DNS 采集。
- `core_network_v1`：读取运行核心身份和能力，以及有界 Fake-IP 查询的管理桥接。

通过 Controller 访问时，`Hello` 请求必须携带所选 `agent_id`，不能使用 Controller 自身的能力代替目标 Agent 能力。旧 Agent 未声明新能力时，界面提示“目标 Agent 版本过旧”，不提交任何探测。通信失败或响应类型错误要报告原错误，不能当作旧版本不支持。

提交时重新读取目标 Agent 的最新 Hello。仅有 `network_diagnostics_jobs_v1` 不能证明旧 Agent 理解新增 `resolver` 字段；显式解析器、DoT/DoH、网卡 TCP 域名或需要解析的网卡 DNS 服务名必须声明解析能力，Outbound 必须声明固定路径能力。新工具、指定地址族、NAT、路径工具与批次还需各自能力；未声明时在提交前明确拒绝，不发送会被忽略的选项；带解析器的请求不能丢弃字段降级为旧工具。探测程序的独立能力校验不能替代这一 Agent 门槛。

三端已移除旧 Agent 准入矩阵：没有任务接口的 Agent 不再执行任何前端探测（包括 TUI DNS 检查器的即时查询），以免旧接口的系统 DNS 行为被当作受约束诊断。

共享客户端位于 `crates/astra-client/src/network_diagnostics.rs`。`Client::new` 在创建时固定当前管理端点和 Agent；后续切换界面目标不会把旧任务的查询或取消发给另一 Agent。`availability` 与每次 `submit` 都读取目标能力；`submit`、`task`、`report`、`cancel` 分别对应提交、通用任务查询、报告分页与取消确认。Web 使用生成的 `apps/web/src/generated/management.ts` 契约。

Agent 能力只说明管理接口，不能代替运行核心或实际探测可执行文件的能力声明：

- 运行核心身份通过现有 `Identity` 返回可选能力列表，身份版本不递增；Fake-IP 最多查询 64 个地址，先确认 `fake_ip_lookup_v1` 再发送。缺少能力字段、明确不支持、通信失败和协议错误分别处理。查询未命中不按地址段推断 Fake-IP。兼容的二进制身份扩展、管理查询及错误分类见 [核心能力与 Fake-IP 查询](core-capabilities.md)。
- 探测前，Agent 对**实际将启动的程序**执行有界 `--program-info` 查询，要求程序身份为 `astra-proxy`，并根据其自身的 `network_diagnostics_stream_v1` 声明选择新详细输出。不能因为正在运行的旧核心或 Agent 声明了某项能力，就假设安装目录中的另一份程序支持它。
- 固定配置出口要求实际程序声明 `network_diagnostics_pinned_v1`；新增解析策略以及 DoT/DoH 要求 `network_diagnostics_resolution_v1`。报告只保留解析端点的协议、主机和端口，省略可能含令牌的 URL 路径、查询参数及用户信息；用于实际请求的 URL 不因此改变。
- 实际程序合法声明不提供新流时，只允许系统 TCP、无引导片段的 UDP/TCP DNS 使用旧输出，并在上下文中标为 `legacy_details`。网卡、配置出口、显式解析器、DoT/DoH 和引导 IP 请求不通过旧流执行。查询失败、超时、损坏输出或重复能力声明直接报告错误，不用旧流掩盖异常。

## 诊断进程环境与准确性边界

环境隔离以诊断准确性为优先：**由 Agent/Manager 启动的**每次新诊断及旧 System/Interface 基础探测，默认使用私有运行目录、临时目录与工作目录，移除继承的 `ASTRA_*` 应用控制变量，再显式设置本次私有 `ASTRA_RUNTIME_DIR`。这些目录只承载本次探测的临时状态，正常结束、失败或取消并确认回收子进程后清理；回收未确认或执行流程异常中断时保留目录，避免移除仍被子进程使用的文件。清理失败保留错误，不能当作已清理成功。持久任务和报告仍由 Agent 管理。旧 System/Interface 入口即使收到非空配置也不加载完整 App，不读取实例订阅缓存或编译无关节点。Agent 启动的能力查询也使用诊断环境；既有节点测速 `--diagnose-stream` 与订阅刷新流程不在本次隔离范围内。

用户独立运行 `astra-proxy --nettool` 时仍继承调用者的主机环境、runtime、临时目录与工作目录，不自动套用 Agent 的私有目录启动器。System/Interface 即使提供 `--config` 也不加载该 profile；Outbound 仍按原配置路径读取配置、订阅缓存和相对资源，保留旧行为。下面的目录隔离矩阵仅针对经过 Agent/Manager 启动器的诊断。

子进程继续处于目标 Agent 当前网络命名空间，保留其账户、权限、真实网卡与路由、TUN、系统解析服务和账户可见信任库。不会为了隔离创建额外虚拟网卡、容器网络、替代 DNS、替代根证书，也不会自动提权或降权改变探测能力。私有目录隔离的是应用运行状态；同账户仍可读取诊断所需的系统配置和明确指定的信任文件，不提供完整文件系统访问沙箱。

`SSL_CERT_FILE` / `SSL_CERT_DIR` 是实际 TLS 信任输入，保留其语义；更换工作目录前按原目录固定相对路径，目录列表使用所在平台的路径分隔规则。系统解析相关环境也保留：Linux 的 `HOSTALIASES` 和 `RESOLV_HOST_CONF` 可指向文件，`LOCALDOMAIN` / `RES_OPTIONS` 可改变搜索域和解析选项；不得把这些输入当作 Astra 注入变量清除。文件型相对路径须在换目录前固定，不能通过“未找到”悄悄换用另一个解析或信任来源。相关系统语义见 [hostname(7)](https://man7.org/linux/man-pages/man7/hostname.7.html)、[host.conf(5)](https://man7.org/linux/man-pages/man5/host.conf.5.html) 与 [resolv.conf(5)](https://man7.org/linux/man-pages/man5/resolv.conf.5.html)。

`PATH` 的相对目录和表示原工作目录的空条目同样固定到原目录。`SSL_CERT_DIR` 的空条目按证书加载器原语义忽略，不能转成原工作目录而扩大信任来源；文件型变量的空值也保持原有含义。任意非空 `LD_*` / `DYLD_*` 动态加载环境采用保守例外：保留原工作目录，仍隔离 runtime 和临时目录；不解析或重写 `$ORIGIN` 等加载器专用语法。这些变量及其值不作为报告中的新字段。

路径改写整体成功才生效；若搜索路径无法无损表示，例如 Unix 原工作目录包含路径列表分隔符 `:`，则保留原工作目录与全部原路径变量，不留下部分改写。这个例外仍使用私有 runtime 和临时目录。环境变量名称按平台规则识别：Unix 区分大小写，Windows 不区分大小写。

| 工具 / 路径 | 保留的真实环境与例外 | 可隔离的应用状态 |
|---|---|---|
| TCP、TCP 多端口（可选地址族） | 真实路由、接口绑定、源地址、地址族与权限；System 域名仍使用当前系统解析。IPv6 业务目标的辅助解析器可以是 IPv4，不能强行把全部 socket 改为同族。 | 私有运行/临时/工作目录；不加载完整 profile。 |
| DNS UDP/TCP | 实际解析端点、出口、分域解析器候选与系统 DNS 上下文；UDP TC 重试仍使用同端点同出口。受约束路径继续禁止隐式系统解析。 | 不共享应用 DNS/订阅缓存；不改主机 DNS 配置。 |
| DoT、DoH | DNS 路径约束，加真实 TLS 名称、引导 IP、当前账户信任库及 `SSL_CERT_*` 输入。 | 私有应用目录；证书证据仅留在任务报告，不写入主机信任库。 |
| TLS / 证书 | 当前账户可见根证书、系统时间、TLS 名称和实际连接目标；加载失败保持失败，内置根仅作可选离线对照。 | 进程内有界证书快照与临时状态；不安装证书、不联网补链。 |
| HTTP / HTTPS 公网 IP | 沿同一诊断 Route 建立连接；HTTPS 保留 TLS 信任输入。公网 IP 结果来自实际远端服务，不能以隔离网络的新出口代替。HTTP 使用显式请求与出口，不读取浏览器 Cookie、配置或代理扩展。 | 有界请求/响应状态与私有目录；不创建共享 Cookie jar 或 HTTP 缓存。 |
| UDP / STUN | 真实 UDP socket、源端口与实际出口；SOCKS5 模式保留本次独立 TCP 控制连接和 UDP relay，不能换到系统直连。 | 每次尝试独立会话，关闭时释放，不复用生产代理会话。 |
| NAT 分级检测 | 用户选定的真实物理接口、源地址、接口身份、路由与绑定读回；映射测试保持同一未连接 UDP socket，过滤测试用新会话。额外网络命名空间或 NAT 会改变待测结果，不能使用。 | 私有进程状态；不改路由/TUN/系统代理，不启动产品 STUN 监听器。 |
| ICMP / 逐跳 | 当前权限和平台原生 API、真实 TUN 状态与物理路径证据；保留 macOS 原始 ICMP 权限失败。System ICMP 的 TUN 不确定性不能通过隐藏 TUN“修复”。 | 私有目录；不启动提权流程、外部 ping/traceroute 工具或替代网络。 |
| 新配置 DIRECT / 固定代理 | 保留 Manager 已确认的固定节点、绑定与选择链；只读这次经 stdin 提供的必要参数，不重新选组。 | 同样使用私有目录；不初始化完整 App、健康检查或订阅刷新。 |
| 任意工具存在非空 `LD_*` / `DYLD_*` | **准确性例外：** 保留原工作目录与加载器语义，不重写相对注入库或 `$ORIGIN`。 | 仍隔离 runtime/临时目录并清除 Astra 控制注入。 |
| 任意工具的路径改写无法无损表示 | **准确性例外：** 整体放弃路径改写，保留原工作目录与全部原路径变量；例如 Unix 原 cwd 中的 `:` 不能直接编码成单个搜索目录条目。 | 仍隔离 runtime/临时目录并清除 Astra 控制注入。 |
| 旧 `NetTool` 配置 Outbound（兼容调用者） | **准确性例外：** 保留原运行目录和工作目录，使订阅缓存、配置侧车、相对资源路径和旧组选路语义不变。旧自动组仍可能执行其原有健康检查，不能声明与新固定路径相同。新三端已拒绝向旧 Agent 提交这类请求。 | 仍清除 Astra 控制注入；不把此例外推广给 System/Interface 或新详细诊断。 |

如果额外的系统扩展或动态加载配置依赖原工作目录，应保留该项必要上下文或明确报告不支持，不能在改变含义后把结果称为原环境诊断。隔离不改变现有能力门槛、旧线协议、请求/报告结构或取消回收语义。

## 三端页面（单页）

三端使用同一套单页布局：GUI/TUI 共享 `astra-client/src/nettools`（`chain.rs`、`nat.rs`、`result.rs`、`full_check.rs`、`form.rs`），Web 在 `networkChain.ts`、`networkNat.ts`、`networkToolsModel.ts` 中按同一规则实现。链路与 NAT 分类由 `crates/astra-client/src/test/fixtures/network_{chain,nat}_cases.json` 共享夹具同时约束 Rust 与 Web。

1. **常驻链路条**：始终显示所选出口的流量路径，顺序为 `系统 › [TUN | 系统代理] › [规则] › 组 → 选择 … › 叶节点 › 网卡（来源）`。数据来自已轮询的运行快照与目标 Agent 网络状态，不额外查询。系统路由仅在 TUN 捕获时接入实例的默认链（规则模式标出“规则”，全局为 `PROXY`，直连为 `DIRECT`）；指定网卡绕过 TUN，只显示网卡；配置出口从该出口沿各组 `selected` 下行，最多 32 层，循环、无选择或快照截断作为提示显示，不猜测。网卡取非虚拟默认路由，标注“默认路由”或“指定网卡”。最近一次 NAT 结果以标签挂在网卡上。
2. **工具**：Ping、TCP 连接（`host:443` 或 `host:80,443,8080`，最多 16 端口）、DNS 查询（UDP/TCP/DoT/DoH）、逐跳路径、HTTP(S)（含 TLS 版本、验证结果与首张证书摘要）、公网 IP、NAT 类型、Fake-IP 与完整检查。出口按工具过滤：逐跳仅网卡或 DIRECT，ICMP 不接受代理节点，NAT 使用手选物理网卡。TCP/HTTP/公网 IP/完整检查可填可选解析器。
3. **完整检查**：一键提交固定批次（DNS TCP 与 DoH、TLS 含内置根对照、HTTPS、IPv4 公网 IP、STUN，可选 IPv6 公网 IP），可选对照出口（第二路径）、两轮对照与 NAT（第三路径）。这样后端分析规则（两轮 DNS/证书差异、IPv4/IPv6 与 HTTPS/STUN 出口差异等）有可比较的证据。TLS 与 STUN 探测只作为完整检查的固定项目，不再单独作为工具入口。
4. **结果（单栏）**：任务状态 → 收发汇总（发送/接收/丢失/min/avg/max）→ 逐次尝试 → 工具详情（DNS 应答与 CNAME、TCP 逐端口状态、HTTP 状态与阶段耗时、TLS 摘要、公网 IP、逐跳跳表）→ 后端分析结论（严重程度、置信度、说明、建议）。批次结果以项目标识前缀分组，只显示每个项目的最新状态。未知事件类型直接跳过，7 天内保存的旧报告仍可读取。
5. **探测路径条**：来自后端 `AnalysisReport.topology`，每条路径按连接/DNS/逐跳分行线性化；边以 `─›` 实测、`┄›` 配置推断、`?›` 未知区分。GUI/Web 可点击节点或边查看证据引用及匹配的原始事件，TUI 以文字行显示。前端不推断拓扑。

页面可见时约每 5 秒刷新运行与系统状态；所有主动探测由用户点击启动。切换目标 Agent 后清空网卡选择，既有任务查询与取消固定在原目标。Web 用 sessionStorage 保存当前 Agent/实例的任务标识用于页面重连；接受回执丢失时只查询原标识，不自动重放。只有 `invalid_request`、`unsupported`、`conflict` 会清除未被接受的任务标识。

Fake-IP 是手动只读查询，限制 1–64 个 IP，先确认实例核心声明 `fake_ip_lookup_v1`；未命中不推断地址用途。

快捷键：Enter 运行/取消，Esc 返回，←→ 或空格切换选项，Ctrl+U 清空当前字段，Ctrl+Y 复制结果，Ctrl+E 复制 JSON，Ctrl+↑/↓ 或 PgUp/PgDn 滚动。NetTool 页面吞掉 F1–F12，避免触发全局绑定。结果用文字与颜色同时标识（绿成功、黄注意、红失败、蓝/青信息、淡色细节）；任务完成不代表探测成功。

### NAT 类型展示

NAT 结果以固定分类表展示，检出行高亮；只有后端给出 `traditional_type`，且测量完整、稳定、物理路径已确认时才命名类别，否则显示“未能确定”及原因，并分别列出测得的映射与过滤行为。

| NAT 类型 | 映射行为 | 入站接受 | P2P 兼容性 |
|---|---|---|---|
| 完全锥型 Full Cone | 单一稳定映射 | 任意外部主机 | 极好 |
| 受限锥型 Restricted Cone | 复用映射 | 仅已联系的 IP | 良好 |
| 端口受限锥型 Port-Restricted Cone | 复用映射 | 仅已联系的 IP+端口 | 一般 |
| 对称型 Symmetric | 每个目标不同映射 | 仅该目标 | 差 |

“未观察到地址转换”单独显示，不放入表格；它不对过滤或可达性作出判断。服务要求见 [NAT 测试与边界](network-nat-testing.md)。

## 目标 Agent 系统网络信息

`Query::NetworkState` 无实例参数，返回目标 Agent 的 `Snapshot`，包含 `agent_id`、独立的系统/DNS 采样时间、网卡、地址、路由、DNS resolver、采集问题和截断标记。该查询不需要运行核心，也不进入配置变更锁。

| 平台 | 当前来源与失败语义 |
|---|---|
| macOS | `getifaddrs` 地址及接口状态；`networksetup -listallhardwareports` 保守确认硬件端口；`netstat -rn` 路由；有界解析 `scutil --dns`，保留接口、作用域、域、优先级、flags/options。 |
| Linux | `getifaddrs`、sysfs 硬件/虚拟设备证据及 `ip -j` 路由；优先读取 systemd-resolved D-Bus 的 Manager DNS/Domains 属性（同时含全局与每链路条目）。不可用或响应无效时报告原因，再读取 `resolv.conf`，明确标为全局降级信息。 |
| Windows | 结构化 Windows 网络 CIM/cmdlet 查询网卡、地址、路由和接口 DNS；有效 NRPT 单独查询并保留命名空间。NRPT 查询失败报告“策略未知”，不当作没有策略。 |

DNS 单独缓存 30 秒；整快照缓存 2 秒以合并并发刷新，读取结果带各自采样时间。单次系统命令限时 3 秒、输出限 512 KiB；最多保留 128 个接口、每接口 64 个地址、512 条路由、256 个 resolver，每个 resolver 最多 64 个域和服务器。超限或采集失败必须结合 `truncated` / `issues` 解释。

物理类型只依据已取得的系统证据，无法确认时为 `unknown`。例如 macOS 的设备名 `en0` 本身不是物理证明，Linux 使用 sysfs 设备来源，Windows 使用系统 `HardwareInterface`/`Virtual` 属性。这里的物理分类只用于选择候选；NAT 还会在请求接收与排队后启动时重新采样，并由平台 `PhysicalGuard` 对接口身份、源地址、绑定读回、受接口约束的路由及 TUN 绕行证据进行复核。不能确证时拒绝。

诊断可从这份信息中选择**明确绑定到所选、仍连接的接口**且域作用范围可表达的解析器。采样截断、DNS/NRPT 采集失败、接口匹配不唯一或存在未明确绑定接口的策略时，不提供自动候选；需要解析的请求会要求显式解析器或 IP。全局降级信息不会冒充网卡 DNS。此能力不声称完整复现操作系统解析器策略。

## DNS、解析边界与探测上下文

**已知旧接口限制：** `astra-proxy/src/nettool/mod.rs::resolve` 对非 IP 使用系统 `lookup_host`；该解析先于业务 socket 的网卡绑定，不能证明 DNS 查询沿所选网卡或配置代理出口。旧 `Command::NetTool`、`--nettool-stream` 和文本入口仍为兼容用途保留，旧调用者直接使用这些入口时仍可能触发此行为。新三端不再调用旧 Agent 的这些入口。新 Agent 的详细诊断使用独立受约束解析层，不以旧入口存在作为新路径已完成约束的证明。兼容调用尚未完全迁移，因此本轮不废弃或删除旧接口。

详细 DNS 支持 `udp://`、`tcp://`、`tls://` / `dot://` 和 `https://`。DoH 通过同一路径的 HTTPS POST 发送 DNS 报文，不跟随重定向；校验 HTTP 状态和 DNS 内容类型，并限制报文大小。响应展示查询类型、记录类型、RCODE、标志、EDNS、问题区、answer/authority/additional 三个记录区段、TTL 和 CNAME 链。DNS AD 是服务器断言，不表示本机执行了 DNSSEC 验证。

收到响应、报文解析成功、NXDOMAIN、无所求记录、服务器失败、报文错误和网络超时分别保留，不把这些状态合并为“成功解析”。UDP 响应截断时，记录可见的 TCP 重试事件，并沿同一解析器端点及出口重试。

新诊断由统一的路径解析层处理业务目标、代理入口和 DNS 服务名，所有本地连接最终使用明确 IP 的 socket 端点；只有允许系统解析的路径才会到达 `resolve_for` 的系统解析边界：

- 系统路由可使用显式解析器，否则使用系统解析，并注明系统内部解析路径可能不可见。未绑定网卡的 DIRECT 仅接受目标 IP 或显式解析器；不会隐式使用系统 DNS，也不声称复现实例的 DNS 配置。
- 指定网卡或绑定网卡的 DIRECT 支持目标 IP，也支持经显式解析器或可确认接口解析器解析的域名。接口候选按可匹配的域范围和优先级选择；没有合适候选时明确拒绝，不回退系统 DNS。自动解析途中 CNAME 跨越候选解析器作用范围时，要求显式解析器，不沿用原作用域猜测。
- 可选 `resolver` 指定 UDP/TCP/DoT/DoH 解析器。其端点必须是 IP 或带 `#bootstrap=IP`，避免解析解析器自身时递归漏回系统 DNS；有网卡绑定时，解析查询也沿该网卡发送。
- DNS 服务名可经上述解析边界获得地址，也可用 `#bootstrap=IP` 指定连接地址，保留原域名用于 TLS/HTTP。例：`https://dns.example.test/dns-query#bootstrap=192.0.2.53` 或 `tls://dns.example.test:853#bootstrap=192.0.2.53`。
- SOCKS5 / HTTP CONNECT 的**代理入口**需 IP、显式解析器或可确认接口解析器；即使没有网卡绑定，也禁止入口域名隐式使用系统解析。**业务目标与 DNS 服务名**通过代理远端解析或显式引导 IP，在报告中注明 `remote_proxy` / `literal`，不把远端解析地址冒充本机获得的地址。
- 代理 DNS UDP、其他未经此解析边界审计的代理协议、没有可确认路径的组合明确拒绝，不静默回落系统路由或 DIRECT。旧接口原有行为不能作为新接口已满足约束的证明。

每个解析边界记录独立 `resolution` 事件：`purpose=business_target|proxy_entry|dns_service`、主机名、解析方式、解析器、地址和网卡。解析尝试及结果都可留证；无结果的尝试不能当作已成功解析。

DoT/DoH 与 TLS/HTTPS 使用目标 Agent 账户可见的系统根证书快照，记录来源与加载错误，不静默换为内置根证书。可以对同一收到的证书链执行内置根证书离线对照，不为此额外连接。验证回调保留有界失败证据并委托原验证器，不跳过校验继续 HTTP；握手未收到证书时明确没有证据。此验证不等同于系统完整证书策略，不联网补链。

每个新报告首先记录 `Context`：探测程序自身身份与能力、采样时间、请求出口及来源、是否使用配置、可用的运行核心身份和配置摘要。运行配置采样前后核对核心身份与配置摘要，匹配时标 `matched_running`；停止实例的已保存配置标 `saved_not_applied`；变化、失败或超时保留 `unconfirmed` 原因。

系统/显式网卡任务使用 `config_used=false`、`egress_source=requested`。配置出口任务通过已保存配置和同版本 Runtime 重建，固定后使用 `config_used=true`、`egress_source=pinned_request`，`actual_egress` 为固定叶节点；`selection_chain` 保留每层的 `group/mode/selected/source/interface`，并记录 `effective_interface/interface_source`。上下文仍不是已经发包或证明物理路径的证据，实际连接、绑定与握手结果由后续事件说明。

固定路径支持可确认的 select、url-test、fallback 层，以及手工选择可达后代时的中间层。外层首个明确网卡优先于内层；无组绑定时只接受可确认的配置默认网卡，并结合当前 Runtime 的 TUN 开关。重建前后核对核心启动/进程/已应用配置，启动子进程前再次核对选择链。循环、层数超过 32、Runtime 缺失或截断、成员/模式不匹配、load-balance 下一连接选择未知、订阅管理路径缺少同版本有效配置、自动默认网卡无法确认等情况均明确拒绝，提示选择可确认的具体本地节点。固定诊断不会触发新组选路或健康检查。

## 常用工具、NAT 与逐跳

TCP 支持单端口及最多 16 个指定端口，分别保留已连接、拒绝、超时、不可达和失败。连接耗时包含解析与代理握手，不是目标 RTT。独立的 IPv6 可达性工具已移除；需要时以 `family: "ipv6"` 提交 TCP 请求，辅助 DNS 可以使用同接口另一地址族，不能因此把辅助解析成功当成 IPv6 目标可达。

HTTP 支持 GET/HEAD、状态、响应头、阶段耗时及最多五次跳转；HTTPS 降级停止。HTTPS 公网 IP 与 STUN 映射保留独立来源。TLS 记录信任来源、加载错误、协议/密码套件/ALPN、证书指纹与有效期、SAN、失败原因和有界 DER；系统与内置根结果差异不单独构成监听结论。

NAT 不继承通用出口或上次选择。必须手选当前 Agent 已确认物理、连接且支持所选族的接口，请求携带 `agent_id/interface_id/family`；每次提交及切换目标/地址族后清空。运行前说明测试服务将看到该物理网络的公网出口 IP。总时限 30 秒；独立未连接 UDP socket 固定本地端口进行映射测试，新会话进行过滤测试。基础只给所测目标映射一致/不同；RFC 5780 高级检测先确认双 IP/双端口与 CHANGE 能力，完整且稳定才给映射/过滤行为和传统名称。事务、长度、来源、FINGERPRINT、重试与速率均受约束。每次发送及有效结果后复查物理证明；失效立即停止，报告更新为未确认，不保留上一次成功证明冒充当前状态。不检测绑定寿命、Hairpin、ALG，不部署产品 STUN 监听器。

独立 UDP/STUN 出口工具允许已验证的 SOCKS5 代理会话，但不做 NAT 分类或多目标映射对照。每次尝试持有自己的 TCP 控制和 UDP relay 生命周期；HTTP CONNECT 及其他 UDP 组合明确不支持。

ICMP/逐跳按目标平台原生能力执行：Linux UDP TTL/Hop-Limit 错误队列、macOS UDP 与 ICMP 错误接收、Windows ICMP API。权限不足不提权；macOS 普通账户不可用的原始 socket 明确报告。无法排除 TUN 时逐跳不可测；TUN 本地 echo 不代表目标可达，不计入延迟或 IPv6 可达性结论。默认 30 跳、每跳 3 次、每次 1000ms，未响应只保留未知。

## 完整诊断与共享结论

`Command::NetworkDiagnosticsBatch { instance, request: { projects, comparison } }` 使用同一后台任务/报告生命周期；每个项目包含 `id/path_id/request`。最多 32 项、3 条路径，同路径出口和解析器一致，地址族可不同。NAT 为独立手选物理路径；批次含 NAT 时先执行 NAT，其他项目显示等待，随后最多四个普通项目并发。整批 180 秒包含排队时间，项目超时从实际执行开始计算。

`comparison` 默认 false；只有主动勾选才对 DNS/TLS 安排额外第二轮，未做对照不生成相应差异或无差异结论。同链内置根对照是另一独立选项，仅离线验证。每条 `project` 事件通过 `detail` 包装子事件并附 `project_id/path_id/round/index/sampled_at_ms`，项目状态与全批终态分开。

后端 `network/analysis` 统一产生版本化固定结论：明文 DNS、未加密代理段、关闭证书验证、TLS 验证失败、信任库差异、两轮 DNS/证书差异、解析路径不符、IPv4/IPv6 出口差异及 HTTPS/STUN 出口差异。规则对身份、时间、目标、地址族、固定成员和成功证据按类核对；不把缺少或截断证据视为更强支持。可信度与严重程度分别显示，不映射成被监控概率。

拓扑分 DNS、连接和逐跳三类边，记录实测/配置推断/未知与证据引用。NAT 附着于路径，不推断设备数量；代理之后不可见区域保留未知。潜在观察点按网关、接入网络、DNS、代理和目标服务分别说明信息可见范围。

## 提交与读取示例

以下是管理 v2 的 JSON 请求体示例，经现有管理传输发送。`id` 标识一次通信；`operation_id` 标识一次逻辑操作，调用者应在发送前保存。远程调用使用真实目标 `agent_id`；直连本地 Agent 可设为 `null`。

```json
{
  "version": 2,
  "id": "request-1",
  "agent_id": "selected-agent",
  "body": {
    "type": "execute",
    "data": {
      "operation_id": "diagnostic-unique-id",
      "command": {
        "type": "network_diagnostics",
        "data": {
          "instance": "proxy-instance-id",
          "request": {
            "egress": { "kind": "system" },
            "probe": { "kind": "tcp", "host": "127.0.0.1", "port": 8080 },
            "count": 3,
            "interval_ms": "1000",
            "timeout_ms": "3000",
            "resolver": null,
            "family": null
          }
        }
      }
    }
  }
}
```

任务及请求指纹持久化后，立即返回 `ResponseBody::Task`。`state: "running"` 表示任务已接受，可能仍在排队，**不表示探测完成或成功**。Controller 将此响应记为 `accepted`。报告中的 `queued` 与 `running` 区分实际执行阶段。

新版 `NetworkDiagnosticsRequest` 保留既有请求字段，增加可选 `resolver` 与 `family`（`ipv4` / `ipv6` / `null`）；省略或 `null` 表示采用该路径允许的解析方式。指定出口可将 `egress` 换为 `{ "kind": "interface", "name": "en0" }` 或 `{ "kind": "outbound", "name": "DIRECT" }` / 实际节点或组名。需要显式解析器时可填 `"resolver": "tcp://192.0.2.53:53"`；示例地址需替换为测试环境真实解析器。固定链、有效网卡、节点凭据和自动接口解析器都由 Agent 准备，客户端不能注入这些内部字段。

DNS 请求可将 `probe` 替换为以下内容。这里的地址应替换为用户选择的测试解析器；示例本身不触发请求。

```json
{
  "kind": "dns",
  "name": "example.test",
  "record": "A",
  "server": "udp://127.0.0.1:5353"
}
```

首次查询报告使用 `cursor: null`，后续传入上一页返回的 `cursor`：

```json
{
  "version": 2,
  "id": "request-2",
  "agent_id": "selected-agent",
  "body": {
    "type": "query",
    "data": {
      "type": "network_report",
      "data": {
        "operation_id": "diagnostic-unique-id",
        "cursor": null
      }
    }
  }
}
```

`Query::Task` 与 `Query::Events` 继续可用。诊断事件只携带阶段、报告修订号和结果项标识；结果内容从 `NetworkReport` 读取。事件批次返回 `resync_required: true` 时，重新查询任务和报告，不把缺失的事件当作没有执行。

## 报告和兼容语义

`NetworkReport.schema_version` 当前为 `1`。`revision`、时间、游标和结果标识使用十进制字符串。每条 `items` 包含 `id`、`sampled_at_ms` 和诊断事件 `data`（包括管理器采集的 `Context`）；事件中的整数计数同样转换为字符串。

| 字段 | 读取方式 |
|---|---|
| `cursor` | 最后已返回结果项的标识；首次为 `"0"`，可继续增量读取。不能提交超前游标。 |
| `has_more` | 当前已有结果中还有未返回的项。`false` 只表示已读完当前数据，不代表任务终止。 |
| `state` | 独立表达排队、运行、完成、部分完成、失败、取消和中断。 |
| `truncated` | 报告预算或单项分页预算导致证据被截断。不能按完整结果解释；原本成功完成的任务改为 `partial`。 |
| `expired` | 证据已按保留策略清理。仍保留操作标识、请求指纹和精简终态，但不再返回原结果项。 |
| `finished_ms` | 终态记录时间；未结束时为空。 |

运行中出现空页是正常情况，应保留游标再查询。只有报告达到终态且当前页 `has_more: false`，才读完该任务当时可用的全部结果。`expired` 与 `truncated` 都应显式展示。

`v2::Task` 的 `deny_unknown_fields` 保持不变，没有新增字段，也没有扩展 `TaskState`。诊断专属状态仅写入 `NetworkReport`，通用任务列表按下表映射：

| `NetworkReport.state` | 现有 `Task.state` |
|---|---|
| `queued` / `running` | `running` |
| `complete` | `complete` |
| `partial` / `failed` / `cancelled` | `failed` |
| `interrupted` | `outcome_unknown` |

因此旧任务列表只提供兼容摘要；新界面必须读取报告解释取消或部分完成，不能仅依据通用 `Task.state` 给出诊断结论。

错误码由内部类型确定，展示文字、语言和附加上下文不参与分类。参数错误为 `InvalidRequest`，明确缺能力为 `Unsupported`，请求指纹冲突或容量不足为 `Conflict`，确实不存在的资源为 `NotFound`；SQL/JSON 故障、关闭的调度通道和未分类错误为 `Internal`。公开错误码、`ApiError.details`、任务格式和数据库格式保持兼容。

数据库写操作未入队失败为 `Internal`；入队后丢失回执为 `OutcomeUnknown`，包括会执行保留清理的报告查询。保存操作保留结构化结果；未知写入结果不会宣称磁盘和运行态均未改变。取消的清理未确认、最终结果无法保存也按类型表达不确定性，并保留阻止新诊断的门禁。已受理任务的执行失败继续记录到报告。

原生客户端保留完整 `ApiError`，单项和批次仅按既有 `Unsupported` / `InvalidRequest` / `Conflict` 错误码确认提交被拒绝；其他未确认回执继续查询原任务标识，不重新执行。进程内执行通道保留错误类型，只在旧协议的文字响应边界转换为字符串。

## 调度、断线与取消

每个 Agent 最多运行 2 个诊断任务，另排队 4 个；达到上限后立即返回 `Conflict` 繁忙错误，不隐式排入更长队列。只有新的只读诊断命令绕过配置变更的全局串行锁，原配置操作保持原有串行行为。

新的单项和批量任务共用上述准入。NAT（包括含 NAT 的批次）占用两份运行许可，整个测试期间独占本 Agent 诊断调度器；旧 `NetTool`、旧节点诊断、顶部出口探测与手动组测速也获取同一许可，不能穿过 NAT 独占窗口；提交时即登记公平队列位置，避免等待中的 NAT 被尚未调度的新普通任务抢占。普通项目另受 Agent 全局四份项目许可约束。含 NAT 的批次会先为其他项目显示“等待 NAT 检测完成”。用户业务流量和核心后台自动健康检查不在此调度器内，不因 NAT 检测暂停。

旧工具的断线、解析错误与超时仍由拥有进程的任务完成显式终止与回收后释放许可；维护也等待这些旧任务。Agent 的顶部探测每轮回收独立核心，不让空闲探测进程越过 NAT 窗口继续执行配置后台工作。手动组测速在原业务核心中运行，因此保留许可至同一核心身份和配置下观察到 `testing=false`；无法确认时阻止后续 NAT/维护，不终止用户业务核心。

任务总时限为 180 秒，包含排队时间。到期后发送取消信号，停止并等待子进程退出，保留已有结果，报告 `partial`。这不是单次 TCP 或 DNS 请求的超时：单次请求仍受 `timeout_ms` 控制。当前请求限制为 `count` 1–50、单次超时 100–30000 毫秒、间隔 0–10000 毫秒；逐跳单次超时最多 1000 毫秒，接收请求时即校验。

请求连接断开、关闭结果页或停止轮询均不取消任务。重连后查询原 `operation_id`；相同标识和相同请求返回原任务，不重复执行；相同标识配不同请求返回冲突。Agent 重启后，未完成诊断标记为 `interrupted`，不会自动重放。报告过期也不会释放操作标识供再次执行。

如果最终状态无法持久化，接口显式报告 `OutcomeUnknown`，并阻止新诊断准入；不能把缺少最终记录解释为任务仍正常运行或已经成功。应保留原操作标识，检查存储失败原因后再恢复服务。

重启时标记中断不等于证明崩溃前的子进程已经退出。Agent 被 `SIGKILL` 等方式强制终止后是否残留诊断子进程，尚未完成跨平台验证；本阶段仅确认正常取消与受控关闭流程会等待子进程回收，不声称所有崩溃场景都能自动回收。

取消使用已有 `Command::CancelTask`，外层取消操作必须使用独立 `operation_id`：

```json
{
  "type": "execute",
  "data": {
    "operation_id": "cancel-unique-id",
    "command": {
      "type": "cancel_task",
      "data": { "operation_id": "diagnostic-unique-id" }
    }
  }
}
```

取消排队任务不会启动探测。取消运行任务会终止隔离的诊断子进程，显式等待回收后才返回终态报告；读取、写入和等待子进程结束都受取消信号控制。部分结果保留。若任务先一步正常完成，应显示实际的 `complete`，不能强行显示已取消。已完成任务可能返回 `Cancellation::AlreadyFinished`，客户端随后读取报告确认实际终态。取消通信失败意味着结果尚未确认，应继续查询原任务。

取消操作即时响应可包含终态报告，但取消操作自身的持久化记录只保存精简回执，避免把报告页复制到永久操作记录中。重复提交同一取消操作后，客户端根据精简回执重新读取报告；证据已经过期时会得到 `expired`，不会恢复已清理的数据。

入队和实际启动时均检查维护状态。更新、安装、恢复更新、关闭服务及维护流程在替换程序前暂停诊断准入、取消现有诊断并等待工作任务与子进程退出；清理失败时不能继续把它当作成功取消。普通配置操作不因运行中的诊断被长期阻塞。

## 数据预算

| 数据 | 当前上限与行为 |
|---|---|
| 单任务报告 | 2 MiB，预留元数据空间；达到预算显式标记 `truncated`。 |
| 报告读取页 | 64 KiB；按完整结果项分页，不拆开单项。无法容纳的单项标记截断。 |
| 诊断事件 | 每条最多 2 KiB、每任务最多 256 条，预留终态事件位置；不复制 DNS 内容等证据。 |
| 证据保留 | 最多 7 天、最近 32 个任务、报告与关联诊断事件合计 64 MiB，先触达者生效。只淘汰终态数据。 |
| 子进程输出 | 单行最多 60 KiB、最多 16,000 条子进程事件；持续读取标准错误，但最多保存 4 KiB 错误内容。 |
| DNS 详情 | 每次响应三个记录区段合计最多 256 条记录，单条文本最多 8 KiB；按有界结果块输出，并保留截断标记。 |
| 探测程序能力查询 | `--program-info` 最多 16 KiB、限时 5 秒；异常不隐式降级。 |
| 操作身份 | 不随证据过期删除；只保留防重放需要的标识、指纹和精简终态。 |

清理在存储初始化、任务与结果写入时执行，Agent 服务另通过 `retain_network` 每 60 秒进行一次清理；读取报告时仍同步检查过期，不等待下一个周期。完整证据和精简操作身份分开管理；7 天等限制作用于可读取的证据，不表示永久删除任务存在过的记录。

16,000 是接收详细 DNS 分块的子进程流上限，**不是**持久化进度事件上限；`Query::Events` 仍最多每任务 256 条、每条 2 KiB。详细结果仍受单任务 2 MiB 和分页 64 KiB 预算约束。

HTTP 头最多 32 KiB、正文预览最多 64 KiB；证书最多 16 张、DER 合计最多 256 KiB，以分块事件保留并标明截断。HTTP 认证与敏感响应头、URL 用户信息/路径/query、敏感预览在输出前过滤；原始表单 URL 不进入 Web 复制摘要和 JSON 导出。

## 验证与后续工作

本阶段自动测试以本地可控输入和子进程为主，不依赖公共 DNS/STUN 服务成功。可在仓库根目录运行：

```sh
cargo test -p astra-manager --lib network
cargo test -p astra-platform --lib network_snapshot
cargo test -p astra-client --lib nettools
cargo test -p astra-proxy --lib nettool
cargo test -p astra-ipc --lib control::fake_ip
cargo run -p astra-management --features bindings --bin export-contracts -- --check
python3 scripts/check_architecture.py
npm --prefix apps/web run typecheck
npm --prefix apps/web test
npm --prefix apps/web run test:e2e -- tests/e2e/network-tools.spec.ts
```

关键验证点包括：运行/排队容量、重复提交与指纹冲突、断线后查询、重启中断、排队取消不启动、运行取消等待进程退出、维护等待退出、报告分页及截断、证据清理后防重放、事件游标失效提示，以及客户端不把接受任务当作任务完成。

新增回归覆盖目标 Agent 状态来源、DNS 缓存与分域解析、受约束路径拒绝系统解析、固定链与网卡优先级、DIRECT/可审计代理边界、DNS 详情/截断/同出口重试和 Core 能力兼容。Web 回归覆盖切换 Agent 不误发旧任务、终态分页读完整、丢失回执不重放、停止观察与原标识恢复、核心重启/摘要/Runtime 变化、64 IP 限制与查询能力门槛，以及固定链和各类解析证据。实际浏览器 fixture 已检查桌面与 390px 窄窗口、表格滚动、长 DNS 记录显示和仅手动触发探测；这些 fixture 不等于真实网络成功验证。GUI/TUI/Web 已接入本轮接口，但编译和自动测试不能代替三端完整人工交互验收。

已进行 macOS 本机采集、普通权限拒绝、root 单跳和物理网卡跨机 STUN，Linux VM 原生 UDP 错误队列及九种内核 NAT 组合，以及 Windows 原生网卡/DNS/路由采集、IPv6 ICMP、双族 socket 绑定读回与系统根加载。实际 Mac 共享客户端经临时 TLS Controller 驱动 Windows Agent/Core 完成五项批量、超过 64 KiB 的分页恢复和取消后不继续发包。Windows IPv4 回环 ICMP 同系统 ping 一样无响应，未将它算作正向回包成功。详情见 [Windows 验证与限制](network-windows-validation.md) 和 [逐项清单](network-plan-acceptance.md)。

Windows 独立新 Agent/Wintun 场景也已实测：Runtime/核心身份确认开启 TUN，普通双族 echo 成功；诊断 ICMP 不记 RTT 或目标可达，System Trace 零探测，虚拟网卡 NAT 与物理口到本机 TUN 目标的 NAT 均拒绝。测试专用接口/路由清理后，全路由和本轮生产 Runtime 前后一致。另有一次显式授权的 Windows 物理网卡公共 STUN Basic：接口 DNS 约束解析 Cloudflare，同 socket 两次合法响应，绑定/路由证明均为 true、完整 Done；只作为独立基础现场样本，不推断完整 NAT 类型，也不让自动测试依赖公共服务。

macOS 独立 TUN 最终样本也通过：真实 Runtime.tun=true、普通 TUN echo 成功，诊断 ICMP 无 RTT/目标可达性，系统逐跳明确拒绝且零探测，虚拟 NAT 拒绝；物理 en0 在开启前后分别收到两次合法 STUN 回复，绑定/路由证据均为 true。临时接口、专用路由和 Core 已清理，原实例、默认路由和系统代理前后一致。**本轮联合验收的具体证据已齐备，覆盖范围仍以逐项清单为准。** 非空企业 NRPT、所有服务账户、物理公网 IPv6 多出口、实物热拔插、企业完整根策略及三端完整人工交叉矩阵属于未覆盖的补充场景；Windows 物理目标 IPv4 正向 ICMP 受当前环境限制，未宣称其成功。未审计的代理协议组合、load-balance 下一连接未知、订阅有效配置不可确认和未知系统解析策略保持明确拒绝。核心业务流量解析审计不在本期网络诊断子进程范围内。

Web 报告接收额外拒绝非数值/倒退/停滞游标、重复或乱序结果、单页超过 64 KiB 及累计超过 2 MiB 的证据。错误保留原任务与已接受证据，只允许继续查询，不重新执行。共享报告导出不包含表单原始解析器/HTTP URL。

NAT 便携式 8 项和 STUN 3 项测试已经通过；独立 Linux ARM64 虚拟机内的网络命名空间九种映射/过滤组合也已实际运行通过。详见 [NAT 测试设施与边界](network-nat-testing.md)。网页已覆盖新工具构造、NAT 手选与清空、能力门槛、批量手动启动、证书失败、TUN 本地应答及可点击未知拓扑；桌面和 390px Chromium 检查通过。完整阶段验收状态以 [逐项清单](network-plan-acceptance.md) 为准。
