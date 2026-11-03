import { useEffect, useRef, useState } from "react";
import * as API from "../api";
import type { Artifact, ArtifactCommand } from "../generated/management";
import { Details } from "./common";
type Status = {
  host_version?: string;
  staged_release_id?: string;
  staged_compatible?: boolean;
  capabilities?: {
    activation_allowed?: boolean;
    activation_blocked_reason?: string;
    rollback_target?: string;
    recovery_operation_id?: string;
  };
  operation?: { phase: string; error?: string };
};
const CHUNK = 256 * 1024;
async function digest(bytes: ArrayBuffer) {
  return [...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes))]
    .map((v) => v.toString(16).padStart(2, "0"))
    .join("");
}
function base64(bytes: Uint8Array) {
  let text = "";
  for (let i = 0; i < bytes.length; i += 32768)
    text += String.fromCharCode(...bytes.subarray(i, i + 32768));
  return btoa(text);
}
export default function Updates({
  agent,
  online,
  onError,
  onExecute,
}: {
  agent: string;
  online: boolean;
  onError: (e: unknown) => void;
  onExecute: (
    type: string,
    data: unknown,
    danger?: string,
  ) => Promise<boolean | undefined>;
}) {
  const [status, setStatus] = useState<Status | null>(null);
  const [file, setFile] = useState<File | null>(null);
  const [artifact, setArtifact] = useState<Artifact | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const uploadId = useRef(crypto.randomUUID());
  const controller = useRef<AbortController | null>(null);
  const refresh = async (signal?: AbortSignal) => {
    const result = await API.query<Status>(
      agent,
      "update_status",
      undefined,
      signal,
    );
    if (!signal?.aborted) setStatus(result.data);
  };
  useEffect(() => {
    const abort = new AbortController();
    void refresh(abort.signal).catch(onError);
    return () => {
      abort.abort();
      controller.current?.abort();
    };
  }, [agent]);
  const transfer = (command: ArtifactCommand, signal?: AbortSignal) =>
    API.request<Artifact>(agent, { type: "artifacts", data: command }, signal);
  const upload = async () => {
    if (!file || busy) return;
    if (file.size > 256 * 1024 * 1024) {
      onError(
        new Error("请选择不超过 256 MiB 的版本包；节点可能配置更低的限制。"),
      );
      return;
    }
    const abort = new AbortController();
    controller.current = abort;
    setBusy(true);
    setMessage("正在计算完整包摘要…");
    try {
      const sha256 = await digest(await file.arrayBuffer());
      if (abort.signal.aborted) return;
      const begin = await transfer(
        {
          type: "begin",
          data: {
            id: uploadId.current,
            name: file.name,
            size: String(file.size),
            sha256,
          },
        },
        abort.signal,
      );
      let current = begin.data;
      setArtifact(current);
      for (let offset = Number(current.received); offset < file.size;) {
        const bytes = await file.slice(offset, offset + CHUNK).arrayBuffer();
        const reply = await transfer(
          {
            type: "write",
            data: {
              id: current.id,
              offset: String(offset),
              base64: base64(new Uint8Array(bytes)),
              sha256: await digest(bytes),
            },
          },
          abort.signal,
        );
        current = reply.data;
        setArtifact(current);
        const next = Number(current.received);
        if (next <= offset)
          throw new Error("节点没有确认上传偏移，已停止发送。");
        offset = next;
      }
      const finished = await transfer(
        { type: "finish", data: { id: current.id } },
        abort.signal,
      );
      setArtifact(finished.data);
      setMessage("上传和内容摘要已验证。导入版本包后仍需单独激活。");
    } catch (e) {
      if (!abort.signal.aborted) {
        setMessage(
          "上传尚未确认完成。保留同一文件后可继续上传，节点会核对已接收偏移。",
        );
        onError(e);
      }
    } finally {
      setBusy(false);
    }
  };
  const update = async (type: string, data: unknown, confirmation?: string) => {
    if (busy) return;
    setBusy(true);
    try {
      await onExecute(type, data, confirmation);
      await refresh();
    } catch (error) {
      onError(error);
    } finally {
      setBusy(false);
    }
  };
  const intent = (intent: unknown) => ({
    request: { operation_id: crypto.randomUUID(), intent },
  });
  return (
    <section className="card card-pad">
      <div className="page-head">
        <h2>版本与更新</h2>
        <button
          className="btn sm"
          disabled={!online || busy}
          onClick={() => refresh().catch(onError)}
        >
          刷新
        </button>
      </div>
      <p className="muted">
        核心更新通过 Agent 执行。Manager 与 Web
        的升级需要安装对应版本包并重启服务。
      </p>
      <div className="toolbar">
        <input
          aria-label="选择版本包"
          type="file"
          accept=".zip,.gz"
          disabled={busy || !online}
          onChange={(e) => {
            setFile(e.target.files?.[0] ?? null);
            uploadId.current = crypto.randomUUID();
            setArtifact(null);
            setMessage("");
          }}
        />
        <button
          className="btn"
          disabled={!file || busy || !online || artifact?.complete}
          onClick={() => void upload()}
        >
          上传 / 继续上传
        </button>
        {busy && (
          <button className="btn" onClick={() => controller.current?.abort()}>
            停止传输
          </button>
        )}
      </div>
      {artifact && (
        <p className="mono">
          节点已确认 {artifact.received} / {artifact.size} 字节{" "}
          {artifact.complete ? "· 摘要已验证" : ""}
        </p>
      )}
      {message && <p role="status">{message}</p>}
      <div className="toolbar">
        <button
          className="btn"
          disabled={!artifact?.complete || busy || !online}
          onClick={() =>
            void update("update_artifact", { artifact_id: artifact!.id })
          }
        >
          校验并导入版本包
        </button>
        <button
          className="btn primary"
          disabled={
            busy ||
            !online ||
            !status?.staged_release_id ||
            status.staged_compatible !== true ||
            !status.capabilities?.activation_allowed
          }
          title={status?.capabilities?.activation_blocked_reason ?? ""}
          onClick={() =>
            void update(
              "update",
              intent({
                kind: "activate",
                release_id: status!.staged_release_id,
              }),
              `在节点 ${agent} 激活版本 ${status!.staged_release_id}？`,
            )
          }
        >
          激活已暂存版本
        </button>
        <button
          className="btn"
          disabled={busy || !online || !status?.capabilities?.rollback_target}
          onClick={() =>
            void update(
              "update",
              intent({
                kind: "rollback",
                target_directory: status!.capabilities!.rollback_target,
              }),
              `将节点 ${agent} 回滚到状态报告中的上一版本？`,
            )
          }
        >
          回滚
        </button>
        {status?.capabilities?.recovery_operation_id && (
          <button
            className="btn"
            disabled={busy || !online}
            onClick={() =>
              void update("recover_update", {
                operation_id: status.capabilities!.recovery_operation_id,
              })
            }
          >
            恢复原更新操作
          </button>
        )}
      </div>
      {status?.capabilities?.activation_blocked_reason && (
        <p className="notice-bar">
          {status.capabilities.activation_blocked_reason}
        </p>
      )}
      <Details value={status} />
    </section>
  );
}
