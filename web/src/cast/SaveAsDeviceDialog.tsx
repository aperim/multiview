// Save-as-device (DEV-D3 over DEV-D2's `POST /cast/sessions/{id}/save`):
// promotes an ephemeral session into a normal `Device{driver: cast}` registry
// entry — desired state that exports with the configuration and survives
// restarts. The promotion keeps the TV playing (the control plane retires the
// ephemeral actor without a receiver STOP and hands supervision to the
// device's actor), so this is the adopt flow's machinery applied to a running
// session, not a duplicate of it.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQueryClient } from '@tanstack/react-query';

import { operationErrorMessage, saveCastSession } from './api';
import type { CastSessionView } from './api';
import { CAST_SESSIONS_QUERY_KEY } from './queries';
import { resourceKeys } from '../resources/queries';
import type { FieldErrors } from '../resources/forms';
import { FormField } from '../resources/FormControls';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { toast } from '../components/ui/use-toast';

/** The save-form fields that can carry a validation error. */
type SaveField = 'deviceId';

const NO_ERRORS: FieldErrors<SaveField> = {};

/** Props for {@link SaveAsDeviceDialog}. */
export interface SaveAsDeviceDialogProps {
  /** The session to promote. */
  readonly session: CastSessionView;
  /** Called when the dialog closes (cancel or success). */
  readonly onClose: () => void;
}

/**
 * The promotion dialog. Mount it keyed by the session id so the form state
 * re-seeds per session.
 */
export function SaveAsDeviceDialog({ session, onClose }: SaveAsDeviceDialogProps): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const [deviceId, setDeviceId] = useState('');
  const [displayName, setDisplayName] = useState(session.name ?? '');
  const [errors, setErrors] = useState<FieldErrors<SaveField>>(NO_ERRORS);
  const [pending, setPending] = useState(false);

  const submit = (): void => {
    const id = deviceId.trim();
    if (id === '') {
      setErrors({ deviceId: 'required' });
      return;
    }
    setErrors(NO_ERRORS);
    setPending(true);
    const name = displayName.trim();
    saveCastSession(session.id, {
      device_id: id,
      ...(name === '' ? {} : { display_name: name }),
    })
      .then((record): void => {
        setPending(false);
        toast({
          title: t`Saved as device`,
          description: t`${record.name} is now a managed cast device: it exports with the configuration and its supervised driver keeps the session running.`,
        });
        // The registry gained a device and the ephemeral session was consumed.
        void queryClient.invalidateQueries({ queryKey: resourceKeys.list('devices') });
        void queryClient.invalidateQueries({ queryKey: CAST_SESSIONS_QUERY_KEY });
        onClose();
      })
      .catch((error: unknown): void => {
        setPending(false);
        toast({
          title: t`Could not save the device`,
          description: operationErrorMessage(error),
          variant: 'destructive',
        });
      });
  };

  return (
    <Dialog
      open
      onOpenChange={(next): void => {
        if (!next) {
          onClose();
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            <Trans>Save as device</Trans>
          </DialogTitle>
          <DialogDescription>
            <Trans>
              Promote this session to a managed cast device: it exports with
              the configuration and survives restarts. The TV keeps playing
              across the promotion.
            </Trans>
          </DialogDescription>
        </DialogHeader>
        <form
          className="flex flex-col gap-4"
          noValidate
          onSubmit={(event): void => {
            event.preventDefault();
            submit();
          }}
        >
          <FormField
            id="cast-save-id"
            label={t`Device identifier`}
            value={deviceId}
            required
            placeholder={t`e.g. dev-lounge-tv`}
            error={errors.deviceId}
            hint={
              <Trans>
                A stable id for the registry — referenced by sync groups and
                config export.
              </Trans>
            }
            onChange={setDeviceId}
          />
          <FormField
            id="cast-save-name"
            label={t`Name`}
            value={displayName}
            placeholder={session.address}
            onChange={setDisplayName}
          />
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>
              <Trans>Cancel</Trans>
            </Button>
            <Button type="submit" disabled={pending}>
              <Trans>Save device</Trans>
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
