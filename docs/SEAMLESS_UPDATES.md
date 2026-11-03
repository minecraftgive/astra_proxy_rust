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

客户端打包脚本会包含 `astra-tui`、`astra-manager`、`astra-host`、`astra-proxy`、`astra-rule`、转换工具和 `manifest.json`。导入器要求前述四个核心的可执行文件和版本声明齐全，缺少 `astra-host` 的旧三核心包不能作为更新包导入。清单记录目标平台、组件版本、宿主协议范围、文件大小和 SHA-256；SHA-256 用于检查包内容完整性，不表示发布者签名认证。支持解压后的版本目录、ZIP、`.tar.gz` 和 `.tgz`，不自动从网络下载版本。

```powershell
# 默认行为：校验并暂存，不切换运行实例。
.\astra-tui.exe update C:\Downloads\astra-client-example.tar.gz
.\astra-tui.exe update-status

# 已在本次宿主会话中完成过实验在线更新时，可回滚新流使用的版本。
.\astra-tui.exe rollback
```

Linux/macOS 使用对应的无 `.exe` 程序名。使用自定义目录时，各命令传入同一组 `--state-dir` 和 `--config-dir`；它们必须分离且不能互相嵌套。升级包存放在状态目录的 `updates/releases/<release_id>`，不覆盖正被运行的可执行文件，也不将示例配置覆盖到用户配置。

默认暂存不会把该版本设为活动版本或下次启动选择。维护时可在完成主动断开后运行已校验版本目录中的新程序，或显式用 `--bin-dir` 指定该目录。已成功在线提交的版本会写入启动选择；下次新启动会校验所选版本，并使用其完整客户端程序。显式 `--bin-dir` 可用于维护时选定程序目录。

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
