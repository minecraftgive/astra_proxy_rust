import { useEffect, useState } from "react";
import { request, Draft } from "../api";
import type {
  EditCommand,
  Field,
  PathPart,
  Schema,
} from "../generated/management";

export default function SchemaEditor({
  agent,
  draft,
  busy,
  onCommand,
  onError,
}: {
  agent: string;
  draft: Draft;
  busy: boolean;
  onCommand: (command: EditCommand) => Promise<void>;
  onError: (error: unknown) => void;
}) {
  const [path, setPath] = useState<PathPart[]>([]);
  const [schema, setSchema] = useState<Schema | null>(null);
  useEffect(() => {
    const abort = new AbortController();
    setSchema(null);
    request<Schema>(
      agent,
      {
        type: "drafts",
        data: { type: "schema", data: { draft_id: draft.id, path } },
      },
      abort.signal,
    )
      .then((r) => {
        if (!abort.signal.aborted) setSchema(r.data);
      })
      .catch((e) => {
        if (!abort.signal.aborted) onError(e);
      });
    return () => abort.abort();
  }, [agent, draft.id, draft.revision, JSON.stringify(path)]);
  const navigate = (next: PathPart[]) => setPath(next);
  return (
    <div className="schema-editor">
      <nav className="toolbar" aria-label="配置位置">
        <button className="btn sm" disabled={busy} onClick={() => navigate([])}>
          配置根目录
        </button>
        {path.map((part, i) => (
          <button
            className="btn sm"
            disabled={busy}
            key={i}
            onClick={() => navigate(path.slice(0, i + 1))}
          >
            {"Key" in part ? part.Key : `#${part.Index + 1}`}
          </button>
        ))}
      </nav>
      {!schema ? (
        <p className="muted">正在读取节点提供的字段描述…</p>
      ) : (
        <>
          {schema.rows.map((row, index) =>
            row.kind === "field" ? (
              <FieldEditor
                key={`${JSON.stringify(path)}/${row.field.key}/${draft.revision}`}
                field={row.field}
                busy={busy}
                apply={(input) =>
                  onCommand({
                    command: "set_field",
                    path,
                    key: row.field.key,
                    input,
                  })
                }
                remove={() =>
                  onCommand({
                    command: "remove_field",
                    path,
                    key: row.field.key,
                  })
                }
              />
            ) : row.kind === "group" ? (
              <button
                key={row.key}
                className="schema-group btn"
                disabled={busy}
                onClick={() => navigate([...path, { Key: row.key }])}
              >
                {row.label} →
              </button>
            ) : (
              <div className="schema-entry toolbar" key={row.index}>
                <button
                  className="btn"
                  disabled={busy}
                  onClick={() => navigate([...path, { Index: row.index }])}
                >
                  条目 {row.index + 1} →
                </button>
                <button
                  className="btn sm"
                  disabled={busy || index === 0}
                  onClick={() =>
                    void onCommand({
                      command: "move_entry",
                      path,
                      from: row.index,
                      to: row.index - 1,
                    })
                  }
                >
                  上移
                </button>
                <button
                  className="btn sm danger"
                  disabled={busy}
                  onClick={() => {
                    if (
                      window.confirm("删除此草稿条目？引用保护仍由节点检查。")
                    )
                      void onCommand({
                        command: "remove_entry",
                        path,
                        index: row.index,
                      });
                  }}
                >
                  删除
                </button>
              </div>
            ),
          )}
          {"collection" in schema && schema.collection === true && (
            <button
              className="btn primary"
              disabled={busy}
              onClick={() => void onCommand({ command: "append_entry", path })}
            >
              新增条目
            </button>
          )}
        </>
      )}
    </div>
  );
}
function FieldEditor({
  field,
  busy,
  apply,
  remove,
}: {
  field: Field;
  busy: boolean;
  apply: (input: string) => Promise<void>;
  remove: () => Promise<void>;
}) {
  const [input, setInput] = useState(field.value ?? "");
  const kind = field.field_type;
  let members: string[] = [];
  if (field.key === "members") {
    try {
      const parsed: unknown = JSON.parse(input);
      if (Array.isArray(parsed) && parsed.every((v) => typeof v === "string"))
        members = parsed;
    } catch {
      /* The text input also accepts TOML arrays. */
    }
  }
  const moveMember = (index: number, direction: number) => {
    const next = index + direction;
    if (next < 0 || next >= members.length) return;
    const reordered = [...members];
    [reordered[index], reordered[next]] = [reordered[next], reordered[index]];
    setInput(JSON.stringify(reordered));
  };
  const common = {
    value: input,
    disabled: busy,
    onChange: (e: React.ChangeEvent<HTMLInputElement | HTMLSelectElement>) =>
      setInput(e.target.value),
    className: "input",
    id: `field-${field.key}`,
  };
  return (
    <form
      className="schema-field"
      onSubmit={(e) => {
        e.preventDefault();
        void apply(input);
      }}
    >
      <div>
        <label htmlFor={common.id}>
          {field.label}
          {field.required ? " *" : ""}
        </label>
        <p className="muted">{field.help}</p>
      </div>
      {members.length > 0 && (
        <div aria-label="成员顺序">
          {members.map((name, index) => (
            <div className="toolbar" key={`${index}-${name}`}>
              <span>
                {index + 1}. {name}
              </span>
              <button
                type="button"
                className="btn sm"
                disabled={busy || index === 0}
                aria-label={`上移 ${name}`}
                onClick={() => moveMember(index, -1)}
              >
                ↑
              </button>
              <button
                type="button"
                className="btn sm"
                disabled={busy || index === members.length - 1}
                aria-label={`下移 ${name}`}
                onClick={() => moveMember(index, 1)}
              >
                ↓
              </button>
            </div>
          ))}
        </div>
      )}
      <div className="toolbar">
        {kind.kind === "boolean" ? (
          <select {...common}>
            <option value="">未设置</option>
            <option value="true">开启</option>
            <option value="false">关闭</option>
          </select>
        ) : kind.kind === "choice" ? (
          <select {...common}>
            <option value="">未设置</option>
            {kind.values.map((v) => (
              <option key={v} value={v}>
                {v}
              </option>
            ))}
          </select>
        ) : (
          <input
            {...common}
            type={
              field.secret
                ? "password"
                : kind.kind === "number"
                  ? "number"
                  : "text"
            }
            min={kind.kind === "number" ? kind.min : undefined}
            max={kind.kind === "number" ? kind.max : undefined}
            placeholder={field.default ? `默认：${field.default}` : "未设置"}
            autoComplete="off"
          />
        )}
        <button
          className="btn sm"
          disabled={busy || input === (field.value ?? "")}
        >
          写入草稿
        </button>
        {!field.required && field.value !== null && (
          <button
            className="btn sm"
            type="button"
            disabled={busy}
            onClick={() => void remove()}
          >
            恢复默认
          </button>
        )}
      </div>
    </form>
  );
}
