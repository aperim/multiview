// Sources — kind-specific typed management of the ingest inputs.
//
// Every form field maps 1:1 onto the config `Source` schema
// (crates/multiview-config/src/schema.rs) via the pure mapping in
// ../resources/forms (unit-tested); the control plane 422s anything else
// (ADR-W015). The LIVE STATUS column reads the realtime tile map the WebSocket
// hook mirrors into the Query cache; a source outside the running engine shows
// "—" with an explanation, because honesty beats a fabricated state.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import type { ColumnDef } from '@tanstack/react-table';

import { useSources } from '../resources/queries';
import type { SaveResourceVars } from '../resources/queries';
import type { SourceView } from '../resources/types';
import { SOURCE_KINDS } from '../resources/types';
import {
  CAPTION_MODES,
  CLOCK_FACES,
  emptySourceForm,
  PIN_VENDORS,
  sourceFormFromRecord,
  sourceFormToBody,
  sourceKindHasUrl,
  validateSourceForm,
  withSourceKind,
} from '../resources/forms';
import type {
  CaptionsMode,
  ClockFace,
  FieldErrors,
  PinVendor,
  RtspTransport,
  SourceField,
  SourceFormKind,
  SourceFormState,
  WallClockChoice,
} from '../resources/forms';
import { CrudPage, KindCell, NameCell, RowActions } from '../resources/CrudPage';
import {
  AdvancedSection,
  ApplySemanticsCallout,
  CheckboxField,
  ExportConfigButton,
  FormField,
  SelectField,
} from '../resources/FormControls';
import { tileForSource, useLiveTiles } from '../resources/useLiveTiles';
import type { LiveTile } from '../realtime/useEngineEvents';
import { HelpLink } from '../components/HelpLink';
import { TileStateBadge } from '../components/TileStateBadge';

/** The form-kind options offered in the picker (no legacy alias). */
const FORM_KINDS: readonly SourceFormKind[] = SOURCE_KINDS.filter(
  (kind): kind is SourceFormKind => kind !== 'test',
);

// Radix Select forbids an empty item value, so the "engine default" choice
// rides a sentinel and maps onto the form's '' (= omit the rtsp block).
type RtspTransportChoice = 'auto' | 'tcp' | 'udp';
const RTSP_TRANSPORT_CHOICES: readonly RtspTransportChoice[] = ['auto', 'tcp', 'udp'];

function toTransportChoice(value: RtspTransport): RtspTransportChoice {
  return value === '' ? 'auto' : value;
}

function fromTransportChoice(value: RtspTransportChoice): RtspTransport {
  return value === 'auto' ? '' : value;
}
const WALLCLOCK_CHOICES: readonly WallClockChoice[] = ['default', 'use', 'discard'];

/** The realtime status cell: the tile badge, or an honest "not running" dash. */
function SourceStatusCell({ tile }: { readonly tile: LiveTile | undefined }): JSX.Element {
  const { t } = useLingui();
  if (tile === undefined) {
    return (
      <span
        className="text-sm text-muted-foreground"
        title={t`This source is not part of the running engine, so it has no live state. Synthetic sources join live once a tile binds them; other kinds join after a config export + restart.`}
      >
        <span aria-hidden="true">—</span>
        <span className="sr-only">
          <Trans>
            Not in the running engine — no live state. Synthetic sources join
            live once a tile binds them; other kinds join after a config export
            + restart.
          </Trans>
        </span>
      </span>
    );
  }
  return <TileStateBadge state={tile.state} />;
}

