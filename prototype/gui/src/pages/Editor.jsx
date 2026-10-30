// 配置档编辑器：左侧分区导航，右侧由字段 Schema 生成表单。
import { S, prof } from "../store.js";
import * as A from "../actions.js";
import { SCHEMA, SECTIONS, RULE_TYPES, getP, fieldError, allErrors } from "../model/schema.js";
import { ICONS, Switch } from "../components/ui.jsx";

const NUMERIC = new Set(["port", "num"]);

function Field({ spec, path, scope }) {
  const E = S.editor;
  const value = getP(E.draft, path);
  const error = fieldError(spec, value, E.draft, scope);
  const id = "f-" + path.replace(/\W/g, "_");
  const set = (v) => A.editField(path, v);
  let control;
  switch (spec.t) {
    case "bool":
      control = <><Switch on={value} onClick={() => set(!value)} label={spec.l} /><span className="muted">{value ? "开启" : "关闭"}</span></>;
      break;
    case "choice":
    case "ref": {
      const choices = spec.t === "ref" ? [...E.draft.nodes.map((n) => n.name), ...E.draft.groups.map((g) => g.name), "DIRECT"] : spec.c;
      control = <select className="input" id={id} value={value} onChange={(e) => set(e.target.value)}>{choices.map((c) => <option key={c}>{c}</option>)}</select>;
      break;
    }
    case "secret": {
      const shown = E.reveal.has(path);
      control = <>
        <input className="input mono" id={id} type={shown ? "text" : "password"} value={value} autoComplete="off" onChange={(e) => set(e.target.value)} />
        <button type="button" className="btn sm" onClick={() => A.toggleSet(E.reveal, path)} title="F2 显示/隐藏">{shown ? "隐藏" : "显示"}</button>
      </>;
      break;
    }
    default:
      control = <input className={`input ${spec.t === "text" ? "" : "mono"}`} id={id} value={value} autoComplete="off"
        inputMode={NUMERIC.has(spec.t) ? "numeric" : undefined}
        onChange={(e) => { const v = e.target.value; set(NUMERIC.has(spec.t) && v !== "" && !isNaN(+v) ? +v : v); }} />;
  }
  return (
    <div className={`field ${E.dirty.has(path) ? "dirty" : ""} ${error ? "bad" : ""}`} data-field={path}>
      <label htmlFor={id}>{spec.l}{spec.req && <span style={{ color: "var(--err)" }}> *</span>}</label>
      <div className="ctl">{control}</div>
      {error ? <div className="error">{error}</div> : spec.h ? <div className="help">{spec.h}</div> : null}
    </div>
  );
}

function Entry({ path, title, summary, children, actions, lock }) {
  const E = S.editor;
  const open = E.open.has(path);
  return (
    <div className={`entry ${open ? "open" : ""}`}>
      <div className="entry-head" onClick={(e) => !e.target.closest("button") && A.toggleSet(E.open, path)}
        role="button" tabIndex={0} aria-expanded={open}
        onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && e.target === e.currentTarget && (e.preventDefault(), A.toggleSet(E.open, path))}>
        <span className="chev">{ICONS.chev}</span><b>{title}</b><span className="sum">{summary}</span>
        {lock ? <span className="pill" title={lock}>内置</span> : actions}
      </div>
      {open && <div className="entry-body">{children}</div>}
    </div>
  );
}

const fields = (list, prefix, item, scope) =>
  list.filter((s) => !s.show || s.show(item)).map((s) => <Field key={s.k} spec={s} path={`${prefix}${s.k}`} scope={scope} />);

