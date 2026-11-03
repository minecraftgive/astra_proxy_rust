import { useState } from "react";
import * as API from "../api";
export default function Login({ onLogin }: { onLogin: () => void }) {
  const [password, setPassword] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <main className="login-page">
      <form
        className="card login-card"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError("");
          try {
            await API.login(password);
            setPassword("");
            onLogin();
          } catch (e) {
            setError(String(e));
          } finally {
            setBusy(false);
          }
        }}
      >
        <div className="brand">
          <span className="brand-mark" />
          Astra
        </div>
        <h1>登录管理控制台</h1>
        <p className="muted">
          使用此网页端点的管理员密码。节点连接凭据由管理服务单独保管。
        </p>
        <label htmlFor="password">管理员密码</label>
        <input
          id="password"
          type="password"
          autoComplete="current-password"
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
          className="input"
          autoFocus
        />
        {error && (
          <p role="alert" className="error-message">
            {error}
          </p>
        )}
        <button className="btn primary" disabled={busy}>
          {busy ? "正在验证…" : "登录"}
        </button>
        <small className="muted">
          尚未设置密码时，请在主机上执行管理员初始化命令。
        </small>
      </form>
    </main>
  );
}
