// 单一可变状态 + 版本号订阅：模拟前端从 manager 事件流得到的 AppState。
// 任何修改后调用 commit()，组件通过 useApp() 重新渲染。
import { useSyncExternalStore } from "react";
import { makeDoc, RULES, clone } from "./model/data.js";

const runtime = (over = {}) => ({
  mode: "rule", sys: false, tun: false, lan: false,
  selected: { PROXY: "自动-香港", "手动-日本": "jp-01", main: "sg-01" },
  delays: {}, alive: {}, testing: {}, ...over,
});

export const S = {
  page: "overview",
  active: "home",
  profiles: [
    { name: "home", state: "running", routing: "rule", rule: "rule-main", since: Date.now() - 7980e3, doc: makeDoc() },
    {
      name: "work", state: "stopped", routing: "standalone", rule: null, since: 0,
      doc: makeDoc({
        general: { standalone: true, standalone_outbound: "sg-01", log_level: "info", watch_config: false },
        inbound: { listen: "127.0.0.1:7891", auth_user: "", auth_pass: "" },
      }),
    },
    {
      name: "lab", state: "failed", routing: "rule", rule: "rule-main", since: 0,
      error: "监听 127.0.0.1:7892 失败：端口已被占用（PID 18240 · node.exe）",
      doc: makeDoc({ inbound: { listen: "127.0.0.1:7892", auth_user: "", auth_pass: "" } }),
    },
  ],
  rules: clone(RULES),
  runtime: {
    home: runtime({
      sys: true,
      delays: { "hk-01": 112, "hk-02": 86, "hk-03": 141, "jp-01": 168, "sg-01": 74, "us-01": 391 },
      alive: { "hk-01": true, "hk-02": true, "hk-03": true, "jp-01": true, "jp-02": false, "sg-01": true, "us-01": true },
    }),
    work: runtime(),
    lab: runtime(),
  },
  traffic: { up: Array(60).fill(0), down: Array(60).fill(0), totalUp: 398e6, totalDown: 2.27e9 },
  conns: [], connSeq: 1, connFilter: "", frozen: false, selConn: null,
  proxyQuery: "", proxySort: "config",
  logs: { src: "proxy", show: "INFO", record: "INFO", q: "", paused: false, wrap: false, lines: { proxy: [], rule: [], manager: [] } },
  explain: { input: "www.youtube.com:443 tcp chrome.exe", result: null },
  tasks: [], taskSeq: 1, tasksOpen: false,
  notices: [],
  editor: null,
  drawer: null,
  overlay: null,
  toasts: [], toastSeq: 1,
  diag: {},
  theme: "system", tray: "minimize",
};
export const newRuntime = runtime;

let version = 0;
const listeners = new Set();
export function commit() {
  version++;
  listeners.forEach((l) => l());
}
const subscribe = (cb) => {
  listeners.add(cb);
  return () => listeners.delete(cb);
};
export function useApp() {
  useSyncExternalStore(subscribe, () => version);
  return S;
}

export const prof = (n = S.active) => S.profiles.find((p) => p.name === n);
export const rt = (n = S.active) => S.runtime[n];
export const isUp = (n = S.active) => prof(n)?.state === "running";