/** The kind-specific locator + parameter fields. */
function SourceKindFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element | null {
  const { t } = useLingui();
  if (sourceKindHasUrl(form.kind)) {
    const placeholder = ((): string => {
      switch (form.kind) {
        case 'rtsp':
          return 'rtsp://[2001:db8::1]:8554/stream';
        case 'hls':
          return 'https://example.com/live/master.m3u8';
        case 'youtube':
          return 'https://www.youtube.com/watch?v=…';
        case 'srt':
          return 'srt://[2001:db8::2]:7001';
        case 'rtmp':
          return 'rtmp://ingest.example/app/key';
        default:
          return 'udp://[ff3e::1]:5004';
      }
    })();
    return (
      <>
        <FormField
          id="source-url"
          label={form.kind === 'youtube' ? t`Watch / live / channel URL` : t`Source URL`}
          value={form.url}
          required
          placeholder={placeholder}
          error={errors.url}
          onChange={(next): void => {
            setForm({ ...form, url: next });
          }}
        />
        {form.kind === 'rtsp' ? (
          <SelectField<RtspTransportChoice>
            label={t`RTSP transport`}
            value={toTransportChoice(form.rtspTransport)}
            options={RTSP_TRANSPORT_CHOICES}
            optionLabel={(option): JSX.Element =>
              option === 'auto' ? <Trans>Automatic (default)</Trans> : <>{option}</>
            }
            onChange={(next): void => {
              setForm({ ...form, rtspTransport: fromTransportChoice(next) });
            }}
          />
        ) : null}
      </>
    );
  }
  switch (form.kind) {
    case 'ndi':
      return (
        <FormField
          id="source-ndi-name"
          label={t`NDI source name`}
          value={form.ndiName}
          required
          placeholder={t`STUDIO (CAM 1)`}
          error={errors.ndiName}
          onChange={(next): void => {
            setForm({ ...form, ndiName: next });
          }}
        />
      );
    case 'file':
      return (
        <FormField
          id="source-path"
          label={t`File path`}
          value={form.path}
          required
          placeholder="/media/clip.mp4"
          error={errors.path}
          onChange={(next): void => {
            setForm({ ...form, path: next });
          }}
        />
      );
    case 'solid':
      return (
        <FormField
          id="source-color"
          label={t`Fill colour`}
          value={form.color}
          required
          placeholder="#101014"
          error={errors.color}
          onChange={(next): void => {
            setForm({ ...form, color: next });
          }}
        />
      );
    case 'clock':
      return (
        <>
          <SelectField<ClockFace>
            label={t`Face`}
            value={form.clockFace}
            options={CLOCK_FACES}
            onChange={(next): void => {
              setForm({ ...form, clockFace: next });
            }}
          />
          <CheckboxField
            id="source-clock-12h"
            label={t`12-hour clock`}
            checked={form.clockTwelveHour}
            onChange={(next): void => {
              setForm({ ...form, clockTwelveHour: next });
            }}
          />
          <FormField
            id="source-clock-tz"
            label={t`Timezone offset (minutes from UTC)`}
            type="number"
            value={form.clockTzMinutes}
            placeholder="0"
            error={errors.clockTzMinutes}
            hint={<Trans>Whole minutes between −720 and 840 (e.g. 600 = UTC+10).</Trans>}
            onChange={(next): void => {
              setForm({ ...form, clockTzMinutes: next });
            }}
          />
        </>
      );
    default:
      // `bars` carries only its kind tag.
      return null;
  }
}

