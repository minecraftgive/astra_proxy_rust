import { useLayoutEffect, useRef } from "react";
import { S, commit, prof } from "../store.js";
import * as A from "../actions.js";
import { Seg } from "../components/ui.jsx";

const ORDER = ["DEBUG", "ROUTE", "INFO", "WARN", "ERROR"];

function highlight(text, q) {
  if (!q) return text;
  const out = [];
  const lower = text.toLowerCase();
  let i = 0;
  for (let j = lower.indexOf(q); j >= 0; j = lower.indexOf(q, i)) {
    out.push(text.slice(i, j), <mark key={j}>{text.slice(j, j + q.length)}</mark>);
    i = j + q.length;
  }
  out.push(text.slice(i));
  return out;
}

export default function Logs() {
  const L = S.logs, p = prof();
  const box = useRef(null);
  const stick = useRef(true);
  const q = L.q.toLowerCase();
  const lines = L.lines[L.src].filter((l) => ORDER.indexOf(l.lv) >= ORDER.indexOf(L.show) && (!q || l.msg.toLowerCase().includes(q)));
  useLayoutEffect(() => {
    if (box.current && stick.current && !L.paused) box.current.scrollTop = box.current.scrollHeight;
  });
  const set = (k, v) => { L[k] = v; commit(); };
  const file = L.src === "manager" ? "manager.log" : `${L.src === "proxy" ? p.name : p.rule || p.name}.log`;
  return (
    <>
      <div className="page-head"><h1>日志</h1><span className="muted">{p.name} · 实时推送 · 状态目录中的 {file}</span></div>
      <div className="card">
        <div className="toolbar">
          <Seg value={L.src} onChange={(v) => set("src", v)} label="来源" options={[
            ["proxy", "代理端"],
            ["rule", "路由引擎", p.rule ? {} : { disabled: true, title: "独立模式没有路由引擎" }],
            ["manager", "后台服务"],
          ]} />
          <label>显示
            <select className="input" value={L.show} onChange={(e) => set("show", e.target.value)}>
              {ORDER.map((l) => <option key={l} value={l}>≥ {l}</option>)}
            </select>
          </label>
          <label title="核心实际写入的级别，立即生效，无需重启">记录
            <select className="input" value={L.record} onChange={(e) => {
              set("record", e.target.value);
              A.task(`设置记录级别 ${e.target.value}`, ["发送到核心（保持 PID 不变）", "已生效"], { step: 250 });
            }}>
              {["INFO", "ROUTE", "DEBUG"].map((l) => <option key={l}>{l}</option>)}
            </select>
          </label>
          <input className="input" id="search" placeholder="搜索（Ctrl-F）" value={L.q} onChange={(e) => set("q", e.target.value)} style={{ flex: 1, minWidth: 140 }} />
          <button className="btn sm" aria-pressed={L.wrap} onClick={() => set("wrap", !L.wrap)}>{L.wrap ? "✓ " : ""}换行</button>
          <button className="btn sm" aria-pressed={L.paused} onClick={() => set("paused", !L.paused)}>{L.paused ? "继续跟随" : "暂停"}</button>
        </div>
        <div className={`logbox ${L.wrap ? "wrap" : ""}`} ref={box}
          onScroll={(e) => { const b = e.currentTarget; stick.current = b.scrollTop + b.clientHeight >= b.scrollHeight - 30; }}>
          {lines.length ? lines.map((l, i) => (
            <div key={i} className={`logline ${l.lv === "ROUTE" ? "route" : ""}`} onClick={l.lv === "ROUTE" ? () => A.openDrawer({ type: "route", line: l }) : undefined}>
              <span className="faint">{l.t}</span> <span className={`lv ${l.lv}`}>{l.lv}</span> {highlight(l.msg, q)}
            </div>
          )) : (
            <div className="empty">没有符合条件的日志。{L.record === "INFO" && "需要逐连接明细时，把“记录”设为 ROUTE。"}</div>
          )}
        </div>
      </div>
    </>
  );
}
