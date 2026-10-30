// 命令注册表：命令面板、菜单与快捷键提示共用；kbd 是 TUI 中同一命令的键位。
import { S, prof, isUp } from "./store.js";
import * as A from "./actions.js";

const needUp = () => isUp() || "先连接配置档";

export const COMMANDS = [
  { scope: "导航", label: "转到 概览", kbd: "1", run: () => A.go("overview") },
  { scope: "导航", label: "转到 代理", kbd: "2", run: () => A.go("proxies") },
  { scope: "导航", label: "转到 连接", kbd: "3", run: () => A.go("connections") },
  { scope: "导航", label: "转到 规则", kbd: "4", run: () => A.go("rules") },
  { scope: "导航", label: "转到 日志", kbd: "5", run: () => A.go("logs") },
  { scope: "导航", label: "转到 配置档", kbd: "6", run: () => A.go("profiles") },
  { scope: "导航", label: "打开 设置", run: () => A.go("settings") },
  { scope: "运行", label: () => (isUp() ? `断开 ${S.active}` : `连接 ${S.active}`), kbd: "c", run: () => A.toggleConnect() },
  { scope: "运行", label: "应用改动（热重载）", kbd: "r", when: () => isUp() || "配置档未运行", run: A.applyChanges },
  { scope: "运行", label: "完整重启", kbd: "R", when: () => isUp() || "配置档未运行", run: () => A.restart() },
  { scope: "运行", label: "模式：规则", kbd: "m", run: () => A.setMode("rule") },
  { scope: "运行", label: "模式：全局", kbd: "m", run: () => A.setMode("global") },
  { scope: "运行", label: "模式：直连", kbd: "m", run: () => A.setMode("direct") },
  { scope: "接管", label: "开关 系统代理", kbd: "S", when: needUp, run: () => A.toggleTakeover("sys") },
  { scope: "接管", label: "开关 TUN（需要管理员）", kbd: "T", when: needUp, run: () => A.toggleTakeover("tun") },
  { scope: "接管", label: "开关 允许局域网", kbd: "A", when: needUp, run: () => A.toggleTakeover("lan") },
  { scope: "接管", label: "重置运行状态", when: needUp, run: A.resetRuntime },
  { scope: "代理", label: "全部代理组测速", kbd: "t", run: A.testAll },
  { scope: "代理", label: "更新订阅", kbd: "U", run: A.updateSubs },
  { scope: "连接", label: "关闭全部连接", kbd: "X", when: () => S.conns.length > 0 || "没有活动连接", run: A.closeAllConns },
  {
    scope: "规则", label: "测试路由…", kbd: "t",
    when: () => !prof().doc.general.standalone || "独立模式不使用规则",
    run: () => { A.go("rules"); setTimeout(() => document.getElementById("explain-in")?.focus(), 30); },
  },
  { scope: "配置档", label: "新建 / 导入配置档…", kbd: "n", run: () => A.openOverlay({ type: "wizard" }) },
  { scope: "配置档", label: () => `编辑 ${S.active}`, kbd: "e", run: () => A.openEditor() },
  { scope: "配置档", label: "切换当前配置档…", kbd: "p", run: () => A.openOverlay({ type: "menu", kind: "profiles", x: 220, y: 50 }) },
  { scope: "通用", label: "打开任务中心", kbd: "F6", run: () => A.toggleTasks(true) },
  { scope: "通用", label: "切换浅色 / 深色", run: A.toggleTheme },
  { scope: "通用", label: "停止全部配置档与后台服务", kbd: "Ctrl-Q", danger: true, run: A.stopAll },
];

export const labelOf = (c) => (typeof c.label === "function" ? c.label() : c.label);
export const whenOf = (c) => (c.when ? c.when() : true);
