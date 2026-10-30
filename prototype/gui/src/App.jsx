import { useEffect } from "react";
import { S, useApp } from "./store.js";
import * as A from "./actions.js";
import { Sidebar, TopBar, TaskBar, TaskCenter, Toasts, PAGES } from "./components/Shell.jsx";
import Drawer from "./components/Drawer.jsx";
import Overlay from "./components/Overlay.jsx";
import Overview from "./pages/Overview.jsx";
import Proxies from "./pages/Proxies.jsx";
import Connections from "./pages/Connections.jsx";
import Rules from "./pages/Rules.jsx";
import Logs from "./pages/Logs.jsx";
import Profiles from "./pages/Profiles.jsx";
import Settings from "./pages/Settings.jsx";

const VIEWS = { overview: Overview, proxies: Proxies, connections: Connections, rules: Rules, logs: Logs, profiles: Profiles, settings: Settings };

// GUI 快捷键：Ctrl 组合，避免与输入冲突；单字母键位属于 TUI，在命令面板中显示。
function onKey(e) {
  const mod = e.ctrlKey || e.metaKey;
  const key = e.key.toLowerCase();
  const typing = /INPUT|TEXTAREA|SELECT/.test(document.activeElement?.tagName);
  if (mod && key === "k") { e.preventDefault(); return S.overlay?.type === "palette" ? A.closeOverlay() : A.openOverlay({ type: "palette" }); }
  if (e.key === "Escape") {
    if (S.overlay) return A.closeOverlay();
    if (S.tasksOpen) return A.toggleTasks(false);
    if (S.drawer) return A.closeDrawer();
    if (S.editor && S.page === "profiles" && !S.editorHidden && !typing) return A.closeEditor();
    return;
  }
  if (S.overlay) return;
  if (e.key === "F6") { e.preventDefault(); return A.toggleTasks(); }
  if (mod && /^[1-6]$/.test(e.key)) { e.preventDefault(); return A.go(PAGES[+e.key - 1][0]); }
  if (mod && key === "f") {
    const el = document.getElementById("search") || document.getElementById("explain-in");
    if (el) { e.preventDefault(); el.focus(); el.select?.(); }
    return;
  }
  if (S.editor && S.page === "profiles" && !S.editorHidden) {
    if (mod && key === "s") { e.preventDefault(); return A.save(); }
    if (mod && key === "z" && !typing) { e.preventDefault(); return A.undo(); }
    if (mod && key === "y") { e.preventDefault(); return A.redo(); }
  }
}

export default function App() {
  useApp();
  useEffect(() => {
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);
  const View = VIEWS[S.page];
  return (
    <>
      <div className="app">
        <Sidebar />
        <TopBar />
        <main><div className="page"><View /></div></main>
        <TaskBar />
      </div>
      <Drawer />
      <TaskCenter />
      <Overlay />
      <Toasts />
    </>
  );
}
