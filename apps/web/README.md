# Astra 管理网页

正式 React + TypeScript 客户端由独立 `astra-web` 进程提供。所有运行状态、草稿、任务、日志和更新结果均来自管理协议 v2；模拟设计数据只保存在 `tests/fixtures`。

从仓库根目录运行 `python scripts/build.py --role web --package`，将安装前端依赖、构建资源并打包独立网页组件。代理、规则和服务端核心的常规构建不依赖 Node。

网页需要本机 Controller 或 Agent 的可信 IPC 端点。部署配置、TLS、管理员初始化、Agent 登记与离线行为见 [管理文档](../../docs/MANAGEMENT.md) 和 [配置示例](../../configs/web.example.toml)。管理员没有默认密码；通过 `astra-web --config web.toml --set-password --password-stdin` 初始化。会话令牌只保存在内存。

开发时在本目录执行：

```sh
npm ci
npm run typecheck
npm test
npm run dev
```

Vite 监听 `127.0.0.1:5173` 并将 `/api` 转发到 `http://127.0.0.1:8787`（可通过 `ASTRA_WEB_DEV_UPSTREAM` 指定）。开发用 gateway 配置应设置 `listen = "127.0.0.1:8787"`、`public_origin = "http://127.0.0.1:5173"`、`dev_http = true`，保持原始 Host/Origin 供同源检查；该 HTTP 模式只允许回环地址。

契约由仓库根目录的 `cargo run -p astra-management --bin export-contracts` 显式生成。追加 `-- --check` 检查是否过期，不会改写源码；普通测试不会生成契约。

组件测试覆盖保存期间的新编辑、草稿冲突基线、切换节点后的迟到回复，以及请求身份和禁止自动重试变更。真实浏览器端到端测试由 `python scripts/test.py management --debug --browser` 启动 TLS Controller、两个 Agent、独立网页和真实核心后运行。也可使用 `--artifact-dir` 对指定安装包索引进行验证。
