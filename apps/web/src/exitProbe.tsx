/** Header exit probes, matching the TUI and desktop header: the exit IP once per
 * node and TCP handshake latency on an interval, each shown only while its
 * setting is on. Each round is one `exit_probe` query to the node's manager.
 */
import { useEffect, useRef, useState } from "react";
import * as API from "./api";

export type ExitProbeSettings = {
  /** Look up the exit node's public IP after connecting or switching nodes. */
  ip: boolean;
  /** Measure TCP latency through the exit node every `interval` seconds. */
  latency: boolean;
  interval: number;
  /** Let the manager keep the probe core loaded and idle between rounds. */
  keepAlive: boolean;
};
/** Seconds between latency rounds, as in the shared preferences. */
export const EXIT_INTERVAL = { min: 5, max: 3600, step: 5, default: 30 };
export const defaultExitProbe: ExitProbeSettings = {
  ip: false,
  latency: true,
  interval: EXIT_INTERVAL.default,
  keepAlive: true,
};
const KEY = "astra-exit-probe";
/** After a failed round (manager busy, core restarting) try again this soon. */
const RETRY_MS = 10_000;
/** A round that never reports is abandoned after this long. */
const ROUND_LIMIT_MS = 20_000;
/** The probe core outlives the gap between rounds by this margin. */
const KEEP_ALIVE_MARGIN_S = 30;

export function clampInterval(value: number): number {
  if (!Number.isFinite(value)) return EXIT_INTERVAL.default;
  return Math.min(
    EXIT_INTERVAL.max,
    Math.max(EXIT_INTERVAL.min, Math.round(value)),
  );
}
export function readExitProbe(): ExitProbeSettings {
  try {
    const raw = JSON.parse(localStorage.getItem(KEY) ?? "null");
    if (!raw || typeof raw !== "object") return defaultExitProbe;
    const flag = (value: unknown, fallback: boolean) =>
      typeof value === "boolean" ? value : fallback;
    return {
      ip: flag(raw.ip, defaultExitProbe.ip),
      latency: flag(raw.latency, defaultExitProbe.latency),
      interval: clampInterval(Number(raw.interval ?? EXIT_INTERVAL.default)),
      keepAlive: flag(raw.keepAlive, defaultExitProbe.keepAlive),
    };
  } catch {
    return defaultExitProbe;
  }
}
export function retainExitProbe(settings: ExitProbeSettings): void {
  try {
    localStorage.setItem(KEY, JSON.stringify(settings));
  } catch {
    /* Keep the in-memory selection. */
  }
}

type Group = { name: string; selected: string | null };
/** Mode root, then each group's selection to the final exit node. */
export function exitChain(runtime: Record<string, unknown> | null): string[] {
  if (!runtime) return [];
  const groups = (runtime.groups ?? []) as Group[];
  const chain = [runtime.mode === "direct" ? "DIRECT" : "PROXY"];
  for (let i = 0; i < 8; i++) {
    const group = groups.find((g) => g.name === chain[chain.length - 1]);
    const next = group?.selected;
    if (!next || chain.includes(next)) break;
    chain.push(next);
  }
  return chain;
}

export type Latency = { ms: number } | "failed";
type Target = { agent: string; instance: string; pid: string; outbound: string };
const sameTarget = (a: Target | null, b: Target | null) =>
  a === b ||
  (!!a &&
    !!b &&
    a.agent === b.agent &&
    a.instance === b.instance &&
    a.pid === b.pid &&
    a.outbound === b.outbound);

/** Runs probe rounds for `target`; null stops them. Every connect or node
 * switch (a new target) runs one round that looks up the IP and times the
 * handshake whatever the settings say; the settings add interval latency
 * rounds and retries of a failed IP lookup. A switch aborts the round in
 * flight, and results are keyed by target, so an old node never shows. */
