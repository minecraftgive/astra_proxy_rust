import { Component, type ErrorInfo, type ReactNode } from "react";

// Browser policy/quota failures must not prevent opening the management UI.
export function readTheme(): string {
  try {
    const theme = localStorage.getItem("astra-theme");
    return theme && ["light", "dark", "system"].includes(theme)
      ? theme
      : "system";
  } catch {
    return "system";
  }
}
export function retainTheme(theme: string): void {
  try {
    localStorage.setItem("astra-theme", theme);
  } catch {
    /* Keep the in-memory selection. */
  }
}

export class ErrorBoundary extends Component<
  { children: ReactNode },
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(_error: Error, _info: ErrorInfo) {
    // Do not log component state: configuration documents can contain credentials.
  }
  render() {
    if (!this.state.failed) return this.props.children;
    return (
      <main className="card" role="alert">
        <h1>界面遇到错误</h1>
        <p>
          后台实例继续运行。已保存的草稿恢复副本仍保留；重新打开后请查询未确认的操作，勿重复提交。
        </p>
        <button className="btn" onClick={() => location.reload()}>
          重新打开界面
        </button>
      </main>
    );
  }
}
