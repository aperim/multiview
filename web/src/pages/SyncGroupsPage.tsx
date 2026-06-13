// Sync groups — synchronized presentation groups over managed devices
// (managed-devices.md §5/§8, ADR-M010).
//
// CRUD over /api/v1/sync-groups with per-member offset_ms and the
// target_skew_ms drift threshold. The tier column is HONEST: a group claims
// only the weakest member's published tier (display nodes frame-accurate,
// vendor decoders bounded skew, never more), and the MEASURED tier arrives on
// the realtime stream after a 202 measure. Cast devices are never offered as
// members (Tier D — never part of a synchronized canvas).
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import type { ColumnDef } from '@tanstack/react-table';
import { Link } from 'react-router-dom';
import { AlertTriangle, FlaskConical, Plus, Ruler, Trash2 } from 'lucide-react';

import { measureSyncGroup, testPatternSyncGroup } from '../devices/api';
import {
  emptySyncGroupForm,
  syncGroupFormFromRecord,
  syncGroupFormToBody,
  syncMemberDeviceOptions,
  validateSyncGroupForm,
} from '../devices/forms';
import type { SyncGroupField, SyncGroupFormState } from '../devices/forms';
import { useDevices, useSyncGroupStatuses, useSyncGroups } from '../devices/queries';
import type {
  DeviceView,
  SyncGroupStatusView,
  SyncGroupView,
  SyncTier,
} from '../devices/types';
import type { SaveResourceVars } from '../resources/queries';
import { CrudPage, NameCell, RowActions } from '../resources/CrudPage';
import type { FieldErrors } from '../resources/forms';
import {
  ApplySemanticsCallout,
  ExportConfigButton,
  FieldErrorMessage,
  FormField,
} from '../resources/FormControls';
import { HelpLink } from '../components/HelpLink';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Label } from '../components/ui/label';
import { toast } from '../components/ui/use-toast';

/** The localized label for an achieved sync tier. */
function tierLabel(tier: SyncTier): JSX.Element {
  switch (tier) {
    case 'frame-accurate':
      return <Trans>Frame-accurate</Trans>;
    case 'bounded-skew':
      return <Trans>Bounded skew</Trans>;
    case 'none':
      return <Trans>Not synchronized</Trans>;
  }
}

/** Format a measured skew (ms) to a stable one-decimal string. */
function formatSkewMs(skewMs: number): string {
  return `${skewMs.toFixed(1)} ms`;
}

/**
 * The server-computed achieved tier (weakest member, never over-claimed) with
 * the sole limiting member named where one exists, plus a text+icon drift-alarm
 * indicator. Until the runtime has a measurement the cell honestly reads
 * "Not measured" rather than guessing a tier from the driver.
 */
function AchievedTierCell({
  status,
}: {
  readonly status: SyncGroupStatusView | undefined;
}): JSX.Element {
  if (status === undefined) {
    return (
      <span className="text-xs text-muted-foreground">
        <Trans>Not measured</Trans>
      </span>
    );
  }
  return (
    <span className="inline-flex flex-wrap items-center gap-1.5">
      <Badge variant="outline">{tierLabel(status.achieved)}</Badge>
      {status.limitedBy !== undefined ? (
        <span className="text-xs text-muted-foreground">
          <Trans>limited by {status.limitedBy}</Trans>
        </span>
      ) : null}
      {status.driftAlarm ? (
        // WCAG: the alarm carries an icon AND text, never colour alone.
        <Badge variant="destructive" className="gap-1">
          <AlertTriangle aria-hidden="true" className="size-3" />
          <Trans>Drift alarm</Trans>
        </Badge>
      ) : null}
    </span>
  );
}

/**
 * The worst measured member skew across the group (the server's runtime
 * measurement). Absent until a measurement exists — never fabricated.
 */
function MeasuredSkewCell({
  status,
}: {
  readonly status: SyncGroupStatusView | undefined;
}): JSX.Element {
  const skew = status?.measuredSkewMs;
  if (skew === undefined) {
    return (
      <span className="text-xs text-muted-foreground">
        <Trans>—</Trans>
      </span>
    );
  }
  const overTarget =
    status !== undefined && skew > status.targetSkewMs;
  return (
    <span className="inline-flex items-center gap-1.5">
      <Badge variant={overTarget ? 'destructive' : 'outline'}>
        {formatSkewMs(skew)}
      </Badge>
      {overTarget ? (
        <span className="sr-only">
          <Trans>over the target skew</Trans>
        </span>
      ) : null}
    </span>
  );
}

