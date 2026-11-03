# 模块边界与依赖方向

Astra 将纯数据模型、平台操作、通信、文档算法、管理服务和界面分开。Linux、macOS、Windows 使用同一份协议与业务代码；平台差异由适配层处理。Controller/Agent 部署与初始化见 [MANAGEMENT.md](MANAGEMENT.md)。

## 运行进程

```text
浏览器 -> astra-web -> Controller 私有 IPC
                           ^
                    Agent 主动 WSS 连接
                           |
astra-tui ----------> Agent 私有 IPC
                           |
                   本地生命周期服务
                    /      |       \
          astra-proxy  astra-rule  astra-server-core
```

`astra-manager` 的配置选择 Controller 或 Agent。Controller 只做节点认证、连接、请求路由、任务及快照索引，不运行 Core。Agent 拥有源配置、草稿、资源、进程与更新操作；关闭 upstream 时保留完整本地模式。远程连接中断不自动结束运行中的 Core，也不会重放未确定结果的修改。

`astra-web` 是独立二进制，静态 React 前端、管理员认证和固定管理 IPC 网关均在该进程。Manager 可监督这个子进程，但 Web 不嵌入 manager/core。`astra-host` 是 manager crate 的兼容入口；`astra-server-tui` 是 tui crate 默认选择 Server 的薄入口，两者都没有复制模块树。

## 库职责

| crate | 所有权与限制 |
|---|---|
| `astra-model` | CoreKind、路由/运行状态/管理业务 DTO、各类配置与默认值、纯字段/引用校验、文档 schema/编辑命令；可选 TS 绑定；无文件、进程或网络访问 |
| `astra-platform` | 私有 IPC 端点、权限、原子文件写入、运行目录、日志、进程标志、提权与 Unix handoff；不依赖 IPC/manager/UI |
| `astra-resource` | 有大小、超时及重定向限制的资源加载和缓存；每个 ResourceLoader 独立选择 Refresh/PreferCache/ReadOnly；不使用全局可变策略 |
| `astra-ipc` | bincode/控制帧、会话、Core 控制服务器、宿主/worker 数据通道及任务进度传输；不再作为平台/业务类型的转发集合 |
| `astra-protocols` | SOCKS/HTTP/Trojan/VLESS/VMess/SS/TUIC/Hysteria2 协议适配、wire/datagram；不依赖 proxy/server/manager |
| `astra-dns` | DNS 编解码、Fake IP 映射等 DNS 能力；配置类型归 model |
| `astra-rule-data` | 规则集资源装配和 GeoIP/GeoSite DAT 解码；纯匹配声明与解析归 model，缓存下载归 resource |
| `astra-convert` | 离线 YAML/分享链接转换；下载由可选 download feature 提供；不依赖 IPC/DNS运行时 |
| `astra-app` | TOML 文档、schema、引用更新、模板、导入、撤销/重做；以禁用下载的 converter 为依赖，无终端或 HTTP 客户端 |
| `astra-management` | 管理 v2 DTO、客户端与部署配置，组合 model 中的公共数据；含 server/host control 客户端，不依赖 Core 或 UI |
| `astra-manager` | Agent 生命周期、草稿/任务数据库、更新与诊断服务；Controller 路由与认证；同一个本地服务被 TUI 和远程入口调用 |
| `astra-update` | 版本清单、解包、校验、版本存储与事务支持；不承担界面或 Controller 路由 |
| `astra-tui-kit` | ratatui 终端会话、输入、列表、面板和布局工具；不依赖管理业务 |
| `astra-tui` | UI 状态、命令与终端渲染，调用 app/management/tui-kit；不包含管理 daemon 实现 |
| `astra-web` / `apps/web` | 独立 HTTPS 网关与浏览器界面，通过管理契约操作 Agent |

依赖方向是从应用到能力库。model 不依赖 platform/ipc；platform 不依赖 ipc；management 不依赖 manager/core/tui；app 不依赖 management，避免文档 schema 与管理 DTO 互相引用。核心直接消费 model DTO 和 ipc framing，客户端 UI 不需要链接代理实现。

## 配置处理的三个阶段

1. 解析：带大小限制的 TOML 文档或类型反序列化。编辑草稿保留注释、格式和未知字段；不完整草稿也可保存为草稿。
2. 纯校验：model 的 `validate_structure` 检查字段、凭据约束、监听/TUN 形状、规则动作/匹配器、引用和跨字段组合，不访问磁盘或系统资源。app 提供统一文档入口。
3. 资源准备：Core 加载证书、规则数据、协议加密参数，检查目标平台能力并准备候选。候选准备失败保留当前运行配置。真正监听与权限变更只在启动/应用阶段执行。

Server 的 `--check-stdin PATH` 保留原配置路径作为相对资源基目录，不创建临时源文件。Agent 的 runtime_dir 被显式传给本地生命周期服务、诊断/校验子进程与更新代；manager 通过带 root 参数的存储 API 读取相同状态，不修改多线程进程的全局环境。

## 协议与平台适配

`astra-protocols::accept` 接收流与凭据，返回请求信息；路由、配额、超时、重载与统计由 Core 负责。客户端本地入口与服务器强制认证是不同策略，共用格式解析不改变认证要求。密码学继续使用已有 meow-proxy/shadowsocks/TLS 库。

`astra-proxy/src/platform/` 保留与代理网络资源紧密相关的 TUN、系统代理、透明代理、接口/进程绑定及 ICMP 适配。通用权限、本地流、日志、提权和存储归 astra-platform。服务器 protocols/transports 模块是服务端会话协调，不复制到客户端。协议库不得引入 Windows API、std::os 或操作系统条件分支。

## 修改与验证

`python scripts/check_architecture.py` 检查 crate 依赖、薄入口、模块与平台边界。构建工具公共代码集中于 scripts/astra_tools，角色包清单和产物索引统一，脚本按 Cargo package/产物定位而非旧源码路径。

修改纯文档或模型后运行对应库测试；改协议或运行时后增加核心互通/生命周期回归。Windows 使用 scripts/build.py 的构建环境找到 NASM/libclang。Linux/macOS 的系统代理、TUN、权限与服务安装仍需要对应平台验收，Windows 的编译/单元测试不能替代它们。

本次是允许不兼容的管理与库边界重构。原生 Core 配置语义尽量保持，新的部署配置和 v2 管理契约以当前文档为准；旧状态迁移、备份与回退步骤见 MANAGEMENT.md。历史验收报告的日期和二进制哈希保持原值。