export function useExitProbe(
  target: Target | null,
  settings: ExitProbeSettings,
): { ip: string | null; latency: Latency | "pending" | null } {
  // Latest latency, tagged with its round's target; `periodic` results are
  // hidden when the latency setting goes off, the connect/switch one is not.
  const [measured, setMeasured] = useState<{
    round: string;
    latency: Latency;
    periodic: boolean;
  } | null>(null);
  // Exit IPs survive switching away and back, keyed by agent/instance/outbound.
  const [ips, setIps] = useState<Record<string, string>>({});
  const current = useRef<Target | null>(null);
  if (!sameTarget(current.current, target)) current.current = target;
  const stable = current.current;
  const key = stable
    ? `${stable.agent}/${stable.instance}/${stable.outbound}`
    : "";
  const round = stable ? `${key}/${stable.pid}` : "";
  const known = useRef(ips);
  known.current = ips;
  // The connect/switch round still owed, per target; settings changes keep it.
  const owed = useRef("");
  const owedFor = useRef("");
  if (owedFor.current !== round) {
    owedFor.current = round;
    owed.current = round;
  }
  useEffect(() => {
    if (!stable) return;
    let active = true;
    let unsupported = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let controller: AbortController | undefined;
    const interval = settings.latency ? settings.interval * 1000 : RETRY_MS;
    const keepAlive = settings.keepAlive
      ? Math.round(interval / 1000) + KEEP_ALIVE_MARGIN_S
      : null;
    const next = async () => {
      if (!active || unsupported) return;
      if (document.hidden) {
        timer = setTimeout(() => void next(), interval);
        return;
      }
      const initial = owed.current === round;
      const ip = initial || (settings.ip && !known.current[key]);
      if (!ip && !settings.latency) return;
      controller = new AbortController();
      const limit = setTimeout(() => controller?.abort(), ROUND_LIMIT_MS);
      let wait = interval;
      try {
        const r = await API.query<{
          connect_ms: number | string | null;
          ip: string | null;
        }>(
          stable.agent,
          "exit_probe",
          {
            instance: stable.instance,
            outbound: stable.outbound,
            ip,
            keep_alive_secs: keepAlive,
          },
          controller.signal,
        );
        if (!active) return;
        if (initial || settings.latency)
          setMeasured({
            round,
            latency:
              r.data.connect_ms == null
                ? "failed"
                : { ms: Number(r.data.connect_ms) },
            periodic: !initial,
          });
        if (initial && owed.current === round) owed.current = "";
        const found = r.data.ip;
        if (found) setIps((old) => ({ ...old, [key]: found }));
        // A failed IP lookup is retried soon while the IP setting is on.
        if (ip && settings.ip && !found) wait = Math.min(interval, RETRY_MS);
      } catch (e) {
        if (!active) return;
        const message = e instanceof Error ? e.message : String(e);
        // The node's manager predates exit probes; stop until the exit changes.
        if (
          (e instanceof API.ApiError && e.code === "unsupported") ||
          message.includes("unknown variant")
        )
          unsupported = true;
        // An unanswered connect/switch round stays owed and is retried.
        wait = Math.min(interval, RETRY_MS);
      } finally {
        clearTimeout(limit);
      }
      if (active) timer = setTimeout(() => void next(), wait);
    };
    void next();
    return () => {
      active = false;
      clearTimeout(timer);
      controller?.abort();
    };
  }, [
    stable,
    key,
    round,
    settings.ip,
    settings.latency,
    settings.interval,
    settings.keepAlive,
  ]);
  return exitProbeFields(settings, key ? ips[key] : undefined, round, measured);
}

/** The shared header rule: a found result always shows; an enabled setting
 * keeps its field's place while nothing has been measured. */
export function exitProbeFields(
  settings: ExitProbeSettings,
  ip: string | undefined,
  round: string,
  measured: { round: string; latency: Latency; periodic: boolean } | null,
): { ip: string | null; latency: Latency | "pending" | null } {
  const latency =
    measured &&
    measured.round === round &&
    (!measured.periodic || settings.latency)
      ? measured.latency
      : null;
  return {
    ip: ip ?? (settings.ip ? "—" : null),
    latency: latency ?? (settings.latency ? "pending" : null),
  };
}

/** `(ip: … | 42 ms)` after the exit node; see `exitProbeFields` for which show. */
export function ExitProbeFields({
  ip,
  latency,
}: {
  ip: string | null;
  latency: Latency | "pending" | null;
}) {
  const fields: React.ReactNode[] = [];
  if (ip != null)
    fields.push(
      <span key="ip" className="muted">
        ip: {ip}
      </span>,
    );
  if (latency != null)
    fields.push(
      latency === "failed" ? (
        <span key="lat" className="lat r">
          超时
        </span>
      ) : latency === "pending" || !latency ? (
        <span key="lat" className="lat n">
          —
        </span>
      ) : (
        <span
          key="lat"
          className={`lat ${latency.ms < 150 ? "g" : latency.ms < 400 ? "y" : "r"}`}
        >
          {latency.ms} ms
        </span>
      ),
    );
  if (!fields.length) return null;
  return (
    <span className="exit-probe mono" aria-label="出口探测">
      <span className="muted">(</span>
      {fields.flatMap((field, index) =>
        index
          ? [<span key={`sep${index}`} className="muted"> | </span>, field]
          : [field],
      )}
      <span className="muted">)</span>
    </span>
  );
}
