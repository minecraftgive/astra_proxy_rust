# UDP/STUN 与 NAT 可控测试

`nettool/stun.rs` 的单服务器 UDP 出口工具只产生独立会话观测，不能代替 NAT 检测。当前代理 UDP 只接入已验证的 SOCKS5 UDP ASSOCIATE；TCP 控制、UDP relay 与 STUN 来源均校验，控制连接与 UDP socket 由本次尝试拥有。HTTP CONNECT 及其他未验证 UDP 适配明确拒绝，不转为 DIRECT。

`nettool/nat.rs` 使用独立物理网卡选择与 `PhysicalGuard`，不继承通用出口。采样固定最多 30 秒，含解析、路由核验及重试。辅助解析在选定物理接口内进行，IPv4/IPv6 解析器可通过同接口的对应族 guard 核验。每次发送前及有效采样后复查；接口身份、源地址或路由证明失效则停止并保留部分证据。

基础映射对照只报告所测目标间一致/不同；只在同一会话、同一目标的重复映射变化时标记不稳定。高级流程遵循 [RFC 5780 第 4.3–4.5 节](https://www.rfc-editor.org/rfc/rfc5780.html)：映射测试复用同一个未连接 UDP socket，过滤测试使用全新本地端口。另一个独立 socket 预先验证双 IP/双端口及 CHANGE-REQUEST 能力，过滤测试不能借用它。过滤负结果还需基本响应和替代来源能力复查；证据不足时不附传统类型名称。实现限制每秒最多十个新事务，交易 ID、来源、报文长度和可选 FINGERPRINT 均校验。

公共预设只用于基础 Binding：Cloudflare 官方列出的 [stun.cloudflare.com:3478/UDP](https://developers.cloudflare.com/realtime/turn/) 与官方 WebRTC 示例的 [stun.l.google.com:19302](https://webrtc.org/getting-started/peer-connections)。这些页面不能证明 RFC 5780 支持，也不保证本地网络可达；自动测试不依赖它们。

## 类型识别与结果含义

客户端的 NAT 工具默认尝试类型识别；“显示原始报告”只控制结果展示，不关闭探测。需要选定物理网卡，并使用支持 RFC 5780 双 IP、双端口及 CHANGE-REQUEST 的服务。普通 STUN 预设不能保证给出类型，服务不支持、网络变化或证据不足时显示“未能确定”，不能将不同服务器看到不同端口直接当作对称型。配置多个服务时，优先使用完整声明 OTHER-ADDRESS 与 RESPONSE-ORIGIN 的基本应答，再实际验证这些能力；不会仅因第一个服务只支持基础 Binding 而跳过后续有效候选。

完整、稳定的采样才会提供传统类型；其他组合保留独立的映射/过滤行为，不强行归入锥型或对称型：

| 映射行为 | 过滤行为 | 类型结论 |
| --- | --- | --- |
| 端点无关 | 端点无关 | 完全锥型 |
| 端点无关 | 地址相关 | 受限锥型 |
| 端点无关 | 地址与端口相关 | 端口受限锥型 |
| 地址与端口相关 | 地址与端口相关 | 对称型 |
| 稳定的映射端点等于本地端点 | 任意已确认过滤行为 | 未观察到地址转换 |

“未观察到地址转换”不代表外部能任意访问本机：防火墙仍可能限制应答，过滤结果单独展示。过滤探测收到明确的 STUN 错误或无效报文达到上限时，终止类型判断，不能把协议失败当作网络过滤。上述名称描述本次所选接口与测试服务之间的观测，不代表路由器永久属性。

## 便携式本地回归

```sh
cargo test -p astra-proxy --lib nettool::nat::tests
cargo test -p astra-proxy --lib nettool::stun::tests
```

测试专用 `network_nat_fixture.rs` 启动四个真实 UDP socket，以双本机 IP/双端口响应 CHANGE-REQUEST、RESPONSE-ORIGIN 和 OTHER-ADDRESS，并模拟三种映射与三种过滤行为的九种组合。Linux 使用回环地址；不支持第二回环地址的主机使用另一个已存在的本机 IPv4 地址。请求始终送往本机地址，不访问公共 STUN。该 fixture、可放行虚拟接口的 guard 和算法测试仅编译到测试程序，不进入产品。

覆盖同端口映射、过滤新会话、替代端点预检、九种行为组合的传统类型、无地址转换但存在过滤、同目标映射不稳定、过滤阶段明确拒绝/无效报文、错误来源/事务/长度、接口失效、取消重传和 SOCKS5 控制会话释放。便携式模拟验证算法与实际 UDP 收发，不能代替操作系统 NAT 或物理网卡/TUN 验收。

## Linux 网络命名空间矩阵

`scripts/test_nat_namespaces.py` 创建三个临时命名空间（客户端、路由器、STUN 服务），只在隔离空间内设置 veth、地址、路由、SNAT/DNAT 与过滤策略。对三种映射、三种过滤执行九次 Rust 原算法。测试结束或失败均终止服务并清理命名空间。宿主机路由和防火墙表不被修改；需要 Linux root、iproute2、iptables 及 xt_recent。

```sh
# 先取得 cargo 输出的 astra-proxy lib 测试 executable 路径。
cargo test -p astra-proxy --lib --no-run
sudo python3 scripts/test_nat_namespaces.py --test-binary /absolute/path/to/astra_proxy-test-executable
```

只在脚本内运行的 ignored Rust 测试使用测试专用固定端口 guard；生产 `PhysicalGuard` 仍拒绝 veth，不因此放宽 NAT 物理网卡门槛。`scripts/nat_test_service.py` 是本地可控服务，不自动部署、不作为产品监听器发布。

2026-11-09 已在 macOS 主机通过便携式 NAT 8 项和 STUN 3 项测试，并在独立 Linux ARM64 虚拟机内实际运行网络命名空间矩阵，九种映射/过滤组合全部通过。内核翻译桶使用独立 conntrack zone，避免不同映射的替代来源应答在 DNAT 后发生反向元组冲突；过滤仍由独立策略决定。此验证覆盖真实 Linux 内核 SNAT/DNAT 与过滤，不等同于三平台物理网卡、权限、IPv6 与 TUN 开关联合验收；后者仍需对应环境实测。
