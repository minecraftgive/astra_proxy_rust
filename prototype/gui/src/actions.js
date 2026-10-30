// 所有用户操作。每个函数对应设计中的一个命令；真实实现会改为发送 ManagerCommand。
import { S, commit, prof, rt, isUp, newRuntime } from "./store.js";
import { clone, makeDoc } from "./model/data.js";
import { groupOf, exitChain } from "./model/routing.js";
import { getP, setP, allErrors } from "./model/schema.js";
import { sleep, rnd } from "./model/format.js";
import { newConn } from "./sim.js";

/* ---------- 提示、浮层、抽屉 ---------- */
export function toast(text, action) {
  const id = S.toastSeq++;
  S.toasts = [...S.toasts.slice(-2), { id, text, action }];
  commit();
  setTimeout(() => dismissToast(id), action ? 6000 : 2600);
}
export function dismissToast(id) {
  S.toasts = S.toasts.filter((t) => t.id !== id);
  commit();
}
export function openOverlay(o) { S.overlay = o; commit(); }
export function closeOverlay() { S.overlay = null; commit(); }
export function confirm(title, text, ok, fn) { openOverlay({ type: "confirm", title, text, ok, fn }); }
export function openDrawer(d) { S.drawer = d; if (d.type === "conn") S.selConn = d.id; commit(); }
export function closeDrawer() { S.drawer = null; S.selConn = null; commit(); }
export function toggleTasks(v = !S.tasksOpen) { S.tasksOpen = v; commit(); }

/* ---------- 任务（模拟 manager 推送的 Task 事件） ---------- */
export async function task(title, stages, opt = {}) {
  const t = { id: S.taskSeq++, title, stages: stages.map((s) => ({ s, st: "wait" })), state: "run", start: Date.now() };
  S.tasks = [t, ...S.tasks].slice(0, 32);
  commit();
  for (let i = 0; i < t.stages.length; i++) {
    t.stages[i].st = "run";
    commit();
    await sleep(opt.step ?? rnd(350, 750));
    if (opt.failAt === i) {
      t.stages[i].st = "fail";
      t.stages[i].s += "：" + opt.error;
      t.state = "fail";
      t.end = Date.now();
      commit();
      throw new Error(opt.error);
    }
    t.stages[i].st = "done";
    commit();
  }
  t.state = "ok";
  t.end = Date.now();
  commit();
  return t;
}

/* ---------- 导航与配置档 ---------- */
export function go(page) {
  S.page = page;
  S.drawer = null;
  S.selConn = null;
  S.overlay = null;
  commit();
  document.querySelector("main")?.scrollTo(0, 0);
}
export function setActive(name) {
  S.active = name;
  S.overlay = null;
  S.conns = isUp(name) ? Array.from({ length: 18 }, newConn) : [];
  commit();
  toast(`当前配置档：${name}`);
}

