// Read hooks for the Sources/Outputs/Overlays resource views.
//
// TODO(api-schema): these endpoints are not in the generated OpenAPI schema yet
// (see ./types.ts). Until they ship, the hooks resolve a small, typed sample so
// the views, the layout-editor palette, and the source-binding dropdowns have
// honest shapes to render — every consumer is already wired to swap to the typed
// client with no shape change. The data is marked `isStub` so the UI can badge
// it as not-yet-live (never presented as authoritative engine state).
import { useQuery } from '@tanstack/react-query';
import type { UseQueryResult } from '@tanstack/react-query';

import type { OutputView, OverlayView, SourceView } from './types';

/** Query keys for the (stubbed) resource lists. */
export const resourceKeys = {
  sources: ['resources', 'sources'] as const,
  outputs: ['resources', 'outputs'] as const,
  overlays: ['resources', 'overlays'] as const,
};

const SAMPLE_SOURCES: readonly SourceView[] = [
  { id: 'cam-north', name: 'North Camera', kind: 'rtsp', url: 'rtsp://cam-north/stream' },
  { id: 'cam-south', name: 'South Camera', kind: 'rtsp', url: 'rtsp://cam-south/stream' },
  { id: 'studio-ndi', name: 'Studio NDI', kind: 'ndi', url: undefined },
  { id: 'bars', name: 'Test Bars', kind: 'test', url: undefined },
];

const SAMPLE_OUTPUTS: readonly OutputView[] = [
  { id: 'program-hls', name: 'Program LL-HLS', kind: 'll-hls', enabled: true },
  { id: 'program-rtsp', name: 'Program RTSP', kind: 'rtsp', enabled: true },
  { id: 'archive-srt', name: 'Archive SRT', kind: 'srt', enabled: false },
];

const SAMPLE_OVERLAYS: readonly OverlayView[] = [
  { id: 'wall-clock', name: 'Wall Clock', kind: 'clock', z: 100 },
  { id: 'tally', name: 'Tally Border', kind: 'tally_border', z: 90 },
  { id: 'lower-third', name: 'Lower Third', kind: 'label', z: 80 },
];

/** Whether the resource APIs are wired (false while stubbed). */
export const RESOURCES_ARE_STUBBED = true;

function useStub<T>(
  key: readonly string[],
  rows: readonly T[],
): UseQueryResult<readonly T[], never> {
  return useQuery<readonly T[], never>({
    queryKey: key,
    queryFn: (): readonly T[] => rows,
    staleTime: Infinity,
  });
}

/** List the (stubbed) managed sources. */
export function useSources(): UseQueryResult<readonly SourceView[], never> {
  return useStub(resourceKeys.sources, SAMPLE_SOURCES);
}

/** List the (stubbed) configured outputs. */
export function useOutputs(): UseQueryResult<readonly OutputView[], never> {
  return useStub(resourceKeys.outputs, SAMPLE_OUTPUTS);
}

/** List the (stubbed) configured overlays. */
export function useOverlays(): UseQueryResult<readonly OverlayView[], never> {
  return useStub(resourceKeys.overlays, SAMPLE_OVERLAYS);
}
