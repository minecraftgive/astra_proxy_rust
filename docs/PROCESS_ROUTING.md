# 系统级进程出口的原生验证

`astra-process-routing` 提供目标主机上的进程枚举、连接归属查询和平台接管原语。管理协议 v2 的 `running_processes_v1` 能力支持 `running_processes` 查询。可执行路径和 PID 来自收到查询的 Agent。搜索先过滤再截取最多 128 项，响应的 `truncated` 表示还有匹配项。

现有 `process_path`、`process_name` 继续用于已经进入核心的连接。系统接管需要显式安装接管策略。由系统 DNS 服务代发的请求归属于该服务。当前 Agent 仅提供进程发现，实例绑定表、跨端编辑及自动启用仍待接入。

## Linux

在可使用 root 的隔离 Linux VM 内运行以下命令。需要 cgroup v2、内核 BTF、支持 `bpf_loop` 和 socket storage 的 eBPF 内核、nftables 的 socket/TPROXY 支持，以及 clang、libbpf 开发头文件、iproute2、util-linux、Python 3 和 Rust。

```sh
python3 scripts/process_capture_linux_regression.py
```

脚本以普通用户构建真实核心和 CO-RE 对象，只对独立网络命名空间内的原生测试调用 sudo。可用 `--bpf-cflags=-I/path/to/include` 指定已安装的 libbpf 头文件位置。

测试把两个不同路径下名为 `client` 的程序放入同一测试 cgroup。IPv4/IPv6 回显验证 TCP、connected UDP 和一个 socket 向多个目的地发送 UDP 时的原始地址和回包。第二个网络命名空间通过两对 veth 接入，以服务端看到的源地址区分出口。测试覆盖切换为 BLOCK、保持已有 socket、网卡断开和核心退出。

`Capture::attach` 在四个 hook 全部加载成功后返回，释放句柄会移除其链接。`Capture::set` 接收规范绝对路径，可以先登记尚未创建的文件。路径身份包括可见根文件系统的设备号和根目录 inode。共享主机根的 private mount namespace 同样参与匹配。通过 `exclude_forwarders` 登记专用子 cgroup 后，放在其中的核心及其转发 worker 可正常拨号。非零 mark 对应调用者已准备好的透明监听器、策略路由和缺少监听器时的丢弃规则。测试脚本负责搭建这些设施。

## macOS

最低目标系统为 macOS 15。先构建宿主和 Network Extension。

```sh
python3 scripts/build_process_routing_macos.py --output target/capture/AstraProcessRouting.app
cargo build -p astra-proxy
python3 scripts/process_capture_channel_regression.py --bin-dir target/aarch64-apple-darwin/debug
```

最后一项在用户态用扩展实际使用的 Swift 通道连接 Rust 核心，使用项目的测试用 Xray 验证代理组选路。Intel Mac 的核心目录为 `target/x86_64-apple-darwin/debug`。Xray 测试依赖由 `scripts/install_test_servers.py` 提供。

系统测试需要 macOS 测试团队为宿主和扩展分别提供 provisioning profile。宿主需要系统扩展安装权限，两者需要 Network Extension 权限以及相匹配的应用组。构建器从扩展到宿主依次签名并执行 `codesign --verify --strict`。以下变量对应测试团队已配置的本地资源。

```sh
python3 scripts/build_process_routing_macos.py \
  --output target/signed/AstraProcessRouting.app --bundle-id "$ASTRA_TEST_BUNDLE" \
  --team "$ASTRA_TEST_TEAM" --sign "$ASTRA_TEST_IDENTITY" \
  --host-profile "$ASTRA_HOST_PROFILE" --extension-profile "$ASTRA_EXTENSION_PROFILE"
```

将已签名的测试宿主安装到 `/Applications` 后运行其 `Contents/MacOS/AstraProcessRouting activate` 并在系统设置中批准。为原生验收准备两个端口，同时提供 IPv4/IPv6 的 TCP、UDP 回显。使用独立的测试 bundle ID，其下应无现存代理配置。

```sh
python3 scripts/process_capture_macos_regression.py \
  --host /Applications/AstraProcessRouting.app/Contents/MacOS/AstraProcessRouting \
  --core target/aarch64-apple-darwin/debug/astra-proxy \
  --ipv4 "$ECHO_V4" --ipv6 "$ECHO_V6" --ports 31441 31442
```

宿主的 `apply policy.json` 接受完整策略列表，以扩展确认作为成功条件。各实例的核心可以通过设置 `ASTRA_PROCESS_INGRESS` 在私有目录创建数据 socket。示例中的 `socket` 需要与相应实例一致。

```json
[
  {"path":"/Applications/Example.app/Contents/MacOS/Example","socket":"/private/var/run/astra/example.sock","egress":{"kind":"interface","name":"en1"}}
]
```

其他出口形式为 `{"kind":"direct"}`、`{"kind":"proxy","tag":"节点或组名"}` 和 `{"kind":"block"}`。`clear` 停用并删除该宿主的透明代理配置，`remove` 还会请求卸载扩展。原生验收脚本结束时执行 `clear`。

## Windows

WFP 模块以动态会话在 IPv4/IPv6 ALE 层事务性安装按应用标识匹配的 TCP/UDP BLOCK 过滤器。`AstraRoute` 的两个 TCP callout 将连接重定向到指定 PID 的本机监听器。接收方用 `RedirectContext::read` 获取原始 IP、端口、IPv6 scope、PID 和 WFP 重定向记录，在连接出站 socket 前可调用 `apply_to_outgoing`。只有两个 callout 均已向 WFP 注册后 `TcpRedirector::open` 才成功。UDP 转发尚未实现。

驱动工程位于 `crates/astra-process-routing/native/windows`，面向 Windows 10 2004 及以后版本的 x64。在安装了匹配版本 Visual Studio、SDK 和 WDK 的开发命令行构建。

```powershell
msbuild AstraRoute.vcxproj /p:Configuration=Release /p:Platform=x64
```

正式发布使用组织的 Windows Hardware Developer Program 帐户。在目标 Windows 版本完成适用的 HLK 测试，用关联的签名证书签署提交包，通过 Hardware Dashboard 提交认证。使用返回的 Microsoft 签名包验证目录签名和 SYS 文件。签名依赖组织的帐户、证书和测试结果。[Microsoft 驱动签名要求](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)

```powershell
signtool verify /kp /c AstraRoute.cat AstraRoute.sys
.\manage-driver.ps1 -PackageDirectory C:\Astra\signed-driver
```

以管理员身份在原生 Windows 运行以下回归。测试只拦截临时目录里的程序副本。

```powershell
cargo test --locked -p astra-process-routing --lib -- --ignored --exact windows::tests::native_block_lease --nocapture
cargo test --locked -p astra-process-routing --lib -- --ignored --exact windows::tests::native_tcp_redirect --nocapture
```

卸载前关闭使用该驱动的接管会话，从 `pnputil /enum-drivers` 确认其发布名称。例如 `.\manage-driver.ps1 -RemovePublishedInf oem42.inf`，脚本会核对原始 INF 文件名和提供商，并通过 Windows 的 primitive driver API 卸载该包。当前仓库交付源码、安装脚本和测试，正式签名及 Windows 原生运行结果仍待取得。[Microsoft primitive driver 安装接口](https://learn.microsoft.com/en-us/windows-hardware/drivers/develop/creating-a-primitive-driver)