export async function toggleConnect(name = S.active) {
  const p = prof(name);
  if (p.busy) return;
  p.busy = true;
  commit();
  try {
    if (p.state === "running") {
      const others = S.profiles.filter((x) => x.rule && x.rule === p.rule && x.state === "running" && x !== p).length;
      await task(`断开 ${name}`, [
        "停止接受新连接", "等待现有会话结束", "停止代理端",
        ...(p.rule ? [others ? `保留路由引擎 ${p.rule}（仍被 ${others} 个配置档使用）` : `停止路由引擎 ${p.rule}`] : []),
      ]);
      p.state = "stopped";
      if (name === S.active) S.conns = [];
      toast(`${name} 已断开`);
    } else {
      const portBusy = name === "lab" && p.doc.inbound.listen.endsWith(":7892");
      await task(
        `连接 ${name}`,
        [...(p.rule ? [`启动路由引擎 ${p.rule}`] : []), "校验配置", "启动代理端", "等待控制端口就绪"],
        portBusy ? { failAt: p.rule ? 2 : 1, error: "端口 7892 已被占用" } : {},
      );
      p.state = "running";
      p.since = Date.now();
      p.error = null;
      S.notices = S.notices.filter((n) => n.profile !== name);
      if (name === S.active) S.conns = Array.from({ length: 12 }, newConn);
      toast(`${name} 已连接`);
    }
  } catch (e) {
    p.state = "failed";
    p.error = `监听 ${p.doc.inbound.listen} 失败：端口已被占用（PID 18240 · node.exe）`;
    toast(`${name} 连接失败：${e.message}`);
  } finally {
    p.busy = false;
    commit();
  }
}
export async function applyChanges() {
  await task(`应用 ${S.active}`, ["校验配置", "热重载代理端", "已应用"]);
  toast("改动已应用");
}
export async function restart(name = S.active) {
  S.overlay = null;
  await task(`重启 ${name}`, ["停止代理端", "启动代理端", "恢复运行状态"]);
  prof(name).since = Date.now();
  commit();
  toast("已完整重启");
}
export function setMode(m) {
  rt().mode = m;
  commit();
  toast({ rule: "规则模式：按规则分流", global: "全局模式：全部经 PROXY 组", direct: "直连模式：全部经 DIRECT 组" }[m]);
}
export function cycleMode() {
  const order = ["rule", "global", "direct"];
  setMode(order[(order.indexOf(rt().mode) + 1) % 3]);
}
export function resetRuntime() {
  Object.assign(rt(), { mode: "rule", sys: false, tun: false, lan: false });
  commit();
  toast("运行状态已重置：规则模式，接管全部关闭");
}

/* ---------- 系统接管 ---------- */
export function toggleTakeover(k) {
  const r = rt();
  if (!isUp()) return toast("先连接配置档，再开启接管");
  if (k === "tun" && !r.tun) return openOverlay({ type: "uac" });
  r[k] = !r[k];
  commit();
  toast({ sys: r.sys ? "系统代理已开启" : "系统代理已关闭", tun: "TUN 已关闭", lan: r.lan ? "已允许局域网设备连接" : "已禁止局域网设备连接" }[k]);
}
export async function resolveUac(ok) {
  closeOverlay();
  if (!ok) {
    await task("开启 TUN", ["请求管理员权限"], { failAt: 0, error: "已取消 UAC" }).catch(() => {});
    S.notices = [{ id: "tun", sev: "warn", text: "TUN 未开启：管理员权限请求被取消", sub: "其他设置没有改变。", act: ["重试", () => toggleTakeover("tun")] }, ...S.notices.filter((n) => n.id !== "tun")];
    return commit();
  }
  await task("开启 TUN", ["请求管理员权限", "以管理员身份重启代理端", "创建 Wintun 网卡 astra0", "写入路由（自动物理出口：以太网）", "已启用"]);
  rt().tun = true;
  S.notices = S.notices.filter((n) => n.id !== "tun");
  commit();
  toast("TUN 已开启");
}

