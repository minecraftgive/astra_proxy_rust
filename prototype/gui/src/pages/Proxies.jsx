import { S, commit, prof, rt, isUp } from "../store.js";
import * as A from "../actions.js";
import { groupOf, selectedOf } from "../model/routing.js";
import { BUILTIN_TAIL } from "../model/data.js";
import { Latency, Seg } from "../components/ui.jsx";

const isGroupLike = (doc, m) => !!groupOf(doc, m) || m === "DIRECT" || m === "REJECT";

function GroupCard({ g }) {
  const doc = prof().doc, r = rt();
  const sel = selectedOf(g, r);
  const auto = g.mode === "auto";
  const q = S.proxyQuery.toLowerCase();
  let members = g.members.filter((m) => !q || m.toLowerCase().includes(q));
  if (S.proxySort === "delay") members = [...members].sort((a, b) => (r.delays[a] ?? 1e9) - (r.delays[b] ?? 1e9));
  if (S.proxySort === "name") members = [...members].sort();
  const testable = g.members.some((m) => !isGroupLike(doc, m));
  return (
    <div className="card group">
      <div className="group-head">
        <h3>{g.name}</h3>
        <span className={`pill ${auto ? "acc" : ""}`}>{auto ? "自动" : "手动"}</span>
        {g.sub && <span className="pill">订阅</span>}
        {g.builtin && <span className="pill">内置</span>}
        <span className="cur">→ {sel}</span>
        {testable && (
          <button className="btn sm" onClick={() => A.testGroup(g.name)} disabled={r.testing[g.name]}>
            {r.testing[g.name] ? <><span className="spin" />测速中</> : "测速"}
          </button>
        )}
      </div>
      <div className="members">
        {members.length ? members.map((m) => {
          const grp = isGroupLike(doc, m);
          return (
            <button key={m} className={`member ${m === sel ? "sel" : ""} ${auto ? "ro" : ""}`}
              title={auto ? "自动组按测速选择；点击查看节点" : `选择 ${m}`}
              onClick={() => (auto ? !grp && A.openDrawer({ type: "node", name: m }) : A.selectMember(g.name, m))}
              onContextMenu={(e) => { if (!grp) { e.preventDefault(); A.openDrawer({ type: "node", name: m }); } }}>
              <span className="nm">{m}</span>
              {grp ? <span className="kind">组</span> : <Latency ms={r.delays[m]} alive={r.alive[m]} />}
            </button>
          );
        }) : <span className="muted">没有匹配的成员</span>}
      </div>
      {auto && <div className="auto-note">按测速自动选择 · 阈值 50 ms · 每 300 秒检测</div>}
    </div>
  );
}

export default function Proxies() {
  const p = prof();
  const groups = [...p.doc.groups, ...BUILTIN_TAIL];
  return (
    <>
      <div className="page-head">
        <h1>代理</h1><span className="muted">{groups.length} 个组 · {p.doc.nodes.length} 个节点 · 右键节点查看详情</span>
        <div className="tools">
          <input className="input" id="search" placeholder="筛选节点（Ctrl-F）" value={S.proxyQuery} style={{ width: 180 }}
            onChange={(e) => { S.proxyQuery = e.target.value; commit(); }} />
          <Seg value={S.proxySort} onChange={(v) => { S.proxySort = v; commit(); }} label="排序"
            options={[["config", "配置顺序"], ["delay", "延迟"], ["name", "名称"]]} />
          <button className="btn" onClick={A.updateSubs}>更新订阅</button>
          <button className="btn primary" onClick={A.testAll}>全部测速</button>
        </div>
      </div>
      {!isUp() && <div className="banner info">{p.name} 未连接，显示已保存的配置。切换选择会在下次连接时生效。</div>}
      {p.doc.general.standalone && <div className="banner info">独立模式：所有流量使用 {p.doc.general.standalone_outbound}，代理组仅用于测速与查看。</div>}
      <div className="groups">{groups.map((g) => <GroupCard key={g.name} g={g} />)}</div>
    </>
  );
}