/** The collapsible Advanced block (captions / colour / pin / wallclock / auth). */
function SourceAdvancedFields({
  form,
  setForm,
  errors,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
  readonly errors: FieldErrors<SourceField>;
}): JSX.Element {
  const { t } = useLingui();
  const captionLabel = (mode: CaptionsMode): JSX.Element => {
    switch (mode) {
      case 'none':
        return <Trans>None (no captions decoded)</Trans>;
      case 'auto':
        return <Trans>Auto (first usable track)</Trans>;
      case 'off':
        return <Trans>Off (pinned off)</Trans>;
      case 'teletext_page':
        return <Trans>DVB teletext page</Trans>;
      case 'track':
        return <Trans>Subtitle track (id / language)</Trans>;
      case 'embedded_cc':
        return <Trans>Embedded CEA-608/708</Trans>;
      case 'sidecar':
        return <Trans>Sidecar file (SRT/WebVTT)</Trans>;
    }
  };
  const wallclockLabel = (choice: WallClockChoice): JSX.Element => {
    switch (choice) {
      case 'default':
        return <Trans>Engine default (reclock to house)</Trans>;
      case 'use':
        return <Trans>Use the source wall-clock when trusted</Trans>;
      case 'discard':
        return <Trans>Discard the source wall-clock</Trans>;
    }
  };
  return (
    <AdvancedSection summary={t`Advanced`}>
      <SelectField<CaptionsMode>
        label={t`Captions`}
        value={form.captionsMode}
        options={CAPTION_MODES}
        optionLabel={captionLabel}
        onChange={(next): void => {
          setForm({ ...form, captionsMode: next });
        }}
      />
      {form.captionsMode === 'teletext_page' ? (
        <FormField
          id="source-captions-page"
          label={t`Teletext page`}
          type="number"
          value={form.captionsPage}
          placeholder="801"
          error={errors.captionsPage}
          hint={<Trans>Magazine-addressed page, 100–899.</Trans>}
          onChange={(next): void => {
            setForm({ ...form, captionsPage: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'track' ? (
        <FormField
          id="source-captions-track"
          label={t`Track id or language tag`}
          value={form.captionsTrack}
          placeholder="eng"
          error={errors.captionsTrack}
          onChange={(next): void => {
            setForm({ ...form, captionsTrack: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'embedded_cc' ? (
        <FormField
          id="source-captions-field"
          label={t`Caption field / service`}
          value={form.captionsField}
          placeholder="cc1"
          error={errors.captionsField}
          onChange={(next): void => {
            setForm({ ...form, captionsField: next });
          }}
        />
      ) : null}
      {form.captionsMode === 'sidecar' ? (
        <FormField
          id="source-captions-path"
          label={t`Sidecar file path`}
          value={form.captionsPath}
          placeholder="/subs/program.vtt"
          error={errors.captionsPath}
          onChange={(next): void => {
            setForm({ ...form, captionsPath: next });
          }}
        />
      ) : null}

      <CheckboxField
        id="source-color-override"
        label={t`Override the detected colour metadata`}
        checked={form.colorOverrideEnabled}
        onChange={(next): void => {
          setForm({ ...form, colorOverrideEnabled: next });
        }}
      />
      {form.colorOverrideEnabled ? (
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id="source-color-primaries"
            label={t`Primaries`}
            value={form.colorPrimaries}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorPrimaries: next });
            }}
          />
          <FormField
            id="source-color-transfer"
            label={t`Transfer`}
            value={form.colorTransfer}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorTransfer: next });
            }}
          />
          <FormField
            id="source-color-matrix"
            label={t`Matrix`}
            value={form.colorMatrix}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorMatrix: next });
            }}
          />
          <FormField
            id="source-color-range"
            label={t`Range`}
            value={form.colorRange}
            placeholder="auto"
            onChange={(next): void => {
              setForm({ ...form, colorRange: next });
            }}
          />
        </div>
      ) : null}

      <CheckboxField
        id="source-gpu-pin"
        label={t`Pin decode to a specific GPU`}
        checked={form.gpuPinEnabled}
        onChange={(next): void => {
          setForm({ ...form, gpuPinEnabled: next });
        }}
      />
      {form.gpuPinEnabled ? (
        <div className="grid grid-cols-2 gap-3">
          <SelectField<PinVendor>
            label={t`GPU vendor`}
            value={form.gpuPinVendor}
            options={PIN_VENDORS}
            onChange={(next): void => {
              setForm({ ...form, gpuPinVendor: next });
            }}
          />
          <FormField
            id="source-gpu-stable-id"
            label={t`Stable device id`}
            value={form.gpuPinStableId}
            placeholder="GPU-9d3b…  /  0000:03:00.0"
            error={errors.gpuPinStableId}
            hint={<Trans>The vendor's stable handle (UUID / PCI bus id), never the index.</Trans>}
            onChange={(next): void => {
              setForm({ ...form, gpuPinStableId: next });
            }}
          />
        </div>
      ) : null}

      <SelectField<WallClockChoice>
        label={t`Wall-clock handling`}
        value={form.wallclock}
        options={WALLCLOCK_CHOICES}
        optionLabel={wallclockLabel}
        onChange={(next): void => {
          setForm({ ...form, wallclock: next });
        }}
      />

      <FormField
        id="source-auth"
        label={t`Credentials secret reference`}
        value={form.authSecretRef}
        placeholder="op://Servers/cam/credentials"
        hint={<Trans>A reference only — never a plaintext secret. Leave blank for none.</Trans>}
        onChange={(next): void => {
          setForm({ ...form, authSecretRef: next });
        }}
      />
    </AdvancedSection>
  );
}

