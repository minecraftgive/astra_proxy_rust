// 浮层：确认、命令面板、配置档菜单、新建向导、UAC、导入。同一时间只显示一个。
import { useEffect, useRef, useState } from "react";
import { S, prof } from "../store.js";
import * as A from "../actions.js";
import { COMMANDS, labelOf, whenOf } from "../commands.js";
import { Dot, stateText } from "./ui.jsx";

function Backdrop({ children, transparent }) {
  return (
    <div className="overlay" style={transparent ? { background: "transparent", padding: 0 } : undefined}
      onMouseDown={(e) => e.target === e.currentTarget && A.closeOverlay()}>
      {children}
    </div>
  );
}

function Confirm({ title, text, ok, fn }) {
  return (
    <Backdrop>
      <div className="modal" role="dialog" aria-modal="true" aria-label={title}>
        <div className="modal-head">{title}</div>
        <div className="modal-body"><p style={{ margin: 0 }}>{text}</p></div>
        <div className="modal-foot">
          <button className="btn" onClick={A.closeOverlay} autoFocus>取消</button>
          <button className="btn danger-solid" onClick={() => { A.closeOverlay(); fn(); }}>{ok}</button>
        </div>
      </div>
    </Backdrop>
  );
}

function Palette() {
  const [q, setQ] = useState("");
  const [i, setI] = useState(0);
  const list = useRef(null);
  const items = COMMANDS.filter((c) => !q || (labelOf(c) + c.scope + (c.kbd || "")).toLowerCase().includes(q.toLowerCase()));
  const idx = Math.min(i, Math.max(0, items.length - 1));
  const exec = (c) => {
    const w = whenOf(c);
    if (w !== true) return A.toast(w);
    A.closeOverlay();
    c.run();
  };
  useEffect(() => { list.current?.querySelector(".on")?.scrollIntoView({ block: "nearest" }); }, [idx]);
  return (
    <Backdrop>
      <div className="modal palette" role="dialog" aria-modal="true" aria-label="命令面板">
        <input autoFocus placeholder="输入命令，例如 TUN、测速、日志…" value={q} autoComplete="off"
          onChange={(e) => { setQ(e.target.value); setI(0); }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") { e.preventDefault(); setI(Math.min(idx + 1, items.length - 1)); }
            if (e.key === "ArrowUp") { e.preventDefault(); setI(Math.max(0, idx - 1)); }
            if (e.key === "Enter" && items[idx]) { e.preventDefault(); exec(items[idx]); }
          }} />
        <div className="plist" ref={list}>
          {items.length ? items.map((c, k) => {
            const w = whenOf(c);
            return (
              <div key={k} className={`pitem ${k === idx ? "on" : ""} ${w !== true ? "dis" : ""}`} onMouseEnter={() => setI(k)} onClick={() => exec(c)}>
                <span className="sc">{c.scope}</span>
                <span style={c.danger ? { color: "var(--err)" } : undefined}>{labelOf(c)}</span>
                {w !== true && <span className="why">· {w}</span>}
                {c.kbd && <kbd>{c.kbd}</kbd>}
              </div>
            );
          }) : <div className="empty">没有匹配的命令</div>}
        </div>
        <div className="modal-foot" style={{ justifyContent: "space-between", fontSize: 12 }}>
          <span className="muted">右侧是 TUI 中同一命令的键位</span><span className="faint">↑↓ 选择 · Enter 执行 · Esc 关闭</span>
        </div>
      </div>
    </Backdrop>
  );
}

