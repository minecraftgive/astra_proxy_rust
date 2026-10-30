import { S, prof, rt, isUp } from "../store.js";
import * as A from "../actions.js";
import { exitChain } from "../model/routing.js";
import { bytes, rate, dur } from "../model/format.js";
import { ICONS, Latency, Switch } from "../components/ui.jsx";

const MODE_TEXT = { rule: "规则", global: "全局", direct: "直连" };
const NOTICE_ACTIONS = { updateSubs: A.updateSubs, editLab: () => A.openEditor("lab", "inbound") };

function TrafficChart() {
  const tr = S.traffic;
  const max = Math.max(1e6, ...tr.down, ...tr.up) * 1.15;
  const W = 600, T = 14, B = 136;
  const y = (v) => B - (v / max) * (B - T);
  const line = (arr) => arr.map((v, i) => `${i ? "L" : "M"}${((i / 59) * W).toFixed(1)},${y(v).toFixed(1)}`).join("");
  const area = (arr) => line(arr) + `L${W},${B}L0,${B}Z`;
  return (
    <svg className="chart" viewBox="0 0 600 150" preserveAspectRatio="none" role="img" aria-label="最近 60 秒流量趋势">
      {[max / 1.15, max / 2.3].map((t) => (
        <g key={t}>
          <line x1="0" x2={W} y1={y(t)} y2={y(t)} stroke="var(--border)" strokeDasharray="3 4" />
          <text x="4" y={y(t) - 4} fill="var(--faint)" fontSize="10" fontFamily="IBM Plex Mono, monospace">{rate(t)}</text>
        </g>
      ))}
      <line x1="0" x2={W} y1={B} y2={B} stroke="var(--border)" />
      <path d={area(tr.down)} fill="var(--down)" fillOpacity=".14" />
      <path d={line(tr.down)} fill="none" stroke="var(--down)" strokeWidth="1.8" vectorEffect="non-scaling-stroke" />
      <path d={area(tr.up)} fill="var(--up)" fillOpacity=".12" />
      <path d={line(tr.up)} fill="none" stroke="var(--up)" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
      <circle cx={W - 2} cy={y(tr.down[59])} r="3" fill="var(--down)" />
    </svg>
  );
}

function Takeover({ k, title, desc, badge }) {
  const up = isUp(), on = up && rt()[k];
  return (
    <div className={`card take ${on ? "on" : ""}`}>
      <div className="row">
        <h3>{title}</h3>{badge}
        <Switch on={on} onClick={() => A.toggleTakeover(k)} label={title} disabled={!up} title={up ? undefined : "先连接配置档"} />
      </div>
      <p>{desc}</p>
    </div>
  );
}

export default function Overview() {
  const p = prof(), r = rt(), up = isUp(), tr = S.traffic;
  const chain = exitChain(p, r);
  const last = chain[chain.length - 1];
  const E = S.editor;
  const notices = [
    ...(E && E.profile === p.name && E.dirty.size ? [{ id: "draft", sev: "info", text: `${p.name} 有 ${E.dirty.size} 处改动未保存`, sub: "保存后会自动热应用到运行中的代理端。", act: ["继续编辑", () => A.openEditor(p.name, E.section)] }] : []),
    ...S.notices,
  ];
  return (
    <>
      <div className="card hero">
        <div style={{ minWidth: 0 }}>
          <div className="state">
            <span className={`dot ${up ? "ok pulse" : p.state === "failed" ? "err" : ""}`} />
            {up ? "已连接" : p.state === "failed" ? "连接失败" : "未连接"}
            <span className="muted" style={{ fontSize: 14, fontWeight: 400 }}>{p.name}</span>
          </div>
          {up ? (
            <>
              <div className="chain" title="当前出口：沿代理组的选择链">
                {chain.map((c, i) => (
                  <span key={i} style={{ display: "contents" }}>
                    {i > 0 && <span className="sep">▸</span>}
                    {i === chain.length - 1
                      ? <button className="hop last" onClick={() => A.openDrawer({ type: "node", name: c })} style={{ border: 0 }}>{c}</button>
                      : <span className="hop">{c}</span>}
                  </span>
                ))}
                <Latency ms={r.delays[last]} alive={r.alive[last]} />
              </div>
              <div className="meta num">
                <span>{MODE_TEXT[r.mode]}模式{p.doc.general.standalone ? " · 独立模式" : ""}</span>
                <span>运行 {dur(Date.now() - p.since)}</span>
                <span>监听 {p.doc.inbound.listen}</span>
                <span>{p.rule ? `路由引擎 ${p.rule}` : "无路由引擎"}</span>
              </div>
            </>
          ) : (
            <div className="meta">
              {p.state === "failed"
                ? <span style={{ color: "var(--err)" }}>{p.error}</span>
                : <span>连接后流量将按{MODE_TEXT[r.mode]}模式转发。</span>}
            </div>
          )}
        </div>
        <div className="actions">
          <button className={`btn ${up ? "" : "primary"}`} onClick={() => A.toggleConnect()} disabled={p.busy}>
            {up ? "断开" : p.state === "failed" ? "重试连接" : "连接"}
          </button>
          {up && <button className="btn" onClick={A.testExit}>诊断出口</button>}
          {p.state === "failed" && <button className="btn" onClick={() => A.openEditor(p.name, "inbound")}>修改监听端口</button>}
        </div>
      </div>

      {notices.length > 0 && (
        <div className="card">
          {notices.map((n) => (
            <div className="notice" key={n.id}>
              <span className={`sev ${n.sev}`} />
              <div className="txt">{n.text}<small>{n.sub}</small></div>
              {n.act && <button className="btn sm" onClick={typeof n.act[1] === "function" ? n.act[1] : NOTICE_ACTIONS[n.act[1]]}>{n.act[0]}</button>}
            </div>
          ))}
        </div>
      )}

      <div>
        <h2 className="section-title">系统接管</h2>
        <div className="grid3">
          <Takeover k="sys" title="系统代理" desc="让浏览器等遵循系统代理设置的应用经过 Astra。关闭时恢复原设置。" />
          <Takeover k="tun" title="TUN" desc="接管整台电脑的流量，包括不支持代理的应用。使用虚拟网卡 astra0。"
            badge={<span className="pill warn" title="开启时会请求管理员权限">{ICONS.shield}管理员</span>} />
          <Takeover k="lan" title="允许局域网" desc="允许同一网络中的其他设备通过本机代理上网。需要非回环监听地址。" />
        </div>
      </div>

      <div className="card traffic">
        <div style={{ minWidth: 0 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
            <h2 className="section-title" style={{ margin: 0 }}>流量 · 最近 60 秒</h2>
            <div className="legend"><span><i style={{ background: "var(--down)" }} />下载</span><span><i style={{ background: "var(--up)" }} />上传</span></div>
          </div>
          <TrafficChart />
        </div>
        <div className="stats num">
          <div className="stat"><span className="muted">下载</span><b style={{ color: "var(--down)" }}>{rate(tr.down[59])}</b></div>
          <div className="stat"><span className="muted">上传</span><b style={{ color: "var(--up)" }}>{rate(tr.up[59])}</b></div>
          <div className="stat"><span className="muted">活动连接</span><b>{up ? S.conns.length : 0}</b></div>
          <div className="stat"><span className="muted">累计</span><b style={{ fontSize: 12.5 }}>↑{bytes(tr.totalUp)} ↓{bytes(tr.totalDown)}</b></div>
          <button className="btn sm" onClick={() => A.go("connections")} style={{ alignSelf: "flex-start" }}>查看连接</button>
        </div>
      </div>
    </>
  );
}
