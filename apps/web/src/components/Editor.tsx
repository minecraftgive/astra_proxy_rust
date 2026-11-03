import { useEffect, useRef, useState } from "react";
import * as API from "../api";
import { recover, retain } from "../recovery";
import SchemaEditor from "./SchemaEditor";
import type { EditCommand, SaveOutcome } from "../generated/management";
function saveDescription(result: SaveOutcome): string {
  const labels: Record<SaveOutcome["state"], string> = {
    applied: "配置保存完成 · 已应用到运行实例",
    next_start: "配置保存完成 · 下次启动时生效",
    rejected: "保存被拒绝 · 草稿仍保留",
    restored: "应用失败 · 已恢复原配置",
    recovery_failed: "应用失败且恢复未完成 · 需要恢复处理",
    outcome_unknown: "结果尚未确认 · 请查询原任务，勿重复提交",
  };
  return `${labels[result.state]}。${result.message}${result.recovery ? ` · ${result.recovery}` : ""} · 任务 ${result.operation_id}`;
}
type EditorProps = {
  agent: string;
  instance: API.Instance;
  onClose: () => void;
  onSaved: () => void;
  onError: (e: unknown) => void;
};
export default function Editor(props: EditorProps) {
  return (
    <DocumentEditor key={`${props.agent}/${props.instance.id}`} {...props} />
  );
}
function DocumentEditor({
  agent,
  instance,
  onClose,
  onSaved,
  onError,
}: EditorProps) {
  const [draft, setDraft] = useState<API.Draft | null>(null);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const [mode, setMode] = useState<"source" | "fields">("source");
  const [notice, setNotice] = useState("");
  const [pendingSave, setPendingSave] = useState<string>();
  const pendingIdentity = useRef<string>();
  const [recovered, setRecovered] = useState<{
    text: string;
    revision: string;
    draftId: string;
  } | null>(null);
  const undo = useRef<string[]>([]);
  const redo = useRef<string[]>([]);
  const newest = useRef("");
  const mounted = useRef(false);
  const draftIdentity = useRef(crypto.randomUUID());
  const key = `${location.origin}/${agent}/${instance.id}`;
  useEffect(() => {
    let active = true;
    mounted.current = true;
    setDraft(null);
    setText("");
    setNotice("");
    const load = async () => {
      const saved = await recover(key).catch(() => undefined);
      let r;
      if (saved) {
        try {
          r = await API.draftCommand(agent, "read", {
            draft_id: saved.draftId,
          });
        } catch (e) {
          if (e instanceof API.ApiError && e.code === "not_found")
            setNotice(
              "节点上的原草稿已不存在。恢复副本仍保留，请对照源配置后合并。",
            );
          else throw e;
        }
      }
      r ??= await API.draftCommand(agent, "open", {
        instance: instance.id,
        draft_id: draftIdentity.current,
      });
      if (r.data.instance !== instance.id)
        throw new Error("恢复草稿绑定的实例已改变，已拒绝加载。");
      return { r, saved };
    };
    load()
      .then(({ r, saved }) => {
        if (!active) return;
        setDraft(r.data);
        setText(r.data.text);
        newest.current = r.data.text;
        pendingIdentity.current = saved?.saveOperationId;
        setPendingSave(saved?.saveOperationId);
        if (saved && saved.text !== r.data.text) setRecovered(saved);
        if (!saved)
          return retain(key, {
            text: r.data.text,
            revision: r.data.revision,
            draftId: r.data.id,
            original: r.data.original,
            sourceRevision: r.data.source_revision,
          });
      })
      .catch((error) => {
        if (active) onError(error);
      });
    return () => {
      active = false;
      mounted.current = false;
    };
  }, [agent, instance.id, key]);
  const edit = (value: string) => {
    undo.current.push(newest.current);
    if (undo.current.length > 200) undo.current.shift();
    redo.current = [];
    newest.current = value;
    setText(value);
    if (draft)
      retain(key, {
        text: value,
        revision: draft.revision,
        draftId: draft.id,
        original: draft.original,
        sourceRevision: draft.source_revision,
        saveOperationId: pendingIdentity.current,
      }).catch(() => setNotice("浏览器恢复副本未能保存，请保持此窗口打开。"));
  };
  const history = (back: boolean) => {
    const from = back ? undo.current : redo.current;
    const to = back ? redo.current : undo.current;
    const value = from.pop();
    if (value !== undefined) {
      to.push(newest.current);
      newest.current = value;
      setText(value);
      if (draft)
        void retain(key, {
          text: value,
          revision: draft.revision,
          draftId: draft.id,
          original: draft.original,
          sourceRevision: draft.source_revision,
          saveOperationId: pendingIdentity.current,
        }).catch(onError);
    }
  };
  const save = async () => {
    if (!draft || busy || pendingIdentity.current) return;
    setBusy(true);
    const submitted = newest.current;
    let saveSent = false;
    let baseline = draft;
    try {
      const updated = await API.draftCommand(agent, "edit", {
        draft_id: draft.id,
        expected_revision: draft.revision,
        text: submitted,
      });
      if (mounted.current) setDraft(updated.data);
      baseline = updated.data;
      const operationId = crypto.randomUUID();
      pendingIdentity.current = operationId;
      if (mounted.current) setPendingSave(operationId);
      // Do not send a save until its identity can survive closing this page.
      await retain(key, {
        text: newest.current,
        revision: updated.data.revision,
        draftId: draft.id,
        original: updated.data.original,
        sourceRevision: updated.data.source_revision,
        saveOperationId: operationId,
      });
      saveSent = true;
      const result = await API.draftCommand(agent, "save", {
        draft_id: draft.id,
        expected_revision: updated.data.revision,
        expected_source_revision: updated.data.source_revision,
        operation_id: operationId,
      });
      if (result.type === "draft") {
        baseline = result.data;
        if (
          !result.data.last_save ||
          !["outcome_unknown", "recovery_failed"].includes(
            result.data.last_save.state,
          )
        ) {
          pendingIdentity.current = undefined;
          if (mounted.current) setPendingSave(undefined);
        }
        if (mounted.current) setDraft(result.data);
        if (mounted.current && newest.current === submitted) {
          setText(result.data.text);
          newest.current = result.data.text;
        }
        if (mounted.current)
          setNotice(
            result.data.last_save
              ? saveDescription(result.data.last_save)
              : "配置保存完成；运行时应用结果请查看任务记录。",
          );
        await retain(key, {
          text: newest.current,
          revision: result.data.revision,
          draftId: result.data.id,
          original: result.data.original,
          sourceRevision: result.data.source_revision,
          saveOperationId: pendingIdentity.current,
        });
      } else if (mounted.current) setNotice("保存请求已返回，请查看任务结果。");
      if (mounted.current) onSaved();
    } catch (e) {
      if (!saveSent) {
        pendingIdentity.current = undefined;
        if (mounted.current) setPendingSave(undefined);
      }
      if (
        e instanceof API.ApiError &&
        e.code !== "outcome_unknown" &&
        (!e.details ||
          !["outcome_unknown", "recovery_failed"].includes(e.details.state))
      ) {
        pendingIdentity.current = undefined;
        if (mounted.current) setPendingSave(undefined);
      }
      if (!pendingIdentity.current) {
        await retain(key, {
          text: newest.current,
          revision: baseline.revision,
          draftId: baseline.id,
          original: baseline.original,
          sourceRevision: baseline.source_revision,
        }).catch(() => {}); // The original failure remains visible; no write operation is replayed.
      }
      if (mounted.current) {
        onError(e);
        setNotice(
          e instanceof API.ApiError && e.details
            ? saveDescription(e.details)
            : pendingIdentity.current
              ? `保存结果尚未确认。任务 ${pendingIdentity.current}，请查询原任务，勿重复提交。`
              : "草稿已保留。遇到版本冲突时，请先对照磁盘版本再保存。",
        );
      }
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  const inspectSave = async () => {
    const operationId = pendingIdentity.current;
    if (!operationId || !draft || busy) return;
    setBusy(true);
    try {
      const task = await API.query<API.Task>(agent, "task", {
        operation_id: operationId,
      });
      const current = await API.draftCommand(agent, "read", {
        draft_id: draft.id,
      });
      if (current.data.instance !== instance.id)
        throw new Error("原草稿的实例身份不一致。");
      const unresolved =
        current.data.last_save?.operation_id === operationId &&
        ["outcome_unknown", "recovery_failed"].includes(
          current.data.last_save.state,
        );
      const finished =
        !unresolved &&
        task.data.id === operationId &&
        ["complete", "failed"].includes(task.data.state);
      if (finished) pendingIdentity.current = undefined;
      await retain(key, {
        text: recovered?.text ?? newest.current,
        revision: current.data.revision,
        draftId: current.data.id,
        original: current.data.original,
        sourceRevision: current.data.source_revision,
        saveOperationId: pendingIdentity.current,
      });
      if (mounted.current) {
        setDraft(current.data);
        setPendingSave(pendingIdentity.current);
        setNotice(
          current.data.last_save?.operation_id === operationId
            ? saveDescription(current.data.last_save)
            : `任务 ${operationId} · ${task.data.state} · ${task.data.message}`,
        );
        if (finished) onSaved();
      }
    } catch (e) {
      if (mounted.current) onError(e);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  const syncDraft = async () => {
    if (!draft) throw new Error("草稿尚未就绪");
    if (newest.current === draft.text) return draft;
    const updated = await API.draftCommand(agent, "edit", {
      draft_id: draft.id,
      expected_revision: draft.revision,
      text: newest.current,
    });
    if (!mounted.current) throw new DOMException("编辑器已关闭", "AbortError");
    setDraft(updated.data);
    return updated.data;
  };
  const fields = async () => {
    if (busy || pendingIdentity.current) return;
    setBusy(true);
    try {
      await syncDraft();
      if (mounted.current) setMode("fields");
    } catch (e) {
      if (mounted.current) onError(e);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  const apply = async (command: EditCommand) => {
    if (busy || pendingIdentity.current) return;
    setBusy(true);
    try {
      const current = await syncDraft();
      const updated = await API.draftCommand(agent, "apply", {
        draft_id: current.id,
        expected_revision: current.revision,
        command,
      });
      if (!mounted.current) return;
      edit(updated.data.text);
      setDraft(updated.data);
      setNotice("字段已写入节点草稿；保存并应用后才修改源配置。");
    } catch (e) {
      if (mounted.current) onError(e);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key === "s") {
        e.preventDefault();
        void save();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  });
  return (
    <section className="card document-editor">
      <div className="editor-top">
        <div>
          <h2>编辑 {instance.name}</h2>
          <span className="mono muted">
            {agent} / {draft?.path ?? instance.config}
          </span>
        </div>
        <button className="btn" onClick={onClose}>
          返回
        </button>
      </div>
      {pendingSave && (
        <div className="notice-bar" role="status">
          <span>
            待确认保存任务：{pendingSave}。草稿保留，确认结果后才能再次保存。
          </span>
          <button
            className="btn sm"
            disabled={busy}
            onClick={() => void inspectSave()}
          >
            查询原保存任务
          </button>
        </div>
      )}
      {recovered && (
        <div className="notice-bar">
          <span>此浏览器保留了尚未同步的编辑内容。</span>
          <button
            className="btn sm"
            onClick={() => {
              edit(recovered.text);
              setRecovered(null);
              setNotice("已恢复本地内容，保存时仍会检查源文件版本。");
            }}
          >
            恢复内容
          </button>
          <button className="btn sm" onClick={() => setRecovered(null)}>
            使用当前文件
          </button>
        </div>
      )}
      <div className="toolbar">
        <button
          className={`btn ${mode === "source" ? "primary" : ""}`}
          disabled={busy}
          onClick={() => setMode("source")}
        >
          TOML
        </button>
        <button
          className={`btn ${mode === "fields" ? "primary" : ""}`}
          disabled={!draft || busy || !!pendingSave}
          onClick={() => void fields()}
        >
          字段编辑
        </button>
        <button
          className="btn"
          disabled={!draft || busy || !!pendingSave}
          onClick={() => void save()}
        >
          保存并应用
        </button>
        <button
          className="btn"
          disabled={!undo.current.length}
          onClick={() => history(true)}
        >
          撤销
        </button>
        <button
          className="btn"
          disabled={!redo.current.length}
          onClick={() => history(false)}
        >
          重做
        </button>
        <label className="btn">
          导入文本
          <input
            hidden
            type="file"
            accept=".toml,.txt"
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (!file) return;
              if (file.size > 480 * 1024) {
                onError(new Error("配置文本不能超过 480 KiB。"));
                return;
              }
              file
                .text()
                .then((value) => {
                  if (mounted.current) edit(value);
                })
                .catch((error) => {
                  if (mounted.current) onError(error);
                });
            }}
          />
        </label>
        <button
          className="btn"
          onClick={() => {
            const url = URL.createObjectURL(
              new Blob([text], { type: "text/plain" }),
            );
            const a = document.createElement("a");
            a.href = url;
            a.download = `${instance.name}.toml`;
            a.click();
            URL.revokeObjectURL(url);
          }}
        >
          导出
        </button>
        <span className="muted">
          {busy
            ? "正在保存…"
            : draft && text !== draft.original
              ? "草稿有改动"
              : "与源文件一致"}
        </span>
      </div>
      {!draft ? (
        <p className="empty-state">正在读取草稿…</p>
      ) : mode === "fields" ? (
        <SchemaEditor
          agent={agent}
          draft={draft}
          busy={busy}
          onCommand={apply}
          onError={onError}
        />
      ) : (
        <textarea
          aria-label="TOML 配置文档"
          className="source-editor"
          spellCheck={false}
          value={text}
          onChange={(e) => edit(e.target.value)}
        />
      )}
      {notice && (
        <p role="status" className="notice-bar">
          {notice}
        </p>
      )}
      <p className="muted editor-footnote">
        修改作用于所选节点。离开编辑器不会保存源配置；保存冲突不会覆盖磁盘上的新版本。
      </p>
    </section>
  );
}