function Menu({ kind, name, x, y }) {
  const style = { left: Math.max(8, Math.min(x, innerWidth - 280)), top: Math.min(y, innerHeight - 240) };
  if (kind === "profiles") {
    return (
      <Backdrop transparent>
        <div className="menu" style={style} role="menu">
          {S.profiles.map((p) => (
            <button key={p.name} role="menuitem" onClick={() => A.setActive(p.name)}>
              <Dot state={p.state} /><b style={{ fontWeight: p.name === S.active ? 600 : 400 }}>{p.name}</b>
              <span className="faint" style={{ marginLeft: "auto" }}>{stateText(p)}</span>
            </button>
          ))}
          <hr />
          <button role="menuitem" onClick={() => A.openOverlay({ type: "wizard" })}>＋ 新建 / 导入配置档…</button>
          <button role="menuitem" onClick={() => A.go("profiles")}>管理配置档</button>
        </div>
      </Backdrop>
    );
  }
  const p = prof(name);
  const close = (fn) => () => { A.closeOverlay(); fn(); };
  return (
    <Backdrop transparent>
      <div className="menu" style={style} role="menu">
        <button onClick={close(() => A.openEditor(name, "advanced"))}>配置文件与路由引擎…</button>
        <button onClick={close(() => A.task(`校验 ${name}`, ["astra-proxy --check", ...(p.rule ? [`astra-rule --check（${p.rule}）`] : []), "通过"]))}>校验配置</button>
        <button onClick={close(() => { S.active = name; S.logs.src = "proxy"; A.go("logs"); })}>查看日志</button>
        {p.state === "running" && <button onClick={() => A.restart(name)}>完整重启</button>}
        <hr />
        <button style={{ color: "var(--err)" }} disabled={p.state === "running"} title={p.state === "running" ? "先断开" : undefined}
          onClick={close(() => A.removeProfile(name))}>移除配置档…</button>
      </div>
    </Backdrop>
  );
}

const SOURCES = [
  ["sub", "订阅链接", "粘贴机场或自建服务提供的订阅地址"],
  ["file", "文件", "Astra TOML 或 Mihomo / Clash YAML，可拖放到此处"],
  ["link", "分享链接", "vless://、trojan://、ss:// 等，一行一个"],
  ["blank", "空白", "从默认模板开始，稍后添加节点"],
];

function Wizard() {
  const [f, setF] = useState({ name: "", source: "sub", content: "", routing: "rule" });
  const [err, setErr] = useState("");
  const set = (k) => (e) => setF({ ...f, [k]: e.target.value });
  const submit = (e) => {
    e.preventDefault();
    const name = f.name.trim(), content = f.content.trim();
    if (!/^[a-z0-9_-]{1,24}$/.test(name)) return setErr("名称只能包含 a-z、0-9、- 和 _，长度 1–24");
    if (prof(name)) return setErr(`已存在名为 ${name} 的配置档`);
    if (f.source !== "blank" && !content) return setErr(`请填写${SOURCES.find((s) => s[0] === f.source)[1]}`);
    A.createProfile({ ...f, name, content });
  };
  return (
    <Backdrop>
      <form className="modal" role="dialog" aria-modal="true" aria-label="新建配置档" onSubmit={submit}
        onDragOver={(e) => e.preventDefault()}
        onDrop={(e) => { e.preventDefault(); const file = e.dataTransfer.files[0]; if (file) setF({ ...f, source: "file", content: file.name }); }}>
        <div className="modal-head">新建配置档</div>
        <div className="modal-body">
          <div>
            <label className="lbl" htmlFor="wz-name">名称</label>
            <input className="input" id="wz-name" style={{ width: "100%" }} value={f.name} onChange={set("name")} placeholder="例如 home" autoFocus autoComplete="off" />
            <div className="faint" style={{ fontSize: 12, marginTop: 4 }}>1–24 个字符：a-z、0-9、- 和 _</div>
          </div>
          <div>
            <span className="lbl">来源</span>
            <div className="radio-row">
              {SOURCES.map(([k, l, h]) => (
                <label key={k} className={`radio ${f.source === k ? "on" : ""}`}>
                  <input type="radio" name="wz-src" value={k} checked={f.source === k} onChange={set("source")} />
                  <span>{l}<small>{h}</small></span>
                </label>
              ))}
            </div>
          </div>
          {f.source !== "blank" && (
            <div>
              <label className="lbl" htmlFor="wz-content">{SOURCES.find((s) => s[0] === f.source)[1]}{f.source === "file" && "路径"}</label>
              {f.source === "link"
                ? <textarea className="input mono" id="wz-content" rows={3} style={{ width: "100%", resize: "vertical" }} value={f.content} onChange={set("content")} />
                : <input className="input mono" id="wz-content" style={{ width: "100%" }} value={f.content} onChange={set("content")} autoComplete="off"
                    placeholder={f.source === "sub" ? "https://" : "D:\\Downloads\\config.yaml"} />}
            </div>
          )}
          <div>
            <span className="lbl">分流</span>
            <div className="radio-row">
              <label className={`radio ${f.routing === "rule" ? "on" : ""}`}>
                <input type="radio" name="wz-rt" value="rule" checked={f.routing === "rule"} onChange={set("routing")} />
                <span>规则模式（推荐）<small>按规则分流，国内直连。自动复用路由引擎 rule-main，并分配不冲突的端口。</small></span>
              </label>
              <label className={`radio ${f.routing === "standalone" ? "on" : ""}`}>
                <input type="radio" name="wz-rt" value="standalone" checked={f.routing === "standalone"} onChange={set("routing")} />
                <span>独立模式<small>所有流量走同一个出口，不启动路由引擎。</small></span>
              </label>
            </div>
          </div>
          {err && <div style={{ color: "var(--err)" }} role="alert">{err}</div>}
        </div>
        <div className="modal-foot">
          <button type="button" className="btn" onClick={A.closeOverlay}>取消</button>
          <button className="btn primary">创建并设为当前</button>
        </div>
      </form>
    </Backdrop>
  );
}

