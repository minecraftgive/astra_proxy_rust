import { S, commit, isUp } from "../store.js";
import * as A from "../actions.js";
import { bytes, dur } from "../model/format.js";

function Exit({ chain }) {
  const last = chain[chain.length - 1];
  if (last === "REJECT") return <span className="pill err">拒绝</span>;
  if (chain[0] === "DIRECT") return <span className="pill">直连</span>;
  return <>{chain.slice(-2).join(" ▸ ")}</>;
}

export default function Connections() {
  if (!isUp()) {
    return (
      <>
        <div className="page-head"><h1>连接</h1></div>
        <div className="card empty">
          <b>{S.active} 未连接</b>连接后，这里会实时列出经过 Astra 的 TCP/UDP 连接。
          <div style={{ marginTop: 12 }}><button className="btn primary" onClick={() => A.toggleConnect()}>连接</button></div>
        </div>
      </>
    );
  }
  const q = S.connFilter.toLowerCase();
  const rows = S.conns.filter((c) => !q || [c.host, c.proc, c.chain.join(" "), c.net].join(" ").toLowerCase().includes(q));
  return (
    <>
      <div className="page-head">
        <h1>连接</h1>
        <span className="muted num">{rows.length} / {S.conns.length} 个活动连接{S.frozen ? " · 已暂停刷新" : ""}</span>
        <div className="tools">
          <input className="input" id="search" placeholder="筛选目标 / 出口 / 进程（Ctrl-F）" value={S.connFilter} style={{ width: 240 }}
            onChange={(e) => { S.connFilter = e.target.value; commit(); }} />
          <button className="btn" aria-pressed={S.frozen} onClick={() => { S.frozen = !S.frozen; commit(); }}>{S.frozen ? "继续刷新" : "暂停刷新"}</button>
          <button className="btn danger" onClick={A.closeAllConns}>全部关闭</button>
        </div>
      </div>
      <div className="card table-wrap">
        <table>
          <thead><tr><th>目标</th><th>网络</th><th>进程</th><th>出口</th><th className="r">上传</th><th className="r">下载</th><th className="r">时长</th><th /></tr></thead>
          <tbody>
            {rows.length ? rows.map((c) => (
              <tr key={c.id} className={`click ${S.selConn === c.id ? "sel" : ""}`} onClick={() => A.openDrawer({ type: "conn", id: c.id })}>
                <td><span className="host" title={c.host}>{c.host}</span><span className="faint">:{c.port}</span></td>
                <td><span className="pill">{c.net.toUpperCase()} · {c.inbound}</span></td>
                <td className="muted">{c.proc}</td>
                <td><Exit chain={c.chain} /></td>
                <td className="r num mono">{bytes(c.up)}</td>
                <td className="r num mono">{bytes(c.down)}</td>
                <td className="r num mono">{dur(Date.now() - c.start)}</td>
                <td><button className="btn ghost sm x" title="关闭连接（x）" onClick={(e) => { e.stopPropagation(); A.closeConn(c.id); }}>关闭</button></td>
              </tr>
            )) : <tr><td colSpan={8} className="empty">没有匹配的连接</td></tr>}
          </tbody>
        </table>
      </div>
    </>
  );
}
