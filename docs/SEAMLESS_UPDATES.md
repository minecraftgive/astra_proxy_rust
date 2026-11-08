# 客户端无缝更新：实验实现与验收边界

当前已实现本地版本包、常驻入口与多代 worker 的更新流程，并通过 Windows 普通用户、本机 TCP 的事务回归，以及 SOCKS5 UDP 和 Fake-IP 的有限场景测试。**默认只校验并暂存更新包，不在线替换运行中的核心。** 三平台全部接管模式的无缝更新尚未完成验收，不能视为正式发布能力。

这里的无缝更新目标是：新连接交给新 worker，已经建立的连接继续由原 worker 服务，直到正常关闭或命中原有连接策略。它不迁移正在使用的 TLS、QUIC、TCP 或 UDP 会话状态。此处“保持连接”描述兼容版本切换时的设计和已验证场景，不承诺零延迟、UDP 零丢包或所有模式均已实测；进程崩溃、主动断开、网络变化及远端断连不在该目标范围内。

## 分布式管理与组件边界

版本包的校验、落盘、候选准备及激活由目标 Agent 执行。Controller 只路由请求与记录任务身份，不运行 Core、不自行解包执行用户提供的程序。远程中断不会自动重放激活；结果不确定时先查询原 operation_id 和目标运行状态。

本文的多代 worker 机制适用于客户端 Proxy/Rule 协作。Server Core 仍遵循其重载/重启边界；Controller、Agent 管理程序和独立 astra-web 需要各自的维护重启，不能据此声称所有组件更新都保留现有会话。Web 的停止或重启不等于停止 Core。注册/凭据、独立部署及备份见 [MANAGEMENT.md](MANAGEMENT.md)。

## 进程分工与切换

```text
astra-tui：可退出、重开的界面
    │ 管理 IPC
astra-manager Agent（astra-host 兼容入口）：本地主机更新事务与版本选择
    ├─ astra-rule：按依赖共享的规则进程及待排空规则代
    └─ astra-proxy：每个实例的常驻网络宿主
         ├─ 公共 TCP/UDP/DNS 监听、TUN/smoltcp、系统设置、DNS 地址映射
         ├─ worker A：服务旧连接，等待自然排空
         └─ worker B：接收新连接
```

`astra-manager` 的 Agent / `astra-host` 兼容入口与 `astra-proxy` 网络宿主承担不同职责。关闭 TUI 不会结束它们。实验模式下，网络宿主持有不能随普通升级丢失的公共监听器、TUN 栈和系统资源；版本 worker 是同一 `astra-proxy` 可执行文件的内部工作模式，承担入站协议处理、DNS 解析、路由与出站协议会话。SOCKS5 UDP 关联的中继 socket 由对应 worker 持有。当前 TCP 通过每流独立的本地数据通道桥接，带有界 DATA、FIN、RESET 帧；因此存在额外 IPC 和复制成本。规则通道仍只传路由元数据。

更新先校验并准备全部候选 worker 和依赖规则，再短暂停止新流分配，切换各实例的活动 generation，随后恢复分配；既有流继续运行。准备失败会撤销已准备的候选。多个代理共享同一规则依赖时，更新事务只准备一份对应的新规则进程。

TCP 按连接固定 generation；SOCKS5 UDP 按控制关联固定，关联内后续新目标继续使用旧 worker；TUN/TPROXY UDP 按流固定。DNS UDP 按查询选择 generation，DNS TCP 按连接固定。DNS 查询在 worker 中执行，解析策略和缓存属于该 worker；Fake-IP 分配、反查及真实地址观察通过经过认证的 RPC 访问常驻网络宿主的唯一映射存储，避免切换 worker 后丢失旧地址的含义。这些实现路径仍需下述平台验收。

日常规则实例继续负责规则文件和远程资源的刷新。在线更新产生的规则代只读取已验证缓存，不下载或覆盖共享资源；缺少有效缓存会使准备失败。管理器观察原规则实例的修订，变化时准备新的规则代和 worker，让后续新流使用新规则，旧流继续使用原代。该刷新与版本更新一样可能因候选失败或 generation 数量上限而延后。