function Uac() {
  return (
    <Backdrop>
      <div className="modal uac" role="dialog" aria-modal="true" aria-label="需要管理员权限">
        <div className="modal-head">需要管理员权限</div>
        <div className="modal-body">
          <p style={{ margin: 0 }}>开启 TUN 需要以管理员身份重启 <b>{S.active}</b> 的代理端。Windows 将弹出用户账户控制（UAC）确认。</p>
          <p className="muted" style={{ margin: 0 }}>只重启代理端，路由引擎和现有设置保持不变；取消时恢复原状态。</p>
          <p className="faint" style={{ margin: 0, fontSize: 12 }}>原型中以下两个按钮模拟 UAC 的结果。</p>
        </div>
        <div className="modal-foot">
          <button className="btn" onClick={() => A.resolveUac(false)}>取消（模拟拒绝）</button>
          <button className="btn primary" onClick={() => A.resolveUac(true)} autoFocus>继续（模拟批准）</button>
        </div>
      </div>
    </Backdrop>
  );
}

function Import() {
  const [text, setText] = useState("");
  const run = () => {
    A.closeOverlay();
    A.structural("导入节点", (E) => {
      E.draft.nodes.push({ name: "kr-01", type: "trojan", server: "kr1.example.net", port: 443, password: "imported-sample", sni: "kr1.example.net", transport: "tcp", security: "tls" });
      E.dirty.add("nodes.import");
    });
    A.toast("已导入 1 个节点到草稿：kr-01", ["撤销", A.undo]);
  };
  return (
    <Backdrop>
      <div className="modal" role="dialog" aria-modal="true" aria-label="导入到草稿">
        <div className="modal-head">导入到草稿</div>
        <div className="modal-body">
          <p className="muted" style={{ margin: 0 }}>粘贴分享链接，或拖放 TOML / Mihomo YAML 文件。导入只修改当前草稿，保存时才校验并写入。</p>
          <textarea className="input mono" rows={4} placeholder="trojan://…" autoFocus value={text} onChange={(e) => setText(e.target.value)} />
        </div>
        <div className="modal-foot"><button className="btn" onClick={A.closeOverlay}>取消</button><button className="btn primary" onClick={run}>导入</button></div>
      </div>
    </Backdrop>
  );
}

export default function Overlay() {
  const o = S.overlay;
  if (!o) return null;
  switch (o.type) {
    case "confirm": return <Confirm {...o} />;
    case "palette": return <Palette />;
    case "menu": return <Menu {...o} />;
    case "wizard": return <Wizard />;
    case "uac": return <Uac />;
    case "import": return <Import />;
    default: return null;
  }
}
