# 安装与启动

当前锁定依赖要求 Rust 1.91 或更新版本；构建和回归脚本使用 Python 3.11+。Web 组件另外需要 Node 22+，纯核心和终端构建不需要 Node。

当前分发分为 client、server、controller、web 四个角色。每包包含对应程序、版本清单，client/server 另带完整 Core 示例；文档、验证记录、许可材料、源码及脚本保留在仓库。当前 Windows 预览包为原生开发构建，其他平台需在对应系统构建验证。

## 客户端

Windows、Linux 和 macOS 均可从普通用户目录运行 Astra。通过 UAC 或 TUI 的 sudo 流程提权时，核心文件及父目录的所有者、写权限或权限信息读取问题只产生警告，不要求迁移到可信安装目录，也不增加确认步骤。警告写入实例日志，并在终端或 TUI 的授权进度中显示；仍建议使用仅管理员可修改的位置（Windows 的 Program Files、Unix 的 `/usr/local/bin`）。文件缺失、路径无法解析或目标不是普通文件仍会报错。Windows 权限服务的受保护副本、系统 sudo、IPC、状态目录及自启动文件目录继续按原规则检查。

解压 `astra-client-版本-平台-架构.tar.gz` 后进入 `client/`。包含 `astra-proxy`、`astra-rule`、`astra-tui`、`astra-manager`、兼容入口 `astra-host`、`astra-convert` 和 `client.example.toml`；Windows 程序带 `.exe` 后缀，有本地 Wintun 时附运行库 `wintun.dll`。直接运行 `astra-tui` 即可管理客户端。

Windows 客户端包另含权限服务 `astra-service.exe`。`astra-tui.exe` 同时提供普通交互模式和 `--silent` 静默模式：普通模式按终端选择打开独立窗口，静默模式不打开终端。设置页中默认关闭的“静默启动”会在 Windows 登录时直接运行 `astra-tui.exe --silent`，后台启动管理器，并按各实例配置中的 `auto_start` 选择启动目标；之后可手动打开普通 TUI 管理。错误记录在状态目录的 `startup.log`。

macOS 可在 **7 设置 → 启动** 开启当前用户的登录自启动，下次登录时在所选终端中打开 TUI；开启“静默启动”后改为后台运行。Linux 提供“systemd 用户服务文件”生成选项，要求 systemd 242 或更新版本。生成后需手动执行界面显示的启用命令：先刷新用户管理器，再比较其已找到的文件；同一文件按名称启用，否则使用生成文件的完整绝对路径，兼容目录别名和自定义 `XDG_CONFIG_HOME`。需要立即运行时再执行 `systemctl --user start '<生成的单元名>'`，查看状态也使用单元名。生成文件不会自动启用、启动服务或请求管理员权限；启动单元不会等待全部实例就绪而阻塞后续登录流程。两者均根据各实例配置的 `auto_start` 选择启动目标，完整命令、状态含义及停止行为见 [TUI 启动设置](TUI.md)。

**7 设置 → 启动 → 终端** 默认“自动”，进入设置时扫描已安装的已知终端，按 r 可重新扫描并从下拉列表选择；Windows 始终可选原生控制台。选择作用于后续普通启动、新窗口及普通登录启动，静默模式不启动终端。macOS / Linux 已在终端内运行且选择“自动”时沿用当前终端。旧版 macOS 登录注册可关闭再开启，以改为直接由 TUI 选择终端。

在需要自动启动的 Proxy、Rule 或 Server 实例 TOML 顶层设置 `auto_start = true`，缺省为 `false`；也可在 TUI 的实例配置编辑器中修改并按 Ctrl-S 保存。主 TUI 打开及静默启动仅主动选择已开启的停止实例，正常绑定依赖仍随启动计划启动；已经运行的实例不会重启，附加窗口不会触发自动启动。旧版全局 `auto_start_instances` 偏好不再生效，也不会自动迁移到各份配置。

