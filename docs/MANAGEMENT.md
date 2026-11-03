# 独立管理服务与多主机管理

`astra-manager` 是独立程序，配置 `role = "controller"` 或 `role = "agent"`。Agent 管理所在主机的 Proxy、Rule 和 Server Core；Controller 接受 Agent 主动建立的认证连接，维护节点与任务索引，并转发请求。Controller 不启动 Core。需要管理 Controller 所在主机时，另开一个 Agent，使用独立状态目录与 IPC 端点。

`astra-web` 是独立二进制，包含 React 静态前端和认证网关。浏览器只连接这个网关，在同一页面选择 Agent；每个 Agent 无需暴露公网管理端口。网关通过固定的本地管理 IPC 连接同机 Controller，也可连接同机独立 Agent。当前没有让浏览器指定任意后端 URL 的代理接口，也没有让 Web 网关跨网络直连 Controller 的配置项。

```text
浏览器 ── HTTPS ── astra-web ── 私有 IPC ── Controller
                                            ▲
                               Agent 主动连接 WSS
                                            │
                                 Agent ── 本地生命周期服务
                                   ▲         ├─ Proxy Core
                            私有 IPC/TUI     ├─ Rule Core
                                             └─ Server Core
```

## 构建与配置

```sh
python scripts/build.py --role controller
python scripts/build.py --role web
python scripts/build.py --role client
python scripts/build.py --role server
```

Web 构建需要 Node/npm；纯客户端、服务端和 Controller 构建无需 Node。程序位于 `target/<本机 Rust target>/{debug,release}`。Windows 使用带 `.exe` 的程序名。构建所有组件可用 `--role all`；独立打包使用 `--package`。

配置模板：

- [Controller](../configs/manager-controller.example.toml)：公网 WSS 监听、TLS 文件、Agent 凭据摘要表。
- [Agent](../configs/manager-agent.example.toml)：默认关闭远程连接，填写身份后可独立本地运行。
- [Web](../configs/web.example.toml)：固定管理端点、独立状态目录和 HTTPS 监听。

路径相对于各自配置文件目录解析。`storage.state_dir`、`storage.config_dir` 必须分离且不能嵌套；`storage.bin_dir` 必须是已存在的程序目录。`local_ipc.endpoint` 指定私有管理端点，未指定时取 `storage.runtime_dir/management.sock`，无 runtime_dir 则取 state_dir。Windows 端点路径映射为当前用户命名管道，不是 TCP 监听。

`storage.runtime_dir` 显式传给 Agent 的生命周期服务，以及启动、校验、诊断、刷新和更新 Core 子进程；管理器读取的运行开关与订阅缓存使用同一目录。未指定时使用 `ASTRA_RUNTIME_DIR` / 平台默认目录。独立启动 Core 时可设置 `ASTRA_RUNTIME_DIR` 指向相同位置。Controller 使用该目录存放本地运行端点，不因此获得 Core 执行职责。

## 注册、轮换与撤销 Agent

为 Controller 配置能验证其域名的 TLS 证书与私钥。Agent 使用 `wss://`，通过系统信任或 `upstream.trust_root` 验证证书，不提供跳过校验选项。服务不会自动申请或续期证书。

在 Controller 主机执行：

```sh
astra-manager provision-agent --config controller.toml --agent-id node-a --token-out node-a.token
astra-manager --config controller.toml
```

注册命令生成随机凭据，写入一个必须尚不存在的私有文件，并把 SHA-256 摘要保存到 Controller 配置。将 token 文件通过可信渠道安装到对应 Agent 主机；不要把 token 写入浏览器、URL、命令行参数或版本库。在 Agent 配置中启用：

```toml
[upstream]
enabled = true
agent_id = "node-a"
name = "Node A"
controller_url = "wss://controller.example.com:9443"
credential_file = "../secrets/node-a.token"
trust_root = "../secrets/controller-ca.crt"
```

Controller 使用公开可信证书时可省略 trust_root。随后运行 `astra-manager --config agent.toml`。凭据绑定 Agent ID；拥有某节点凭据不代表能够冒充另一节点。连接进行协议版本与能力协商，心跳超时后重连。

轮换和撤销使用同一配置文件：

```sh
astra-manager rotate-agent --config controller.toml --agent-id node-a --token-out node-a.next.token
astra-manager revoke-agent --config controller.toml --agent-id node-a
```

轮换输出也必须是新文件；将新凭据安装到 Agent 的 credential_file 后，Agent 在下一次连接时读取它。Controller 每两秒检查配置中的凭据表，变更或移除凭据会撤销对应现有连接。无效的新配置不会替换当前凭据。该热检查仅应用凭据表；监听地址、TLS 文件等部署变更需重启管理服务。撤销远程访问不会停止 Agent 上正在运行的 Core。

## 启动独立 Web

Web 为单管理员设计，不提供多租户、用户角色或组织权限。没有默认密码，启动前通过标准输入初始化至少 12 字节的管理员密码：

