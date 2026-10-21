# Windows TUN IPv6 回环导致 Proxy Core 约 4 GiB 占用

## 现场证据

2026-10-20，用户截图显示 `astra-proxy.exe` PID 25424 占用约 4.1 GB。检查时仍是同一进程，工作集已自行回落至约 77 MiB，Windows 保留的历史峰值工作集为 4,462,682,112 字节。用户反馈 TUN 从启动时开启、曾热重载配置，当时主要观看 DIRECT 直播，升高后自行回落。

读取现有日志，香港时间 17:50–17:57：

- 对一个 `fe80::/10` 链路本地目的地址的 TCP 53 端口，出现 12,867 次 `inbound=Tun, action=Direct` 转发。
- 从 Edge 发起的最初请求之后，后续请求的进程身份变成 Proxy Core 自己（PID 25424）。检查 17:50–18:03 的窗口，日志明确记录 12,856 次 TCP、1,022 次 UDP 自进程被 TUN 再捕获。
- TCP 活动数在窗口内达到默认上限 4096；随后大量连接关闭，内存回落。另两批链路本地 UDP 流各达到 512。
- 当前 TUN 配置只有设备名，没有 `tun.interface`；Windows 为该适配器自动配置了 IPv6 链路本地地址和 `fe80::/64` 路由，即使未显式配置 `tun.ipv6_address` 也会出现。

这组日志明确表明自身连接被重新代理。没有证据把此次峰值归因于直播码率或 GEOIP；也没有直接证明某次热重载触发了最初请求。

## 原因

TUN IP 包不包含 IPv6 zone/scope ID。旧逻辑将目的地址转为不带 scope 的字符串，在没有物理出口约束时再创建 DIRECT socket；系统路由再次选择 TUN，形成：

```text
应用 → TUN → DIRECT socket → TUN → DIRECT socket → … → 连接额度耗尽
```

原有进程归属查询已经识别出 Proxy Core 自己，但路由代码没有拒绝。独立模式还会跳过身份查询。

`netstack-smoltcp 0.2.4` 的 `tcp.rs` 为每条新 TCP 流创建两块 smoltcp socket buffer 和两块适配层 ring buffer。当前各为 256 KiB：

```text
4096 × 4 × 256 KiB = 4 GiB
```

这是缓冲容量推算，与现场峰值量级吻合，不是逐分配调用栈测量。应用层许可检查发生在底层创建 TCP 流之后；因此仅看 `max_connections` 不能把它视作协议栈分配前的硬内存限制。

## 修复

最新正常路径为 **核心创建出站 socket → 绑定物理出口 → 连接原目标或代理服务器**。下面的入口拒绝仅是异常回环兜底，不是正常流量的处理方式。

- Windows 优先使用显式 `tun.interface`（名称或索引）；未指定时，从默认路由中选择工作中的硬件网卡，排除虚拟适配器和 TUN，按路由与接口 metric 之和排序，优先 IPv4 默认路由、其次 IPv6。此机器实测选中 `Ethernet 2`，索引 28。
- 出口安装到 `meow_common::outbound_iface`，覆盖原生协议的 TCP/UDP 连接；普通 SOCKS/HTTP/TLS 连接也统一使用受保护拨号，TUIC 使用已绑定的 UDP socket 创建 QUIC endpoint。DIRECT TCP/UDP 使用当前代数的明确接口，并恢复链路本地 IPv6 scope。
- 订阅下载及嵌套 HTTP provider 使用 Proxy Core 的 DIRECT 拨号，保留 TLS 校验、最多三次重定向、20 秒时限及 8 MiB 响应上限。转换器独立运行时仍用原下载器。
- 早期版本在 `tun.interface` 缺失时自动识别，并在配置提交时补写该字段；使用保留格式的 TOML 编辑器，原注释和其他字段不变。已有明确指定的接口不覆盖。保存前检查文件是否被修改，使用临时文件原子替换，并迁移匹配的订阅缓存，保持离线重启可用；保存失败会明确记录警告。接口绑定有所有权，准备失败释放新 owner 后恢复旧 owner；已建立 socket 保留原绑定。禁用 TUN 后撤销全局绑定。
- 不再强制要求每个代理服务器 IP 都写入 `tun.bypass`；受保护的出站 socket 已绑定出口。显式 bypass 路由仍可使用。

当前采用单一出口，不自动处理 IPv4/IPv6 分别位于不同物理网卡的情况。早期版本保存后按明确接口使用；当前版本已增加定期检测、自动网卡/网关选择及 Windows 路由租约切换，检测结果改写独立缓存，详见 [出口切换](EGRESS_SWITCHING.md)。找不到可用出口时拒绝启动/重载该 TUN 配置。操作系统 DNS 解析服务的 socket 不属于本进程的套接字绑定范围。

可在不启动 TUN、不改变系统路由的情况下执行 `astra-proxy.exe --configure-egress <配置路径>`，自动识别并补写接口。该命令也保留已有接口，不会覆盖用户选择。

