# 模块边界与依赖方向

所有操作系统使用相同的客户端和服务器入口、路由、认证与协议代码。平台能力（例如 Linux 透明代理、Windows 系统代理）通过适配层提供，不复制一套平台专属协议核心。

```text
astra-tui / astra-server-tui                  管理界面
astra-proxy / astra-server-core              客户端 / 服务器应用库与薄启动入口
  src/proxy/                                生命周期、路由、连接、配额与转发
  src/inbound/（客户端）                      本地入口与路由协调
  src/protocols/（服务器）                    协议扩展与应用会话协调
  src/transports/（服务器）                   入站传输、安全与 XHTTP 会话
                 ↓
astra-protocols                              可供两端复用的协议库
  socks5/  http/  trojan/  vless/             独立协议：请求、握手、认证
  vmess.rs  shadowsocks.rs                   独立协议适配器 / 入站支持
  tuic.rs  hysteria2.rs                      QUIC 协议认证、请求与报文处理
  native.rs / outbound.rs                    配置编译、适配器注册与出站分发
  wire.rs / datagram.rs                      公共地址编码、QUIC 分片与资源限制
  xhttp.rs                                  客户端 XHTTP 传输
                 ↓
astra-ipc                                   两端共享的数据类型、IPC 与平台服务

astra-proxy/src/platform/                    TUN、路由、接口绑定、系统代理、透明代理
astra-ipc/src/platform.rs                    本地 IPC、权限、进程启动、信号与文件身份
```

## 协议层

`astra-protocols` 不依赖客户端或服务器 crate，也不包含 OS 条件分支。客户端和服务器均直接依赖它。协议库中的 `accept` 接收流和凭据，返回目标或请求类型；路由策略、超时、连接配额、热更新及流量统计由应用层负责。客户端的无认证本地 SOCKS5 入口和服务器的强制认证入口是不同策略，不应为了合并代码而放松服务器认证。

VMess、Shadowsocks、VLESS 加密等底层密码学继续使用已有的 `meow-proxy` / `shadowsocks` 实现。协议模块负责装配这些实现，不另写第二套密码学。TUIC 两端复用报文解码和分片编码；SOCKS/Trojan/VLESS/TUIC 地址使用公共 wire 编码。

服务器的 `protocols/` 是依赖服务器会话上下文的协调代码，例如把解码后的 UDP 包送入受配额约束的转发队列；这些模块不应被复制到客户端。可复用的格式解析应继续下沉到 `astra-protocols`。服务器端 Reality、Vision、Mux 和传输会话仍由服务器库组合，不能把客户端与服务器的不同角色误认为操作系统差异。

## 平台层

系统代理、TUN/路由、透明代理和网卡绑定归 `astra-proxy/src/platform/`；本地 IPC、进程标志、信号及文件权限归 `astra-ipc::platform`。业务代码调用统一接口。普通 HTTP 转发现在在所有平台启用，不再以 Windows 为条件编译。

平台特有的功能限制由适配层校验并返回明确错误。平台相关测试可以保留条件编译；协议代码不可引入 `std::os`、Windows API 或 OS 条件编译。

## 修改与验证

1. 新增或修改协议时，先修改对应的 `astra-protocols` 模块，再在分发层接线；不要在入口或路由循环中内嵌握手。
2. 改变平台行为时，修改 `platform`，不要复制客户端或服务器入口。
3. `python scripts/check_architecture.py` 检查共享库的依赖方向、薄入口与 OS 边界。
4. `cargo test --workspace` 运行原有测试及共享协议互通测试；Windows 使用 `scripts/build.py` 的构建环境以找到 NASM / libclang。
5. 实际的 Linux/macOS 系统代理、TUN、路由及权限行为需要对应平台的运行验证；Windows 编译和测试不能替代这些验证。

现有程序名称、配置字段及控制接口保持兼容。源文件路径已调整，维护脚本应按 Cargo package 定位构建目标，不应依赖旧的源文件布局。
