import { useEffect, useRef } from "react";

/**
 * Schedule read-only status refreshes only while their screen is visible.
 * The visible screen gets an immediate load; routine ticks and focus events
 * are coalesced so a slow backend read cannot trigger duplicate requests.
 */
export function useVisibleViewRefresh(
  visible: boolean,
  onEnter: () => Promise<unknown>,
  onPoll: () => Promise<unknown>,
  intervalMs: number,
): void {
  const actions = useRef({ onEnter, onPoll });
  actions.current = { onEnter, onPoll };

  useEffect(() => {
    if (!visible) return;
    let disposed = false;
    let pending = false;
    const run = async (initial: boolean) => {
      if (disposed || pending) return;
      pending = true;
      try {
        if (initial) await actions.current.onEnter();
        else await actions.current.onPoll();
      } catch {
        // A transient local status read must not break desktop navigation.
      } finally {
        pending = false;
      }
    };
    void run(true);
    const poll = () => void run(false);
    const timer = window.setInterval(poll, intervalMs);
    window.addEventListener("focus", poll);
    return () => {
      disposed = true;
      window.clearInterval(timer);
      window.removeEventListener("focus", poll);
    };
  }, [visible, intervalMs]);
}
