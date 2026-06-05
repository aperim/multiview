// The Sources / Outputs / Overlays resource views — fully manageable.
//
// Each page lists its resource (read through the projected query hooks), and
// offers create / edit / delete: a labelled, accessible Dialog form for create
// and edit (edit loads the current record via GET, prefills, and PUTs with
// `If-Match`), and a confirmation Dialog for delete — all with success/error
// toasts. Status is conveyed by text + glyph, never colour alone. Visible
// strings are i18n'd via Lingui.
import { useId, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Pencil, Plus, Trash2 } from 'lucide-react';
import type { ColumnDef } from '@tanstack/react-table';

import {
  getResource,
  outputHasCodec,
  outputTargetKey,
  sourceLocatorKey,
  toOutputView,
  toOverlayView,
  toSourceView,
} from '../resources/api';
import {
  useDeleteResource,
  useOutputs,
  useOverlays,
  useSaveResource,
  useSources,
} from '../resources/queries';
import type { ResourceContext, SaveResourceVars } from '../resources/queries';
import type {
  OutputKind,
  OutputView,
  OverlayKind,
  OverlayView,
  ResourceKind,
  SourceKind,
  SourceView,
} from '../resources/types';
import {
  OUTPUT_KINDS,
  OVERLAY_KINDS,
  SOURCE_KINDS,
} from '../resources/types';
import { ResourceTable } from '../resources/ResourceTable';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';
import { toast } from '../components/ui/use-toast';

// --- shared form-state types ------------------------------------------------

interface SourceFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: SourceKind;
  /** The kind's locator (url/name/path); unused for the `test` kind. */
  readonly locator: string;
}

interface OutputFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: OutputKind;
  /** The kind's target (mount/path/url/name). */
  readonly target: string;
  /** The video codec (unused for NDI). */
  readonly codec: string;
}

interface OverlayFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: OverlayKind;
  /** Attachment target (`canvas` or a cell id). */
  readonly target: string;
  readonly z: number;
}

/** Map an OutputView display kind back to the config wire kind. */
function outputWireKind(kind: OutputKind): string {
  switch (kind) {
    case 'rtsp':
      return 'rtsp_server';
    case 'll-hls':
      return 'll_hls';
    default:
      return kind;
  }
}

// --- a labelled <Select> over a fixed kind list -----------------------------

