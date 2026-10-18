# 安装与启动

当前分发分为 client 和 server，两端完全独立。每包只有对应运行程序和一份完整示例配置；文档、验证记录、许可材料、源码及脚本保留在仓库。当前 Windows 预览包为原生开发构建，其他平台需在对应系统构建验证。

## 客户端

解压 `astra-client-版本-平台-架构.tar.gz` 后进入 `client/`。包含 `astra-proxy`、`astra-rule`、`astra-tui`、`astra-convert` 和 `client.example.toml`；Windows 程序带 `.exe` 后缀，有本地 Wintun 时附运行库 `wintun.dll`。直接运行 `astra-tui` 即可管理客户端。

完整示例默认独立模式，按注释填写节点后运行 `astra-proxy --check client.example.toml` 校验，然后运行 `astra-proxy client.example.toml`。需要规则核心时，按文件末尾说明把注释中的完整规则模板另存为 `rule.toml`，修改独立模式设置后分别启动两个核心；也可由 TUI 管理。示例列出全部协议变体，部署时删除不用的节点。客户端默认本机入站没有认证，保持回环监听。

## 服务端

解压 `astra-server-版本-平台-架构.tar.gz` 后进入 `server/`。仅包含 `astra-server-core`、`astra-server-tui` 与 `server.example.toml`。按配置注释替换凭据和证书路径，运行 `astra-server-core --config server.example.toml --check`，再运行 `astra-server-tui --config server.example.toml`。

示例覆盖全部八类入站协议、传输与扩展；服务端私钥与客户端公钥分别配置。部署和管理方法见 [服务端说明](SERVER.md)，扩展字段见 [服务端扩展](SERVER_EXTENSIONS.md)。

## 从源码构建及打包

### Windows 原生构建

