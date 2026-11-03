# 公共库与管理重构：Linux 验证记录

验证日期：2026-11-03。环境为本机 WSL2 的 CachyOS，架构 `x86_64-unknown-linux-gnu`，内核 `6.18.33.2-microsoft-standard-WSL2`。这是 Linux 编译与本机库测试记录，不代替原生主机部署、系统接管或 macOS 验收。

## 工具与隔离目录

| 工具 | 版本 / 来源 |
| --- | --- |
| Rust / Cargo | 1.99.0，官方 Linux 独立组件；rustc commit `b940084d7` |
| CMake | 3.31.10，PyPI Linux wheel |
| libclang | 18.1.1，PyPI Linux wheel |
| C / C++ | CachyOS 已有 GCC 16.2.1 |
| make / Perl / pkg-config | 使用发行版已有程序 |

Rust 组件来自 [官方 1.99.0 发布清单](https://static.rust-lang.org/dist/channel-rust-1.99.0.toml)，CMake 和 libclang 来自 [CMake 3.31.10](https://pypi.org/project/cmake/3.31.10/) 与 [libclang 18.1.1](https://pypi.org/project/libclang/18.1.1/)。下载经 Windows 网络完成，压缩包分别按官方清单或 PyPI 元数据的 SHA-256 校验，再解压到 WSL 临时目录。

没有安装系统包、执行 sudo、更改用户默认 Rust 工具链或降级项目依赖。初始 Rust 1.88 不满足 uuid 的要求；尝试的 1.90 不满足 shadowsocks 的要求，最终统一使用 1.99。Cargo 索引和依赖包复制到独立临时缓存，缺少的六个包按 Cargo.lock 校验和补齐；最终命令均使用 `--locked --offline`。

工具位于 `/tmp/astra-linux-tools-20261103`，Cargo 缓存位于 `/tmp/astra-linux-common-cargo`，构建产物位于 `/tmp/astra-linux-common-check`。仓库使用 Windows 工作区的 WSL 挂载路径，未使用或覆盖 Windows target 产物。

## 复现命令

在已准备上述临时工具的 WSL shell 中：

```sh
cd /mnt/d/Users/minecraftgive/Desktop/astra-dual-core
export PATH=/tmp/astra-linux-tools-20261103/rust199/bin:/tmp/astra-linux-tools-20261103/cmake/cmake/data/bin:/usr/sbin:/usr/bin:/sbin:/bin
export LIBCLANG_PATH=/tmp/astra-linux-tools-20261103/libclang/libclang-18.1.1.data/platlib/clang/native
export BINDGEN_EXTRA_CLANG_ARGS=-isystem/usr/lib/gcc/x86_64-pc-linux-gnu/16/include
export CARGO_HOME=/tmp/astra-linux-common-cargo
export CARGO_BUILD_JOBS=4

cargo check --workspace --all-targets --locked --offline \
  --target-dir /tmp/astra-linux-common-check

cargo test --locked --offline \
  -p astra-model -p astra-app -p astra-platform -p astra-ipc \
  -p astra-manager -p astra-management -p astra-web -p astra-tui-kit \
  --lib --target-dir /tmp/astra-linux-common-check \
  -- --test-threads=1 --skip real_update_transaction_preserves_live_connections
```

临时 libclang wheel 不带完整 Clang 资源头，因此显式使用已有 GCC 的标准头目录供 bindgen 解析。一般系统安装了完整 LLVM 的环境不需要这个临时适配。

## 结果

`cargo check --workspace --all-targets` 成功，包含全部库、二进制与测试目标。项目自身没有编译警告；vendor 中保留三个既有弃用警告（一个 atomic 方法和两个 Boring AEAD 方法）。本记录没有把 check 当作 Linux Clippy 验收。

库测试合计 **112 通过、0 失败、7 ignored、1 显式过滤**：

| 包 | 通过 |
| --- | ---: |
| astra-model | 8 |
| astra-app | 15 |
| astra-platform | 11 |
| astra-ipc | 17 |
| astra-management | 5 |
| astra-manager | 45 |
| astra-tui-kit | 8 |
| astra-web | 3 |

覆盖包含：Unix peer credentials、交接帧与票据、拒绝符号链接的原子替换、独立运行目录和子进程环境、IPC 半关闭/背压/大小限制、不自动重放丢失的修改、文档与草稿冲突、未知持久状态保留、数据库版本保护、Controller 认证与离线节点路由、任务结果、Web 认证/Origin/会话撤销、终端恢复及 Unicode 编辑。

## 明确未执行的范围

`real_update_transaction_preserves_live_connections` 初次运行因缺少 `/tmp/astra-linux-common-check/debug/astra-proxy` 等完整 Linux 程序而失败；最终命令显式过滤此测试。该测试没有被改成成功或删除。七个原有 ignored 测试中，六个要求完整 Core 程序，另一个是该升级测试专用的子进程入口。

本轮没有执行 Linux 完整协议互通矩阵、完整程序在线升级、真实 TUN/路由/透明代理接管、生产 TLS 部署、跨主机故障恢复或终端视觉验收。macOS 本轮没有实际执行构建或测试。历史平台报告的日期和结论不因本记录被扩展。

本轮 Linux 检查发现并修正了 Unix 提权模块重复导入、Linux capability 解析的借用生命周期，以及若干仅在 Linux 出现的条件编译警告；这些修正没有改变原有权限判定条件。
