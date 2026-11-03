import { useCallback, useEffect, useRef, type DependencyList } from "react";

/** Serial requests, immediate event refresh, and a slower fallback while visible.
 * A null interval disables the subscription. Callbacks must ignore aborted results.
 */
export function usePolling(
  fn: (signal: AbortSignal) => Promise<void>,
  intervalMs: number | null,
  deps: DependencyList,
) {
  const latest = useRef(fn);
  latest.current = fn;
  const trigger = useRef<() => void>(() => {});
  useEffect(() => {
    if (intervalMs === null) return;
    let active = true;
    let running = false;
    let pending = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let controller: AbortController | undefined;
    const run = async () => {
      clearTimeout(timer);
      if (!active || document.hidden) return;
      if (running) {
        pending = true;
        return;
      }
      running = true;
      pending = false;
      controller = new AbortController();
      try {
        await latest.current(controller.signal);
      } catch (error) {
        // Consumers normally report their own errors; keep the fallback alive.
        if (!controller.signal.aborted)
          console.error("Polling request failed", error);
      } finally {
        running = false;
        if (active && !document.hidden) {
          timer = setTimeout(() => void run(), pending ? 0 : intervalMs);
        }
      }
    };
    const refresh = () => void run();
    const visibility = () => {
      if (document.hidden) {
        clearTimeout(timer);
        controller?.abort();
      } else refresh();
    };
    trigger.current = refresh;
    document.addEventListener("visibilitychange", visibility);
    refresh();
    return () => {
      active = false;
      trigger.current = () => {};
      clearTimeout(timer);
      controller?.abort();
      document.removeEventListener("visibilitychange", visibility);
    };
  }, [intervalMs, ...deps]);
  return useCallback(() => trigger.current(), []);
}
