export function bytes(n) {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < 4) {
    n /= 1024;
    i++;
  }
  return (i ? n.toFixed(n < 10 ? 2 : n < 100 ? 1 : 0) : Math.round(n)) + " " + u[i];
}
export const rate = (n) => bytes(n) + "/s";
export function dur(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), x = s % 60;
  return h ? `${h}h${String(m).padStart(2, "0")}m` : m ? `${m}m${String(x).padStart(2, "0")}s` : `${x}s`;
}
export const clock = (t = Date.now()) => new Date(t).toTimeString().slice(0, 8);
export const rnd = (a, b) => a + Math.random() * (b - a);
export const pick = (a) => a[Math.floor(Math.random() * a.length)];
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
