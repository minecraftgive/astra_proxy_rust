# 日常代理功能对照与运行时控制

本项目继续使用原生 Rust 双核心；参照 Mihomo 的[通用配置](https://wiki.metacubex.one/en/config/general/)、[入站](https://wiki.metacubex.one/en/config/inbound/)和[代理组](https://wiki.metacubex.one/en/config/proxy-groups/)划分常用能力，不启动或内嵌 Mihomo。以下是当前实现范围，不代表配置与外部 API 完全兼容。

| 日常能力 | 当前实现 |
|---|---|
| 本地入站 | SOCKS5 TCP/UDP、HTTP CONNECT、流式 HTTP 转发；Mixed 端口自动识别 SOCKS5/HTTP |
| 入站访问控制 | 用户名/密码认证、LAN 新连接准入、IPv4/IPv6 来源 CIDR 允许/拒绝、按 CIDR 免认证 |
| 出站协议 | SS/SS2022、VMess、Trojan、VLESS、Hysteria2、TUIC、SOCKS5、HTTP；具体传输/平台边界见 [兼容性](COMPATIBILITY.md) |
| 路由 | 域名、IP/CIDR、端口、网络、进程、GeoIP/GeoSite、规则集；规则解释和热重载 |
| 模式 | TUI/CLI 即时规则、全局、直连切换；规则模式使用配置原路由，独立配置继续使用 standalone_outbound，全局使用 PROXY 组 |
| 手动代理组 | 实时选择成员，包括有效的嵌套组选择；默认保留已建立流，遵循 disconnect_on_manual 策略 |
| URL 测速组 | 按 HTTP(S) 检测耗时自动选择，支持间隔、超时、切换容差及独立断开策略 |
| 故障转移 | fallback 按成员配置顺序选择最近检测成功的第一个成员，恢复后回到更高优先级；全部失败时拒绝转发 |
| 负载均衡 | load_balance / load-balance 在检测健康的成员间按新 TCP/UDP 流轮询；全失败时拒绝转发 |
| 订阅 | 多来源、受保护下载、缓存/离线恢复、动态成员、手动刷新；保留本地修改 |
| DNS | UDP/TCP、DoH/DoT、缓存、Fake IP、域名还原、TUN DNS 接管；见 [DNS](DNS.md) |
| 系统接管 | 系统代理、TUN；Linux TPROXY/REDIRECT；原生权限和平台限制继续适用 |
| 状态面板 | 实际上传/下载计数、采样速率与趋势、活动流/累计流、成员检测结果；不依赖 ROUTE 日志 |
| 连接管理 | 查看 TCP/UDP 目标、来源、出口、字节和时长；筛选、关闭单个流、关闭全部流 |
| 操作入口 | 鼠标/键盘 TUI、当前用户专用本地 IPC、JSON 命令行控制 |
| 状态分离 | 原配置、管理状态、运行时覆盖、下载缓存、日志和备份分开存储；运行时控制不写原 TOML |

## 使用 TUI

先在配置页选择已运行的代理实例。1 仪表盘，2 代理组，3 配置，4 连接，5 工具；Tab 切换代理/规则核心，L 打开日志。鼠标可点击导航、运行模式、开关、代理组、成员、测速和关闭连接按钮。

- 代理页：↑↓ 选择组，Enter 查看成员；手动组 Enter/点击切换，t/测速按钮在后台检测。自动组成员只读，重测后更新结果。
- 仪表盘：m 轮换模式，S 系统代理，T TUN，A LAN，0 清除运行时覆盖并恢复配置默认。开启系统代理/TUN 前必须已有相应配置；开启 LAN 前必须已有非回环监听。权限不足会拒绝变更并保留原状态。
- 连接页：/ 输入筛选，Enter 结束输入，Esc 清空；d 关闭选中流，D 关闭全部，也可点击对应按钮。UDP 关联可以随后创建新流，关闭操作不等于永久阻断目标。

速率由相邻快照的累计字节差计算；计数从当前核心启动开始，重载保留，核心重启归零。转发流字节包含其实际传输的协议数据；不是网卡层或系统全部应用的流量。HTTP 普通代理包含转发的 HTTP 字节，UDP 统计载荷。入站握手失败和被规则直接拒绝的请求不算活动转发流。

为限制管理帧和终端绘制开销，一次快照展示最近 128 条活动流，以及最多 64 个组、2048 个成员，裁剪时有 `+` 提示；总活动数/累计字节仍覆盖全部被跟踪流。完整名称的组仍可通过 CLI 控制。

## 原生配置

这些字段是 Astra TOML 字段，Mihomo YAML 对应项可用 astra-convert 转换：

```toml
# 和独立的 SOCKS5/HTTP 监听任选；同一地址不能重复注册。
mixed_listen = "0.0.0.0:7890"
authentication = ["alice:replace-this-password"]
lan_allowed_ips = ["127.0.0.0/8", "192.168.1.0/24"]
lan_disallowed_ips = ["192.168.1.100/32"]
skip_auth_prefixes = []
```

LAN 开关只保存在独立运行状态，默认关闭并拒绝非回环新连接；通过 TUI A 或运行时命令开启。来源必须同时通过允许列表和拒绝列表，拒绝优先；允许列表为空表示不额外限制来源，拒绝列表为空表示无额外拒绝。CIDR 限制只作用于显式 SOCKS5/HTTP/Mixed 入站，不改变 TUN/透明代理流量的规则路由。`authentication` 非空时，SOCKS5 使用 RFC 1929、HTTP 使用 Basic；HTTP 每个请求单独检查并剥离 Proxy-Authorization，不转发给目标。`skip_auth_prefixes` 是明确指定的免认证来源，不跳过 LAN/CIDR 检查。

配置文件不再存放 `allow_lan` 开关；转换器不生成该项，编辑器保存时移除旧项。运行时关闭 LAN 只拒绝后续非回环连接，不搬动监听地址、不改配置文件、不主动关闭原连接。

组模式可以是 select、auto、fallback、load_balance，别名 url-test / load-balance 也可使用。测速组继续使用 url、interval、timeout_ms、tolerance_ms 和 disconnect_on_* 字段，见 [TUI 代理组设置](TUI.md)。

## JSON 命令行入口

`<socket>` 是代理核心管理端点：TUI 管理的实例为状态目录的 `<实例名>.sock`。独立启动可通过 ASTRA_CONTROL_SOCKET 指定；Windows 仍是命名管道标识。命令返回 JSON，失败返回非零退出码。

```sh
astra-proxy --runtime <socket> snapshot
astra-proxy --runtime <socket> mode rule
astra-proxy --runtime <socket> mode global
astra-proxy --runtime <socket> mode direct
astra-proxy --runtime <socket> select PROXY Tokyo
astra-proxy --runtime <socket> test PROXY
astra-proxy --runtime <socket> close 17
astra-proxy --runtime <socket> close-all
astra-proxy --runtime <socket> system-proxy off
astra-proxy --runtime <socket> tun off
astra-proxy --runtime <socket> allow-lan off
astra-proxy --runtime <socket> reset
```

同名选择必须仍是组的可达成员；自动组不能强行选点。Test 命令发起后台任务并立即返回，随后读取 snapshot 查看结果。重复测速不会无上限排队；重载/停止会取消旧的手动测速任务。TUI 请求携带所见核心 PID，避免核心重启后用旧连接 ID 关闭新连接。

持久覆盖位于 `ASTRA_RUNTIME_DIR/overrides/<配置路径摘要>.cache`，默认根目录为 `~/.astra/runtime`（Windows 的 `~` 表示 LOCALAPPDATA）。重启恢复模式、手动组选择及系统代理/TUN/LAN 开关；0/reset 恢复配置默认。草稿编辑和 Ctrl-S 保存仍属于明确的配置修改操作。

## 尚未提供的兼容能力

没有实现 Mihomo 的 HTTP REST/WebSocket API 和第三方面板直接连接、relay/dialer-proxy 代理链、consistent-hashing/sticky-sessions 算法、MRS、脚本扩展，以及所有 Mihomo 扩展协议。导入非轮询 load-balance 会提示改用健康节点轮询；不能认为原算法被保留。仪表盘尚无系统内存、DNS 查询明细和规则命中历史接口。

Windows TUN、IPv6、协议传输与其他平台接管仍遵循既有验证范围；这一轮增加本地运行时/混合端口/认证/UDP/组切换回归，不把未执行的真实管理员设备测试算作通过。
