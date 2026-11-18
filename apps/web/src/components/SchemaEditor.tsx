import { useEffect, useState } from "react";
import { request, Draft } from "../api";
import type {
  EditCommand,
  Field,
  PathPart,
  Schema,
} from "../generated/management";

function parseTomlStringValue(raw: string): string | undefined {
  const trimmed = raw.trim();
  if (!trimmed) return undefined;
  if (trimmed.startsWith('"')) {
    let out = "";
    for (let i = 1; i < trimmed.length; i++) {
      const ch = trimmed[i];
      if (ch === "\\" && i + 1 < trimmed.length) {
        out += trimmed[++i];
      } else if (ch === '"') {
        return out;
      } else {
        out += ch;
      }
    }
    return undefined;
  }
  if (trimmed.startsWith("'")) {
    const end = trimmed.indexOf("'", 1);
    return end >= 1 ? trimmed.slice(1, end) : undefined;
  }
  const value = trimmed.split("#")[0]?.trim();
  return value || undefined;
}

export function summarizeEntry(
  sourceText: string | undefined,
  collectionPath: PathPart[],
  index: number,
): string {
  const fallback = `条目 ${index + 1}`;
  if (!sourceText) return fallback;
  const last = collectionPath[collectionPath.length - 1];
  if (!last || !("Key" in last)) return fallback;
  const targetCollection = last.Key;
  const parentListener =
    targetCollection === "users" &&
    collectionPath.length >= 3 &&
    "Key" in collectionPath[0] &&
    collectionPath[0].Key === "listeners" &&
    "Index" in collectionPath[1]
      ? collectionPath[1].Index
      : null;

  const entries: Record<string, string>[] = [];
  let listenerIndex = -1;
  let activeCollection = "";
  let current: Record<string, string> | null = null;

  for (const rawLine of sourceText.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    if (line.startsWith("[[") && line.endsWith("]]")) {
      const header = line.slice(2, -2).trim();
      if (header === "listeners") listenerIndex++;
      activeCollection = header;
      const matchesTarget =
        (parentListener !== null &&
          header === "listeners.users" &&
          listenerIndex === parentListener) ||
        (parentListener === null && header === targetCollection);
      if (matchesTarget) {
        current = {};
        entries.push(current);
      } else {
        current = null;
      }
      continue;
    }
    if (line.startsWith("[") && line.endsWith("]")) {
      const header = line.slice(1, -1).trim();
      if (
        !current ||
        (parentListener === null &&
          header !== activeCollection &&
          !header.startsWith(`${activeCollection}.`)) ||
        (parentListener !== null && !header.startsWith("listeners.users."))
      ) {
        current = null;
      }
      continue;
    }
    if (!current) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) continue;
    const key = line.slice(0, eq).trim();
    if (
      ![
        "tag",
        "kind",
        "protocol",
        "value",
        "action",
        "listen",
        "name",
      ].includes(key)
    ) {
      continue;
    }
    const val = parseTomlStringValue(line.slice(eq + 1));
    if (val && !(key in current)) current[key] = val;
  }

  const entry = entries[index];
  if (!entry) return fallback;

  let detail = "";
  if (targetCollection === "outbounds") {
    const proto =
      entry.kind === "native" && entry.protocol
        ? entry.protocol
        : entry.protocol || entry.kind;
    detail = [entry.tag, proto ? `[${proto}]` : ""]
      .filter(Boolean)
      .join(" ");
  } else if (targetCollection === "rules") {
    const lhs = [entry.kind, entry.value].filter(Boolean).join(" ");
    detail = entry.action ? `${lhs}${lhs ? " " : ""}→ ${entry.action}` : lhs;
  } else if (targetCollection === "listeners") {
    const head = [entry.tag, entry.listen].filter(Boolean).join(" ");
    detail = [head, entry.protocol ? `[${entry.protocol}]` : ""]
      .filter(Boolean)
      .join(" ");
  } else {
    detail = entry.name || entry.tag || "";
  }
  return detail ? `${fallback} · ${detail}` : fallback;
}

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
        (() => {
          const entryCount = schema.rows.filter(
            (row) => row.kind === "entry",
          ).length;
          return (
            <>
              {schema.rows.map((row) =>
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
                      {summarizeEntry(draft.text, path, row.index)} →
                    </button>
                    <button
                      className="btn sm"
                      disabled={busy || row.index === 0}
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
                      className="btn sm"
                      disabled={busy || row.index >= entryCount - 1}
                      onClick={() =>
                        void onCommand({
                          command: "move_entry",
                          path,
                          from: row.index,
                          to: row.index + 1,
                        })
                      }
                    >
                      下移
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
          );
        })()
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
  const [revealed, setRevealed] = useState(false);
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
                ? revealed
                  ? "text"
                  : "password"
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
        {field.secret && (
          <button
            type="button"
            className="btn sm"
            disabled={busy}
            onClick={() => setRevealed((v) => !v)}
          >
            {revealed ? "隐藏" : "显示"}
          </button>
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