1. 在 TUN 包进入协议栈前，拒绝缺少明确物理出口的 IPv6 链路本地目的流量。TCP、UDP 共用这层检查，避免先分配大缓冲再拒绝。首次拒绝记录提示，避免逐包刷日志。
2. 读取任务逐包查看当前配置代数中的出口设置，兼容复用同一设备、同一读取任务的热重载。
3. TUN 路由拒绝进程 ID 等于当前核心的流量；独立模式也执行 TUN 进程归属查询。显式 SOCKS 使用不受此规则影响。
4. 配置物理出口时，为 DIRECT TCP/UDP 的 IPv6 链路本地目的地址填写该接口的 scope ID，并绑定出口。没有出口时在拨号前返回明确错误，覆盖入口检查与配置切换之间的时间窗口。
5. 后续增加入口级自身排除：在原始 TUN 包进入 smoltcp 前，按连接归属识别本进程 PID 并丢弃。TCP 检查新建连接的 SYN，UDP 检查数据报；覆盖 IPv4/IPv6，独立于 Rule Core 规则与配置代数。Windows 直接查询 owner PID，省去进程路径查询。

入口级排除针对已被系统路由送回 TUN 的自身包，不会把这些包重新投递到物理网卡；正常出站仍依靠物理接口绑定与 bypass 路由。UDP 的非自身/归属未知结果缓存 500 ms、最多 4096 项；自身匹配不缓存，避免旧端口被其他进程复用后遭到误丢弃。归属查询与原有查询共享四个阻塞工作额度、250 ms 总时限；无法确认时继续由后续路由检查兜底。短时 UDP 端口复用也可能暂时命中非自身缓存，因此入口检查不取代后续检查。当前进程归属实现覆盖 Windows/Linux。

实现文件：

- `crates/astra-proxy/src/platform/tun_ingress.rs`
- `crates/astra-proxy/src/platform/tun.rs`
- `crates/astra-proxy/src/platform/egress.rs`
- `crates/astra-proxy/src/proxy/udp.rs`
- `crates/astra-proxy/src/lib.rs`

需要访问物理 LAN 上链路本地地址时，应设置 `tun.interface` 为真实物理出口。未配置出口时普通 IPv4、全局 IPv6、ULA 和回环目的地址继续使用原有路由；链路本地目的地址会被拒绝。

## 复现与验证范围

- 先新增“自身 TUN 连接必须拒绝”的回归测试，旧路由逻辑实际失败：`recaptured Tcp must not open another outbound`。修复后通过，同时覆盖 TCP/UDP、独立模式、其他进程和显式 SOCKS 使用。
- Windows 真实 loopback TCP socket 的进程归属查询验证通过，确认独立模式不会跳过自进程识别。
- 对真实内存内 smoltcp 栈回放 32 个不同源端口的链路本地 DNS SYN；旧入口路径创建 32 条 TCP 流，使用生产入口过滤函数后创建 0 条。该受控复现不需要管理员、不创建系统 TUN、不访问外网，也没有故意把内存推到 4 GiB。
- 测试覆盖物理接口 scope 填充、无接口拒绝、普通 loopback DIRECT、全局 IPv6/ULA 放行、已有原始 TUN TCP 握手及 UDP 回包。
- `cargo test -p astra-proxy -p astra-protocols -p astra-convert` 全部通过；后续新增的订阅下载/嵌套 provider 测试也单独通过。
- 独立进程的 `outbound_binding` 集成测试读取 Windows socket 选项，验证 IPv4/IPv6 UDP 绑定、loopback 豁免以及 owner 回退。在本机物理地址上完成 TCP 连接，确认 TCP/UDP 都绑定 `Ethernet 2`（28）；这不等同于 Wintun 全链路或外网连通测试。
- Windows `IP_UNICAST_IF` 设置使用网络字节序，读取使用主机字节序，参见 [Microsoft 文档](https://learn.microsoft.com/en-us/windows/win32/winsock/ipproto-ip-socket-options)。
- `scripts/process_routing_regression.py`：PASS，覆盖进程归属、TCP/UDP、DIRECT 回退及重载。
- `scripts/subscription_regression.py`：PASS，覆盖定时替换、失败更新保留旧配置、离线缓存重启及核心负责的 URL 引导下载。
- 当前配置 `t1.toml` 的新 release `--check` 通过。TUIC 单元测试和构建通过，跨程序 TUIC 回归未完成：测试所需 OpenSSL 不在默认 PATH，现存 sing-box 测试夹具为 Mach-O，无法在 Windows 执行。

本次未在管理员权限下重新创建真实 Wintun 适配器，也未重启现场正在使用的核心。物理 LAN 链路本地 DNS 的真实联通仍需要带正确出口接口的运行环境验证。此修复阻断已确认的自回环；普通大并发下的缓冲预算、底层 SYN 去重和分配前总量限制仍是独立的后续优化项。

## 构建

Windows release 产物：`target/x86_64-pc-windows-msvc/release/astra-proxy.exe`。

产物在物理出口绑定及订阅下载改造完成后重新编译。

```powershell
cargo test -p astra-proxy --lib
cargo build --release --target x86_64-pc-windows-msvc -p astra-proxy
```

本机执行时使用 `scripts/build.py` 的 `build_environment()` 配置 NASM、libclang 等本地依赖。构建仍会输出 vendored `meow-proxy` 原有的两条 BoringSSL 弃用警告。

运行中的进程只有在使用新程序重启后才获得修复；单独热重载 TOML 无法替换进程代码。