/** The editable member rows inside the create/edit dialog. */
function MemberRows({
  form,
  setForm,
  errors,
  devices,
}: {
  readonly form: SyncGroupFormState;
  readonly setForm: (next: SyncGroupFormState) => void;
  readonly errors: FieldErrors<SyncGroupField>;
  readonly devices: readonly DeviceView[];
}): JSX.Element {
  const { t } = useLingui();
  const options = syncMemberDeviceOptions(devices);
  return (
    <fieldset className="flex flex-col gap-2 rounded-md border p-3">
      <legend className="px-1 text-sm font-medium">
        <Trans>Members</Trans>
      </legend>
      <p className="text-xs text-muted-foreground">
        <Trans>
          Cast devices are never offered: they cannot join a synchronized
          canvas (Tier D). The group claims the weakest member's tier — never
          more.
        </Trans>
      </p>
      {form.members.map((member, index) => {
        const selectId = `sync-member-device-${String(index)}`;
        const rowError = errors[`member-${String(index)}`];
        return (
          <div key={selectId} className="flex flex-wrap items-end gap-2">
            <div className="flex flex-col gap-1">
              <Label htmlFor={selectId}>
                <Trans>Member device {index + 1}</Trans>
              </Label>
              <select
                id={selectId}
                className="h-9 rounded-md border border-input bg-transparent px-2 text-sm"
                value={member.device}
                onChange={(event): void => {
                  setForm({
                    ...form,
                    members: form.members.map((row, i) =>
                      i === index ? { ...row, device: event.target.value } : row,
                    ),
                  });
                }}
              >
                {options.map((id) => (
                  <option key={id} value={id}>
                    {id}
                  </option>
                ))}
              </select>
            </div>
            <FormField
              id={`sync-member-offset-${String(index)}`}
              label={t`Offset trim (ms)`}
              type="number"
              value={member.offsetMs}
              error={rowError}
              onChange={(next): void => {
                setForm({
                  ...form,
                  members: form.members.map((row, i) =>
                    i === index ? { ...row, offsetMs: next } : row,
                  ),
                });
              }}
            />
            <Button
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`${t`Remove member`}: ${member.device}`}
              onClick={(): void => {
                setForm({
                  ...form,
                  members: form.members.filter((_row, i) => i !== index),
                });
              }}
            >
              <Trash2 aria-hidden="true" />
              <Trans>Remove</Trans>
            </Button>
          </div>
        );
      })}
      <div>
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={options.length === 0}
          onClick={(): void => {
            const first = options.at(0);
            if (first === undefined) {
              return;
            }
            setForm({
              ...form,
              members: [...form.members, { device: first, offsetMs: '0' }],
            });
          }}
        >
          <Plus aria-hidden="true" />
          <Trans>Add member</Trans>
        </Button>
      </div>
      {errors.members !== undefined ? (
        <p className="text-sm text-destructive">
          <FieldErrorMessage code={errors.members} />
        </p>
      ) : null}
    </fieldset>
  );
}