需要 TUN 无提示启动时，在本地 TUI 的 **7 设置 → 启动 → Windows 权限服务** 完成一次 UAC 注册。安装器将服务、代理核心及 Wintun DLL 复制到受管理员保护的目录；需要完整客户端包中的 `wintun.dll` 和当前账户的管理员身份。之后本地手动启动、开启 TUN、重启和静默自启动统一优先使用服务；直接运行 `astra-proxy` 启动 TUN 也采用相同策略，无需先以管理员身份运行终端。只有未注册服务且允许交互提权时才请求 UAC；静默启动缺少服务时记录错误。服务查询或启动失败、受保护核心版本不符时直接报告错误，不退回 UAC。

安装、卸载或更新服务仍需 UAC；更新前先停止 TUN 实例，再关闭、开启此开关。服务只接受安装账户的本地 TUN 启动请求，普通管理器保持原权限。通过 Controller / Agent 或其他管理客户端开启 TUN 时，可由目标管理器使用已注册服务，但不能触发交互式 UAC；缺少服务且需要提权时仍需本地操作。详情见 [TUI 启动设置](TUI.md)。Windows 权限服务程序应与客户端包一同保留，其他平台不分发。

Windows 从旧版静默启动升级后，需用原状态目录和管理目标打开新版 TUI，将登录自启动关闭再开启，以更新仍指向旧辅助程序的快捷方式；保留“静默启动”开启即可。操作细节及脚本等待方式见 [TUI 启动设置](TUI.md)。

完整示例默认独立模式，按注释填写节点后运行 `astra-proxy --check client.example.toml` 校验，然后运行 `astra-proxy client.example.toml`。需要规则核心时，按文件末尾说明把注释中的完整规则模板另存为 `rule.toml`，修改独立模式设置后分别启动两个核心；也可由 TUI 管理。示例列出全部协议变体，部署时删除不用的节点。客户端默认本机入站没有认证，保持回环监听。

## 服务端

解压 `astra-server-版本-平台-架构.tar.gz` 后进入 `server/`。包含 `astra-server-core`、`astra-server-tui`、`astra-manager`、`astra-host` 与 `server.example.toml`。按注释替换凭据和证书，运行 `astra-server-core --config server.example.toml --check`。终端入口 `astra-server-tui` 默认打开 Server 配置档，在界面中注册并绑定配置；也可通过 Agent/Web 管理。旧专用 TUI 的 `--config`/`--control` 用法不适用于统一 TUI。

Windows 服务端入口 `astra-server-tui.exe` 同样提供普通模式和 `--silent`，设置页的静默登录启动直接使用该程序，不需要额外启动辅助程序。用法与客户端一致，详见 [TUI 启动设置](TUI.md)。

macOS / Linux 的 `astra-server-tui` 同样支持相应平台的登录启动或用户服务文件生成，使用当前服务端程序及所选目录，不需要额外启动辅助程序。

示例覆盖全部八类入站协议、传输与扩展；服务端私钥与客户端公钥分别配置。部署和管理方法见 [服务端说明](SERVER.md)，扩展字段见 [服务端扩展](SERVER_EXTENSIONS.md)。

## Controller 与 Web

Controller 包包含独立 `astra-manager`；Web 包包含独立 `astra-web`，编译时嵌入 React 构建产物。二者运行时不需要 Node/Python。Web 构建需要 Node/npm，其他三个角色不需要。配置模板和初始化流程见 [MANAGEMENT.md](MANAGEMENT.md)；没有预置 Agent token 或管理员密码。

```sh
python scripts/build.py --role controller --profile release --package
python scripts/build.py --role web --profile release --package
```

首次迁移前备份旧状态、配置、更新目录和凭据，正常退出旧管理服务。新的角色配置与 v2 API 不保证旧管理程序兼容；不可让两个管理进程共用同一状态目录。

## 在 UI 中安装当前程序包

客户端 TUI、服务端 TUI 和 Web 均提供 **设置 → 安装**。首次连接支持此功能的管理目标时会提示，可选择稍后；静默启动和附加 TUI 窗口不弹出提示。安装页列出目标主机、执行账户、可发现的完整包、源目录及安装位置。Web 与远程 TUI 操作所选 Agent 所在机器，不操作浏览器所在电脑。

