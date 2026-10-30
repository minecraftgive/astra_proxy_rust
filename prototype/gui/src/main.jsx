import { createRoot } from "react-dom/client";
import App from "./App.jsx";
import { S } from "./store.js";
import { seed, tick } from "./sim.js";
import "./styles.css";

try {
  const t = localStorage.getItem("astra-theme");
  if (t && t !== "system") {
    S.theme = t;
    document.documentElement.dataset.theme = t;
  }
} catch {
  // 无法读取本地存储时使用系统主题。
}

seed();
setInterval(tick, 1000);
createRoot(document.getElementById("root")).render(<App />);
