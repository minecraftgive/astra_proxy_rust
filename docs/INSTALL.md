# 安装与启动

当前提供 macOS arm64 的本地构建包。统一分发包含 `astra-proxy`、`astra-rule`、`astra-tui` 三个原生 Rust 可执行文件、示例配置、可选 Python 启动器、验收说明及依赖许可。已通过的能力与服务端限制见 [兼容性说明](COMPATIBILITY.md)，证据见 [验收记录](V1_ACCEPTANCE.md)。未签名、未公证，尚未验证其他 macOS 版本或 Linux 二进制。

## 解压运行

使用原生 TUI 不需要 Python；使用可选的 `scripts/astra.py` 双 core 启动器才需要 Python 3。

```sh
shasum -a 256 -c astra-0.1.0-darwin-arm64.tar.gz.sha256
tar -xzf astra-0.1.0-darwin-arm64.tar.gz
cd astra-0.1.0-darwin-arm64
./bin/astra-tui
```

也可以使用可选的双 core 启动器：

```sh
python3 scripts/astra.py --check
python3 scripts/astra.py
```

默认是本地直连代理：SOCKS5 `127.0.0.1:1080`，HTTP CONNECT `127.0.0.1:8080`。示例规则包含少量演示域名拦截。连接自己的加密节点时，先修改 `configs/native.toml` 或 `configs/tuic.toml` 中的服务器、凭据及 TLS 配置，再运行：

```sh
python3 scripts/astra.py --check --proxy configs/native.toml --rules configs/proxy-all-rule.toml
python3 scripts/astra.py --proxy configs/native.toml --rules configs/proxy-all-rule.toml
```

`proxy-all-rule.toml` 默认使用标签 `edge`；原始 `rule.toml` 默认直连。`--check` 只检查配置，不验证远端服务器和凭据。默认入站没有认证，应维持本机监听。

按 Ctrl-C 停止；启动器先停止代理并等待会话退出，再停止规则进程。正常退出后删除 UDS。若提示 `/tmp/astra-rule.sock` 已存在，先确认是否有另一实例运行；不要删除仍在使用的 socket。

可以直接运行 `bin/astra-rule 配置路径` 和 `bin/astra-proxy 配置路径`。原生二进制不需要 Python，也不会启动、下载或调用 Xray/sing-box。依赖的 macOS 动态库为系统库。

## 原生 TUI

新构建包包含 `bin/astra-tui`，可直接运行，无需 Python。支持分别管理两个 core 的实例、配置切换、内置编辑与校验；详见 [TUI 管理说明](TUI.md)。旧的已发布安装包不会自动增加该二进制，需要重新构建打包。

## 从源码构建及打包

需要 Rust、CMake、Clang/C++、libclang；Linux 构建产物仍需在目标机器验证。参考服务端仅用于开发测试，不放入运行包。

```sh
./scripts/build.sh
```

脚本从任何工作目录执行均可。默认使用锁定依赖构建本机 release 版本，依次运行工作区测试、双 core 管理 IPC 测试，然后生成并验证包含三个程序的压缩包。自动使用项目内已有的 `.build-tools/bin/cmake`（系统没有 CMake 时）。构建和打包阶段需要 Python 3.8+，分发的三个程序无需 Python。

```sh
./scripts/build.sh --debug                        # 调试构建并打包
./scripts/build.sh --build-only                   # 只编译和测试
./scripts/build.sh --label 0.1.0-tui --output-dir dist
./scripts/build.sh --skip-tests                   # 显式跳过测试
```

二进制输出位于 `target/<本机 Rust target>/release/`（调试版本为 `debug/`），支持 `CARGO_TARGET_DIR`。脚本仅打包当前机器平台，不进行交叉编译。默认标签来自三个程序共同的 Cargo 版本；`--label` 可覆盖归档标签。输出包含 `.tar.gz`、`.tar.gz.sha256` 和 `.tar.gz.verification.json`（未跳过测试时）。同名输出会被替换，保留旧包时请使用不同标签或输出目录。

已有二进制也可以单独打包：

```sh
python3 scripts/package_release.py --bin-dir target/release
```

解压包中的三个程序统一位于 `bin/`，TUI 自动找到旁边的两个 core。打包脚本从 Cargo 的实际依赖图收集许可文件；缺少许可时停止。包内 `manifest.json` 记录文件 SHA-256，包外 `.sha256` 校验整个压缩包。归档验证会检查三个程序及执行权限、文件校验和、TUI 管理 IPC 启停、配置校验、双 core 启动和 SOCKS5 回显。

本机性能基线见 [PERFORMANCE.md](PERFORMANCE.md)。这些数字包含本地测试端开销，不能当作加密代理生产容量承诺。
