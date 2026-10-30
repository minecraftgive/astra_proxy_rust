# Astra GUI 交互原型

[UI 重设计方案](../../docs/UI_REDESIGN.md) 第 7 节 GUI 的可点击原型，用 React + Vite 编写。数据全部在浏览器内模拟，不连接 manager，也不修改任何配置。

```sh
npm install
npm run dev      # http://localhost:5173
npm run build    # 输出到 dist/
```

## 可以试的流程

- 顶栏：切换配置档、连接/断开、规则/全局/直连
- 概览：出口链路、系统代理 / TUN（模拟 UAC）/ LAN 开关、流量趋势、带操作按钮的提醒
- 代理：点击成员切换节点，按组测速，右键节点打开诊断抽屉
- 连接：筛选、暂停刷新、关闭单个或全部，点击行查看命中的规则
- 规则：输入域名测试路由，命中规则高亮
- 日志：切换来源、显示级别与记录级别（ROUTE 行可点开）
- 配置档：新建向导；编辑器支持分区导航、字段校验、撤销/重做、删除后撤销、保存并应用；`lab` 配置档演示端口冲突与修复
- `Ctrl-K` 命令面板（右侧显示 TUI 中同一命令的键位）、`Ctrl-1…6` 切页、`F6` 任务中心、`Esc` 关闭最上层

## 结构

```text
src/
  store.js        AppState 与订阅（对应 astra-app 的状态层）
  actions.js      用户操作，对应设计中的命令
  commands.js     命令注册表：命令面板与键位提示共用
  sim.js          模拟 manager 事件流（流量、连接、日志）
  model/          路由解释、字段 Schema、示例数据、格式化
  components/     外框、抽屉、浮层、通用控件
  pages/          概览、代理、连接、规则、日志、配置档、编辑器、设置
```
