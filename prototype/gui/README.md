# Astra GUI 原型迁移记录

原型已迁入正式的 [`apps/web`](../../apps/web/README.md)。旧的模拟状态、任务、编辑器与入口已删除；设计样本仅保存在 `apps/web/tests/fixtures/prototype-data.js`，不会进入生产构建。

网页由独立 `astra-web` 进程提供，使用真实管理 API v2。部署与鉴权见 [管理文档](../../docs/MANAGEMENT.md)。原始视觉设计背景仍可阅读 [UI 重设计方案](../../docs/UI_REDESIGN.md)，该文档描述历史设计，不代表当前运行契约。
