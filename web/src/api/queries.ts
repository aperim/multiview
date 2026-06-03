// React Query bindings over the typed API client.
//
// Each read hook calls the generated, typed client (see `client.ts`) so the
// query data shapes are exactly the control-plane response schemas. The engine
// is isolated (invariant #10): the UI is a best-effort reader and must degrade
// to loading / error states rather than assume a response.
//
// Writes go through `./layouts.ts` (see its header: the write path operations
// are not in the generated schema yet, so they use an explicitly-typed
// view-model that reuses the generated request/response types). Mutations apply
// OPTIMISTIC cache updates and roll back on error, and track per-layout ETags
// for `If-Match` optimistic concurrency.
import {
  useMutation,
  useQuery,
  useQueryClient,
} from '@tanstack/react-query';
import type {
  QueryClient,
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import type { MosaicApiClient } from './client';
import {
  deleteLayout,
  LayoutApiError,
  writeLayout,
} from './layouts';
import type { Layout, LayoutInput } from './layouts';

export type { Layout, LayoutInput } from './layouts';

/** A failed API call, normalized to a single human-readable message. */
export class ApiError extends Error {
  /** The HTTP status code, when the failure carried an RFC 9457 problem body. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
  }
}

/** Stable React Query keys for the control-plane resources. */
export const queryKeys = {
  layouts: ['layouts'] as const,
  etags: ['layouts', 'etags'] as const,
};

/** Per-layout ETag map, mirrored into the cache for `If-Match`. */
export type EtagMap = Readonly<Record<string, string>>;

/** Read the cached ETag map (empty when nothing has been stored yet). */
export function readEtags(queryClient: QueryClient): EtagMap {
  return queryClient.getQueryData<EtagMap>(queryKeys.etags) ?? {};
}

/**
 * Fetch all layouts via `GET /api/v1/layouts`. The result type is inferred from
 * the OpenAPI schema, so a drift between the Rust API and this client is a
 * compile error.
 */
export function useLayouts(client: MosaicApiClient): UseQueryResult<Layout[], ApiError> {
  return useQuery<Layout[], ApiError>({
    queryKey: queryKeys.layouts,
    queryFn: async (): Promise<Layout[]> => {
      // openapi-fetch returns a discriminated `{ data } | { error }`: when
      // `error` is absent, `data` is the typed success body (the Layout array).
      const { data, error } = await client.GET('/api/v1/layouts');
      if (error !== undefined) {
        throw new ApiError(error.title, error.status);
      }
      return data;
    },
  });
}

/** Connection options threaded into the write helpers. */
export interface MutationContext {
  /** Base URL for the write helpers (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token. */
  readonly token?: string;
}

function toApiError(error: unknown): ApiError {
  if (error instanceof LayoutApiError) {
    return new ApiError(error.message, error.status);
  }
  if (error instanceof Error) {
    return new ApiError(error.message);
  }
  return new ApiError('Unknown error');
}

/** The shape passed to {@link useSaveLayout}. */
export interface SaveLayoutVars {
  /** The create/update payload. */
  readonly input: LayoutInput;
  /** The resource id when updating; omit to create. */
  readonly id?: string;
}

interface SaveSnapshot {
  readonly previous: Layout[] | undefined;
}

/**
 * Create or update a layout with an optimistic cache update. On error the
 * previous layouts list is restored. ETags are read for `If-Match` (update) and
 * the response ETag is stored for the next write.
 */
export function useSaveLayout(
  context: MutationContext = {},
): UseMutationResult<Layout, ApiError, SaveLayoutVars, SaveSnapshot> {
  const queryClient = useQueryClient();
  return useMutation<Layout, ApiError, SaveLayoutVars, SaveSnapshot>({
    mutationFn: async ({ input, id }): Promise<Layout> => {
      const etags = readEtags(queryClient);
      const etag = id !== undefined ? etags[id] : undefined;
      let result;
      try {
        result = await writeLayout(
          input,
          {
            ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
            ...(context.token !== undefined ? { token: context.token } : {}),
            ...(etag !== undefined ? { etag } : {}),
          },
          id,
        );
      } catch (error) {
        throw toApiError(error);
      }
      if (result.etag !== undefined) {
        queryClient.setQueryData<EtagMap>(queryKeys.etags, (current) => ({
          ...(current ?? {}),
          [result.layout.id]: result.etag ?? '',
        }));
      }
      return result.layout;
    },
    onMutate: async ({ input, id }): Promise<SaveSnapshot> => {
      await queryClient.cancelQueries({ queryKey: queryKeys.layouts });
      const previous = queryClient.getQueryData<Layout[]>(queryKeys.layouts);
      queryClient.setQueryData<Layout[]>(queryKeys.layouts, (current) =>
        applyOptimisticSave(current ?? [], input, id),
      );
      return { previous };
    },
    onError: (_error, _vars, snapshot): void => {
      if (snapshot?.previous !== undefined) {
        queryClient.setQueryData<Layout[]>(queryKeys.layouts, snapshot.previous);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.layouts });
    },
  });
}

function applyOptimisticSave(
  current: readonly Layout[],
  input: LayoutInput,
  id: string | undefined,
): Layout[] {
  if (id !== undefined) {
    return current.map((layout) =>
      layout.id === id
        ? { ...layout, name: input.name, body: input.body }
        : layout,
    );
  }
  // Create: append a placeholder with a temporary id until the server replies.
  const optimistic: Layout = {
    id: `optimistic-${String(current.length)}`,
    name: input.name,
    body: input.body,
  };
  return [...current, optimistic];
}

interface DeleteSnapshot {
  readonly previous: Layout[] | undefined;
}

/**
 * Delete a layout with an optimistic removal; restores the list on error.
 */
export function useDeleteLayout(
  context: MutationContext = {},
): UseMutationResult<undefined, ApiError, string, DeleteSnapshot> {
  const queryClient = useQueryClient();
  return useMutation<undefined, ApiError, string, DeleteSnapshot>({
    mutationFn: async (id): Promise<undefined> => {
      const etag = readEtags(queryClient)[id];
      try {
        await deleteLayout(id, {
          ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
          ...(context.token !== undefined ? { token: context.token } : {}),
          ...(etag !== undefined ? { etag } : {}),
        });
      } catch (error) {
        throw toApiError(error);
      }
      return undefined;
    },
    onMutate: async (id): Promise<DeleteSnapshot> => {
      await queryClient.cancelQueries({ queryKey: queryKeys.layouts });
      const previous = queryClient.getQueryData<Layout[]>(queryKeys.layouts);
      queryClient.setQueryData<Layout[]>(queryKeys.layouts, (current) =>
        (current ?? []).filter((layout) => layout.id !== id),
      );
      return { previous };
    },
    onError: (_error, _id, snapshot): void => {
      if (snapshot?.previous !== undefined) {
        queryClient.setQueryData<Layout[]>(queryKeys.layouts, snapshot.previous);
      }
    },
    onSettled: (): void => {
      void queryClient.invalidateQueries({ queryKey: queryKeys.layouts });
    },
  });
}
