type Notice = {
  state: "recovered" | "pending" | "conflict";
  timestamp_ms: number;
  message: string;
  journal: string;
};
export default function NetworkRecovery({ details }: { details: unknown }) {
  const value = (details as { network_recovery?: Notice } | null)
    ?.network_recovery;
  if (
    !value ||
    !["recovered", "pending", "conflict"].includes(value.state) ||
    typeof value.message !== "string" ||
    typeof value.journal !== "string"
  )
    return null;
  const label = {
    recovered: "网络设置已恢复",
    pending: "网络恢复待处理",
    conflict: "网络恢复存在冲突",
  }[value.state];
  const date = new Date(value.timestamp_ms);
  return (
    <section
      className="card card-pad"
      role={value.state === "recovered" ? "status" : "alert"}
    >
      <h2>{label}</h2>
      <p>{value.message}</p>
      <p className="muted">
        {Number.isFinite(date.getTime()) ? date.toLocaleString() : "时间未知"}
      </p>
      <p className="mono">恢复记录：{value.journal}</p>
    </section>
  );
}
