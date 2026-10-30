// 右侧详情抽屉：节点、连接、路由日志。
import { S, commit, prof, rt } from "../store.js";
import * as A from "../actions.js";
import { explain } from "../model/routing.js";
import { bytes, dur } from "../model/format.js";
import { Latency } from "./ui.jsx";

function NodeDetail({ name }) {
  const doc = prof().doc, r = rt(), g = S.diag[name];
  const n = doc.nodes.find((x) => x.name === name);
  if (!n) return <p className="muted">{name} 是代理组或内置出口。</p>;
  const groups = doc.groups.filter((gg) => gg.members.includes(name)).map((gg) => gg.name);
  const val = (v, fail = "失败") => (v != null ? v : g?.state === "run" ? "…" : g?.state === "fail" ? fail : "—");
  return (
    <>
      <dl className="kv">
        <dt>协议</dt><dd>{n.type}{n.security !== "none" && ` · ${n.security}`} · {n.transport}</dd>
        <dt>服务器</dt><dd className="mono">{n.server}:{n.port}</dd>
        <dt>测速</dt><dd><Latency ms={r.delays[name]} alive={r.alive[name]} /> <span className="faint">HTTP(S) 往返</span></dd>
        <dt>所属组</dt><dd>{groups.join("、") || "—"}</dd>
      </dl>
      <div>
        <h3 className="section-title">诊断 · 绕过规则直接经此节点</h3>
        <div className="diag">
          <div><small>TCP 延迟</small><b>{val(g?.tcp != null ? `${g.tcp} ms` : null)}</b></div>
          <div><small>连通性</small><b>{val(g?.state === "ok" ? "正常" : null)}</b></div>
          <div><small>出口 IP</small><b style={{ fontSize: 11.5 }}>{val(g?.ip)}</b></div>
        </div>
        <p className={g?.why ? "" : "muted"} style={{ margin: "8px 0 0", color: g?.why ? "var(--err)" : undefined }}>
          {g?.region ? `出口位于${g.region}。` : g?.why || "TCP 延迟包含代理握手，不是 ICMP ping。"}
        </p>
      </div>
      <div className="row-tools">
        <button className="btn primary" onClick={() => A.diagnose(name)} disabled={g?.state === "run"}>{g ? "重新诊断" : "诊断"}</button>
        <button className="btn" onClick={() => A.openEditor(S.active, "nodes", `nodes.${doc.nodes.indexOf(n)}`)}>在编辑器中打开</button>
      </div>
    </>
  );
}

function ConnDetail({ id }) {
  const c = S.conns.find((x) => x.id === id);
  if (!c) return <p className="muted">该连接已结束。开启 ROUTE 日志后，结束时的字节数与时长会写入日志。</p>;
  const mode = rt().mode;
  const e = explain(S.rules, c.host, c.proc);
  const steps = mode === "rule" ? e.path : [mode === "global" ? "全局模式" : "直连模式"];
  const gotoRule = () => {
    S.explain.result = null;
    S.explain.focus = e.index;
    commit();
    A.go("rules");
  };
  return (
    <>
      <dl className="kv num">
        <dt>目标</dt><dd className="mono">{c.host}:{c.port}</dd>
        <dt>网络</dt><dd>{c.net.toUpperCase()} · 入站 {c.inbound}</dd>
        <dt>来源</dt><dd className="mono">{c.src}</dd>
        <dt>进程</dt><dd>{c.proc}</dd>
        <dt>出口</dt><dd>{c.chain.join(" ▸ ")}</dd>
        <dt>流量</dt><dd>↑ {bytes(c.up)} · ↓ {bytes(c.down)}</dd>
        <dt>时长</dt><dd>{dur(Date.now() - c.start)}</dd>
      </dl>
      <div>
        <h3 className="section-title">为什么走这里</h3>
        <div className="crumbs">
          {steps.map((x, i) => <span key={i} style={{ display: "contents" }}><span>{x}</span><i>▸</i></span>)}
          <span className="act">{c.chain.join(" ▸ ")}</span>
        </div>
      </div>
      <div className="row-tools">
        <button className="btn danger" onClick={() => A.closeConn(c.id)}>关闭连接</button>
        {e.index >= 0 && mode === "rule" && <button className="btn" onClick={gotoRule}>查看规则 #{e.index + 1}</button>}
      </div>
    </>
  );
}

function RouteDetail({ line: l }) {
  return (
    <>
      <dl className="kv">
        <dt>时间</dt><dd>{l.t}</dd>
        <dt>目标</dt><dd className="mono">{l.c.host}:{l.c.port}</dd>
        <dt>网络</dt><dd>{l.c.net.toUpperCase()}</dd>
        <dt>进程</dt><dd>{l.c.proc}</dd>
        <dt>规则</dt><dd className="mono">{l.c.why}</dd>
        <dt>出口</dt><dd>{l.c.chain.join(" ▸ ")}</dd>
      </dl>
      <pre className="mono raw">{l.msg}</pre>
    </>
  );
}

export default function Drawer() {
  const d = S.drawer;
  let title = "", body = null;
  if (d?.type === "node") { title = d.name; body = <NodeDetail name={d.name} />; }
  if (d?.type === "conn") { title = S.conns.find((x) => x.id === d.id)?.host || "连接已关闭"; body = <ConnDetail id={d.id} />; }
  if (d?.type === "route") { title = "路由记录"; body = <RouteDetail line={d.line} />; }
  return (
    <aside className={`drawer ${d ? "open" : ""}`} aria-label="详情" aria-hidden={!d}>
      {d && (
        <>
          <div className="drawer-head"><h2>{title}</h2><button className="btn ghost sm" onClick={A.closeDrawer}>关闭 Esc</button></div>
          <div className="drawer-body">{body}</div>
        </>
      )}
    </aside>
  );
}
