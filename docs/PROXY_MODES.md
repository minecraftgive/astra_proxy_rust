# 系统接管模式与配置热重载

运行本次构建的 `target/release/astra-proxy`、`astra-rule` 和 `astra-tui`。管理与路由 IPC 已更新为 v3，旧程序与新程序不能混用。后台 TUI 管理服务也需在更新后重新启动；停止已有实例会断开其连接，请在合适的时间操作。

## 选择接管方式

| 方式 | 平台 | 流量范围 | 要求 |
|---|---|---|---|
| SOCKS5 / HTTP CONNECT | Linux/macOS/Windows | 应用显式发送给监听端口的流量；Windows 另支持普通 HTTP 转发 | 无管理员权限要求 |
| `[system_proxy]` | macOS、Linux GNOME、Windows | 遵循桌面系统代理的应用 | macOS 网络服务写权限；GNOME 用户 D-Bus；Windows 当前用户 |
| `[tun]` | Linux/macOS/Windows | 路由进入虚拟网卡的 TCP/UDP | root / Linux CAP_NET_ADMIN；Windows 管理员 + Wintun |
| `[tproxy]` | Linux | 策略路由与 TPROXY 送入的 TCP/UDP | CAP_NET_ADMIN 或 CAP_NET_RAW、内核 TPROXY、nftables/iptables |
| `[redirect]` | Linux | NAT REDIRECT 送入的 TCP | 外部 NAT 规则；UDP 使用 TPROXY |

这些模式可并用。系统代理和 TUN 的启停已移出配置，保存在独立运行目录 `overrides/`；首次启动默认关闭，通过 TUI 的 S/T 或 `--runtime SOCKET system-proxy on|off`、`--runtime SOCKET tun on|off` 切换。对应配置表只定义固定网络参数，缺失时使用运行时默认参数；重置会关闭两种模式。TPROXY/REDIRECT 的监听定义仍属于固定入站配置。TPROXY/REDIRECT 在 macOS/Windows 的配置检查阶段就会被拒绝。普通监听、原生出站和 standalone 保持原有语义。Windows TUN、系统代理与普通 HTTP 转发已通过本机集成检查，范围及复现见 [Windows 模式验证](WINDOWS_MODES_VERIFICATION.md)。

完整字段见 `example_proxy.toml`，精简模板见 `configs/modes.toml`。

## 系统代理

固定监听在代理配置顶层设置；系统代理使用 TUI 的 S 开启，不需要添加配置表。仅需指定 macOS 网络服务时可保留固定参数表：

```toml
socks5_listen = "127.0.0.1:1080"

[system_proxy]
services = ["Wi-Fi"] # macOS；省略或 [] 自动发现启用的网络服务
```

Linux GNOME 不需要 `[system_proxy]`，不要填写 macOS 的服务名。服务运行在当前桌面用户的 D-Bus 会话中；无 GNOME 会话的 Linux 服务器使用 TUN/TPROXY。

macOS/Linux 系统集成使用 SOCKS，HTTP 入站在这两个平台仍只支持 CONNECT。启用时临时关闭优先级更高的 HTTP/HTTPS/PAC 自动设置，GNOME 清空 HTTP/HTTPS/FTP 代理 host；原值全部记录后才写入，正常停止或关闭此模式时恢复。系统代理仍由应用决定是否遵循，不能强制接管忽略代理的程序。已有需要认证的 macOS SOCKS 设置不能完整读回密码，因此会拒绝启用并保留原设置。

Windows 使用回环 HTTP 代理：

```toml
http_listen = "127.0.0.1:8080"

[system_proxy]
# Windows 不填 services；该字段仅用于 macOS。
```

Windows 通过 WinINet per-connection API 管理当前用户的 LAN HTTP/HTTPS 代理。普通 HTTP 请求由 Hyper 校验消息边界、移除逐跳头并流式转发，HTTPS 沿用 CONNECT 隧道；普通 HTTP 当前每条连接处理一个请求，`Upgrade` 需走独立 CONNECT（不支持明文 WebSocket 升级）。启用时记录手动代理地址、绕过列表、PAC URL 和自动检测标志，再设置 `回环地址:端口`（所有协议共用，与 Windows 设置界面写入的格式一致）、`<local>` 绕过并临时关闭 PAC/自动检测；正常关闭时恢复原值并通知应用。WinHTTP、VPN/RAS 连接、其他用户账户以及不遵循系统代理的应用不在该设置范围内。