/* ---------- 代理 ---------- */
export function selectMember(group, member) {
  const g = groupOf(prof().doc, group);
  if (g.mode === "auto") return toast("自动组按测速结果选择成员");
  rt().selected[group] = member;
  commit();
  toast(`${group} → ${member}`);
}
export async function testGroup(name) {
  const doc = prof().doc, r = rt();
  const g = groupOf(doc, name);
  if (!g || r.testing[name]) return;
  const targets = g.members.filter((m) => !groupOf(doc, m) && m !== "DIRECT" && m !== "REJECT");
  if (!targets.length) return;
  r.testing[name] = true;
  const t = { id: S.taskSeq++, title: `测速 ${name}`, stages: [{ s: `0/${targets.length}`, st: "run" }], state: "run", start: Date.now() };
  S.tasks = [t, ...S.tasks].slice(0, 32);
  commit();
  let done = 0;
  await Promise.all(targets.map(async (m) => {
    await sleep(rnd(250, 1600));
    const ok = Math.random() > (m === "jp-02" ? 0.7 : 0.06);
    r.alive[m] = ok;
    if (ok) r.delays[m] = Math.round(m.startsWith("us") ? rnd(280, 460) : m.startsWith("sg") ? rnd(60, 110) : rnd(70, 220));
    else delete r.delays[m];
    done++;
    t.stages[0].s = `${done}/${targets.length}`;
    commit();
  }));
  t.stages[0].st = "done";
  t.state = "ok";
  t.end = Date.now();
  r.testing[name] = false;
  commit();
}
export function testAll() {
  if (S.page !== "proxies") go("proxies");
  prof().doc.groups.forEach((g) => testGroup(g.name));
}
export async function diagnose(node) {
  S.diag[node] = { state: "run" };
  commit();
  await sleep(rnd(300, 800));
  if (rt().alive[node] === false || Math.random() < 0.05) {
    S.diag[node] = { state: "fail", why: "TCP 握手超时（3 秒）" };
    return commit();
  }
  const tcp = Math.round(rnd(40, 160));
  S.diag[node] = { state: "run", tcp };
  commit();
  await sleep(rnd(400, 900));
  const region = { hk: "香港", jp: "日本", sg: "新加坡", us: "美国" }[node.slice(0, 2)] || "—";
  S.diag[node] = { state: "ok", tcp, ip: `203.0.113.${(node.charCodeAt(0) * 7 + node.charCodeAt(3)) % 250}`, region };
  commit();
}
export function testExit() {
  const c = exitChain(prof(), rt());
  const node = c[c.length - 1];
  openDrawer({ type: "node", name: node });
  diagnose(node);
}
export async function updateSubs() {
  await task(`更新订阅 · ${S.active}`, ["下载 main（sub.example.net）", "解析 7 个节点", "合并并校验配置", isUp() ? "热应用到代理端" : "写入节点缓存"]);
  S.notices = S.notices.filter((n) => n.id !== "sub");
  commit();
  toast("订阅 main 已更新：7 个节点，无变化");
}

/* ---------- 连接 ---------- */
export function closeConn(id) {
  const c = S.conns.find((x) => x.id === id);
  S.conns = S.conns.filter((x) => x.id !== id);
  commit();
  if (c) toast(`已关闭 ${c.host}`);
}
export function closeAllConns() {
  if (!S.conns.length) return toast("没有活动连接");
  confirm("关闭全部连接？", `将关闭 ${S.conns.length} 个 TCP/UDP 连接，应用会自行重连。`, "全部关闭", () => {
    S.conns = [];
    commit();
    toast("已关闭全部连接");
  });
}

