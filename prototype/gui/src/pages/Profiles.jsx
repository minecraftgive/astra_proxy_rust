import { S, rt } from "../store.js";
import * as A from "../actions.js";
import { dur } from "../model/format.js";
import { Dot, stateText } from "../components/ui.jsx";
import Editor from "./Editor.jsx";

function ProfileCard({ p }) {
  const up = p.state === "running", r = rt(p.name);
  const shared = p.rule && S.profiles.filter((x) => x.rule === p.rule).length > 1;
  const takeover = [r.sys && "系统代理", r.tun && "TUN", r.lan && "LAN"].filter(Boolean).join("、") || "无";
  return (
    <div className={`card pcard ${p.name === S.active ? "active" : ""}`}>
      <div className="h">
        <Dot state={p.state} /><b>{p.name}</b>
        {p.name === S.active && <span className="pill acc">当前</span>}
        <span className={`pill ${up ? "ok" : p.state === "failed" ? "err" : ""}`} style={{ marginLeft: "auto" }}>
          {stateText(p)}{up && " " + dur(Date.now() - p.since)}
        </span>
      </div>
      {p.error && <div style={{ color: p.state === "failed" ? "var(--err)" : "var(--muted)", fontSize: 12.5 }}>{p.error}</div>}
      <dl className="kv">
        <dt>路由</dt>
        <dd>{p.routing === "rule" ? <>规则模式 · {p.rule}{shared && <span className="faint"> 共享</span>}</> : `独立模式 · ${p.doc.general.standalone_outbound}`}</dd>
        <dt>监听</dt><dd className="mono">{p.doc.inbound.listen}</dd>
        <dt>节点</dt><dd>{p.doc.nodes.length} 个 · {p.doc.subs.length} 个订阅</dd>
        <dt>接管</dt><dd>{up ? takeover : "—"}</dd>
      </dl>
      <div className="acts">
        <button className={`btn sm ${up ? "" : "primary"}`} onClick={() => A.toggleConnect(p.name)} disabled={p.busy}>{up ? "断开" : "连接"}</button>
        <button className="btn sm" onClick={() => A.openEditor(p.name)}>编辑</button>
        {p.name !== S.active && <button className="btn sm" onClick={() => A.setActive(p.name)}>设为当前</button>}
        <button className="btn sm ghost" style={{ marginLeft: "auto" }} aria-label={`${p.name} 的更多操作`}
          onClick={(e) => { const b = e.currentTarget.getBoundingClientRect(); A.openOverlay({ type: "menu", kind: "profile", name: p.name, x: b.right - 260, y: b.bottom + 6 }); }}>
          更多…
        </button>
      </div>
    </div>
  );
}

export default function Profiles() {
  if (S.editor && !S.editorHidden) return <Editor />;
  return (
    <>
      <div className="page-head">
        <h1>配置档</h1><span className="muted">每个配置档 = 代理端 + 可选的路由引擎</span>
        <div className="tools">
          {S.editor && <button className="btn" onClick={() => A.openEditor(S.editor.profile, S.editor.section)}>继续编辑 {S.editor.profile}（{S.editor.dirty.size} 处改动）</button>}
          <button className="btn primary" onClick={() => A.openOverlay({ type: "wizard" })}>新建 / 导入</button>
        </div>
      </div>
      <div className="profiles">{S.profiles.map((p) => <ProfileCard key={p.name} p={p} />)}</div>
    </>
  );
}