默认用户级安装，也可选择系统级。安装源是目标机器当前核心目录、Manager 自身目录和已配置的 Web 程序目录中的完整包；支持 client、server、controller、web，仅展示实际发现的包。缺少 `manifest.json` 的开发目录、缺文件、摘要不符或平台不匹配的包不可安装；不会在线下载其他角色，也不会复制清单外的配置或凭据。

| 平台 | 用户级程序目录 | 系统级程序目录 |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%/Programs/Astra/<角色>` | 系统 Program Files 下的 `Astra/<角色>` |
| Linux | `~/.local/share/astra/<角色>` | `/opt/astra/<角色>` |
| macOS | `~/Library/Application Support/Astra/<角色>` | `/Library/Application Support/Astra/<角色>` |

Windows 创建对应角色的开始菜单入口。Linux/macOS 提供 `astra-client`、`astra-server`、`astra-controller`、`astra-web` 命令，用户级位于 `~/.local/bin`，系统级位于 `/usr/local/bin`；不会覆盖其他程序的同名入口。若用户命令目录不在 PATH 中，界面显示设置方法，不自动修改 shell 配置。用户级使用 Agent 运行账户的目录；系统级需要管理员权限，本机 TUI 可通过一次 UAC/sudo 授权完成，Web 和远程 TUI 权限不足时提示到目标机器操作。

安装会校验、暂存复制并发布到独立部署目录，重复安装可校验现有副本并补齐入口，修复时不覆盖正在运行的程序。完成后使用新入口启动；当前进程、配置和数据保留原处，界面同时给出携带现有自定义目录参数的启动命令。既有客户端更新选择继续生效，可能优先于新入口所带基础版本。自启动、Windows TUN 权限服务仍通过原设置单独管理；安装不会开启它们，也不删除源包或旧部署。

## 从源码构建及打包

### Windows 原生构建

需要 Rust MSVC 工具链、Visual Studio C++ 构建工具及 Windows SDK、CMake、NASM（x64 的 BoringSSL 依赖）、libclang 和 Python 3.11+。NASM 可从 [官方发布目录](https://www.nasm.us/pub/nasm/releasebuilds/) 获取；将 `nasm.exe` 加入 PATH。libclang 可通过 LLVM 安装或 `python -m pip install --target .build-tools/windows/python libclang` 提供，系统安装时设置 `LIBCLANG_PATH` 指向包含 `libclang.dll` 的目录。构建脚本会识别项目内 `.build-tools/windows/nasm-*/nasm.exe` 和 `.build-tools/windows/python/clang/native/libclang.dll`。

```powershell
./scripts/build.ps1                         # debug 客户端（不测试、不打包）
./scripts/build.ps1 --profile release --package
./target/x86_64-pc-windows-msvc/debug/astra-tui.exe
python scripts/test.py --debug   # 单元、live 与本地回归
python scripts/windows_regression.py --bin-dir target/x86_64-pc-windows-msvc/debug
python scripts/astra.py --debug --check
python scripts/astra.py --debug
```

归档仍使用 `.tar.gz`，程序带 `.exe` 后缀；可使用 Windows 自带 `tar` 解压。TUI 默认状态目录是 `%LOCALAPPDATA%/.astra/tui`，配置目录是 `%LOCALAPPDATA%/.astra/config`，订阅和远程资源缓存位于 `%LOCALAPPDATA%/.astra/runtime`（缺少 LOCALAPPDATA 时回退 USERPROFILE）。可分别用 `--state-dir`、`--config-dir` 和 `ASTRA_RUNTIME_DIR` 自定义，配置与状态目录不能重合或嵌套；支持空格、中文及超过 Unix socket 长度限制的状态路径。命名管道仅允许当前用户访问，状态文件和 TUI 保存的配置使用当前用户 ACL。

默认 MSVC 构建动态依赖 `VCRUNTIME140.dll`，代理还依赖 `MSVCP140.dll`。目标 Windows 机器需要安装匹配架构且不旧于构建工具版本的 [Microsoft Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170)。运行程序不需要 Rust、CMake、NASM 或 libclang。

配置里的 `socket`、`rule_socket` 和 `ASTRA_CONTROL_SOCKET` 仍是本地端点标识路径；Windows 会将其映射为命名管道，并在该路径保存占用标记。示例中的 `/tmp/文件名` 映射至当前用户 `%TEMP%`。自定义路径应使用存在的父目录，TOML 中建议写 `C:/目录/file.sock` 或单引号包裹的 Windows 路径。正常关闭自动清理标记，崩溃后需确认没有对应进程再删除遗留标记。

Ctrl-C、Ctrl-Break 和管理 IPC 都能请求正常停止；Python 启动器在 Windows 用管理 IPC 先排空代理会话再停止规则核心。也可执行 `astra-proxy.exe --stop 管理端点路径` 或 `astra-rule.exe --stop 管理端点路径`。Windows 热重载使用管理 IPC（`--reload` / `--switch`、TUI 的 `R`）或 `watch_config = true`，不使用 SIGHUP。

此前的 Windows 本地回归覆盖双核心 IPC、SOCKS/HTTP CONNECT、64 KiB TCP 与 EOF、SOCKS UDP、独立模式启动器、正常退出及规则重载/失败回滚；工作区和 TUI live 测试另覆盖配置生命周期与日志轮转。该范围不等于全部加密协议已在 Windows 验收。2026-10-19 新增真实 WinINet HTTP/HTTPS、接管恢复，以及 Wintun 双栈 TCP/UDP、重载和设备/测试路由清理验证；已修复 TUN UDP 回包路径，范围见 [Windows 模式验证](WINDOWS_MODES_VERIFICATION.md)。TPROXY/REDIRECT 仍仅支持 Linux。

Windows 系统接管示例见 `configs/windows.toml`，两种模式默认均关闭。`[system_proxy]` 使用回环 `http_listen`，管理当前用户 WinINet LAN 的 HTTP/HTTPS 代理，不修改 WinHTTP、VPN/RAS 连接或其他用户设置；HTTPS 使用 CONNECT。TUN 需要管理员授权及对应架构的官方签名 Wintun DLL；注册权限服务后由服务完成提权，调用终端可保持普通权限：

```powershell
# 仅下载并核对 SHA-256、复制 DLL 与许可；此命令不加载或安装驱动。
python scripts/install_wintun.py --bin-dir target/x86_64-pc-windows-msvc/debug
# 离线时用 --archive 指向官方 wintun-0.14.1.zip。
# 构建脚本会把缓存中的 DLL 与许可复制进新产物：
./scripts/build.ps1 --debug
```

默认从 `astra-proxy.exe` 旁加载 `wintun.dll`；`ASTRA_WINTUN_DLL` 可指定另一可信 DLL 的完整路径。通过权限服务启动时固定使用安装时保存的受保护 DLL 副本，此环境变量不能替换服务使用的 DLL。精简包只复制运行 DLL；许可和来源信息保留在准备脚本的本地输出中。没有 DLL 时普通 SOCKS/HTTP 模式仍可运行，只有启用 TUN 才报缺少驱动。Wintun 版本、下载地址及固定 SHA-256 保存在准备脚本及本地来源记录中，来源是 [Wintun 官方发布](https://www.wintun.net/)。具体路由、权限、恢复说明见 [系统模式](PROXY_MODES.md)。

### Linux / macOS 构建

需要当前稳定版 Rust、CMake、Clang/C++、libclang；本轮使用 Rust 1.99，当前锁定的 shadowsocks 依赖至少需要 Rust 1.91。已完成 WSL/CachyOS 全 workspace 编译检查和基础/管理库测试，范围与命令见 [Linux 验证记录](LINUX_REFACTOR_VERIFICATION.md)。Linux 程序部署与系统接管仍需在目标机器验证。启用 Unix TUN 或需要特权路由时，请在终端中用 `sudo` 重新启动对应程序；Astra 不读取、保存或转发 sudo 密码，也不会在 TUI 中提供密码输入框。普通 SOCKS/HTTP 模式无需 root。

```sh
./scripts/build.sh
./scripts/build.sh --debug
```

与 Windows 一致，`build.sh` 是 POSIX `sh` 脚本（不依赖 bash），优先使用 `PYTHON` 环境变量指定的解释器，否则依次查找 `python3`、`python3.14`…`python3.11`、`python` 中第一个 3.11+ 版本，并原样返回 `build.py` 的退出码。构建前检查 Cargo、rustc、CMake 及 C/C++ 编译器（`CC`/`CXX`，或 `cc`/`clang`/`gcc` 与 `c++`/`clang++`/`g++`），对应 Windows 的 MSVC/NASM 检查。未设置 `LIBCLANG_PATH` 时，与 Windows 相同地识别项目内 libclang：`python3 -m pip install --target .build-tools/unix/python libclang`，或安装在 `.build-tools` 虚拟环境中的 libclang；系统 LLVM/Xcode 的 libclang 由 bindgen 自行查找。

`build.sh`、`build.ps1` 都只是转调 `scripts/build.py`，参数相同，从任何工作目录执行均可。默认使用锁定依赖，以 `fast` 模式构建本机 `debug` 客户端程序，**不运行测试、不打包**。在终端运行时显示表单，输入字段编号修改 profile（debug/release/both）、模式（fast/runtime）、架构（native/64bits）、角色、打包及耗时报告；输入选项编号或值并回车。表单显示实际 Cargo profile，命令行参数作为初始值。输入 `B` 并回车开始编译，`Q` 取消；没有倒计时自动构建，输入结束或 Ctrl-C 也会取消。`--interactive` 强制显示表单，`--non-interactive` 跳过；非终端输入默认直接使用命令行参数。自动使用项目内已有的 `.build-tools/bin/cmake`（系统没有 CMake 时）。构建、打包和测试需要 Python 3.11+，分发程序无需 Python。

架构默认 `native`，使用当前 Rust host target。选择 `64bits`（或传 `--arch 64bits`）时，会校验该目标的指针宽度为 64 位，支持本机 x86_64 和 ARM64；32 位 Rust 工具链会在构建前报错，需先切换为本机 64 位工具链。这个选项不启用交叉编译，实际 target 会打印到终端并记录在产物索引中。

```powershell
# 默认：编译最快的 debug（Cargo dev）
./scripts/build.ps1

