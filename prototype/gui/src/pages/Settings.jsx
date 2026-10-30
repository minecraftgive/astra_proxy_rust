import { S, commit } from "../store.js";
import * as A from "../actions.js";
import { Seg } from "../components/ui.jsx";

const Row = ({ label, children, help }) => (
  <div className="field"><label>{label}</label><div className="ctl">{children}</div>{help && <div className="help">{help}</div>}</div>
);

export default function Settings() {
  return (
    <>
      <div className="page-head"><h1>设置</h1></div>
      <div className="card form">
        <Row label="语言" help="只影响界面文字，配置字段与节点名称保持原样。">
          <select className="input" defaultValue="zh-CN"><option value="zh-CN">简体中文</option><option value="zh-TW">繁體中文</option><option value="en">English</option></select>
        </Row>
        <Row label="外观">
          <Seg value={S.theme} onChange={A.setTheme} label="外观" options={[["system", "跟随系统"], ["light", "浅色"], ["dark", "深色"]]} />
        </Row>
        <Row label="关闭窗口时" help="两种方式都不会停止代理；要停止请使用“停止全部”。">
          <Seg value={S.tray} onChange={(v) => { S.tray = v; commit(); }} label="关闭窗口时" options={[["minimize", "最小化到托盘"], ["quit", "退出界面"]]} />
        </Row>
      </div>
      <h2 className="section-title" style={{ marginTop: 8 }}>后台服务</h2>
      <div className="card form">
        <Row label="状态"><span className="dot ok" /> 运行中 · v0.2.0 · PID 9132 · 已连接界面：GUI、TUI</Row>
        <Row label="目录" help="可通过 --config-dir、--state-dir 与 ASTRA_RUNTIME_DIR 修改。">
          <div className="mono" style={{ display: "grid", gap: 2, wordBreak: "break-all" }}>
            <span>配置 %LOCALAPPDATA%\.astra\config</span><span>状态 %LOCALAPPDATA%\.astra\tui</span><span>缓存 %LOCALAPPDATA%\.astra\runtime</span>
          </div>
        </Row>
        <Row label="维护">
          <button className="btn sm" onClick={() => A.toast("原型：重启后台服务（会先正常停止核心）")}>重启后台服务</button>
          <button className="btn sm danger" onClick={A.stopAll}>停止全部…</button>
        </Row>
      </div>
    </>
  );
}