需要 Rust MSVC 工具链、Visual Studio C++ 构建工具及 Windows SDK、CMake、NASM（x64 的 BoringSSL 依赖）、libclang 和 Python 3.8+。NASM 可从 [官方发布目录](https://www.nasm.us/pub/nasm/releasebuilds/) 获取；将 `nasm.exe` 加入 PATH。libclang 可通过 LLVM 安装或 `python -m pip install --target .build-tools/windows/python libclang` 提供，系统安装时设置 `LIBCLANG_PATH` 指向包含 `libclang.dll` 的目录。构建脚本会识别项目内 `.build-tools/windows/nasm-*/nasm.exe` 和 `.build-tools/windows/python/clang/native/libclang.dll`。

```powershell
./scripts/build.ps1                         # release 构建、测试、打包并验证
./scripts/build.ps1 --debug --build-only    # debug 构建和测试
./target/x86_64-pc-windows-msvc/debug/astra-tui.exe
# 已有普通 cargo build 的产物：
python scripts/windows_regression.py --bin-dir target/debug
python scripts/astra.py --debug --check
python scripts/astra.py --debug
```

归档仍使用 `.tar.gz`，程序带 `.exe` 后缀；可使用 Windows 自带 `tar` 解压。TUI 默认状态目录是 `%LOCALAPPDATA%/.astra/tui`（缺少 LOCALAPPDATA 时回退 USERPROFILE），支持空格、中文及超过 Unix socket 长度限制的状态路径。命名管道仅允许当前用户访问，状态文件和 TUI 保存的配置使用当前用户 ACL。

默认 MSVC 构建动态依赖 `VCRUNTIME140.dll`，代理还依赖 `MSVCP140.dll`。目标 Windows 机器需要安装匹配架构且不旧于构建工具版本的 [Microsoft Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170)。运行程序不需要 Rust、CMake、NASM 或 libclang。

配置里的 `socket`、`rule_socket` 和 `ASTRA_CONTROL_SOCKET` 仍是本地端点标识路径；Windows 会将其映射为命名管道，并在该路径保存占用标记。示例中的 `/tmp/文件名` 映射至当前用户 `%TEMP%`。自定义路径应使用存在的父目录，TOML 中建议写 `C:/目录/file.sock` 或单引号包裹的 Windows 路径。正常关闭自动清理标记，崩溃后需确认没有对应进程再删除遗留标记。

Ctrl-C、Ctrl-Break 和管理 IPC 都能请求正常停止；Python 启动器在 Windows 用管理 IPC 先排空代理会话再停止规则核心。也可执行 `astra-proxy.exe --stop 管理端点路径` 或 `astra-rule.exe --stop 管理端点路径`。Windows 热重载使用管理 IPC（`--reload` / `--switch`、TUI 的 `R`）或 `watch_config = true`，不使用 SIGHUP。

此前的 Windows 本地回归覆盖双核心 IPC、SOCKS/HTTP CONNECT、64 KiB TCP 与 EOF、SOCKS UDP、独立模式启动器、正常退出及规则重载/失败回滚；工作区和 TUI live 测试另覆盖配置生命周期与日志轮转。该范围不等于全部加密协议已在 Windows 验收。2026-10-19 新增真实 WinINet HTTP/HTTPS、接管恢复，以及 Wintun 双栈 TCP/UDP、重载和设备/测试路由清理验证；已修复 TUN UDP 回包路径，范围见 [Windows 模式验证](WINDOWS_MODES_VERIFICATION.md)。TPROXY/REDIRECT 仍仅支持 Linux。

Windows 系统接管示例见 `configs/windows.toml`，两种模式默认均关闭。`[system_proxy]` 使用回环 `http_listen`，管理当前用户 WinINet LAN 的 HTTP/HTTPS 代理，不修改 WinHTTP、VPN/RAS 连接或其他用户设置；HTTPS 使用 CONNECT。TUN 需要管理员身份及对应架构的官方签名 Wintun DLL：

```powershell
# 仅下载并核对 SHA-256、复制 DLL 与许可；此命令不加载或安装驱动。
python scripts/install_wintun.py --bin-dir target/debug
# 离线时用 --archive 指向官方 wintun-0.14.1.zip。
# 构建脚本会把缓存中的 DLL 与许可复制进新产物；需要时可跳过所有测试：
./scripts/build.ps1 --debug --skip-tests
```

默认从 `astra-proxy.exe` 旁加载 `wintun.dll`；`ASTRA_WINTUN_DLL` 可指定另一可信 DLL 的完整路径。精简包只复制运行 DLL；许可和来源信息保留在准备脚本的本地输出中。没有 DLL 时普通 SOCKS/HTTP 模式仍可运行，只有启用 TUN 才报缺少驱动。Wintun 版本、下载地址及固定 SHA-256 保存在准备脚本及本地来源记录中，来源是 [Wintun 官方发布](https://www.wintun.net/)。具体路由、权限、恢复说明见 [系统模式](PROXY_MODES.md)。

### Linux / macOS 构建

需要 Rust、CMake、Clang/C++、libclang；Linux 构建产物仍需在目标机器验证。服务端通过独立的 build_server.py 构建，不混入客户端运行包。

```sh
./scripts/build.sh
```

脚本从任何工作目录执行均可。默认使用锁定依赖构建本机 release 版本，依次运行工作区测试、双 core 管理 IPC 测试，然后生成并验证包含四个客户端程序的压缩包。自动使用项目内已有的 `.build-tools/bin/cmake`（系统没有 CMake 时）。构建和打包阶段需要 Python 3.8+，分发程序无需 Python。

```sh
./scripts/build.sh --debug                        # 调试构建并打包
./scripts/build.sh --build-only                   # 只编译和测试
./scripts/build.sh --label 0.1.0-tui --output-dir dist
./scripts/build.sh --skip-tests                   # 显式跳过测试
```

二进制输出位于 `target/<本机 Rust target>/release/`（调试版本为 `debug/`），支持 `CARGO_TARGET_DIR`。脚本仅打包当前机器平台，不进行交叉编译。默认标签来自客户端程序共同的 Cargo 版本；`--label` 可覆盖归档标签。分发目录仅生成 `.tar.gz`，SHA-256 输出到控制台；验证报告保存到仓库 `.test-tools/`（未跳过测试时）。同名输出会被替换，保留旧包时请使用不同标签或输出目录。

已有二进制也可以单独打包：

```sh
python3 scripts/package_release.py --bin-dir target/release
```

解压包中的程序与完整示例配置直接位于 `client/` 或 `server/`，TUI 自动找到旁边的核心。归档验证检查精确文件集合、完整配置、TUI 管理 IPC、原生核心启动及 SOCKS5 回显。服务端包通过 `python scripts/package_server.py --bin-dir target/release` 单独生成。

本机性能基线见 [PERFORMANCE.md](PERFORMANCE.md)。这些数字包含本地测试端开销，不能当作加密代理生产容量承诺。