Windows 每个账户只允许一个 Astra 实例管理系统代理。跨进程独占锁与待恢复文件位置记在 `%LOCALAPPDATA%/.astra/system-proxy.lock`；发生崩溃时新实例会指出要恢复的文件，不能用另一配置覆盖原快照。恢复命令必须以原来的用户身份执行；UAC 提升应使用同一个账户。TUI 状态目录中的恢复文件持续保留；Python 启动器在 `%LOCALAPPDATA%/.astra/launchers/` 保存控制端点，恢复失败时保留该目录。

写设置前创建 0600 恢复文件：有 `ASTRA_CONTROL_SOCKET` 时在该 socket 同目录、同文件名但扩展名为 `.system-proxy-state.toml`；否则写入 `~/.astra/runtime/system-proxy/<源路径摘要>.cache`（`ASTRA_RUNTIME_DIR` 可自定义根目录）。旧配置旁若存在未恢复记录，会提示先用恢复命令处理，避免遗漏旧的系统设置快照。存在未恢复文件时拒绝覆盖。SIGKILL 或掉电后，在确认旧 core 已停止后恢复：

```sh
./target/release/astra-proxy --restore-system-proxy /absolute/path/to/modes.system-proxy-state.toml
```

恢复失败会保留文件供重试。正常退出后文件删除。不要删除尚未恢复的文件。

Windows 使用相同入口，例如 `./target/debug/astra-proxy.exe --restore-system-proxy C:/Astra/state/proxy.system-proxy-state.toml`。此命令会实际改动系统设置，应在确认旧实例停止且需要恢复时执行。

## 原生 TUN

TUN 设备由 `tun-rs` 创建，数据包由 Rust `netstack-smoltcp` 拆成 TCP 流和 UDP 会话，继续使用 Astra 的原生协议出站；不启动外部代理核心。TCP/UDP IPv4/IPv6 协议栈可用，未配置 DNS 时，DNS 作为普通 UDP/TCP 转发；可通过独立 DNS 模块启用 53 端口接管、DoH/DoT、Fake IP 和域名还原，详见 [DNS](DNS.md)。无域名映射的 IP 流量按 IP/端口规则匹配。ICMP 由本地用户态栈响应，不代表远端目标的真实 ping 结果。

最低配置（路由由外部管理）：

```toml
[tun]
address = "198.18.0.1"
prefix = 30
mtu = 1500
routes = []
```

全局 IPv4 接管示例，必须替换物理网卡、网关、节点和 DNS 地址：

```toml
[tun]
name = "utun20" # macOS；Linux 可用 astra0，也可省略自动分配
address = "198.18.0.1"
prefix = 30
mtu = 1500
interface = "en0" # Linux 示例 eth0，必须是接管前的物理出口
routes = ["0.0.0.0/1", "128.0.0.0/1"]
gateway = "192.168.1.1"
bypass = ["203.0.113.10/32", "8.8.8.8/32"] # 节点 IP + 原 DNS
```

Linux/macOS 的进程需以具备建卡/改路由权限的身份运行。Windows 启动含 `[tun]` 的配置时会检查当前令牌，仅在尚未提权时通过系统 UAC 请求管理员权限；Astra 不读取或保存管理员密码。TUI 和 manager 保持原权限，仅该代理核心作为独立管理员进程运行，规则核心和其他普通代理不随之提权。已具备管理员权限时直接启动，不重复弹窗；`--check` 和查询、停止等控制命令不触发 UAC。取消授权会报告启动失败。

