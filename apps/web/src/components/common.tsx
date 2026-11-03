type Row = Record<string, unknown>;
const fmt = (v: unknown) =>
  v == null ? "—" : typeof v === "object" ? JSON.stringify(v) : String(v);
export function Details({ value }: { value: unknown }) {
  return <pre className="data-preview">{JSON.stringify(value, null, 2)}</pre>;
}
export function Empty({ children }: { children: React.ReactNode }) {
  return <div className="card empty-state">{children}</div>;
}
export function Table({
  rows,
  columns,
  action,
}: {
  rows: Row[];
  columns: [string, string][];
  action?: (row: Row) => React.ReactNode;
}) {
  return (
    <div className="card table-scroll">
      <table>
        <thead>
          <tr>
            {columns.map(([key, label]) => (
              <th key={key}>{label}</th>
            ))}
            {action && <th>操作</th>}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={fmt(row.id ?? row.name ?? i)}>
              {columns.map(([key]) => (
                <td key={key} className={key.includes("id") ? "mono" : ""}>
                  {fmt(row[key])}
                </td>
              ))}
              {action && <td>{action(row)}</td>}
            </tr>
          ))}
        </tbody>
      </table>
      {rows.length === 0 && (
        <p className="empty-state muted">没有符合条件的记录</p>
      )}
    </div>
  );
}