function KindSelect<K extends string>({
  labelId,
  value,
  options,
  onChange,
}: {
  readonly labelId: string;
  readonly value: K;
  readonly options: readonly K[];
  readonly onChange: (next: K) => void;
}): JSX.Element {
  return (
    <Select
      value={value}
      onValueChange={(next): void => {
        const picked = options.find((option) => option === next);
        if (picked !== undefined) {
          onChange(picked);
        }
      }}
    >
      <SelectTrigger aria-labelledby={labelId}>
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {options.map((option) => (
          <SelectItem key={option} value={option}>
            {option}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

// --- a small labelled text field --------------------------------------------

function TextField({
  id,
  label,
  value,
  onChange,
  disabled,
  required,
  placeholder,
  type,
}: {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  readonly onChange: (next: string) => void;
  readonly disabled?: boolean;
  readonly required?: boolean;
  readonly placeholder?: string;
  readonly type?: string;
}): JSX.Element {
  return (
    <div className="flex flex-col gap-1">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        type={type ?? 'text'}
        value={value}
        required={required ?? false}
        disabled={disabled ?? false}
        {...(placeholder !== undefined ? { placeholder } : {})}
        onChange={(event): void => {
          onChange(event.target.value);
        }}
      />
    </div>
  );
}

// --- the generic CRUD page scaffold -----------------------------------------

interface CrudPageProps<View, Form> {
  readonly kind: ResourceKind;
  readonly title: JSX.Element;
  readonly description: JSX.Element;
  readonly newLabel: string;
  readonly dialogCreateTitle: string;
  readonly dialogEditTitle: string;
  readonly dialogDescription: string;
  readonly caption: string;
  readonly emptyMessage: JSX.Element;
  readonly list: readonly View[];
  readonly isPending: boolean;
  readonly isError: boolean;
  readonly errorMessage: string | undefined;
  readonly loadingMessage: JSX.Element;
  readonly errorPrefix: JSX.Element;
  readonly columns: (onEdit: (row: View) => void, onDelete: (row: View) => void) => ColumnDef<View>[];
  /** A fresh, empty form for a create. */
  readonly emptyForm: () => Form;
  /** Project a server record onto the editable form (for edit prefill). */
  readonly formFromRecord: (record: { id: string; name: string; body: Record<string, unknown> }) => Form;
  /**
   * Validate the form; return a localized error message to display, or
   * `undefined` when the form is complete and safe to submit.
   */
  readonly validate: (form: Form, creating: boolean) => string | undefined;
  /** Build the save vars (id + payload) from a valid form. */
  readonly toSaveVars: (form: Form, creating: boolean) => SaveResourceVars;
  /** The id of a view row (for delete + edit addressing). */
  readonly rowId: (row: View) => string;
  /** The display name of a view row (for the delete confirmation). */
  readonly rowName: (row: View) => string;
  /** Render the kind-specific form fields. */
  readonly renderFields: (
    form: Form,
    setForm: (next: Form) => void,
    creating: boolean,
    ids: { readonly id: string; readonly name: string; readonly kind: string },
  ) => JSX.Element;
}

const RESOURCE_CONTEXT: ResourceContext = {};

function CrudPage<View, Form>(props: CrudPageProps<View, Form>): JSX.Element {
  const { t } = useLingui();
  const save = useSaveResource(props.kind, RESOURCE_CONTEXT);
  const remove = useDeleteResource(props.kind, RESOURCE_CONTEXT);

  const [form, setForm] = useState<Form | null>(null);
  const [creating, setCreating] = useState(true);
  const [pendingDelete, setPendingDelete] = useState<View | null>(null);
  const [fieldError, setFieldError] = useState<string | undefined>(undefined);

  const idFieldId = useId();
  const nameFieldId = useId();
  const kindFieldId = useId();

  const openCreate = (): void => {
    setCreating(true);
    setFieldError(undefined);
    setForm(props.emptyForm());
  };

  const openEdit = (row: View): void => {
    setCreating(false);
    setFieldError(undefined);
    void getResource(props.kind, props.rowId(row), RESOURCE_CONTEXT)
      .then((result): void => {
        setForm(props.formFromRecord(result.record));
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not load for editing`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };

  const columns = props.columns(openEdit, setPendingDelete);

  const closeForm = (): void => {
    setForm(null);
  };

  const submitForm = (): void => {
    if (form === null) {
      return;
    }
    const invalid = props.validate(form, creating);
    if (invalid !== undefined) {
      setFieldError(invalid);
      return;
    }
    setFieldError(undefined);
    save.mutate(props.toSaveVars(form, creating), {
      onSuccess: (): void => {
        toast({ title: creating ? t`Created` : t`Saved` });
        closeForm();
      },
      onError: (error): void => {
        toast({
          title: creating ? t`Could not create` : t`Could not save`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
  };

  const confirmDelete = (): void => {
    const target = pendingDelete;
    if (target === null) {
      return;
    }
    remove.mutate(props.rowId(target), {
      onSuccess: (): void => {
        toast({ title: t`Deleted` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not delete`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingDelete(null);
  };

  return (
    <>
      <PageHeader
        title={props.title}
        description={props.description}
        actions={
          <Button onClick={openCreate}>
            <Plus aria-hidden="true" />
            {props.newLabel}
          </Button>
        }
      />

      {props.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          {props.loadingMessage}
        </p>
      ) : props.isError ? (
        <p role="alert" className="text-sm text-destructive">
          {props.errorPrefix} {props.errorMessage ?? ''}
        </p>
      ) : (
        <ResourceTable
          rows={props.list}
          columns={columns}
          caption={props.caption}
          emptyMessage={props.emptyMessage}
        />
      )}

      <Dialog
        open={form !== null}
        onOpenChange={(open): void => {
          if (!open) {
            closeForm();
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              {creating ? props.dialogCreateTitle : props.dialogEditTitle}
            </DialogTitle>
            <DialogDescription>{props.dialogDescription}</DialogDescription>
          </DialogHeader>
          {form !== null ? (
            <form
              className="flex flex-col gap-4"
              onSubmit={(event): void => {
                event.preventDefault();
                submitForm();
              }}
            >
              {props.renderFields(form, setForm, creating, {
                id: idFieldId,
                name: nameFieldId,
                kind: kindFieldId,
              })}
              {fieldError !== undefined ? (
                <p role="alert" className="text-sm text-destructive">
                  {fieldError}
                </p>
              ) : null}
              <DialogFooter>
                <Button type="button" variant="outline" onClick={closeForm}>
                  <Trans>Cancel</Trans>
                </Button>
                <Button type="submit" disabled={save.isPending}>
                  {creating ? <Trans>Create</Trans> : <Trans>Save</Trans>}
                </Button>
              </DialogFooter>
            </form>
          ) : null}
        </DialogContent>
      </Dialog>

      <Dialog
        open={pendingDelete !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setPendingDelete(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Delete this resource?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>
                This permanently removes the resource. Running outputs are not
                affected until a new configuration is applied.
              </Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingDelete !== null ? (
            <p className="text-sm">
              <span lang="" dir="auto" className="font-medium">
                {props.rowName(pendingDelete)}
              </span>
            </p>
          ) : null}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setPendingDelete(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button
              variant="destructive"
              disabled={remove.isPending}
              onClick={confirmDelete}
            >
              <Trans>Delete</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}

// --- shared bits ------------------------------------------------------------

function NameCell({ value }: { readonly value: string }): JSX.Element {
  return (
    <span lang="" dir="auto" className="font-medium">
      {value}
    </span>
  );
}

function KindCell({ value }: { readonly value: string }): JSX.Element {
  return <Badge variant="outline">{value}</Badge>;
}

function RowActions<View>({
  row,
  name,
  editLabel,
  deleteLabel,
  onEdit,
  onDelete,
}: {
  readonly row: View;
  readonly name: string;
  readonly editLabel: string;
  readonly deleteLabel: string;
  readonly onEdit: (row: View) => void;
  readonly onDelete: (row: View) => void;
}): JSX.Element {
  return (
    <div className="flex items-center gap-2">
      <Button
        variant="outline"
        size="sm"
        aria-label={`${editLabel}: ${name}`}
        onClick={(): void => {
          onEdit(row);
        }}
      >
        <Pencil aria-hidden="true" />
        <Trans>Edit</Trans>
      </Button>
      <Button
        variant="ghost"
        size="sm"
        aria-label={`${deleteLabel}: ${name}`}
        onClick={(): void => {
          onDelete(row);
        }}
      >
        <Trash2 aria-hidden="true" />
        <Trans>Delete</Trans>
      </Button>
    </div>
  );
}

// --- Sources ----------------------------------------------------------------

/** Sources management (ingest). */
export function SourcesPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();

  // The label + placeholder for a source kind's single locator field. `test`
  // has no locator (handled by the `renderFields` guard below).
  const locatorLabel = (kind: SourceKind): string => {
    switch (kind) {
      case 'ndi':
        return t`NDI source name`;
      case 'file':
        return t`File path`;
      default:
        return t`Locator URL`;
    }
  };
  const locatorPlaceholder = (kind: SourceKind): string => {
    switch (kind) {
      case 'ndi':
        return t`STUDIO (CAM 1)`;
      case 'file':
        return t`/media/clip.mp4`;
      default:
        return 'rtsp://host/stream';
    }
  };

  const columns = (
    onEdit: (row: SourceView) => void,
    onDelete: (row: SourceView) => void,
  ): ColumnDef<SourceView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      accessorKey: 'kind',
      header: t`Kind`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.kind} />,
    },
    {
      accessorKey: 'locator',
      header: t`Locator`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.locator ?? '—'}
        </code>
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit source`}
          deleteLabel={t`Delete source`}
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<SourceView, SourceFormState>
      kind="sources"
      title={<Trans>Sources</Trans>}
      description={<Trans>Add and manage live ingest sources.</Trans>}
      newLabel={t`New source`}
      dialogCreateTitle={t`New source`}
      dialogEditTitle={t`Edit source`}
      dialogDescription={t`A source is an ingest input bound into the multiview by id.`}
      caption={t`Configured ingest sources.`}
      emptyMessage={<Trans>No sources configured.</Trans>}
      loadingMessage={<Trans>Loading sources…</Trans>}
      errorPrefix={<Trans>Could not load sources:</Trans>}
      list={sources.data ?? []}
      isPending={sources.isPending}
      isError={sources.isError}
      errorMessage={sources.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={(): SourceFormState => ({ id: '', name: '', kind: 'rtsp', locator: '' })}
      formFromRecord={(record): SourceFormState => {
        const view = toSourceView(record);
        return {
          id: view.id,
          name: view.name,
          kind: view.kind,
          locator: view.locator ?? '',
        };
      }}
      validate={(form, creating): string | undefined => {
        if (creating && form.id.trim() === '') {
          return t`An identifier is required.`;
        }
        if (form.name.trim() === '') {
          return t`A name is required.`;
        }
        // Every kind except `test` needs its locator (url/name/path).
        if (sourceLocatorKey(form.kind) !== undefined && form.locator.trim() === '') {
          return t`A locator is required for this source kind.`;
        }
        return undefined;
      }}
      toSaveVars={(form, creating): SaveResourceVars => {
        const body: Record<string, unknown> = { kind: form.kind };
        const locatorKey = sourceLocatorKey(form.kind);
        if (locatorKey !== undefined) {
          body[locatorKey] = form.locator.trim();
        }
        return {
          id: creating ? form.id.trim() : form.id,
          create: creating,
          input: { name: form.name.trim(), body },
        };
      }}
      renderFields={(form, setForm, creating, ids): JSX.Element => (
        <>
          <TextField
            id={ids.id}
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. cam-north`}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <TextField
            id={ids.name}
            label={t`Name`}
            value={form.name}
            required
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <div className="flex flex-col gap-1">
            <Label id={ids.kind}>{t`Kind`}</Label>
            <KindSelect<SourceKind>
              labelId={ids.kind}
              value={form.kind}
              options={SOURCE_KINDS}
              onChange={(next): void => {
                setForm({ ...form, kind: next });
              }}
            />
          </div>
          {sourceLocatorKey(form.kind) !== undefined ? (
            <TextField
              id={`${ids.id}-locator`}
              label={locatorLabel(form.kind)}
              value={form.locator}
              required
              placeholder={locatorPlaceholder(form.kind)}
              onChange={(next): void => {
                setForm({ ...form, locator: next });
              }}
            />
          ) : null}
        </>
      )}
    />
  );
}

// --- Outputs ----------------------------------------------------------------

/** Outputs / transcoding. */
export function OutputsPage(): JSX.Element {
  const { t } = useLingui();
  const outputs = useOutputs();

  // The label + placeholder for an output kind's single target field.
  const targetLabel = (kind: OutputKind): string => {
    switch (outputTargetKey(kind)) {
      case 'mount':
        return t`Mount point`;
      case 'path':
        return t`Output path`;
      case 'name':
        return t`NDI source name`;
      default:
        return t`Destination URL`;
    }
  };
  const targetPlaceholder = (kind: OutputKind): string => {
    switch (outputTargetKey(kind)) {
      case 'mount':
        return '/multiview';
      case 'path':
        return '/var/www/hls';
      case 'name':
        return t`Multiview Program`;
      default:
        return 'rtmp://host/app/key';
    }
  };

  const columns = (
    onEdit: (row: OutputView) => void,
    onDelete: (row: OutputView) => void,
  ): ColumnDef<OutputView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      accessorKey: 'kind',
      header: t`Transport`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.kind} />,
    },
    {
      accessorKey: 'target',
      header: t`Destination`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.target ?? '—'}
        </code>
      ),
    },
    {
      accessorKey: 'codec',
      header: t`Codec`,
      cell: (ctx): JSX.Element => (
        <span className="text-sm text-muted-foreground">
          {ctx.row.original.codec ?? '—'}
        </span>
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit output`}
          deleteLabel={t`Delete output`}
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<OutputView, OutputFormState>
      kind="outputs"
      title={<Trans>Outputs</Trans>}
      description={<Trans>Configure output servers and renditions.</Trans>}
      newLabel={t`New output`}
      dialogCreateTitle={t`New output`}
      dialogEditTitle={t`Edit output`}
      dialogDescription={t`An output is a sink/server that publishes the program.`}
      caption={t`Configured output sinks.`}
      emptyMessage={<Trans>No outputs configured.</Trans>}
      loadingMessage={<Trans>Loading outputs…</Trans>}
      errorPrefix={<Trans>Could not load outputs:</Trans>}
      list={outputs.data ?? []}
      isPending={outputs.isPending}
      isError={outputs.isError}
      errorMessage={outputs.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={(): OutputFormState => ({
        id: '',
        name: '',
        kind: 'rtsp',
        target: '',
        codec: 'h264',
      })}
      formFromRecord={(record): OutputFormState => {
        const view = toOutputView(record);
        return {
          id: view.id,
          name: view.name,
          kind: view.kind,
          target: view.target ?? '',
          codec: view.codec ?? 'h264',
        };
      }}
      validate={(form, creating): string | undefined => {
        if (creating && form.id.trim() === '') {
          return t`An identifier is required.`;
        }
        if (form.name.trim() === '') {
          return t`A name is required.`;
        }
        if (form.target.trim() === '') {
          return t`A destination is required for this output kind.`;
        }
        if (outputHasCodec(form.kind) && form.codec.trim() === '') {
          return t`A codec is required for this output kind.`;
        }
        return undefined;
      }}
      toSaveVars={(form, creating): SaveResourceVars => {
        const body: Record<string, unknown> = { kind: outputWireKind(form.kind) };
        body[outputTargetKey(form.kind)] = form.target.trim();
        if (outputHasCodec(form.kind)) {
          body.codec = form.codec.trim();
        }
        return {
          id: creating ? form.id.trim() : form.id,
          create: creating,
          input: { name: form.name.trim(), body },
        };
      }}
      renderFields={(form, setForm, creating, ids): JSX.Element => (
        <>
          <TextField
            id={ids.id}
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. program-hls`}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <TextField
            id={ids.name}
            label={t`Name`}
            value={form.name}
            required
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <div className="flex flex-col gap-1">
            <Label id={ids.kind}>{t`Transport`}</Label>
            <KindSelect<OutputKind>
              labelId={ids.kind}
              value={form.kind}
              options={OUTPUT_KINDS}
              onChange={(next): void => {
                setForm({ ...form, kind: next });
              }}
            />
          </div>
          <TextField
            id={`${ids.id}-target`}
            label={targetLabel(form.kind)}
            value={form.target}
            required
            placeholder={targetPlaceholder(form.kind)}
            onChange={(next): void => {
              setForm({ ...form, target: next });
            }}
          />
          {outputHasCodec(form.kind) ? (
            <TextField
              id={`${ids.id}-codec`}
              label={t`Codec`}
              value={form.codec}
              required
              placeholder="h264"
              onChange={(next): void => {
                setForm({ ...form, codec: next });
              }}
            />
          ) : null}
        </>
      )}
    />
  );
}