旧 worker 不因升级超时被强制结束；UDP 空闲回收等既有策略仍有效。当前每实例最多保留 16 个 generation，达到上限会拒绝继续准备更新。状态和排空由真实会话引用决定，不能用 QUIC 底层连接数代替逻辑连接数。回滚只改变后续新流所用版本，仍在工作的各代连接继续保留。

候选持有订阅展开后的配置快照；准备期间不会重新读取或刷新共享订阅缓存。管理器在准备前捕获运行配置、进程身份与 Rule 修订，并在准备完成和暂停入口后复核。每代 Proxy 与 Rule 都保留独立日志，`update-status` 中的 Proxy generation 包含日志路径。外部连接 ID 使用宿主启动时随机初始化的有界映射，迟到的断开命令不会按新进程重新计数的连接编号执行。

## 本地包与命令

客户端打包脚本会包含 `astra-tui`、`astra-manager`、`astra-host`、`astra-proxy`、`astra-rule`、转换工具和 `manifest.json`；Windows 包还包含权限服务 `astra-service.exe`，其版本和文件摘要同样写入清单。普通与静默启动均由 TUI 程序提供，静默模式使用 `--silent`。旧版 Windows 静默快捷方式需在新版 TUI 中关闭、重新开启登录自启动来更新，见 [TUI 启动设置](TUI.md)。导入器要求前述四个核心的可执行文件和版本声明齐全，缺少 `astra-host` 的旧三核心包不能作为更新包导入。清单记录目标平台、组件版本、宿主协议范围、文件大小和 SHA-256；SHA-256 用于检查包内容完整性，不表示发布者签名认证。支持解压后的版本目录、ZIP、`.tar.gz` 和 `.tgz`，不自动从网络下载版本。

Windows 权限服务安装在受保护目录，其核心副本不会随普通版本导入自动替换。下述维护事务通过一次 UAC 更新原来已安装的权限服务，停止服务副本的受管核心后更新，并在恢复 TUN 前验证服务启动；服务失败等待手动处理，不隐式退回逐次 UAC。

```powershell
# 默认行为：校验并暂存，不切换运行实例。
.\astra-tui.exe update C:\Downloads\astra-client-example.tar.gz
.\astra-tui.exe update-status

# 已在本次宿主会话中完成过实验在线更新时，可回滚新流使用的版本。
.\astra-tui.exe rollback
```

Linux/macOS 使用对应的无 `.exe` 程序名。使用自定义目录时，各命令传入同一组 `--state-dir` 和 `--config-dir`；它们必须分离且不能互相嵌套。升级包存放在状态目录的 `updates/releases/<release_id>`，不覆盖正被运行的可执行文件，也不将示例配置覆盖到用户配置。

默认暂存不会把该版本设为活动版本或下次启动选择。维护时可在完成主动断开后运行已校验版本目录中的新程序，或显式用 `--bin-dir` 指定该目录。已成功在线提交的版本会写入启动选择；下次新启动会校验所选版本，并使用其完整客户端程序。显式 `--bin-dir` 可用于维护时选定程序目录。

## 安装后的维护重启（Windows / macOS / Linux）

本机 client / server 完整包安装完成后，TUI / GUI 默认倒计时 **30 秒**执行维护重启；本地在线激活完成后也使用此流程。连接会中断。安装将包括正在运行的 TUI、GUI、管理器和核心在内的文件写入独立版本目录，保留旧文件，不覆盖运行中的可执行文件。

安装页提供立即执行、自动/手动切换、取消、重试新版、手动恢复旧版和查看日志。TUI 快捷键为 `l` 立即执行、`a` 自动/手动、`c` 停机前取消、`m` 重试、`b` 手动旧版、`v` 日志。取消倒计时后保留手动启动入口；重新启用自动模式会重新计时 30 秒。停机开始后不能取消。

维护范围是当前本机管理目录的管理器、原来正在运行的 Proxy / Rule / Server 实例、托管组件及已登记的界面。原本停止的实例继续保持停止，即使它们配置了自动启动或与运行中的 Rule 绑定。远程 Agent、其他管理目录和独立部署的 Controller / Web 不参与。管理器托管的 Web 必须包含在目标包中，否则预检拒绝停止旧后台。