function NodesSection() {
  const E = S.editor;
  const add = () => A.structural("添加节点", (E) => {
    E.draft.nodes.push({ name: `new-${E.draft.nodes.length + 1}`, type: "vless", server: "", port: 443, password: "", sni: "", transport: "tcp", security: "tls" });
    const p = `nodes.${E.draft.nodes.length - 1}`;
    E.open.add(p);
    E.dirty.add(p + ".name");
  });
  const dup = (i) => A.structural("复制节点", (E) => {
    const n = JSON.parse(JSON.stringify(E.draft.nodes[i]));
    let k = 2;
    while (E.draft.nodes.some((x) => x.name === `${n.name}-${k}`)) k++;
    n.name = `${n.name}-${k}`;
    E.draft.nodes.splice(i + 1, 0, n);
    E.dirty.add(`nodes.${i + 1}.name`);
  });
  return (
    <>
      <div className="row-tools">
        <span className="muted">代理组和规则按名称引用节点；改名会同步更新引用。</span>
        <button className="btn sm" onClick={() => A.openOverlay({ type: "import" })}>导入链接 / 文件</button>
        <button className="btn sm primary" onClick={add}>添加节点</button>
      </div>
      <div className="card">
        {E.draft.nodes.length ? E.draft.nodes.map((n, i) => (
          <Entry key={i} path={`nodes.${i}`} title={n.name || "（未命名）"}
            summary={`${n.type} · ${n.server || "?"}:${n.port}${n.security !== "none" ? " · " + n.security : ""}`}
            actions={<>
              <button className="btn ghost sm" onClick={() => dup(i)} title="复制（y）">复制</button>
              <button className="btn ghost sm" onClick={() => A.deleteEntry("nodes", i)} title="删除（d，可撤销）">删除</button>
            </>}>
            {fields(SCHEMA.node, `nodes.${i}.`, n, "nodes")}
          </Entry>
        )) : <div className="empty">还没有节点。添加节点，或导入分享链接与订阅。</div>}
      </div>
    </>
  );
}

function SubsSection() {
  const E = S.editor;
  const add = () => A.structural("添加订阅", (E) => {
    E.draft.subs.push({ name: `sub${E.draft.subs.length + 1}`, url: "", interval: 86400, join_proxy: true });
    const p = `subs.${E.draft.subs.length - 1}`;
    E.open.add(p);
    E.dirty.add(p + ".url");
  });
  return (
    <>
      <div className="row-tools"><span className="muted">每个订阅的节点自动组成同名代理组。</span><button className="btn sm primary" onClick={add}>添加订阅</button></div>
      <div className="card">
        {E.draft.subs.length ? E.draft.subs.map((s, i) => (
          <Entry key={i} path={`subs.${i}`} title={s.name} summary={`每 ${Math.round(s.interval / 3600)} 小时 · ${s.join_proxy ? "加入 PROXY" : "不加入 PROXY"}`}
            actions={<button className="btn ghost sm" onClick={() => A.deleteEntry("subs", i)}>删除</button>}>
            {fields(SCHEMA.sub, `subs.${i}.`, s)}
          </Entry>
        )) : <div className="empty">没有订阅</div>}
      </div>
    </>
  );
}

function GroupsSection() {
  const E = S.editor, d = E.draft;
  return (
    <div className="card">
      {d.groups.map((g, i) => {
        const candidates = [...d.nodes.map((n) => n.name), ...d.groups.filter((x) => x.name !== g.name).map((x) => x.name), "DIRECT"];
        return (
          <Entry key={i} path={`groups.${i}`} title={g.name} summary={`${g.mode === "auto" ? "自动测速" : "手动选择"} · ${g.members.length} 个成员`}
            lock={g.builtin ? "内置组，不能改名或删除" : null}
            actions={<button className="btn ghost sm" onClick={() => A.deleteEntry("groups", i)}>删除</button>}>
            <div className="field">
              <label>选择方式</label>
              <div className="ctl"><div className="seg">
                {[["select", "手动"], ["auto", "自动测速"]].map(([k, l]) => (
                  <button key={k} type="button" className={g.mode === k ? "on" : ""}
                    onClick={() => A.structural("修改组模式", (E) => { E.draft.groups[i].mode = k; E.dirty.add(`groups.${i}.mode`); })}>{l}</button>
                ))}
              </div></div>
            </div>
            <div className="field">
              <label>成员</label>
              <div className="ctl"><div className="chips">
                {candidates.map((m) => {
                  const on = g.members.includes(m);
                  return (
                    <button key={m} type="button" className={`chip ${on ? "on" : ""}`} aria-pressed={on}
                      onClick={() => A.structural("修改成员", (E) => {
                        const gg = E.draft.groups[i];
                        gg.members = on ? gg.members.filter((x) => x !== m) : [...gg.members, m];
                        if (!gg.members.length) gg.members = [m];
                        E.dirty.add(`groups.${i}.members`);
                      })}>{on ? "✓ " : ""}{m}</button>
                  );
                })}
              </div></div>
              <div className="help">点击勾选，顺序即添加顺序。不允许循环引用，成员不能为空。</div>
            </div>
          </Entry>
        );
      })}
    </div>
  );
}

