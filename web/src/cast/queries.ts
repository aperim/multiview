// React Query bindings for the ephemeral cast sessions (DEV-D3).
//
// The REST list is the fallback lane: each served doc already carries its
// lifecycle state, and the list re-polls on an interval so the panel degrades
// gracefully when the realtime stream is down (invariant #10). The conflated
// `device.status` WebSocket lane stays primary for state freshness — session
// actors publish through the SAME latest-wins status registry as devices,
// keyed by the session id — which pages merge via ../devices/queries
// `useDeviceStatuses` (reused, not duplicated).
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import { listCastSessions } from './api';
import type { CastSessionView } from './api';

/** The Query key for the ephemeral cast-session list. */
export const CAST_SESSIONS_QUERY_KEY = ['cast', 'sessions'] as const;

/** How often the REST session list re-polls (the WS lane is primary). */
const CAST_SESSIONS_REFETCH_MS = 15_000;

/** The live ephemeral cast sessions (REST snapshot, interval fallback). */
export function useCastSessions(): UseQueryResult<readonly CastSessionView[]> {
  return useQuery<readonly CastSessionView[]>({
    queryKey: CAST_SESSIONS_QUERY_KEY,
    queryFn: async (): Promise<readonly CastSessionView[]> => listCastSessions(),
    refetchInterval: CAST_SESSIONS_REFETCH_MS,
  });
}
