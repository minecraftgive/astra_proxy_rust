# 出口网卡检测与切换

## 配置

在 TUI 编辑器的 TUN 设置中，“出口网卡（物理或虚拟）”列出本机网卡，可输入名称筛选、用 ↑↓ 选择并按 Enter 确认。每次打开选择框都会刷新列表，包含断开的网卡和虚拟网卡，排除回环及配置中的 TUN 自身接口。选择具体网卡会关闭自动网卡选择；选择“自动选择”会恢复自动跟随，并清除手动接口。自动网关仍可单独设置。

```toml
[tun]
auto_interface = true
auto_gateway = true
# 其他 TUN 地址、接管路由、bypass 等设置保持按实际网络配置。
```

自动网卡模式忽略 interface 中的旧提示，每两秒检查物理默认出口，连续两次观察到相同变化后
重新准备出口。Windows 选择工作中的硬件网卡，排除 TUN/虚拟网卡，按默认路由与接口 metric
之和选择；优先 IPv4 默认出口，IPv6-only 网络可选择 IPv6 默认出口。auto_gateway=true
从同一网卡读取 IPv4/IPv6 默认网关，覆盖有效运行配置中的 gateway/gateway6。

手动固定 Wi-Fi，但自动跟随它的 DHCP 网关：

```toml
[tun]
auto_interface = false
interface = "Wi-Fi" # Windows 支持接口别名或索引
auto_gateway = true
```

完全手动设置出口和网关：

```toml
[tun]
auto_interface = false
interface = "Wi-Fi"
auto_gateway = false
gateway = "192.168.1.1" # 替换为该网卡实际网关
# gateway6 = "fe80::1"
```

省略 auto_interface 时，没有 interface 表示自动；已有 interface 表示手动，兼容旧配置。
因此旧版自动识别后曾补写 interface 的文件，需要显式设置 auto_interface=true 才会重新跟随
网络。auto_gateway 默认 true；使用自定义静态网关时显式关闭它。

自动检测不依赖 watch_config。网络临时断开或新出口准备失败时保留当前配置并继续重试，
相同错误不会每两秒重复刷屏。成功切换后关闭旧出口上的 TCP/UDP 会话，让应用重连；
已建立的 TCP 连接不能透明迁移到另一网卡。

## 每个代理组绑定不同网卡

代理组也有“出口网卡（物理或虚拟）”选项。可分别选择 Wi-Fi、以太网、VPN 或其他本机虚拟网卡。留空表示沿用外层组的绑定；整条选择路径都未指定时沿用 TUN 默认出口，未启用 TUN 时由系统路由决定。

```toml
[proxy_groups.wifi]
members = ["node-a"]
interface = "Wi-Fi"

[proxy_groups.wired]
members = ["node-a"]
interface = "Ethernet"
```

同一个节点可以在不同组中使用不同网卡。TCP、UDP 及组测速均按组绑定；复用连接和 QUIC 会话按网卡隔离。嵌套组采用选择路径上第一个明确设置的网卡，即外层明确绑定优先；外层未绑定时可使用子组绑定。绑定也适用于组内 DIRECT。所选网卡不存在或无法绑定时连接失败，不自动回退到其他网卡；配置中的网卡名称应与本机列表一致。

更换组的网卡并保存、重载后，新连接使用新网卡。已有连接保留原出口；开启组的“成员或模式变更时断开”选项后，网卡变更也会关闭该组旧连接。

## Windows 路由所有权

修复了接口切换时报“prefix ... already exists on interface ...”的情况：旧配置的路由还活着，
此前被统一当作外部路由拒绝。现在每条本进程创建的 Windows 路由持有共享租约，比较完整的
前缀、接口索引、下一跳/IPv6 scope、metric 和协议来识别所有权。

切换可先创建新出口的绕行路由，提交后释放旧会话及其租约。Ethernet → Wi-Fi → Ethernet
可复用尚存的同一条路由，最后一份租约释放时才删除。准备失败仅回收候选的新资源。
其他进程创建的路由、外部修改的路由仍会拒绝，不清除或接管它们。

第一次使用修复版时先正常停止旧版实例，再启动新版，以便旧版释放其路由。
旧进程异常退出遗留的路由、其他代理实例的路由不自动认领；报错会区分“不属于本实例或已被外部修改”。

## 缓存与平台

检测结果写到独立运行目录的 `~/.astra/runtime/egress/<源路径摘要>.cache`（可通过 `ASTRA_RUNTIME_DIR` 自定义根目录），临时文件写完并同步后原子替换。
缓存只包含接口和网关，不包含订阅或认证信息；它是诊断快照，不用于认领路由或在断网时
强行采用过期网关。自动模式不再把检测结果补写成配置里的手动 interface。

检测策略在共享层，网卡和路由 API 在平台适配层。Windows 使用 IP Helper，Linux 使用
iproute2 的默认路由/物理设备信息，macOS 使用 route 默认网关查询；当前自动/手动切换的
完整路由租约修复及发布验证针对 Windows。Linux/macOS 的同前缀路由迁移仍依赖各自的
路由命令语义，不应把 Windows 的验证结果视为其他系统的验收。

## 验证

使用模拟路由表验证已持有的 /32 和 IPv6 /128、跨接口共存、外部下一跳/metric 修改及
外部同前缀路由拒绝；测试不会添加系统路由。回归还覆盖检测防抖、自动/手动选择、原配置
不被缓存覆盖、原子缓存更新，以及旧 TCP 会话结束后同一监听器接受新会话。