维护助手由目标包中的独立 `astra-host` 运行，持有管理目录独占锁。管理器先冻结启动、停止、配置修改及其他更新请求，再保存运行实例、配置修订、运行模式、组选择、TUN / 系统代理 / LAN 设置和管理器部署配置。已登记的 TUI / GUI 必须完成各自的会话保存。包清单摘要、目标目录及助手文件摘要均固定；新版核心先检查恢复配置，授权完成后才停止旧后台。

助手等待旧管理器、核心和托管组件退出、端点释放，才启动目标版本的普通用户管理器。新管理器核对操作 ID、进程身份、存储目录和目标目录，通过专用接口按依赖顺序恢复精确实例清单；不会调用会扩展实例范围的普通自动启动。后台恢复成功后切换各个界面并更新原来已启用的自启动项；助手核对新版界面的首帧就绪确认后，才发布下次启动选择并完成事务。恢复用的管理器配置保留在状态目录，后续普通启动继续使用它。

Windows 已安装且属于客户端维护范围的 TUN 权限服务通过一次 UAC 授权更新受保护副本；原来没有服务时不会安装服务，TUN 实例使用本次授权助手启动。若同一账号的其他管理目录仍有使用该服务副本的核心，预检拒绝更新，不停止那些核心。macOS / Linux GUI 复用安全密码输入窗口，TUI 先退出终端交互模式，让 `sudo` 为本次固定助手授权；无图形界面参与时保留系统授权入口。授权助手只接受本次事务中的服务更新、等待停机和 TUN 交接，不接受任意命令；界面和管理器保持原用户身份。

状态目录下 `maintenance/current.json` 持久保存目标、阶段、进程身份、恢复清单和错误；`maintenance/operation.log` 是助手日志，`manager.log` 及各实例日志保留详细故障信息。任一步骤失败即停止后续推进，不自动回退旧版。只清理能核对身份的本次新进程；无法清理的进程会在错误中列出。重新打开程序遇到未完成事务时进入安装/恢复页面，禁止普通后台和业务自动启动。选择“重试新版”或“手动恢复旧版”才会继续。没有完整且可校验旧包时，手动恢复会说明缺失原因。

旧管理器不支持 `maintenance-restart-v1` 时会在停机前拒绝自动维护，需要先完成一次手动维护升级。仅暂存、在线 ABI 不兼容的完整客户端包可显式选择“维护重启到暂存版本”；仍须通过完整包、架构、文件及配置预检，不绕过在线激活保护。

### 维护回归与真实权限验收

CI 在 Windows x86_64、Linux x86_64、macOS ARM64/x86_64 原生构建后执行 `crates/astra-manager/tests/maintenance.rs` 的真实进程回归。它使用独立临时状态、配置和版本目录，检查客户端/服务端切换、绑定实例精确恢复、重复准备、停机前取消、助手退出记录、包损坏、后台启动失败、部分实例恢复失败清理及手动恢复；共享客户端另测 30 秒边界和取消后不重新计时。Windows 普通用户回归要求隔离测试机未安装 TUN 权限服务。可在构建 client / server 后设置 `ASTRA_TEST_BIN_DIR` 为实际二进制目录，运行：

```sh
cargo test -p astra-manager --test maintenance -- --ignored --test-threads=1
```

这些测试不修改日常 TUN、系统代理或服务注册。**CI 配置、交叉编译及普通用户进程测试不代表三平台权限验收已完成。** 发布前须在专用管理员测试机上完成以下实测并留存事务和日志：

| 场景 | 必须核对的结果 |
| --- | --- |
| 三平台已有 TUN / 系统代理，混合运行和停止实例 | 仅恢复原运行清单和开关；路由、DNS、服务副本使用目标版本；UI / 管理器仍为原用户 |
| 拒绝授权、取消、两个界面竞争 | 停机前保留旧后台；同目录仅一项维护；每个界面草稿和窗口会话可恢复 |
| Windows 已安装 / 未安装权限服务 | 一次 UAC 更新已有服务并验证 TUN；无服务时不擅自注册；其他目录核心不被终止 |
| 停机超时、文件损坏、服务更新失败 | 不启动第二套后台，不自动回退，保留日志并给出手动处理入口 |
| 新后台失败、实例部分恢复失败、强制结束维护助手 | 新业务进程清理或明确列出残留；重开后先进入恢复页面，不重复启动 |
| 旧 GUI/TUI 保持打开完成安装，手动及自动维护 | 全部更新文件来自独立目标目录；界面、后台和核心使用目标版本；下一次入口与原自启动项仍指向正确版本 |