```sh
astra-web --config web.toml --set-password --password-stdin
astra-web --config web.toml
```

首条命令从标准输入读取密码直至 EOF；可由密码管理器管道输入，避免把明文写入命令参数。凭据保存为 Argon2 哈希。重复设置密码会使既有浏览器会话失效；退出登录也会撤销当前会话。凭据文件与 Web 状态独立于 Agent token；可用 Web 配置 `credentials_file`（Manager 监督模式为 `[web].credentials_file`）指定哈希文件，默认是 Web state_dir 下的 `administrator.phc`。

生产或局域网访问必须配置 HTTPS 与精确的 `public_origin`；Origin 不带路径、查询或结尾斜线。只有显式 `dev_http = true` 且监听回环地址才允许无 TLS 的开发模式。接口执行认证、来源检查、会话及请求大小限制。浏览器不会取得 Agent token，也不能触发本地提权或任意 shell。

可在 Manager 配置启用 `[web]`，由它监督单独的 astra-web 子进程；Web 崩溃会独立重启，不嵌入 Controller、Agent 或 Core。此方式把网关配置写入 `state_dir/web-gateway.toml`，使用 `web.state_dir` 或默认 `state_dir/web`。仍需显式执行 `--set-password --password-stdin` 初始化同一状态目录的凭据。也可以完全自行部署 Web 进程。

## 无 Controller 的本地模式

Agent 配置保留 `upstream.enabled = false` 即为本地管理模式，不建立外网连接。TUI 使用私有 IPC 操作本地主机。`astra-host` 是 astra-manager 提供的本地 Agent 兼容入口，不再包含第二份 TUI/管理实现。普通 SOCKS/HTTP 管理不要求管理员权限；系统代理、TUN、透明代理的权限与平台限制仍由目标主机检查。

本地模式也可单独启动 Web，并将 manager_socket 指向该 Agent 的端点；这不会创建隐含 Controller，也不会改变 Core 的流量路径。关闭 TUI 或 Web 不等于停止 Core；停止操作必须通过明确的生命周期命令执行。

## 数据、断线与修改语义

源配置、草稿、资源缓存和更新包保存在执行操作的 Agent。Controller 保存节点身份、最近快照和路由任务索引；浏览器通过选定 Agent 的逻辑实例 ID 请求数据。离线节点不会接收排队的写操作，旧快照必须按离线/过期状态理解。

修改操作携带 operation_id。Agent 持久记录请求指纹及结果：相同 ID 和相同输入返回既有结果，相同 ID 配不同输入产生冲突。连接中断时不会自动重放修改；无法确定结果会标记 outcome_unknown。恢复连接后先查询原 operation_id，再检查实际运行状态，不能把网络错误当成操作未执行。Agent 重启前未记录结果的操作同样保留未知结果状态。

草稿使用 revision 检查并发编辑，保存还检查来源版本。TUI 和 Web 通过 astra-app 共享 schema、字段/集合编辑及引用重命名规则；TUI 的导入与文档历史也使用该库，Web 在当前浏览器编辑会话维护撤销重做。编辑过程尽量保留 TOML 注释、格式和未知字段。校验分为 TOML 解析、model 纯结构/引用校验、目标 Core 的文件/资源/平台准备。Server 候选经 `--check-stdin PATH` 校验，以原配置路径解析相对证书，校验不改写源文件。

## 更新与旧状态

客户端在线 worker 切换仍受实验门控，见 [更新边界](SEAMLESS_UPDATES.md)。Server Core 的正常重载规则与客户端 worker 升级不同；Controller、Agent 管理进程、Web 的程序更新也不能表述为所有连接无缝迁移。更新包完整性校验不等于发布者签名认证。

从旧版切换前，先正常停止旧管理服务并备份状态目录、配置目录、更新版本目录及各处私有凭据。已有保存操作的 `backups/` 只保留对应配置的旧内容，不是完整系统备份。旧 instances.toml 会复制到分离的配置目录，旧原文件保留不变，已有目标注册文件优先；已有配置引用保留，程序不会移动源文件或自动替换外部资源路径。无效注册项、未知状态字段、损坏草稿或未来数据库版本会报错并保留原数据，不会重置为空配置。旧注册表中的 Server 项目以及 manager、management、server-* 保留名称需要人工迁移，程序不自动改名或改写源配置。新管理数据库与旧部署配置格式不保证旧程序可读；回退前恢复匹配的备份与旧二进制，不让新旧管理进程同时占用同一目录。

本轮完成 Windows 本机构建、单元和本机集成测试，以及 WSL/CachyOS 的全 workspace 编译检查和 112 项基础/管理库测试，详见 [Linux 验证记录](LINUX_REFACTOR_VERIFICATION.md)。Linux 的完整程序升级、服务部署、证书安装和真实系统接管仍须在目标主机验收；macOS 本轮没有实际执行。既有协议性能或旧版本验证报告不代表新分布式管理拓扑已完成生产验收。