function RulesSection({ p }) {
  const E = S.editor, d = E.draft;
  if (p.routing !== "rule" || d.general.standalone) {
    return <div className="card empty"><b>独立模式不使用规则</b>在“常规”中关闭独立模式后，可为此配置档选择路由引擎。</div>;
  }
  const shared = S.profiles.filter((x) => x.rule === p.rule).map((x) => x.name);
  const edit = (i, k, v) => A.structural("修改规则", (E) => { E.rules[i][k] = v; E.dirty.add(`rules.${i}`); });
  const move = (i, dlt) => A.structural("调整规则顺序", (E) => { const [r] = E.rules.splice(i, 1); E.rules.splice(i + dlt, 0, r); E.dirty.add("rules.order"); });
  const actions = ["direct", "block", ...d.groups.map((g) => "proxy:" + g.name)];
  return (
    <>
      {shared.length > 1 && <div className="banner">这些规则属于路由引擎 {p.rule}，由 {shared.join("、")} 共用；保存后对它们同时生效。</div>}
      <div className="row-tools">
        <span className="muted">按顺序匹配，第一条命中的规则生效。</span>
        <button className="btn sm primary" onClick={() => A.structural("添加规则", (E) => { E.rules.push({ type: "domain_suffix", value: "", action: "proxy:PROXY", sec: "域名" }); E.dirty.add(`rules.${E.rules.length - 1}`); })}>添加规则</button>
      </div>
      <div className="card">
        {E.rules.map((r, i) => (
          <div className="rule-edit" key={i}>
            <span className="faint mono" style={{ textAlign: "right" }}>{i + 1}</span>
            <select className="input typ" value={r.type} onChange={(e) => edit(i, "type", e.target.value)} aria-label="类型">
              {RULE_TYPES.map((t) => <option key={t}>{t}</option>)}
            </select>
            <input className="input mono" value={r.value} onChange={(e) => edit(i, "value", e.target.value)} aria-label="匹配内容" />
            <select className="input act" value={r.action} onChange={(e) => edit(i, "action", e.target.value)} aria-label="动作">
              {actions.map((a) => <option key={a} value={a}>{a === "direct" ? "DIRECT" : a === "block" ? "REJECT" : a.slice(6)}</option>)}
            </select>
            <span style={{ display: "flex", gap: 2 }}>
              <button className="btn ghost sm" disabled={!i} onClick={() => move(i, -1)} title="上移（K）" aria-label="上移">↑</button>
              <button className="btn ghost sm" disabled={i === E.rules.length - 1} onClick={() => move(i, 1)} title="下移（J）" aria-label="下移">↓</button>
              <button className="btn ghost sm" title="删除（d，可撤销）"
                onClick={() => { A.structural("删除规则", (E) => { E.rules.splice(i, 1); E.dirty.add("rules.del"); }); A.toast(`已删除规则 #${i + 1}`, ["撤销", A.undo]); }}>删除</button>
            </span>
          </div>
        ))}
      </div>
    </>
  );
}

