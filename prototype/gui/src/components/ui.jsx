// 小型通用组件。
export const ICONS = {
  overview: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><circle cx="10" cy="10" r="7" /><path d="M10 10l3.5-3.5" /><circle cx="10" cy="10" r="1.2" fill="currentColor" /></svg>,
  proxies: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><path d="M3 6h11M11 3l3 3-3 3M17 14H6M9 11l-3 3 3 3" /></svg>,
  connections: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><path d="M3 5h14M3 10h14M3 15h9" /></svg>,
  rules: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><path d="M4 3v14M4 4h10l-2 3 2 3H4" /></svg>,
  logs: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><rect x="3" y="3" width="14" height="14" rx="2" /><path d="M6 8l2 2-2 2M10 12h4" /></svg>,
  profiles: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><rect x="3" y="4" width="14" height="5" rx="1.5" /><rect x="3" y="11" width="14" height="5" rx="1.5" /></svg>,
  settings: <svg viewBox="0 0 20 20" fill="none" stroke="currentColor" strokeWidth="1.6"><circle cx="10" cy="10" r="2.5" /><path d="M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M4.7 15.3l1.4-1.4M13.9 6.1l1.4-1.4" /></svg>,
  chev: <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M6 4l4 4-4 4" /></svg>,
  shield: <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" strokeWidth="1.5"><path d="M8 1.5l5 2v4c0 3-2.2 5.4-5 6.5-2.8-1.1-5-3.5-5-6.5v-4z" /></svg>,
};

export function Latency({ ms, alive }) {
  if (alive === false) return <span className="lat r">✗ 超时</span>;
  if (ms == null) return <span className="lat n">—</span>;
  return <span className={`lat ${ms < 150 ? "g" : ms < 400 ? "y" : "r"}`}>{ms} ms</span>;
}

export const Dot = ({ state, pulse }) => (
  <span className={`dot ${state === "running" ? "ok" : state === "failed" ? "err" : ""} ${pulse ? "pulse" : ""}`} />
);

export function Switch({ on, onClick, label, disabled, title }) {
  return <button type="button" className={`switch ${on ? "on" : ""}`} onClick={onClick} aria-pressed={!!on} aria-label={label} disabled={disabled} title={title} />;
}

export function Seg({ value, options, onChange, label }) {
  return (
    <div className="seg" role="group" aria-label={label}>
      {options.map(([k, l, extra]) => (
        <button type="button" key={k} className={value === k ? "on" : ""} onClick={() => onChange(k)} {...extra}>{l}</button>
      ))}
    </div>
  );
}

export function ActionPill({ action }) {
  if (action === "direct") return <span className="pill">DIRECT</span>;
  if (action === "block") return <span className="pill err">REJECT</span>;
  return <span className="pill acc">{action.slice(6)}</span>;
}

export const stateText = (p) => (p.state === "running" ? "运行中" : p.state === "failed" ? "失败" : "已停止");
