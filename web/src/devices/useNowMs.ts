// A shared ticking wall-clock store so renders stay pure (react-hooks/purity):
// `Date.now()` is read inside the interval, cached, and exposed as an
// external-store snapshot. The interval only runs while subscribers are
// mounted. Lives in its own module (not alongside components) so a component
// file importing it does not trip react-refresh/only-export-components.
import { useSyncExternalStore } from 'react';

const NOW_TICK_MS = 1_000;

let cachedNowMs = Date.now();
const nowListeners = new Set<() => void>();
let nowTimer: ReturnType<typeof setInterval> | undefined;

function subscribeNow(listener: () => void): () => void {
  nowListeners.add(listener);
  nowTimer ??= setInterval(() => {
    cachedNowMs = Date.now();
    for (const notify of nowListeners) {
      notify();
    }
  }, NOW_TICK_MS);
  return () => {
    nowListeners.delete(listener);
    if (nowListeners.size === 0 && nowTimer !== undefined) {
      clearInterval(nowTimer);
      nowTimer = undefined;
    }
  };
}

function readNowMs(): number {
  return cachedNowMs;
}

/** The current wall time in ms, ticking once a second while subscribed. */
export function useNowMs(): number {
  return useSyncExternalStore(subscribeNow, readNowMs, readNowMs);
}