// --- Overlays ---------------------------------------------------------------

/** Overlays + subtitles. */
export function OverlaysPage(): JSX.Element {
  const { t } = useLingui();
  const overlays = useOverlays();

  const columns = (
    onEdit: (row: OverlayView) => void,
    onDelete: (row: OverlayView) => void,
  ): ColumnDef<OverlayView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      accessorKey: 'kind',
      header: t`Kind`,
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.kind} />,
    },
    {
      accessorKey: 'target',
      header: t`Target`,
      cell: (ctx): JSX.Element => (
        <code className="text-xs text-muted-foreground" lang="" dir="auto">
          {ctx.row.original.target}
        </code>
      ),
    },
    {
      accessorKey: 'z',
      header: t`Stacking`,
      cell: (ctx): JSX.Element => (
        <span className="tabular-nums">{ctx.row.original.z}</span>
      ),
    },
    {
      id: 'actions',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <RowActions
          row={ctx.row.original}
          name={ctx.row.original.name}
          editLabel={t`Edit overlay`}
          deleteLabel={t`Delete overlay`}
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<OverlayView, OverlayFormState>
      kind="overlays"
      title={<Trans>Overlays</Trans>}
      description={<Trans>Manage overlay layers and subtitles.</Trans>}
      newLabel={t`New overlay`}
      dialogCreateTitle={t`New overlay`}
      dialogEditTitle={t`Edit overlay`}
      dialogDescription={t`An overlay is a layer composited over the program at a stacking order.`}
      caption={t`Configured overlay layers.`}
      emptyMessage={<Trans>No overlays configured.</Trans>}
      loadingMessage={<Trans>Loading overlays…</Trans>}
      errorPrefix={<Trans>Could not load overlays:</Trans>}
      list={overlays.data ?? []}
      isPending={overlays.isPending}
      isError={overlays.isError}
      errorMessage={overlays.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={(): OverlayFormState => ({
        id: '',
        name: '',
        kind: 'clock',
        target: 'canvas',
        z: 0,
      })}
      formFromRecord={(record): OverlayFormState => {
        const view = toOverlayView(record);
        return {
          id: view.id,
          name: view.name,
          kind: view.kind,
          target: view.target,
          z: view.z,
        };
      }}
      validate={(form, creating): string | undefined => {
        if (creating && form.id.trim() === '') {
          return t`An identifier is required.`;
        }
        if (form.name.trim() === '') {
          return t`A name is required.`;
        }
        if (form.target.trim() === '') {
          return t`A target is required.`;
        }
        return undefined;
      }}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: {
          name: form.name.trim(),
          body: { kind: form.kind, target: form.target.trim(), z: form.z, params: {} },
        },
      })}
      renderFields={(form, setForm, creating, ids): JSX.Element => (
        <>
          <TextField
            id={ids.id}
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. wall-clock`}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <TextField
            id={ids.name}
            label={t`Name`}
            value={form.name}
            required
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <div className="flex flex-col gap-1">
            <Label id={ids.kind}>{t`Kind`}</Label>
            <KindSelect<OverlayKind>
              labelId={ids.kind}
              value={form.kind}
              options={OVERLAY_KINDS}
              onChange={(next): void => {
                setForm({ ...form, kind: next });
              }}
            />
          </div>
          <TextField
            id={`${ids.id}-target`}
            label={t`Target`}
            value={form.target}
            required
            placeholder={t`canvas or a cell id`}
            onChange={(next): void => {
              setForm({ ...form, target: next });
            }}
          />
          <TextField
            id={`${ids.id}-z`}
            label={t`Stacking order`}
            value={String(form.z)}
            type="number"
            onChange={(next): void => {
              const parsed = Number.parseInt(next, 10);
              setForm({ ...form, z: Number.isFinite(parsed) ? parsed : 0 });
            }}
          />
        </>
      )}
    />
  );
}