/** Sync-groups management. */
export function SyncGroupsPage(): JSX.Element {
  const { t } = useLingui();
  const groups = useSyncGroups();
  const devices = useDevices();
  // The server computes the achieved tier (weakest member) + per-member skew +
  // drift state; the SPA reads it, it never re-derives the tier from drivers.
  const groupIds = (groups.data ?? []).map((group) => group.id);
  const statuses = useSyncGroupStatuses(groupIds);

  const measureNow = (group: SyncGroupView): void => {
    measureSyncGroup(group.id)
      .then((accepted): void => {
        toast({
          title: t`Measurement running`,
          description: t`Operation ${accepted.operation_id}; the measured skew arrives on the realtime stream.`,
        });
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not measure`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };

  const testPatternNow = (group: SyncGroupView): void => {
    testPatternSyncGroup(group.id)
      .then((accepted): void => {
        toast({
          title: t`Test pattern running`,
          description: t`Operation ${accepted.operation_id}; the group's displays show a frame counter + flash for visual sync verification.`,
        });
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not start the test pattern`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };

  const columns = (
    onEdit: (row: SyncGroupView) => void,
    onDelete: (row: SyncGroupView) => void,
  ): ColumnDef<SyncGroupView>[] => [
    {
      accessorKey: 'name',
      header: t`Name`,
      cell: (ctx): JSX.Element => <NameCell value={ctx.row.original.name} />,
    },
    {
      id: 'members',
      header: t`Members`,
      cell: (ctx): JSX.Element => (
        <span className="text-sm">{ctx.row.original.members.length}</span>
      ),
    },
    {
      id: 'target',
      header: t`Target skew`,
      cell: (ctx): JSX.Element => (
        <Badge variant="outline">{`${String(ctx.row.original.targetSkewMs)} ms`}</Badge>
      ),
    },
    {
      id: 'tier',
      header: (): JSX.Element => (
        <span className="inline-flex items-center gap-1.5">
          <Trans>Achieved tier</Trans>
          <HelpLink to="/help/sync" label={t`About sync tiers`} compact />
        </span>
      ),
      cell: (ctx): JSX.Element => (
        <AchievedTierCell status={statuses[ctx.row.original.id]} />
      ),
    },
    {
      id: 'skew',
      header: t`Measured skew`,
      cell: (ctx): JSX.Element => (
        <MeasuredSkewCell status={statuses[ctx.row.original.id]} />
      ),
    },
    {
      id: 'measure',
      header: t`Actions`,
      cell: (ctx): JSX.Element => (
        <span className="inline-flex flex-wrap items-center gap-1.5">
          <Button
            variant="outline"
            size="sm"
            aria-label={`${t`Measure skew`}: ${ctx.row.original.name}`}
            onClick={(): void => {
              measureNow(ctx.row.original);
            }}
          >
            <Ruler aria-hidden="true" />
            <Trans>Measure</Trans>
          </Button>
          <Button
            variant="outline"
            size="sm"
            aria-label={`${t`Show test pattern`}: ${ctx.row.original.name}`}
            onClick={(): void => {
              testPatternNow(ctx.row.original);
            }}
          >
            <FlaskConical aria-hidden="true" />
            <Trans>Test pattern</Trans>
          </Button>
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
          editLabel={t`Edit sync group`}
          deleteLabel={t`Delete sync group`}
          editDisabledReason={
            ctx.row.original.editable
              ? undefined
              : t`this document's shape isn't editable in this UI; it is preserved as authored`
          }
          onEdit={onEdit}
          onDelete={onDelete}
        />
      ),
    },
  ];

  return (
    <CrudPage<SyncGroupView, SyncGroupFormState, SyncGroupField>
      kind="sync-groups"
      title={<Trans>Sync groups</Trans>}
      description={
        <span className="inline-flex flex-wrap items-center gap-x-3 gap-y-1">
          <Trans>
            Group display devices for synchronized presentation, with
            per-member offset trims and a drift alarm threshold.
          </Trans>
          <HelpLink to="/help/sync" label={t`About synchronized output`} />
        </span>
      }
      newLabel={t`New sync group`}
      dialogCreateTitle={t`New sync group`}
      dialogEditTitle={t`Edit sync group`}
      dialogDescription={t`A sync group aligns its member devices' presentation; the achieved tier is measured, never assumed.`}
      caption={t`Configured sync groups.`}
      emptyMessage={<Trans>No sync groups configured.</Trans>}
      loadingMessage={<Trans>Loading sync groups…</Trans>}
      errorPrefix={<Trans>Could not load sync groups:</Trans>}
      headerExtras={
        <>
          <Button asChild variant="outline">
            <Link to="/devices">
              <Trans>Devices</Trans>
            </Link>
          </Button>
          <ExportConfigButton compact />
        </>
      }
      callout={
        <ApplySemanticsCallout
          helpTo="/help/sync"
          helpLabel={t`How sync groups apply`}
          message={
            <Trans>
              Groups are stored and exported with the configuration; alignment
              and drift measurement run against the adopted member devices.
              Program output is never interrupted by sync changes.
            </Trans>
          }
        />
      }
      savedDescription={t`Stored. Drift is measured against the group's target skew; the achieved tier is reported, never over-claimed.`}
      deletedDescription={t`Removed. Member devices keep running; only the alignment grouping is gone.`}
      list={groups.data ?? []}
      isPending={groups.isPending}
      isError={groups.isError}
      errorMessage={groups.error?.message}
      columns={columns}
      rowId={(row): string => row.id}
      rowName={(row): string => row.name}
      emptyForm={emptySyncGroupForm}
      formFromRecord={syncGroupFormFromRecord}
      validate={validateSyncGroupForm}
      toSaveVars={(form, creating): SaveResourceVars => ({
        id: creating ? form.id.trim() : form.id,
        create: creating,
        input: { name: form.name.trim(), body: syncGroupFormToBody(form) },
      })}
      renderFields={(form, setForm, creating, errors): JSX.Element => (
        <>
          <FormField
            id="sync-group-id"
            label={t`Identifier`}
            value={form.id}
            disabled={!creating}
            required={creating}
            placeholder={t`e.g. lobby-wall`}
            error={errors.id}
            onChange={(next): void => {
              setForm({ ...form, id: next });
            }}
          />
          <FormField
            id="sync-group-name"
            label={t`Name`}
            value={form.name}
            required
            error={errors.name}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          <FormField
            id="sync-group-skew"
            label={t`Target skew (ms)`}
            type="number"
            value={form.targetSkewMs}
            required
            placeholder="100"
            error={errors.targetSkewMs}
            hint={
              <Trans>
                Drift-alarm threshold, 1–10000 ms: a member drifting past this
                raises a warning alarm.
              </Trans>
            }
            onChange={(next): void => {
              setForm({ ...form, targetSkewMs: next });
            }}
          />
          <MemberRows
            form={form}
            setForm={setForm}
            errors={errors}
            devices={devices.data ?? []}
          />
        </>
      )}
    />
  );
}
