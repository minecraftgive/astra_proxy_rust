import { useEffect, useState } from "react";
import { S, prof, rt } from "../store.js";
import * as A from "../actions.js";
import { explain, outboundFor } from "../model/routing.js";
import { ActionPill } from "../components/ui.jsx";

const actionName = (a) => (a === "direct" ? "DIRECT" : a === "block" ? "REJECT" : a.slice(6));

export default function Rules() {
  const p = prof();
  const [input, setInput] = useState(S.explain.input);
  const req = S.explain.result;
  const ex = req && explain(S.rules, req.host, req.proc);
  const ob = req && outboundFor(p, rt(), S.rules, req.host, req.proc).chain;
  const focus = S.explain.focus;
  useEffect(() => {
    const i = ex?.index >= 0 ? ex.index : focus;
    if (i != null && i >= 0) document.getElementById(`rule-${i}`)?.scrollIntoView({ block: "nearest" });
  }, [req, focus]); // eslint-disable-line react-hooks/exhaustive-deps

  if (p.doc.general.standalone) {
    return (
      <>
        <div className="page-head"><h1>规则</h1></div>
        <div className="card empty">
          <b>独立模式</b>{p.name} 的所有流量使用 {p.doc.general.standalone_outbound}，不经过路由引擎。
          <div style={{ marginTop: 12 }}><button className="btn" onClick={() => A.openEditor(p.name, "general")}>切换为规则模式</button></div>
        </div>
      </>
    );
  }
  const shared = S.profiles.filter((x) => x.rule === p.rule).map((x) => x.name);
  let lastSec = null;
  return (
    <>
      <div className="page-head">
        <h1>规则</h1><span className="muted">路由引擎 {p.rule} · {S.rules.length} 条 · 按顺序首条命中</span>
        <div className="tools"><button className="btn" onClick={() => A.openEditor(p.name, "rules")}>编辑规则</button></div>
      </div>
      <div className="card explain">
        <form onSubmit={(e) => { e.preventDefault(); A.runExplain(input); }}>
          <input className="input" id="explain-in" value={input} onChange={(e) => setInput(e.target.value)}
            placeholder="host[:port] [tcp|udp] [进程名或路径]" aria-label="测试路由" autoComplete="off" />
          <button className="btn primary">测试路由</button>
        </form>
        {ex ? (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div className="crumbs">
              {ex.path.map((x, i) => <span key={i} style={{ display: "contents" }}><span>{x}</span><i>▸</i></span>)}
              <span className="act">{actionName(ex.action)}</span>
            </div>
            <div className="muted">
              {ex.index >= 0 ? `命中第 ${ex.index + 1} 条规则。` : "没有显式规则命中，使用默认动作。"}
              {" "}当前实际出口：<b style={{ color: "var(--text)" }}>{ob.join(" ▸ ")}</b>
              {rt().mode !== "rule" && `（${rt().mode === "global" ? "全局" : "直连"}模式下规则不生效）`}
              。查询使用已应用的规则，不发送流量、不解析域名。
            </div>
          </div>
        ) : (
          <div className="muted">输入域名或 IP，查看它会命中哪条规则、最终走哪个出口。例如 <span className="mono">github.com</span>、<span className="mono">update.steampowered.com:443 tcp steam.exe</span></div>
        )}
      </div>
      {shared.length > 1 && <div className="banner info">路由引擎 {p.rule} 由 {shared.join("、")} 共用，修改规则会同时影响它们。</div>}
      <div className="card">
        {S.rules.map((r, i) => {
          const head = r.sec !== lastSec;
          lastSec = r.sec;
          return (
            <div key={i}>
              {head && <div className="rule-sec"><h4>{r.sec}</h4></div>}
              <div className={`rule ${(ex && ex.index === i) || (!ex && focus === i) ? "hit" : ""}`} id={`rule-${i}`}>
                <span className="idx">{i + 1}</span>
                <span className="typ muted mono">{r.type}</span>
                <span className="val">{r.value}{r.note && <span className="faint"> · {r.note}</span>}</span>
                <ActionPill action={r.action} />
                <button className="btn ghost sm edit" onClick={() => A.openEditor(p.name, "rules")}>编辑</button>
              </div>
            </div>
          );
        })}
        <div className="rule" style={{ borderTop: "1px solid var(--border)" }}>
          <span className="idx">—</span><span className="typ muted mono">default</span>
          <span className="val muted">未命中以上规则（本地 IP 先直连）</span><ActionPill action="proxy:PROXY" /><span />
        </div>
      </div>
    </>
  );
}
