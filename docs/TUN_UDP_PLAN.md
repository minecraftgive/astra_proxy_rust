# TUN、UDP 与复杂协议的下一阶段设计（尚未实现）

当前 `astra-dual-core` 的可运行数据路径只有 **TCP SOCKS5 / HTTP CONNECT**。本文件说明怎样把 TUN 和 UDP 接入已经定义好的 Proxy/Rule Core，而不是虚称已支持。

## 1. TUN 模块分层

```text
Linux /dev/net/tun, multiqueue (tun-rs)
       |
   TunIngress (MTU, checksums, GSO/GRO)
       |
  Userspace TCP/IP stack (smoltcp / tun2socks adaptor)
       |                               |
  AcceptedTcpStream                  UdpDatagram
       |                               |
  NewFlow(Tcp)                    NewFlow(Udp)
       +-------------+-----------------+
                     |
               Route / Cache
                     |
                Outbound
```

- TUN 只产出 IP 包，不会自动生成 `TcpStream`。必须使用真正的用户态 TCP 栈，完成握手、重传、窗口管理、FIN/RST、乱序重组、IPv4/IPv6 等。
- 建议先评估 `ts_netstack_smoltcp` 0.6.x、`netstack-smoltcp` 和 `smoltcp` 0.14+；适配到独立 `proxy-netstack` crate，不能把其中一种绑定死到协议插件。
- 在 Linux 上再评估 `tun-rs` 的多队列、GSO/GRO，macOS 使用 utun；Windows 需要 Wintun/其他兼容层。
- 不应直接拿到目的 IP 后 `TcpStream::connect` 便丢弃 TUN 的 TCP 状态，那不会处理本机 TCP 连接。

## 2. Flow API 建议

```rust
pub enum FlowTransport { Tcp, Udp }
pub struct Flow {
    pub network: FlowTransport,
    pub original_destination: std::net::SocketAddr,
    pub domain_hint: Option<String>,
    pub inbound_id: u32,
}
```

Netstack 应该将流转换成这个接口；Rule Core IPC 延伸 `source_ip`、`dest_ip`、`domain_hint`、`ingress_mark`，每条流只查询一次（DNS 属性变化时更新）。

## 3. UDP 关联与回包

- Key: `(inbound id, src ip, src port, dst ip, dst port)`；每个 NAT 会话拥有确定的出口与回包映射。
- 分离 idle timeout（例如 DNS 短、QUIC 长）、最大会话数、LRU 回收；不要使用永久不清理的 HashMap。
- SOCKS5 UDP ASSOCIATE 需要 RFC1928 UDP 头解析、可选 UDP FRAG 处理和 TCP 控制连接生命周期。
- 直连 UDP 可以用 Tokio `UdpSocket`，Trojan/VLESS/HY2/TUIC 的 UDP 必须实现各自封装和复用语义，不能直接把数据发给远端 UDP 端口。
- IPv6、ICMP 错误、路径 MTU、数据报最大长度必须测试。

## 4. DNS 与规则归属

TUN 常只能看到目的 IP，因此 `domain_suffix` 规则不会天然命中。建议引入独立的 DNS 元数据接口：

- Rule Core 或其受控 DNS 子模块维护 `FakeIP <-> Domain` 的 TTL 映射。
- Proxy Core 在 TUN 建立新 flow 时，仅查询一次映射，并将 `domain_hint` 传入 Rule Core。
- 本地 DNS 解析、Fake-IP 映射、IP 规则匹配的优先级要明确；不允许在缺少域名映射时悄悄猜测。
- TLS SNI sniff 可以作为后备，但 ECH 会隐藏部分域名。QUIC SNI 亦不保证可见。
- Rule Core 改规则时通过 IPC 推送 policy epoch，使 Proxy Core 的缓存立即失效（现有 TCP 连接是否重拨必须单独定义）。

## 5. Linux / OpenWrt 回环问题

- 区分 **被代理的流量** 与 **代理核心自己发出的服务器连接**。
- 通过 `SO_MARK` / fwmark 策略路由、路由表与 nftables 排除 Proxy Core 出站连接；TUN 路由不可再次捕获自己的上游连接。
- 提前处理 RFC1918、本机、网关、局域网、multicast、IPv6 ULA/link-local；使用显式规则，而非默认全部重定向。
- 当 Rule Core 失联，默认拒绝新连接，不回退到 Direct。
- OpenWrt 上先用命名空间和测试网段验证，然后才启用全局 route，防止 SSH 断连和 DNS 自循环。

## 6. 复杂协议推荐次序

1. Direct TCP/UDP + SOCKS5 UDP，建立基本测试场景。
2. Shadowsocks TCP/UDP（AEAD 方法、重放保护、密钥更新等必须符合对应协议）；Trojan UDP。
3. VLESS TCP、VLESS UDP、基础 TLS，然后独立实现 REALITY / Vision 互操作测试。
4. Hysteria2/TUIC：分别在 Quinn 上实现对应会话管理、报文格式、拥塞控制、认证，不是调用 Quinn 就完成协议。
5. Multiqueue TUN、GSO/GRO、缓冲池、低分配路径，只有在 profiler 显示瓶颈后再调整。

## 7. 如何验收

- `cargo test` + `cargo clippy --workspace -- -D warnings` + Rust fuzz（SOCKS5/HTTP/IPC/TUN 包解析）
- 单连接、1k/10k 并发、连接 churn、长连接 24h、TCP half-close、断连恢复
- 真实 Xray/Trojan/sing-box 服务端互操作矩阵（加密、证书、IPv6、错误响应）
- 对照 Mihomo / sing-box：同硬件、同协议/加密、相同连接数，报告吞吐、CPU/GB、RSS、P50/P99，避免只报最高速度