TUI manager 持有系统返回的进程句柄，继续校验核心 PID、读取原实例日志、通过 IPC 热重载和优雅停止；IPC 失败或停止超时才通过持有的句柄强制终止。每次重启均重新判断权限，不保存持久管理员授权。在 TUI 中为运行中的普通权限代理开启 TUN 时，先弹出 UAC，原核心在等待授权期间继续服务；取消或授权失败时原核心与运行开关均保持不变。批准后原核心才排空退出，新的管理员核心等它退出（最多 60 秒）后再加载配置并接管端点，切换窗口只包含排空时间。UAC 必须使用原 Windows 账户，避免切换账户后操作错误的用户代理设置或失去私有 IPC 访问权限。

直接运行 `astra-proxy.exe 配置文件` 也支持按需提权，原进程等待管理员核心退出，日志写入独立运行目录的 `elevated-logs/<源路径摘要>.cache`；Ctrl+C 会通过管理端点请求停止。没有指定 `ASTRA_CONTROL_SOCKET` 时使用独立运行目录的 `elevated-control/<源路径摘要>.cache` 作为 Windows 命名管道标识。普通权限实例通过热重载首次开启 TUN 时会拒绝候选配置并保留当前运行状态，需要重启该实例以请求 UAC。关闭 TUN 会清理设备和路由；该核心的管理员令牌持续到进程退出，若要恢复普通权限，应关闭 TUN 后重启实例。