2026-11-09 在 macOS ARM64 本机通过了上述 6 项独立进程回归（含实际强制结束维护助手），以及 client / management / manager / TUI / GUI / update 的 657 项单元测试。相关 crate 的 Clippy、格式、生成协议一致性检查通过；Windows 平台层和共享管理协议交叉编译通过。完整 Windows 客户端交叉编译受本机缺少 Windows C SDK 限制；Windows / Linux / macOS x86_64 原生回归及上表真实权限验收尚未执行。仓库现有架构检查仍报告未改动的 `scripts/tests/test_gui_package.py -> package_gui` 导入规则问题。

## 实验启用与首次迁移

环境变量 `ASTRA_HOSTED_EXPERIMENTAL` 必须在启动管理器和代理实例之前设置。当前检查的是变量是否存在，设置为 `0` 也会开启；关闭时应删除变量。它不会开启 TUN、系统代理或 LAN，但仍需使用独立测试配置，避免继承日常配置已经保存的运行开关。

下面示例使用独立状态及配置目录，适合先测试普通 SOCKS/HTTP 流量：

```powershell
$env:ASTRA_HOSTED_EXPERIMENTAL = '1'
.\astra-tui.exe --state-dir .\Local\update-test-state --config-dir .\Local\update-test-config

# 在界面中配置、启动测试实例后，在另一终端对同一个管理器提交本地包。
.\astra-tui.exe --state-dir .\Local\update-test-state --config-dir .\Local\update-test-config update C:\Downloads\astra-client-example.tar.gz
.\astra-tui.exe --state-dir .\Local\update-test-state --config-dir .\Local\update-test-config update-status
.\astra-tui.exe --state-dir .\Local\update-test-state --config-dir .\Local\update-test-config rollback
```

Unix 的启动方式相应为 `ASTRA_HOSTED_EXPERIMENTAL=1 ./astra-tui --state-dir ... --config-dir ...`。已经运行的管理器不会因为新终端设置变量而改变模式；仅重开界面也不会替换它。旧架构首次迁入需要一次主动停止、重启，已有连接会在这次维护中结束。

在线期间，`astra-host` 本体、每个实例的常驻网络宿主以及其 TUN 栈继续使用启动时的版本。它们在下一次主动维护重启时更新；关闭界面 `q` 不构成维护重启。`--shutdown` 会停止管理器及其管理的实例，随后重新启动才使用选定的新宿主。不兼容当前宿主 ABI 的包仅暂存，不能绕过兼容检查在线激活。

Windows 已加入使用 filtered UAC token 启动普通权限 worker 的代码：只继承指定标准句柄，配置经私有 stdin 传入，在执行包代码前检查子进程令牌。Unix worker 会下降到调用用户，Linux 同时清除 ambient、permitted 和 inheritable capabilities。权限降低失败会拒绝候选，不退回高权限执行。提权启动路径显式传递实验开关，worker 端点检查私有目录和对端身份；这些检查的实现与普通用户测试不等于完整提权生命周期已验收。

在线激活要求管理器本身以普通用户运行；直接以管理员或 root 启动管理器会拒绝在线激活。需要管理网络设置时，由独立的网络宿主提权。

## 已验证与仍待验收

2026-11-02 的本地验证包括：