function AdvancedSection({ p }) {
  const row = (label, control, help) => (
    <div className="field"><label>{label}</label><div className="ctl">{control}</div>{help && <div className="help">{help}</div>}</div>
  );
  return (
    <div className="card form">
      {row("配置文件", <>
        <span className="mono" style={{ wordBreak: "break-all" }}>%LOCALAPPDATA%\.astra\config\configs\{p.name}.toml</span>
        <button className="btn sm" onClick={() => A.toast("原型：打开文件选择器，校验后切换")}>更换…</button>
        <button className="btn sm" onClick={() => A.toast(`原型：复制到同目录的 ${p.name}-2.toml`)}>复制</button>
      </>, "保存前会备份到状态目录 backups/。")}
      {row("路由引擎", <select className="input" defaultValue={p.rule || ""}><option value={p.rule || ""}>{p.rule || "无（独立模式）"}</option><option>新建路由引擎…</option></select>,
        "连接时会先启动所选路由引擎；端点地址自动匹配，不需要手填 rule_socket。")}
      {row("外部编辑器", <button className="btn sm" onClick={() => A.toast("原型：用 ASTRA_EDITOR 打开草稿副本，关闭后读回")}>用外部编辑器打开</button>,
        "打开当前草稿的副本；关闭编辑器后读回，再保存。")}
      {row("移除配置档", <button className="btn sm danger" onClick={() => A.removeProfile(p.name)} disabled={p.state === "running"} title={p.state === "running" ? "先断开" : undefined}>移除 {p.name}…</button>,
        "只移除注册信息，配置文件保留，可以再导入。")}
    </div>
  );
}

export default function Editor() {
  const E = S.editor, d = E.draft, p = prof(E.profile);
  const errors = allErrors(d);
  const errorSections = new Set(errors.map((e) => e[0]));
  const counts = { nodes: d.nodes.length, groups: d.groups.length, subs: d.subs.length, rules: E.rules.length };
  const sec = E.section;
  let body;
  if (["general", "inbound", "takeover", "dns"].includes(sec)) body = <div className="card form">{fields(SCHEMA[sec], "", d)}</div>;
  else if (sec === "nodes") body = <NodesSection />;
  else if (sec === "subs") body = <SubsSection />;
  else if (sec === "groups") body = <GroupsSection />;
  else if (sec === "rules") body = <RulesSection p={p} />;
  else body = <AdvancedSection p={p} />;
  const n = E.dirty.size;
  return (
    <>
      <div className="page-head">
        <button className="btn ghost sm" onClick={A.closeEditor} title="Esc">← 配置档</button>
        <h1>编辑 {p.name}</h1>
        {p.state === "running" ? <span className="pill ok">运行中 · 保存后自动应用</span> : <span className="pill">已停止</span>}
        <div className="tools">
          <button className="btn sm" onClick={A.undo} disabled={!E.undo.length} title="撤销（Ctrl-Z）">撤销</button>
          <button className="btn sm" onClick={A.redo} disabled={!E.redo.length} title="重做（Ctrl-Y）">重做</button>
        </div>
      </div>
      <div className="editor">
        <nav className="card sections" aria-label="分区">
          {SECTIONS.map(([k, l]) => (
            <button key={k} className={sec === k ? "on" : ""} onClick={() => A.setSection(k)}>
              <span>{l}{k === "rules" && p.rule && <span className="faint"> ⇢ {p.rule}</span>}</span>
              {errorSections.has(k) ? <span className="err-dot" title="有错误" /> : counts[k] != null && <span className="cnt">{counts[k]}</span>}
            </button>
          ))}
        </nav>
        <div className="editor-main">
          {body}
          <div className="savebar">
            <span className="grow">
              {n ? <><b>{n}</b> 处改动未保存</> : <span className="muted">没有未保存的改动</span>}
              {errors.length > 0 && <> · <button className="linklike" onClick={A.nextError}>{errors.length} 个错误</button></>}
            </span>
            <button className="btn" onClick={A.discard} disabled={!n}>放弃</button>
            <button className="btn primary" onClick={A.save} disabled={!n || errors.length > 0} title="Ctrl-S">{p.state === "running" ? "保存并应用" : "保存"}</button>
          </div>
        </div>
      </div>
    </>
  );
}
