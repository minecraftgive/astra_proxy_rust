# Astra 管理网页

正式 React + TypeScript 客户端由独立 `astra-web` 进程提供。所有运行状态、草稿、任务、日志和更新结果均来自管理协议 v2；模拟设计数据只保存在 `tests/fixtures`。

从仓库根目录运行 `python scripts/build.py --role web --package`，将安装前端依赖、构建资源并打包独立网页组件。代理、规则和服务端核心的常规构建不依赖 Node。

网页需要本机 Controller 或 Agent 的可信 IPC 端点。部署配置、TLS、管理员初始化、Agent 登记与离线行为见 [管理文档](../../docs/MANAGEMENT.md) 和 [配置示例](../../configs/web.example.toml)。管理员没有默认密码；通过 `astra-web --config web.toml --set-password --password-stdin` 初始化。会话令牌只保存在内存。

首次查看目标节点时，网页会提示将节点上已有的完整包安装到固定目录并创建启动入口；可选择“暂不安装”，之后随时从 **设置 → 安装** 进入。提示按目标节点保存在当前浏览器中。安装默认仅针对目标账户，也可选择目标机器的所有用户；客户端、服务端、控制器与网页服务分别显示包是否完整及缺失原因。网页不下载程序包、不迁移配置，也不重启当前服务。需要管理员授权时，请在目标机使用本地安装入口；网页不会调用浏览器所在电脑的提权程序。回复丢失时查询原安装任务，不能自动重新提交。旧节点没有安装能力时保留设置入口并显示原因。

开发时在本目录执行：

```sh
npm ci
npm run typecheck
npm test
npm run dev
```

Vite 监听 `127.0.0.1:5173` 并将 `/api` 转发到 `http://127.0.0.1:8787`（可通过 `ASTRA_WEB_DEV_UPSTREAM` 指定）。开发用 gateway 配置应设置 `listen = "127.0.0.1:8787"`、`public_origin = "http://127.0.0.1:5173"`、`dev_http = true`，保持原始 Host/Origin 供同源检查；该 HTTP 模式只允许回环地址。

契约由仓库根目录的 `cargo run -p astra-management --features bindings --bin export-contracts` 显式生成。追加 `-- --check` 检查是否过期，不会改写源码；普通测试不会生成契约。

组件测试覆盖保存期间的新编辑、草稿冲突基线、切换节点后的迟到回复，以及请求身份和禁止自动重试变更。真实浏览器端到端测试由 `python scripts/test.py management --debug --browser` 启动 TLS Controller、两个 Agent、独立网页和真实核心后运行。也可使用 `--artifact-dir` 对指定安装包索引进行验证。