# 运行最快、可分发的 release（Cargo release）
./scripts/build.ps1 --profile release

# 同时生成两种类型并记录耗时
./scripts/build.ps1 --profile both --timings

# 64 位 release；跳过表单直接构建
./scripts/build.ps1 --profile release --arch 64bits --non-interactive
```

```sh
./scripts/build.sh --profile both --timings   # 测量 debug 和 release
./scripts/build.sh --role server              # debug 服务端
./scripts/build.sh --profile release --role all --package
./scripts/build.sh --profile release --package --label 0.1.0-tui --output-dir dist --non-interactive
```

工作区只定义两个 Cargo profile（`Cargo.toml`），不允许新增自定义 profile；`--debug` 是 `--profile debug` 的别名，`--profile both` 依次构建两者：

| 版本 | Cargo profile | 二进制目录（相对 `target/<本机 Rust target>/`） | 目标与配置 |
|---|---|---|---|
| debug（默认） | `dev` | `debug/` | 编译最快：`opt-level=0`、无调试信息、256 个代码生成单元、增量编译，保留断言与溢出检查 |
| release | `release` | `release/` | 运行最快：`opt-level=3`、完整 LTO、单个代码生成单元、关闭增量编译、关闭断言与溢出检查 |

Cargo 内置的 `test`、`bench` profile 分别继承 `dev`、`release`，不单独配置。debug 不生成调试信息，panic 回溯只显示函数名；需要行号或变量级调试时，可在独立终端设置 `CARGO_PROFILE_DEV_DEBUG=line-tables-only`（或 `2`），退出该终端即可恢复默认，改变后会触发重编译。**debug 不优化运行速度**，吞吐和 CPU 开销明显逊于 release，性能测试只用 release。

两者都保留 `panic = "unwind"`（astra-manager 的数据库线程依赖 `catch_unwind`，tokio 依赖展开隔离任务 panic）。release 使用目标平台默认 CPU 基线，不设置 `target-cpu=native`，不引入 nightly 或 PGO，产物可分发到同架构的任意机器。配置选项见 [Cargo profile 官方文档](https://doc.rust-lang.org/cargo/reference/profiles.html)。BoringSSL、AWS-LC 等原生依赖仍需编译。

仓库的 `.cargo/config.toml` 将默认目标设为 `host-tuple`，因此普通 `cargo build` / `cargo build --release` 与脚本使用完全相同的 profile 与 `target/<本机 Rust target>/{debug,release}/` 布局。

输出支持 `CARGO_TARGET_DIR`；脚本仅针对当前机器平台，不进行交叉编译。`--package` 的默认标签来自所选程序共同的 Cargo 版本，`--label` 可覆盖；分发目录生成 `.tar.gz`，SHA-256 输出到控制台。每次构建使用独立时间戳目录并生成 `artifacts.json`，记录角色、debug/release 和 Cargo profile；有多次产物时，通过 `--artifact-dir` 明确选择待验证的目录。四个角色分别打包，Web 与 Controller 不被嵌入 Core。

新安装包的 `manifest.json` 也记录 Cargo profile。本次更新后的导入器仍接受旧安装包；更新前的导入器会拒绝这些新增字段，因此首次升级到此版本须正常退出程序后手动解压、替换程序，不能通过旧程序的包导入功能升级。

加 `--timings` 生成 Cargo HTML 编译报告，并将每个 profile 的报告保存到 `dist/<本次构建目录>/cargo-timing-<Cargo profile>.html`；脚本打印确切路径，原始报告也保留在目标目录的 `cargo-timings/` 中。对比时保持角色、目标、工具链和机器一致，分别记录空缓存首次构建、无改动重复构建，以及修改 Rust 文件后的增量构建；冷构建可用独立 `CARGO_TARGET_DIR`，无需删除日常缓存。仅切换 profile 后的首次耗时不能代表稳定的增量速度。构建脚本不会自动运行性能测试。

解压包中的程序位于对应角色目录；client/server 包另含 Core 示例，TUI 使用旁边的管理程序和 Core。

### 测试

测试与构建分开，由 `scripts/test.py` 针对 `build.py` 的产物运行。debug 产物添加 `--debug`，release 产物不加；指定 `--artifact-dir` 时从索引读取程序目录，旧 profile（`release-fast` 等）构建的索引会被拒绝，需重新构建。启动器 `scripts/astra.py` 使用相同的 `--debug` 与 `--artifact-dir` 选择规则。

```sh
python3 scripts/test.py --debug             # 默认套件：unit live client
python3 scripts/test.py --debug unit        # 只跑工作区单元/集成测试
python3 scripts/test.py --artifact-dir dist/<本次构建目录> server
python3 scripts/test.py --artifact-dir dist/<本次构建目录> package
python3 scripts/test.py --debug --keep-going # 失败后继续，最后汇总
```

| 套件 | 内容 |
|---|---|
| `unit` | `cargo test --workspace` |
| `live` | TUI 驱动真实 Proxy/Rule Core 的端到端测试 |
| `client` | 订阅诊断、DNS、路由日志回归；Windows 另跑系统集成、规则集、代理组、单/多订阅回归 |
| `server` | 服务端互通与生命周期回归 |
| `package` | 归档文件集合、完整配置、TUI 管理 IPC、核心启动及 SOCKS5 回显；客户端报告写入 `.test-tools/` |
| `release` | 完整协议矩阵门禁（`release_check.py`），需先用 `install_test_servers.py` 准备参考服务端 |

单个回归脚本仍可直接运行，例如 `python scripts/windows_regression.py --bin-dir target/x86_64-pc-windows-msvc/debug`。旧目录或外部产物可显式传 `--bin-dir`。

本机性能基线见 [PERFORMANCE.md](PERFORMANCE.md)。这些数字包含本地测试端开销，不能当作加密代理生产容量承诺。