/** Sources management (ingest). */
export function SourcesPage(): JSX.Element {
  const { t } = useLingui();
  const sources = useSources();
  const tiles = useLiveTiles();

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
      cell: (ctx): JSX.Element => <KindCell value={ctx.row.original.rawKind} />,
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
      id: 'status',
      header: (): JSX.Element => (
        <span className="inline-flex items-center gap-1.5">
          <Trans>Live status</Trans>
          <HelpLink
            to="/help/concepts/resilience#tile-lifecycle"
            label={t`About tile lifecycle states`}
            compact
          />
        </span>
      ),
      cell: (ctx): JSX.Element => (
        <SourceStatusCell tile={tileForSource(tiles, ctx.row.original.id)} />
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
          editDisabledReason={
            ctx.row.original.editable
              ? undefined
              : t`kind "${ctx.row.original.rawKind}" isn't editable in this UI; the document is preserved as authored`
          }
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<SourceView, SourceFormState, SourceField>
      kind="sources"
      title={<Trans>Sources</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>Add and manage live ingest sources.</Trans>
          <HelpLink to="/help/concepts/transports#choosing" label={t`About source transports`} />
        </span>
      }
      newLabel={t`New source`}
      dialogCreateTitle={t`New source`}
      dialogEditTitle={t`Edit source`}
      dialogDescription={t`A source is an ingest input bound into the multiview by id.`}
      caption={t`Configured ingest sources.`}
      emptyMessage={<Trans>No sources configured.</Trans>}
      loadingMessage={<Trans>Loading sources…</Trans>}
      errorPrefix={<Trans>Could not load sources:</Trans>}
      headerExtras={<ExportConfigButton compact />}
      callout={
        <ApplySemanticsCallout
          helpTo="/help/config#sources"
          helpLabel={t`How configuration applies`}
          message={
            <Trans>
              Synthetic sources (bars, solid colour, clock) apply to the running
              engine immediately when saved, and deleting any source removes it
              from the running engine immediately. Network and file sources are
              stored and go live via config export + restart. Each save response
              declares which applied (X-Multiview-Apply: live or restart).
            </Trans>
          }
        />
      }
      savedDescription={t`Stored. Synthetic sources (bars, solid colour, clock) apply to the running engine immediately; other kinds go live via config export + restart.`}
      deletedDescription={t`Removed. The running engine drops the source immediately — tiles bound to it show the no-signal slate until re-routed.`}
      list={sources.data ?? []}
      isPending={sources.isPending}
      isError={sources.isError}
      errorMessage={sources.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptySourceForm}
      formFromRecord={sourceFormFromRecord}
      validate={validateSourceForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: sourceFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          <FormField
            id="source-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. cam-north`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="source-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <SelectField<SourceFormKind>
            label={t`Kind`}
            value={form.kind}
            options={FORM_KINDS}
            trailing={
              <HelpLink
                to="/help/concepts/transports#choosing"
                label={t`About source transports`}
                compact
              />
            }
            onChange={(next): void => {
              setForm(withSourceKind(form, next));
            }}
          />
          <SourceKindFields form={form} setForm={setForm} errors={errors} />
          <SourceAdvancedFields form={form} setForm={setForm} errors={errors} />
        </>
      )}
    />
  );
}
