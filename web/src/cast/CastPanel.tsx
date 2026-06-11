// The cast surface on the Devices page (DEV-D3, ADR-M011): the ephemeral
// session list + the start sheet + save-as-device.
//
// Sessions are runtime-only (never exported) and each row shows its lifecycle
// state as icon+text (never colour alone) plus the honest Tier-D latency
// badge. State rides the conflated `device.status` realtime lane — session
// actors publish through the SAME latest-wins status registry as devices,
// keyed by the session id — with the per-id REST snapshot as fallback
// (useDeviceStatuses, reused from the devices domain), and the session doc's
// own server-resolved state as the final fallback. Stopping DELETEs the
// session: the control plane sends the receiver STOP that actually clears
// the TV.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQueryClient } from '@tanstack/react-query';
import { Cast } from 'lucide-react';

import { asDeviceState, operationErrorMessage, stopCastSession } from './api';
import type { CastSessionView } from './api';
import { CastLatencyBadge } from './CastLatencyBadge';
import { CastStartDialog } from './CastStartDialog';
import { SaveAsDeviceDialog } from './SaveAsDeviceDialog';
import { CAST_SESSIONS_QUERY_KEY, useCastSessions } from './queries';
import { DeviceStateBadge } from '../devices/DeviceStateBadge';
import { LastSeenCell } from '../devices/lastSeen';
import { useDeviceStatuses, useEngineClockRef } from '../devices/queries';
import type { DeviceStatus } from '../realtime/generated-types';
import type { EngineClockRef } from '../realtime/useEngineEvents';
import { HelpLink } from '../components/HelpLink';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../components/ui/card';
import { toast } from '../components/ui/use-toast';

/** One session row: identity, rendition, live state, honest latency, verbs. */
function SessionRow({
  session,
  status,
  clock,
  onSave,
}: {
  readonly session: CastSessionView;
  readonly status: DeviceStatus | undefined;
  readonly clock: EngineClockRef | undefined;
  readonly onSave: (session: CastSessionView) => void;
}): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  // The realtime/REST status lane wins; the doc's server-resolved state is
  // the fallback. An unrecognized token renders raw — never invented.
  const state = status?.state ?? asDeviceState(session.state);
  const title = session.name ?? session.address;

  const stopNow = (): void => {
    stopCastSession(session.id)
      .then((): void => {
        toast({
          title: t`Cast stopped`,
          description: t`The receiver app was stopped; the device returns to its idle screen.`,
        });
        void queryClient.invalidateQueries({ queryKey: CAST_SESSIONS_QUERY_KEY });
      })
      .catch((error: unknown): void => {
        toast({
          title: t`Could not stop the cast`,
          description: operationErrorMessage(error),
          variant: 'destructive',
        });
      });
  };

  return (
    <li className="flex flex-wrap items-center justify-between gap-2 rounded-md border p-2">
      <span className="inline-flex flex-wrap items-center gap-2">
        <span lang="" dir="auto" className="font-medium">
          {title}
        </span>
        <code className="text-xs text-muted-foreground" dir="ltr">
          {session.address}
        </code>
        <Badge variant="outline">{session.output}</Badge>
        {state !== undefined ? (
          <DeviceStateBadge state={state} />
        ) : (
          <code className="text-xs">{session.state}</code>
        )}
        <CastLatencyBadge />
        <LastSeenCell lastSeenTs={status?.last_seen_ts ?? undefined} clock={clock} />
      </span>
      <span className="inline-flex items-center gap-2">
        <Button
          size="sm"
          variant="outline"
          aria-label={`${t`Save as device`}: ${title}`}
          onClick={(): void => {
            onSave(session);
          }}
        >
          <Trans>Save as device…</Trans>
        </Button>
        <Button
          size="sm"
          variant="outline"
          aria-label={`${t`Stop cast`}: ${title}`}
          onClick={stopNow}
        >
          <Trans>Stop</Trans>
        </Button>
      </span>
    </li>
  );
}

/** The cast panel: ephemeral sessions + the ad-hoc start sheet. */
export function CastPanel(): JSX.Element {
  const { t } = useLingui();
  const sessions = useCastSessions();
  const sessionIds = (sessions.data ?? []).map((session) => session.id);
  const statuses = useDeviceStatuses(sessionIds);
  const clock = useEngineClockRef();
  const [startOpen, setStartOpen] = useState(false);
  const [saveFor, setSaveFor] = useState<CastSessionView | undefined>(undefined);

  return (
    <Card className="mb-4" data-testid="cast-panel">
      <CardHeader>
        <CardTitle className="flex flex-wrap items-center justify-between gap-2 text-base">
          <span className="inline-flex items-center gap-2">
            <Cast className="size-4" aria-hidden="true" />
            <Trans>Cast</Trans>
          </span>
          <Button
            variant="outline"
            size="sm"
            onClick={(): void => {
              setStartOpen(true);
            }}
          >
            <Trans>Cast to a device…</Trans>
          </Button>
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 text-sm">
        <p className="text-muted-foreground">
          <Trans>
            Cast an HLS rendition the engine already serves to a Google Cast
            device. Sessions here are ephemeral — they are not part of the
            configuration until saved as a device — and playback runs seconds
            behind live.
          </Trans>{' '}
          <HelpLink to="/help/casting" label={t`About casting`} compact />
        </p>
        {sessions.isError ? (
          <p className="text-destructive">
            <Trans>Could not load cast sessions:</Trans>{' '}
            {sessions.error.message}
          </p>
        ) : (sessions.data ?? []).length === 0 ? (
          <p className="text-muted-foreground">
            <Trans>No cast sessions running.</Trans>
          </p>
        ) : (
          <ul className="flex flex-col gap-2">
            {(sessions.data ?? []).map((session) => (
              <SessionRow
                key={session.id}
                session={session}
                status={statuses[session.id]}
                clock={clock}
                onSave={setSaveFor}
              />
            ))}
          </ul>
        )}
      </CardContent>
      <CastStartDialog open={startOpen} onOpenChange={setStartOpen} />
      {saveFor !== undefined ? (
        <SaveAsDeviceDialog
          key={saveFor.id}
          session={saveFor}
          onClose={(): void => {
            setSaveFor(undefined);
          }}
        />
      ) : null}
    </Card>
  );
}