Windows 使用 [Wintun 官方签名 DLL](https://www.wintun.net/)。运行前按 [安装说明](INSTALL.md) 准备匹配架构的 DLL 和许可文件；准备脚本只下载和复制文件，不加载驱动。DLL 默认从可执行文件目录按完整路径加载，也可由 `ASTRA_WINTUN_DLL` 指定可信路径。同名设备存在时尝试打开已有 Wintun 并建立会话，成功后按配置设置地址、MTU 和 metric；设备被占用、不是 Wintun 或无法访问时返回底层错误。退出时关闭本次会话和句柄，不卸载共享驱动；打开外部创建的设备不会取得其删除所有权。

Windows 配置模板见 `configs/windows.toml`，两项模式默认关闭。启用 TUN 时 `name` 可用 `Astra`，`interface` 填实际物理网卡别名（如 `Ethernet`、`Wi-Fi`、中文别名）或接口索引。不能将 `interface` 指向 TUN 本身。托管路由使用 Windows IP Helper API 的临时路由，保留物理默认路由并使用两个 `/1` 接管路由；不接受 `/0`。已有冲突路由会拒绝，不覆盖外部条目；失败时清理本次新建项。直连 TCP/UDP 使用 `IP_UNICAST_IF` / `IPV6_UNICAST_IF` 绑定原出口，回环目的地址保持回环。

可选 `ipv6_address = "fd00:198:18::1"`、`ipv6_prefix = 126` 配置 TUN 的 IPv6 地址；Windows 托管 IPv6 路由必须有该地址。双栈时 IPv4 网关写 `gateway`，IPv6 网关写 `gateway6`，例如 `fe80::1`；链路本地 IPv6 网关的 scope 根据物理 `interface` 填入。接管 IPv6 时使用 `::/1`、`8000::/1`，同时为 IPv6 节点和 DNS 添加更具体的 bypass。只有 IPv4 配置不会自动接管 IPv6。Windows 网卡和路由管理要求管理员权限，由启动时按需提权的代理核心承担，非管理员 manager 无需重启为管理员。

先安装 bypass，再安装接管路由；只添加自己拥有的路由，遇到已有冲突路由时拒绝并回滚新添加项，不覆盖外部路由。运行前检查被接管范围内的节点 IP 是否被 bypass 覆盖，防止节点连接回到 TUN。域名节点在准备时解析并检查所有结果；DNS 本身也应走原网关，节点 DNS 记录改变后需要更新 bypass。局域网 DNS 若已有比接管路由更具体的物理接口路由，则不会被接管，不需重复添加 bypass；不要重复添加原网关已有的主机路由。直连 TCP/UDP 绑定指定物理接口，避免直连规则形成循环。未接管的 IPv6 不会因上述 IPv4 配置而自动进入 TUN，IPv6 路由与 bypass 需要另行配置匹配的网关。

关闭时删除本实例添加的路由并关闭设备。异常退出后系统通常会删除依赖设备的路由，但绕过路由可能保留；按配置中的 bypass 使用系统路由工具检查并清理，重启时遇到重复项会明确报错。不同实例使用不同设备名、地址段和不冲突的路由。

已有 TUN 设备的节点/规则切换及路由列表更新会保留协议栈和会话。修改 MTU、设备地址或更换设备名属于设备重建：旧设备的会话不能保留；修改 MTU 需要选择新设备名。把同一目标接管路由移至新设备时，旧路由仍占用会导致候选被拒绝；先热关闭旧 TUN，再开启新设备，或由外部管理路由。IP 分片重组、超 MTU UDP 的完整行为尚未验收，使用适当 MTU 和应用报文大小。

## Linux TPROXY / REDIRECT

TPROXY 使用 `IP_TRANSPARENT` / `IPV6_TRANSPARENT`。TCP 从原目的地址建立原生出站；UDP 从 `recvmsg` 的原目的地址控制消息恢复目标，并以原目的 IP/端口发送回包。REDIRECT 通过 `SO_ORIGINAL_DST` / IPv6 对应选项读取 NAT 前地址，仅接收 TCP。

```toml
[tproxy]
listen = "0.0.0.0:7893" # IPv4；IPv6 使用 [::]:7893 并配置对应策略

[redirect]
listen = "0.0.0.0:7894"
```

Astra 不会自动改防火墙。以下是网关转发入站的 IPv4 nftables 示例，`eth1` 换成 LAN 接口。先启动已启用 TPROXY 的 Astra，再设置规则；需管理员权限，表名/路由表/优先级必须与现有配置不冲突：

```sh
ip rule add priority 11077 fwmark 0xa57a/0xffffffff lookup 11077
ip route add local 0.0.0.0/0 dev lo table 11077
nft -f - <<'NFT'
table ip astra_tproxy {
  chain inbound {
    type filter hook prerouting priority mangle; policy accept;
    iifname "eth1" ip daddr { 0.0.0.0/8, 10.0.0.0/8, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 } return
    iifname "eth1" meta l4proto { tcp, udp } tproxy to :7893 meta mark set 0xa57a accept
  }
}
NFT
```

该例只接管指定 LAN 接口送入的公网 IPv4 流量，不拦截 Astra 自身新建的本机出站，因此避免自环。需要本机 OUTPUT 接管时还需单独配置回送与排除进程策略，不能直接套用此网关示例。IPv6 需使用 IPv6 规则、`ip -6 rule`、`ip -6 route ... local ::/0` 和 IPv6 listener。

撤销本示例（仅在确定这些条目是刚才自己创建时执行）：

```sh
nft delete table ip astra_tproxy
ip rule del priority 11077 fwmark 0xa57a/0xffffffff lookup 11077
ip route del local 0.0.0.0/0 dev lo table 11077
```

REDIRECT 示例：在自己管理的 NAT PREROUTING 链中将指定 LAN 的 TCP `redirect to :7894`，并排除保留网段/路由器本身；无需 TPROXY 的 mark 与 local 策略表。不要同时让同一批 TCP 匹配两种透明代理规则。

## 热重载与配置切换

代理和规则核心均支持三种入口：

1. TUI：保存运行中实例的配置或用 `c` 切换文件时自动热应用；`R` 手动重载，`r` 仍是完整重启。
2. 信号：`kill -HUP PID` 重读当前文件，失败记日志并继续使用旧配置。
3. 文件监测：顶层 `watch_config = true`，每秒检查内容变化。失败文件内容不会重复刷日志；修正内容后再次尝试。支持原子替换文件。

需要可确认结果的脚本控制时，启动时指定管理 socket（0600、父目录应仅当前用户可访问）：

```sh
ASTRA_CONTROL_SOCKET=/tmp/astra-proxy-control.sock ./target/release/astra-proxy /path/to/proxy-standalone.toml
# 另一个终端：
./target/release/astra-proxy --reload /tmp/astra-proxy-control.sock
./target/release/astra-proxy --switch /tmp/astra-proxy-control.sock /absolute/path/to/other-proxy.toml
# 规则核心同样支持 --reload / --switch，目标为其独立管理 socket。
```

先解析、校验并编译候选，再准备新增监听/设备/路由；成功后一次切换后续会话使用的配置。返回失败时保留运行中的配置；端口占用、无权限、协议参数错误不会通过先停止旧 core 来处理。`--switch` 成功后会记住新路径，后续 SIGHUP/监测针对新文件。管理状态会报告实际加载的配置路径和内容摘要，自动重载后 TUI 的 pending 状态也会更新。

已经接收的 TCP 会话和 SOCKS5 UDP association 保留其配置；后续新连接/新关联使用新配置。TUN/TPROXY 已有 UDP flow 保留原出口，新 flow 使用新配置。规则核心在每个新路由请求时读取当前规则；持久 IPC 连接无需重连。规则更改通过独立订阅通道推送版本，代理缓存按版本隔离；规则服务断连时停止使用旧版本缓存，已有数据连接继续。推送传播有短暂异步延迟，不是跨进程的同步事务。

TCP/UDP 限额跨配置版本共享；降低额度不切断已有会话，降到新额度以下才接受新会话。关闭某种接管模式或重建 TUN 设备可能结束该模式关联的流；系统代理只影响之后应用新建的连接。多个实例共用配置时，只有显式重载或启用文件监测的实例才会同步应用。

## 验证与边界

2026-10-19 已完成 Windows 实际设备与 WinINet 集成检查：系统 HTTP/HTTPS、独占接管、热开关、正常/崩溃恢复；Wintun 双栈 TCP 1..1MiB、UDP 0..1200 字节、重载及设备/测试路由清理。首轮发现并修复 TUN UDP 回包方向错误。仅使用专用测试地址路由，未验证全局路由接管或公网部署，详细记录见 [Windows 模式验证](WINDOWS_MODES_VERIFICATION.md)。

本次在 macOS 验证了配置切换、SIGHUP、文件监测、监听端口更新、解析/绑定失败回滚、旧 TCP 保留、规则持久连接更新和缓存版本通知。用户态 TUN 测试注入原始 TCP/UDP 包并通过选定 SOCKS5 出站回显，覆盖 TCP 握手以及 UDP 空包、普通包、1200 字节包；IPv4/IPv6 UDP 包编码均有检查。系统代理的设置计划与恢复顺序有隔离测试。

Linux 专用透明 socket、原目的地址解析和出口绑定代码通过 x86_64-unknown-linux-gnu 交叉编译检查。Linux 运行、系统桌面设置和防火墙端到端仍需在 Linux 目标机器验收；Windows 的本机测试不能替代 Linux 验收。

```sh
python3 scripts/reload_regression.py --bin-dir target/release
python3 scripts/standalone_regression.py --bin-dir target/release
cargo test -p astra-proxy tun::tests
```

实现参考：[tun-rs](https://github.com/tun-rs/tun-rs)、[netstack-smoltcp](https://github.com/cavivie/netstack-smoltcp)、[Linux 内核 TPROXY 文档](https://www.kernel.org/doc/html/latest/networking/tproxy.html)。

系统代理支持回环监听及通配监听：`0.0.0.0:PORT` 自动使用 `127.0.0.1:PORT` 作为系统代理目标，`[::]:PORT` 自动使用 `[::1]:PORT`。监听绑定不变；零端口及仅绑定非回环具体 IP 的配置仍会被拒绝。Windows 使用 HTTP 监听，macOS/Linux 使用 SOCKS5 监听。

出口网卡可设 `auto_interface=true` 在运行时跟随物理默认出口；`auto_gateway=true`（默认）跟随所选接口的网关。手动固定 Wi-Fi 时设 `auto_interface=false`、`interface="Wi-Fi"`；完全静态网关还需 `auto_gateway=false`。旧版写入的 interface 不再代表自动模式，需显式开启。详见 [出口切换](EGRESS_SWITCHING.md)。
