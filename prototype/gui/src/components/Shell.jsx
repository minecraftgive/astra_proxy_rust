// 外框：侧栏导航、顶栏三件套、底部任务条、任务中心与提示。
import { S, prof, rt, isUp } from "../store.js";
import * as A from "../actions.js";
import { ICONS, Dot, Seg } from "./ui.jsx";
import { rate, clock } from "../model/format.js";

export const PAGES = [
  ["overview", "概览"], ["proxies", "代理"], ["connections", "连接"], ["rules", "规则"], ["logs", "日志"], ["profiles", "配置档"],
];
export const MODES = [["rule", "规则"], ["global", "全局"], ["direct", "直连"]];

export function Sidebar() {
  const nav = (k, l, i) => (
    <button key={k} className={`nav ${S.page === k ? "on" : ""}`} onClick={() => A.go(k)} title={i != null ? `${l}（Ctrl-${i + 1}）` : l}>
      {ICONS[k]}<span className="lbl">{l}</span>{i != null && <span className="key">{i + 1}</span>}
    </button>
  );
  return (
    <aside className="side">
      <div className="brand"><i className="brand-mark" /><span>Astra</span></div>
      {PAGES.map(([k, l], i) => nav(k, l, i))}
      <div className="spacer" />
      {nav("settings", "设置")}
    </aside>
  );
}

export function TopBar() {
  const p = prof(), r = rt(), up = isUp();
  const status = p.busy ? (up ? "断开中…" : "连接中…") : up ? "已连接" : p.state === "failed" ? "连接失败" : "未连接";
  const tr = S.traffic;
  return (
    <header className="top">
      <button className="profile-btn" title="切换配置档（p）"
        onClick={(e) => { const b = e.currentTarget.getBoundingClientRect(); A.openOverlay({ type: "menu", kind: "profiles", x: b.left, y: b.bottom + 6 }); }}>
        <Dot state={p.state} /><b>{p.name}</b><span className="faint">▾</span>
      </button>
      <button className={`connect ${up ? "on" : ""} ${p.busy ? "busy" : ""}`} onClick={() => A.toggleConnect()} aria-pressed={up} title={`${up ? "断开" : "连接"}（c）`}>
        <span className="knob" />{status}
      </button>
      <Seg value={r.mode} options={MODES} onChange={A.setMode} label="模式" />
      <div className="rates num">
        <span>↑ <span className="u">{rate(tr.up[59])}</span></span>
        <span>↓ <span className="d">{rate(tr.down[59])}</span></span>
      </div>
      <button className="kbtn" onClick={() => A.openOverlay({ type: "palette" })} title="命令面板">命令 <kbd>Ctrl K</kbd></button>
    </header>
  );
}

const elapsed = (t) => (((t.end || Date.now()) - t.start) / 1000).toFixed(1) + "s";

export function TaskBar() {
  const t = S.tasks[0];
  const running = S.tasks.filter((x) => x.state === "run").length;
  let text = "就绪 · 后台服务 v0.2.0 · 2 个界面已连接";
  if (t) {
    const cur = t.stages.find((s) => s.st === "run") || t.stages[t.stages.length - 1];
    text = `${t.state === "run" ? "" : t.state === "ok" ? "✓ " : "✗ "}${t.title} · ${cur.s} · ${elapsed(t)}`;
  }
  return (
    <footer className="taskbar">
      <button onClick={() => A.toggleTasks()} title="任务中心（F6）">
        {running > 0 && <span className="spin" />}<span className="t">{text}</span>
      </button>
      <span className="right">{running ? `${running} 个任务进行中` : ""}</span>
    </footer>
  );
}

export function TaskCenter() {
  if (!S.tasksOpen) return null;
  return (
    <div className="tasks card" style={{ boxShadow: "var(--shadow)" }}>
      <div className="drawer-head" style={{ padding: "10px 14px" }}>
        <h2 style={{ fontSize: 13 }}>任务中心 <span className="faint">· 所有界面共享</span></h2>
        <button className="btn ghost sm" onClick={() => A.toggleTasks(false)}>关闭</button>
      </div>
      {S.tasks.length ? S.tasks.map((t) => (
        <div className="task" key={t.id}>
          <div className="h">
            {t.state === "run" ? <span className="spin" /> : <span className={`dot ${t.state === "ok" ? "ok" : "err"}`} />}
            <b>{t.title}</b>
            <span className="faint num" style={{ marginLeft: "auto" }}>{clock(t.start)} · {elapsed(t)}</span>
          </div>
          <ol>{t.stages.map((s, i) => <li key={i} className={s.st === "done" ? "done" : s.st === "fail" ? "fail" : ""}>{s.st === "run" ? "… " : ""}{s.s}</li>)}</ol>
        </div>
      )) : <div className="empty">还没有任务</div>}
    </div>
  );
}

export function Toasts() {
  return (
    <div className="toasts" aria-live="polite">
      {S.toasts.map((t) => (
        <div className="toast" key={t.id}>
          <span>{t.text}</span>
          {t.action && <button onClick={() => { t.action[1](); A.dismissToast(t.id); }}>{t.action[0]}</button>}
        </div>
      ))}
    </div>
  );
}