- Windows 当前构建的 host/TUI/proxy/rule 编译，以及 host 数据通道的有限缓冲、异常帧、空 UDP、最大 UDP 帧和双向半关闭测试；其中半关闭使用真实 Windows 本地命名管道。
- 两个真实 SOCKS5 代理共享一个规则实例：第二个实例准备失败时，第一候选撤销，两个旧 generation 与连接保持可用；成功 A→B→回滚期间，原 A 和 B 的长连接均继续回显，新流进入对应的新 generation。
- 本机代理生命周期测试：TCP 跨三代及失败准备后继续回显；SOCKS5 UDP 旧关联在切换后访问新目标仍归旧代，并保持全局流配额；暂停/恢复分配和回滚；更新前查询得到的 Fake-IP 可以由更新后的 worker 还原并连接。这里的 UDP/Fake-IP 验证不包含 TUN 或系统 DNS 接管。
- DNS crate 的解析器与共享映射测试。
- transition 串行化、取消候选、失败提交边界、宿主重启后拒绝旧连接 ID，以及显式组切换断开当前代与排空代连接的回归；只读 Rule 缓存不会下载或覆盖资源的隔离进程测试。
- Windows 令牌检查、环境过滤和非法路径输入的单元测试；这不等同于提权宿主启动普通 worker 的完整实机验收。
- 版本包导入器测试，以及不启动程序的 Python 打包函数回归：检查四个核心、清单和逐文件哈希，拒绝缺少宿主的包；已编译 `astra-host --help` 可正常退出。
- 下述仅使用本机回显的性能脚本；没有运行系统流量接管。

仍需在目标机器验收 Windows Wintun、macOS/Linux TUN、系统代理、Linux TPROXY/REDIRECT，以及完整的提权生命周期、真实系统 DNS/Fake-IP 场景和各加密协议的长连接/UDP/QUIC 更新矩阵。既有模式的独立验收不能代替该模式在版本切换期间的验收。进程崩溃、断电期间的运行事务恢复也不能仅靠版本目录日志视为已经保证。默认暂存门控在这些验收完成前保留。

可复现事务回归需要先构建当前四个程序；测试缺少程序会明确失败：

```sh
cargo build -p astra-proxy -p astra-rule -p astra-tui -p astra-manager --bins
cargo check --workspace --all-targets
cargo test -p astra-proxy --test hosted_lifecycle
cargo test -p astra-manager --lib real_update_transaction_preserves_live_connections -- --nocapture
```

测试通过单独子进程隔离实验环境变量，使用临时配置和本机 TCP，Windows 下要求普通用户执行，不请求 UAC。

## 本地性能参考

`scripts/hosted_benchmark.py` 使用同一份当前 proxy/rule 构建，交错测量普通代理与常驻 worker 模式的 SOCKS5 建连 p50/p95/p99 和持续回显吞吐。每轮创建只有默认直连动作的 Rule Core，不访问外网，不配置 TUN、系统代理或 DNS 监听。

```sh
python scripts/hosted_benchmark.py --bin-dir target/debug --connections 30 --warmup 5 --rounds 2 --seconds 1 --streams 1 --output docs/hosted-performance.json
```

原始结果、二进制 SHA-256 和每轮条件见 [hosted-performance.json](hosted-performance.json)。该记录已使用本次最终构建重新测量，适用范围以记录中的二进制哈希为准。这是 Windows debug 构建的短时开发机参考，期间机器还有开发任务，不能用它推断 release 性能或生产容量。脚本默认每轮 100 次建连、3 轮、持续 3 秒，可用 release 构建和更长负载重测。

此次小负载结果如下。建连列是各轮分位数的中位数，并非合并样本后的分位数；吞吐为各轮中位数，RSS 取各轮采样峰值的最大值。

| 模式 | 建连 p50 ms | 建连 p95 ms | 回显 payload MiB/s | 聚合 RSS 峰值 MiB |
|---|---:|---:|---:|---:|
| 普通代理 | 3.275 | 42.328 | 13.622 | 31.87 |
| 常驻宿主 + worker | 4.518 | 48.612 | 7.085 | 62.34 |

样本中常驻模式有明显额外开销，尾延迟也存在轮间波动；这些结果用于记录待优化项，不用于给出稳定的性能损耗百分比。

吞吐只计算收到回显确认的 payload 一次，不把上传和下载相加。CPU/RSS 在安装了 `psutil` 时采集，包含代理、规则及 worker 子进程；Windows 的 `conhost.exe` 若属于该进程树也包含在内。CPU 以一个逻辑核 100% 计，RSS 是约 100 ms 采样的聚合峰值，不是操作系统记录的进程终生峰值。Python 客户端和回显服务的 CPU/RSS 不计入，但它们仍影响测得的延迟和吞吐。
