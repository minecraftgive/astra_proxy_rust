# 系统接管模式与配置热重载

运行本次构建的 `target/release/astra-proxy`、`astra-rule` 和 `astra-tui`。管理与路由 IPC 已更新为 v2，旧程序与新程序不能混用。后台 TUI 管理服务也需在更新后重新启动；停止已有实例会断开其连接，请在合适的时间操作。

## 选择接管方式

| 方式 | 平台 | 流量范围 | 要求 |
|---|---|---|---|
| SOCKS5 / HTTP CONNECT | Linux/macOS | 应用显式发送给监听端口的流量 | 无管理员权限要求 |
| `[system_proxy]` | macOS、Linux GNOME | 遵循桌面系统代理的应用 | macOS 网络服务写权限；GNOME 用户 D-Bus 会话 |
| `[tun]` | Linux/macOS | 路由进入虚拟网卡的 TCP/UDP | root / Linux CAP_NET_ADMIN |
| `[tproxy]` | Linux | 策略路由与 TPROXY 送入的 TCP/UDP | CAP_NET_ADMIN 或 CAP_NET_RAW、内核 TPROXY、nftables/iptables |
| `[redirect]` | Linux | NAT REDIRECT 送入的 TCP | 外部 NAT 规则；UDP 使用 TPROXY |

这些模式可并用；配置表存在即启用，删除整张表即可关闭。TPROXY/REDIRECT 在 macOS 的配置检查阶段就会被拒绝。普通监听、原生出站和 standalone 保持原有语义。

完整字段见 `example_proxy.toml`，精简模板见 `configs/modes.toml`。

## 系统代理

代理配置顶层开启 SOCKS5 监听，然后添加表：

```toml
socks5_listen = "127.0.0.1:1080"

[system_proxy]
services = ["Wi-Fi"] # macOS；省略或 [] 自动发现启用的网络服务
```

Linux GNOME 使用空表 `[system_proxy]`，不要填写 macOS 的服务名。服务运行在当前桌面用户的 D-Bus 会话中；无 GNOME 会话的 Linux 服务器使用 TUN/TPROXY。

系统集成使用 SOCKS。现有 HTTP 入站仅支持 CONNECT，不会将它错误注册为支持普通 HTTP 请求的系统 HTTP 代理。启用时临时关闭优先级更高的 HTTP/HTTPS/PAC 自动设置，GNOME 清空 HTTP/HTTPS/FTP 代理 host；原值全部记录后才写入，正常停止或关闭此模式时恢复。系统代理仍由应用决定是否遵循，不能强制接管忽略代理的程序。已有需要认证的 macOS SOCKS 设置不能完整读回密码，因此会拒绝启用并保留原设置。

写设置前创建 0600 恢复文件：有 `ASTRA_CONTROL_SOCKET` 时在该 socket 同目录、同文件名但扩展名为 `.system-proxy-state.toml`；否则在初始代理配置旁，例如 `configs/modes.system-proxy-state.toml`。存在未恢复文件时拒绝覆盖。SIGKILL 或掉电后，在确认旧 core 已停止后恢复：

```sh
./target/release/astra-proxy --restore-system-proxy /absolute/path/to/modes.system-proxy-state.toml
```

恢复失败会保留文件供重试。正常退出后文件删除。不要删除尚未恢复的文件。

## 原生 TUN

TUN 设备由 `tun-rs` 创建，数据包由 Rust `netstack-smoltcp` 拆成 TCP 流和 UDP 会话，继续使用 Astra 的原生协议出站；不启动外部代理核心。TCP/UDP IPv4/IPv6 协议栈可用，DNS 作为普通 UDP/TCP 转发，没有 Fake-IP、域名嗅探或 DNS 劫持；IP 流量按 IP/端口规则匹配。ICMP 由本地用户态栈响应，不代表远端目标的真实 ping 结果。

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

进程需以具备建卡/改路由权限的身份运行；TUI 不会自动提权，也不会读取管理员密码。建议由系统服务管理权限，TUI 使用对应管理服务进行操作。

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

本次在 macOS 验证了配置切换、SIGHUP、文件监测、监听端口更新、解析/绑定失败回滚、旧 TCP 保留、规则持久连接更新和缓存版本通知。用户态 TUN 测试注入原始 TCP/UDP 包并通过选定 SOCKS5 出站回显，覆盖 TCP 握手以及 UDP 空包、普通包、1200 字节包；IPv4/IPv6 UDP 包编码均有检查。系统代理的设置计划与恢复顺序有隔离测试。

Linux 专用透明 socket、原目的地址解析和出口绑定代码通过 x86_64-unknown-linux-gnu 交叉编译检查。当前主机没有 Linux 内核环境，也未改动用户的系统代理、TUN 或系统路由；管理员权限下的设备/桌面设置及 Linux 防火墙端到端仍需在目标机器验收，不能视为已经实测。

```sh
python3 scripts/reload_regression.py --bin-dir target/release
python3 scripts/standalone_regression.py --bin-dir target/release
cargo test -p astra-proxy tun::tests
```

实现参考：[tun-rs](https://github.com/tun-rs/tun-rs)、[netstack-smoltcp](https://github.com/cavivie/netstack-smoltcp)、[Linux 内核 TPROXY 文档](https://www.kernel.org/doc/html/latest/networking/tproxy.html)。
