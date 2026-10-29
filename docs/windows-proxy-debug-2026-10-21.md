# Astra Proxy Windows 调试记录

日期：2026-10-21（北京时间）

## 当前结果

系统代理 HTTP/HTTPS：127.0.0.1:8080；SOCKS5：127.0.0.1:1080。
TUN：AstraTun，IPv4 分割默认路由，物理出口 Ethernet 2 / 10.42.0.1，MTU 1400。
TUN DNS：8.8.8.8、8.8.4.4；物理网卡 DNS 保持 223.6.6.6、1.1.1.1。TUN DNS 在网卡重建后由启动脚本自动补回。
系统 IPv6 地址优先级恢复为调试前的 IPv4 precedence 60 / label 4。当前不启用 IPv6 TUN 路由：节点的 IPv6 TLS 请求实测失败，启用会造成浏览器连接失败。IPv4 自动连接已验证。

## 修复

- 管理进程以当前用户的管理员权限运行，解决普通权限核心无法热启用 TUN 的问题。
- DNS 和节点服务器显式从物理出口绕行，避免 TUN 启动递归；订阅服务器加入绕行后成功更新 32 个节点。
- 默认 PROXY 组使用实测可用的 AEA-80-Direct-Vision / AJP-80-Direct-Vision 两个节点自动选择，每 30 秒检测，200ms 容差。普通测速切换保留现有连接，节点失败时断开失效连接以便应用重连。
- 其他原有节点和订阅保留，未把不可用节点放进默认使用组。
- 日志级别由 route 改为 info，减少高频逐连接日志。
- 计划任务 Astra Proxy Manager：登录后启动管理进程和 t1（t1 绑定 t2），持续恢复重建的 TUN 网卡 DNS；管理任务失败后最多重试 3 次、间隔 1 分钟。

## 验证

- 完整停止、重新启动管理进程和核心，TUN、系统代理与 DNS 恢复。
- 最终连续 10 轮：TUN 无显式代理请求 10/10 HTTP 200，HTTP 系统代理请求 10/10 HTTP 200。
- Windows 默认代理被正确识别为 127.0.0.1:8080，Windows WebRequest 访问外网 HTTP 200。
- SOCKS5 验证成功；日本出口 IP 实测 <节点A出口IP>。
- 国内网站 HTTP 200；通过 TUN 完整下载 1,048,576 字节，HTTP 200。
- 将 AJP 节点端口临时改为不可用端口后，PROXY 自动切至 AEA，HTTP 与 TUN 请求出口均为 <节点B出口IP>。恢复节点后，自动切回 AJP，健康切换未强制断开连接。临时修改和测试防火墙规则均已移除。
- 最终请求记录：%LOCALAPPDATA%\.astra\tui\stability-final-results.json。

## 文件和操作

配置实际位于 %LOCALAPPDATA%\.astra\tui\configs\t1.toml 和 t2.toml。
启动脚本：%LOCALAPPDATA%\.astra\tui\start-stable.ps1。
管理辅助脚本：%LOCALAPPDATA%\.astra\tui\manager-control.ps1。
启动日志：%LOCALAPPDATA%\.astra\tui\startup-stability.log。
修改前配置和订阅缓存使用 .before-stability-时间戳 后缀备份；instances.toml.before-stability 为注册表备份。

完全停止自动运行时，先在任务计划程序中禁用并结束 Astra Proxy Manager，再在 Astra TUI 停止实例。直接使用 TUI 正常停止 t1 时，DNS 监视循环不会自行拉起该实例。

本次验证覆盖当前网络、当前节点和重启恢复；没有重启整台 Windows，也不能保证第三方节点长期不掉线。网络出口换成 Wi-Fi 等其他网卡时，需要相应调整 tun.interface/gateway/bypass。