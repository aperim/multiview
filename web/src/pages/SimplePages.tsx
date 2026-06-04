// The Sources / Outputs / Overlays resource views.
//
// Read-only, typed lists rendered with TanStack Table. The data is stubbed (see
// resources/queries.ts) until each resource's management API ships; the columns
// are typed against the resource view-models so wiring the live client later is
// a drop-in. Status is conveyed by text + glyph, never color alone.
import { useMemo } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Check, CircleSlash } from 'lucide-react';
import type { ColumnDef } from '@tanstack/react-table';

import { useOutputs, useOverlays, useSources } from '../resources/queries';
import type {
  OutputView,
  OverlayView,
  SourceView,
} from '../resources/types';
import { ResourceTable, StubNotice } from '../resources/ResourceTable';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';

/** Sources management (ingest). */
export function SourcesPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();
  const columns = useMemo<ColumnDef<SourceView>[]>(
    () => [
      {
        accessorKey: 'name',
        header: t`Name`,
        cell: (ctx): JSX.Element => (
          <span lang="" dir="auto" className="font-medium">
            {ctx.row.original.name}
          </span>
        ),
      },
      {
        accessorKey: 'kind',
        header: t`Kind`,
        cell: (ctx): JSX.Element => (
          <Badge variant="outline">{ctx.row.original.kind}</Badge>
        ),
      },
      {
        accessorKey: 'url',
        header: t`Locator`,
        cell: (ctx): JSX.Element => (
          <code className="text-xs text-muted-foreground" lang="" dir="auto">
            {ctx.row.original.url ?? '—'}
          </code>
        ),
      },
    ],
    [t],
  );

  return (
    <>
      <PageHeader
        title={<Trans>Sources</Trans>}
        description={<Trans>Add and manage live ingest sources.</Trans>}
      />
      <StubNotice />
      <ResourceTable
        rows={sources.data ?? []}
        columns={columns}
        caption={t`Configured ingest sources.`}
        emptyMessage={<Trans>No sources configured.</Trans>}
      />
    </>
  );
}

/** Outputs / transcoding. */
export function OutputsPage(): JSX.Element {
  const { t } = useLingui();
  const outputs = useOutputs();
  const columns = useMemo<ColumnDef<OutputView>[]>(
    () => [
      {
        accessorKey: 'name',
        header: t`Name`,
        cell: (ctx): JSX.Element => (
          <span lang="" dir="auto" className="font-medium">
            {ctx.row.original.name}
          </span>
        ),
      },
      {
        accessorKey: 'kind',
        header: t`Transport`,
        cell: (ctx): JSX.Element => (
          <Badge variant="outline">{ctx.row.original.kind}</Badge>
        ),
      },
      {
        accessorKey: 'enabled',
        header: t`State`,
        cell: (ctx): JSX.Element =>
          ctx.row.original.enabled ? (
            <span className="inline-flex items-center gap-1 text-sm">
              <Check className="size-4" aria-hidden="true" />
              <Trans>Enabled</Trans>
            </span>
          ) : (
            <span className="inline-flex items-center gap-1 text-sm text-muted-foreground">
              <CircleSlash className="size-4" aria-hidden="true" />
              <Trans>Disabled</Trans>
            </span>
          ),
      },
    ],
    [t],
  );

  return (
    <>
      <PageHeader
        title={<Trans>Outputs</Trans>}
        description={<Trans>Configure output servers and renditions.</Trans>}
      />
      <StubNotice />
      <ResourceTable
        rows={outputs.data ?? []}
        columns={columns}
        caption={t`Configured output sinks.`}
        emptyMessage={<Trans>No outputs configured.</Trans>}
      />
    </>
  );
}

/** Overlays + subtitles. */
export function OverlaysPage(): JSX.Element {
  const { t } = useLingui();
  const overlays = useOverlays();
  const columns = useMemo<ColumnDef<OverlayView>[]>(
    () => [
      {
        accessorKey: 'name',
        header: t`Name`,
        cell: (ctx): JSX.Element => (
          <span lang="" dir="auto" className="font-medium">
            {ctx.row.original.name}
          </span>
        ),
      },
      {
        accessorKey: 'kind',
        header: t`Kind`,
        cell: (ctx): JSX.Element => (
          <Badge variant="outline">{ctx.row.original.kind}</Badge>
        ),
      },
      {
        accessorKey: 'z',
        header: t`Stacking`,
        cell: (ctx): JSX.Element => (
          <span className="tabular-nums">{ctx.row.original.z}</span>
        ),
      },
    ],
    [t],
  );

  return (
    <>
      <PageHeader
        title={<Trans>Overlays</Trans>}
        description={<Trans>Manage overlay layers and subtitles.</Trans>}
      />
      <StubNotice />
      <ResourceTable
        rows={overlays.data ?? []}
        columns={columns}
        caption={t`Configured overlay layers.`}
        emptyMessage={<Trans>No overlays configured.</Trans>}
      />
    </>
  );
}
