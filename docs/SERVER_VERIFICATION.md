# 服务端 Windows 验证记录

环境：Windows AMD64，Rust 1.99 MSVC，原生开发构建。运行中的协议核心均为 Astra 自身程序；未使用外部代理核心。服务端核心、服务端 TUI 与客户端分别运行。

## 已通过

- 19 组本地客户端/服务端互通：SOCKS5、HTTP CONNECT、SS 三种 AEAD、SS2022 三种密码套件、VMess AES/ChaCha、VLESS、Trojan、Hysteria2、TUIC native/quic，以及 VLESS 的 WS、HTTP Upgrade、gRPC、XHTTP stream-one。
- 每组 TCP 逐项回传 1、4096、65536、1048576 字节；除 HTTP 外，各组 UDP 回传 0、1、64、1200、4096、8192 字节，覆盖 IPv4、IPv6、域名目标。
- 同样 19 组错误凭据拒绝，9 组默认私网目的地址拦截，合计 47 组协议/策略检查。
- 6 项核心测试：配置认证约束、地址策略（包括 IPv4 映射 IPv6）、共享额度释放、QUIC 分片乱序与空数据报、分片冲突/长度上限、跨配置版本 VMess 请求重放拒绝。
- 启动第二个监听失败时撤回第一个监听并删除控制端点；认证热重载立即作用于新连接并保留既有流；无效重载保留旧配置；停止关闭会话并清理控制端点。
- TUI 在实际终端中完成初始化、配置校验、启动、显示状态；退出 TUI 后核心保持运行，并通过独立控制命令停止。
- 早期含文档包曾验证文件哈希及依赖许可；现改为下方所述精简运行包。

复现入口：`scripts/server_regression.py`、`scripts/server_lifecycle.py`、`scripts/server_package_regression.py`。详细命令见 SERVER.md。

## 范围

当前交付为 Windows 原生开发构建预览包，尚无优化构建的吞吐基线、长时间负载和公网部署验收；Linux/macOS 服务端运行需在对应系统补验。

服务器扩展已继续实现并单独验证，详细记录见下方扩展验收；客户端现有的组合限制保持不变。配置角色与字段说明见 SERVER_EXTENSIONS.md。

本记录的服务端验收没有启用 Windows TUN 或系统代理；之后单独进行的客户端验证见 [Windows 模式验证](WINDOWS_MODES_VERIFICATION.md)。

## 2026-10-19 扩展与精简包验证

- 33 组扩展互通通过：REALITY、Vision、三类 sing-mux、Mux.Cool/XUDP、VLESS 解密及混合密钥/恢复、三类 XHTTP 上传、Salamander，以及叠加组合；同样 33 组错误用户凭据拒绝通过。
- 4 类错误扩展参数拒绝：REALITY 公钥、short ID、VLESS 解密公钥和 Salamander 密码。
- Mux 每组运行 96 条子流（分三批并发），覆盖流回收；Vision 内层 TLS1.3 回传 2MiB，并检查 TCP 双向 DIRECT。REALITY 普通 TLS 伪装回落通过。
- 9 项核心单元检查通过，新增 REALITY 身份/时间/重放及截断输入、微小读取缓冲的 TLS 记录边界、带填充 smux。19 组基础协议回归再次通过。
- 最后构建再次验证 X25519、ML-KEM、混合解密与 Vision+REALITY+解密组合；客户端独立构建检查通过。
- client/server 分开打包：每包仅对应执行文件和一份完整配置；客户端可附运行用 Wintun DLL。归档精确文件集合、解压后配置解析、原生进程运行/停止及回声转发验证由 package_regression.py / server_package_regression.py 执行。规则完整模板作为客户端配置注释附录校验。
- 未启用 Windows TUN 或系统代理；服务端组合测试使用原生 Astra 客户端作为互通对端，不代表第三方所有组合都已验证。
