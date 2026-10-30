// 模拟 manager 事件流：每秒推送流量、连接与日志。真实实现中由 Subscribe 事件替代。
import { S, commit, prof, rt, isUp } from "./store.js";
import { HOSTS } from "./model/data.js";
import { outboundFor, groupOf, selectedOf } from "./model/routing.js";
import { bytes, dur, clock, rnd, pick } from "./model/format.js";

export function newConn() {
  const [host, port, net, proc] = pick(HOSTS);
  const ob = outboundFor(prof(), rt(), S.rules, host, proc);
  return {
    id: S.connSeq++, host, port, net, proc,
    src: "127.0.0.1:" + Math.floor(rnd(50000, 65000)),
    inbound: rt().tun && Math.random() > 0.5 ? "tun" : "mixed",
    chain: ob.chain, why: ob.why,
    up: rnd(500, 8000), down: rnd(2000, 90000), start: Date.now() - rnd(0, 300e3), weight: rnd(0.2, 1),
  };
}

function logLine(src) {
  const c = S.conns.length ? pick(S.conns) : newConn();
  const route = S.logs.record !== "INFO";
  const lv = Math.random() < 0.03 ? "ERROR" : pick(route ? ["ROUTE", "ROUTE", "ROUTE", "INFO", "INFO", "WARN"] : ["INFO", "INFO", "INFO", "INFO", "WARN"]);
  const p = prof();
  const auto = groupOf(p.doc, "自动-香港");
  const autoSel = auto ? selectedOf(auto, rt()) : "—";
  const msgs = {
    proxy: {
      ROUTE: `route ${c.net} ${c.inbound} ${c.src} -> ${c.host}:${c.port} via ${c.chain.join(">")} rule=${c.why.split(" ▸ ")[0]}`,
      INFO: pick([
        `group 自动-香港 auto selected ${autoSel} (${rt().delays[autoSel] ?? "—"} ms)`,
        `connection closed ${c.host}:${c.port} up=${bytes(c.up)} down=${bytes(c.down)} elapsed=${dur(Date.now() - c.start)}`,
        "subscription main cached 7 nodes",
        `inbound mixed listening on ${p.doc.inbound.listen}`,
      ]),
      WARN: pick(["outbound jp-02 handshake timeout after 3000 ms", `udp association idle timeout ${c.src}`]),
      ERROR: "outbound us-01 tls: peer closed connection without close_notify",
    },
    rule: {
      ROUTE: `received peer=proxy#1 req=${S.connSeq} ${c.host}:${c.port} ${c.net} process=${c.proc} → decision revision=14 path=${c.why.replace(/ ▸ /g, " -> ")} → responded elapsed_us=${Math.floor(rnd(8, 60))}`,
      INFO: pick(["rule set ads loaded 12 entries", "geosite cache valid (geosite/geosite.dat, 6.1 MB)", "revision 14 active, 8 rules, 41203 compiled"]),
      WARN: "geoip download skipped: cache fresh",
      ERROR: "route query decode_failed conn=7",
    },
    manager: {
      ROUTE: `task #${S.taskSeq} stage update`,
      INFO: pick([`watch runtime ${p.name} interval=500ms`, "client connected (astra-gui 0.2.0)", "client connected (astra-tui 0.2.0)", `instance ${p.name} status ${p.state}`]),
      WARN: "instance lab exited with status 1",
      ERROR: "instance lab: bind 127.0.0.1:7892: address in use",
    },
  };
  return { t: clock(), lv, msg: msgs[src][lv], c };
}

export function seed() {
  for (let i = 0; i < 26; i++) S.conns.push(newConn());
  for (let i = 0; i < 60; i++) {
    S.traffic.up[i] = rnd(40e3, 300e3);
    S.traffic.down[i] = rnd(0.6e6, 7e6) * (0.6 + Math.sin(i / 7) * 0.4);
  }
  for (const s of ["proxy", "rule", "manager"]) for (let i = 0; i < 80; i++) S.logs.lines[s].push(logLine(s));
  S.notices = [
    { id: "sub", sev: "warn", text: "订阅 main 已 18 小时未更新", sub: "上次更新 10-29 17:42 · 每 24 小时自动更新", act: ["立即更新", "updateSubs"] },
    { id: "lab", profile: "lab", sev: "err", text: "配置档 lab 启动失败", sub: "监听 127.0.0.1:7892 失败：端口已被占用（PID 18240 · node.exe）", act: ["修改监听端口", "editLab"] },
  ];
}

export function tick() {
  const up = isUp();
  const tr = S.traffic;
  const nu = up ? Math.min(900e3, Math.max(5e3, tr.up[59] * rnd(0.7, 1.35) + rnd(-20e3, 20e3))) : 0;
  const nd = up ? Math.max(30e3, Math.min(12e6, (tr.down[59] || 2e6) * rnd(0.72, 1.32))) : 0;
  tr.up = [...tr.up.slice(1), nu];
  tr.down = [...tr.down.slice(1), nd];
  tr.totalUp += nu;
  tr.totalDown += nd;
  if (up && !S.frozen) {
    for (const c of S.conns) {
      c.up += (nu / S.conns.length) * c.weight;
      c.down += (nd / S.conns.length) * c.weight;
    }
    if (Math.random() < 0.5 && S.conns.length) S.conns.splice(Math.floor(rnd(0, S.conns.length)), 1);
    if (Math.random() < 0.6 && S.conns.length < 60) S.conns.unshift(newConn());
  }
  if (up && !S.logs.paused) {
    for (const s of ["proxy", "rule", "manager"]) {
      if (Math.random() < (s === "manager" ? 0.15 : 0.8)) {
        S.logs.lines[s].push(logLine(s));
        if (S.logs.lines[s].length > 400) S.logs.lines[s].shift();
      }
    }
  }
  commit();
}
