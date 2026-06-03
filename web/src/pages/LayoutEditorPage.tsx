// The layout editor route: hosts the LayoutEditor for a new draft (`/layouts/new`)
// or an existing layout (`/layouts/:id`). It loads the layout via the typed query
// cache, maps the opaque `body` to the editor view-model, and saves through the
// CRUD mutation hooks (optimistic + ETag).
import { useMemo } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useNavigate, useParams } from 'react-router-dom';

import { createApiClient } from '../api/client';
import { useLayouts, useSaveLayout } from '../api/queries';
import { fromLayoutBody } from '../layout/model';
import type { LayoutModel } from '../layout/model';
import { LayoutEditor } from '../layout/components/LayoutEditor';
import type { LayoutSavePayload } from '../layout/components/LayoutEditor';
import { useOverlays, useSources } from '../resources/queries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { toast } from '../components/ui/use-toast';

/** The editor page (new or edit). */
export function LayoutEditorPage(): JSX.Element {
  const { t } = useLingui();
  const params = useParams();
  const navigate = useNavigate();
  const layoutId = params.id;
  const isNew = layoutId === undefined;

  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const sources = useSources();
  const overlays = useOverlays();
  const save = useSaveLayout();

  const existing = useMemo(
    () => layouts.data?.find((layout) => layout.id === layoutId),
    [layouts.data, layoutId],
  );

  // Map the opaque persisted body into the editor view-model. `undefined` means
  // the body is a non-absolute (grid/preset) layout this editor cannot edit.
  const initial = useMemo<LayoutModel | undefined>(() => {
    if (isNew) {
      return undefined;
    }
    if (existing === undefined) {
      return undefined;
    }
    return fromLayoutBody(existing.id, existing.name, existing.body);
  }, [existing, isNew]);

  const handleSave = (payload: LayoutSavePayload): void => {
    save.mutate(
      {
        input: { name: payload.name, body: payload.body },
        ...(payload.id !== '' ? { id: payload.id } : {}),
      },
      {
        onSuccess: (): void => {
          toast({ title: t`Layout saved` });
          void navigate('/layouts');
        },
        onError: (error): void => {
          toast({
            title: t`Could not save layout`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  if (!isNew && layouts.isPending) {
    return (
      <>
        <PageHeader title={<Trans>Edit layout</Trans>} />
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading layout…</Trans>
        </p>
      </>
    );
  }

  if (!isNew && existing === undefined) {
    return (
      <>
        <PageHeader title={<Trans>Edit layout</Trans>} />
        <p role="alert" className="text-sm text-destructive">
          <Trans>That layout no longer exists.</Trans>
        </p>
      </>
    );
  }

  // The layout exists but is not the absolute-placement kind the editor supports.
  if (!isNew && existing !== undefined && initial === undefined) {
    return (
      <>
        <PageHeader
          title={<Trans>Edit layout</Trans>}
          description={
            <span lang="" dir="auto">
              {existing.name}
            </span>
          }
        />
        <Badge variant="outline">
          <Trans>
            This layout uses a grid or preset placement that the free-form editor
            cannot modify yet. Editing is read-only.
          </Trans>
        </Badge>
      </>
    );
  }

  return (
    <>
      <PageHeader
        title={isNew ? <Trans>New layout</Trans> : <Trans>Edit layout</Trans>}
        description={
          <Trans>
            Compose the output mosaic. The Canvas tab is drag-and-drop; the Cells
            tab is the fully keyboard-operable equivalent.
          </Trans>
        }
      />
      <LayoutEditor
        {...(initial !== undefined ? { initial } : {})}
        sources={sources.data ?? []}
        overlays={overlays.data ?? []}
        onSave={handleSave}
        isSaving={save.isPending}
      />
    </>
  );
}