/* ---------- 规则 ---------- */
export function runExplain(input) {
  S.explain.input = input;
  const parts = input.trim().match(/"[^"]+"|\S+/g) || [];
  const target = parts[0] || "";
  const host = target.startsWith("[") ? target.slice(1, target.indexOf("]")) : target.replace(/:\d+$/, "");
  if (!host) return;
  const proc = parts.slice(1).find((x) => !/^(tcp|udp)$/i.test(x))?.replace(/"/g, "").split(/[\\/]/).pop();
  S.explain.result = { host, proc };
  S.explain.focus = null;
  commit();
}

/* ---------- 停止全部 ---------- */
export function stopAll() {
  confirm("停止全部？", "先停止所有代理端，再停止路由引擎与后台服务。界面保持打开，可以随时重新连接。", "停止全部", async () => {
    const n = S.profiles.filter((p) => p.state === "running").length;
    await task("停止全部", [`停止代理端 ×${n}`, "停止路由引擎", "停止后台服务"]);
    S.profiles.forEach((p) => p.state === "running" && (p.state = "stopped"));
    S.conns = [];
    commit();
  });
}
export function removeProfile(name) {
  confirm(`移除配置档 ${name}？`, "只移除注册信息，配置文件会保留，之后可以重新导入。", "移除", () => {
    S.profiles = S.profiles.filter((p) => p.name !== name);
    if (S.editor?.profile === name) S.editor = null;
    if (S.active === name) S.active = S.profiles[0]?.name;
    go("profiles");
    toast(`已移除 ${name}`);
  });
}

/* ---------- 新建 / 导入 ---------- */
export async function createProfile({ name, source, content, routing }) {
  closeOverlay();
  const port = 7890 + S.profiles.length + 3;
  await task(`创建 ${name}`, [
    `生成 configs/${name}.toml`,
    ...(source === "sub" ? ["下载订阅（sub.example.net）", "解析 7 个节点"] : source === "blank" ? [] : ["转换并检查（跳过 0 项）"]),
    routing === "rule" ? "关联路由引擎 rule-main" : "独立模式：不使用路由引擎",
    `分配监听端口 127.0.0.1:${port}`,
    "校验配置",
  ]);
  const p = { name, state: "stopped", routing, rule: routing === "rule" ? "rule-main" : null, since: 0, doc: makeDoc({ inbound: { listen: "127.0.0.1:" + port, auth_user: "", auth_pass: "" } }) };
  if (routing === "standalone") p.doc.general = { standalone: true, standalone_outbound: "PROXY", log_level: "info", watch_config: false };
  if (source === "blank") {
    p.doc.nodes = [];
    p.doc.groups = [{ name: "PROXY", mode: "select", members: ["DIRECT"], builtin: true }];
    p.doc.subs = [];
  }
  void content;
  S.profiles.push(p);
  S.runtime[name] = newRuntime({ selected: {} });
  S.active = name;
  S.page = "overview";
  S.conns = [];
  commit();
  toast(`${name} 已创建，点击“连接”开始使用`);
}

/* ---------- 编辑器 ---------- */
const snapshot = (E) => ({ draft: clone(E.draft), rules: clone(E.rules), dirty: [...E.dirty] });

export function openEditor(name = S.active, section = "general", openPath) {
  if (!S.editor || S.editor.profile !== name) {
    S.editor = { profile: name, section, draft: clone(prof(name).doc), rules: clone(S.rules), dirty: new Set(), undo: [], redo: [], open: new Set(), reveal: new Set() };
  } else S.editor.section = section;
  if (openPath) S.editor.open.add(openPath);
  S.editorHidden = false;
  go("profiles");
}
export function closeEditor() {
  const E = S.editor;
  if (E.dirty.size) {
    toast(`草稿已保留（${E.dirty.size} 处改动），可随时回来继续`);
    S.editorHidden = true;
    return go("overview");
  }
  S.editor = null;
  commit();
}
export function setSection(sec) {
  S.editor.section = sec;
  commit();
  document.querySelector("main")?.scrollTo(0, 0);
}
export function toggleSet(set, key) {
  set.has(key) ? set.delete(key) : set.add(key);
  commit();
}

function renameRefs(draft, from, to) {
  for (const g of draft.groups) g.members = g.members.map((m) => (m === from ? to : m));
  if (draft.general.standalone_outbound === from) draft.general.standalone_outbound = to;
}
export function editField(path, value) {
  const E = S.editor;
  const old = getP(E.draft, path);
  if (old === value) return;
  E.undo.push({ path, old, value, wasDirty: E.dirty.has(path) });
  E.redo = [];
  setP(E.draft, path, value);
  if (getP(prof(E.profile).doc, path) === value) E.dirty.delete(path);
  else E.dirty.add(path);
  if (/^nodes\.\d+\.name$/.test(path)) renameRefs(E.draft, old, value);
  commit();
}
export function structural(label, mutate) {
  const E = S.editor;
  const before = snapshot(E);
  mutate(E);
  E.undo.push({ snap: before, label });
  E.redo = [];
  commit();
}
export function undo() {
  const E = S.editor;
  const u = E?.undo.pop();
  if (!u) return;
  if (u.snap) {
    E.redo.push({ snap: snapshot(E), label: u.label });
    Object.assign(E, { draft: u.snap.draft, rules: u.snap.rules, dirty: new Set(u.snap.dirty) });
  } else {
    E.redo.push(u);
    setP(E.draft, u.path, u.old);
    if (/^nodes\.\d+\.name$/.test(u.path)) renameRefs(E.draft, u.value, u.old);
    u.wasDirty ? E.dirty.add(u.path) : E.dirty.delete(u.path);
  }
  commit();
  toast("已撤销" + (u.label ? "：" + u.label : ""));
}
export function redo() {
  const E = S.editor;
  const u = E?.redo.pop();
  if (!u) return;
  if (u.snap) {
    E.undo.push({ snap: snapshot(E), label: u.label });
    Object.assign(E, { draft: u.snap.draft, rules: u.snap.rules, dirty: new Set(u.snap.dirty) });
  } else {
    E.undo.push(u);
    setP(E.draft, u.path, u.value);
    if (/^nodes\.\d+\.name$/.test(u.path)) renameRefs(E.draft, u.old, u.value);
    E.dirty.add(u.path);
  }
  commit();
}
export function deleteEntry(kind, i) {
  const E = S.editor;
  const item = E.draft[kind][i];
  const used = kind === "nodes" ? E.draft.groups.filter((g) => g.members.includes(item.name)).map((g) => g.name) : [];
  structural(`删除 ${item.name}`, (E) => {
    E.draft[kind].splice(i, 1);
    if (kind === "nodes") E.draft.groups.forEach((g) => (g.members = g.members.filter((m) => m !== item.name)));
    E.dirty.add(kind);
  });
  toast(used.length ? `已删除 ${item.name}，并从 ${used.join("、")} 中移除` : `已删除 ${item.name}`, ["撤销", undo]);
}
export async function save() {
  const E = S.editor;
  if (!E || !E.dirty.size || allErrors(E.draft).length) return;
  const p = prof(E.profile);
  const rulesChanged = JSON.stringify(E.rules) !== JSON.stringify(S.rules);
  await task(`保存 ${p.name}`, [
    "校验代理配置（astra-proxy --check-stdin）",
    ...(rulesChanged ? [`校验规则（${p.rule}）`] : []),
    `备份并写入 ${p.name}.toml`,
    ...(p.state === "running" ? ["热应用到代理端", ...(rulesChanged ? ["热应用到路由引擎"] : [])] : []),
  ]);
  p.doc = clone(E.draft);
  if (rulesChanged) S.rules = clone(E.rules);
  E.dirty.clear();
  E.undo = [];
  E.redo = [];
  if (p.state === "failed" && !p.doc.inbound.listen.endsWith(":7892")) {
    p.error = "配置已修改，点击“重试连接”";
    S.notices = S.notices.filter((n) => n.profile !== p.name);
  }
  commit();
  toast(p.state === "running" ? "已保存并应用" : "已保存");
}
export function discard() {
  const E = S.editor;
  confirm("放弃改动？", `将丢弃 ${E.dirty.size} 处未保存的改动，恢复为已保存的配置。`, "放弃", () => {
    const { profile, section } = E;
    S.editor = null;
    openEditor(profile, section);
    toast("已放弃改动");
  });
}
export function nextError() {
  const E = S.editor;
  const errs = allErrors(E.draft);
  if (!errs.length) return;
  const [sec, path] = errs[0];
  E.section = sec;
  const m = path.match(/^(nodes|subs)\.(\d+)/);
  if (m) E.open.add(`${m[1]}.${m[2]}`);
  commit();
  setTimeout(() => {
    const w = document.querySelector(`[data-field="${CSS.escape(path)}"]`);
    w?.scrollIntoView({ block: "center" });
    w?.querySelector("input,select")?.focus();
  }, 30);
}

/* ---------- 主题 ---------- */
export function setTheme(t) {
  S.theme = t;
  if (t === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = t;
  try { localStorage.setItem("astra-theme", t); } catch { /* 无痕模式等 */ }
  commit();
}
export function toggleTheme() {
  const dark = document.documentElement.dataset.theme === "dark" || (!document.documentElement.dataset.theme && matchMedia("(prefers-color-scheme: dark)").matches);
  setTheme(dark ? "light" : "dark");
}
