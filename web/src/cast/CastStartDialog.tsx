// The ad-hoc cast sheet (DEV-D3, ADR-M011): pick a cast target (adopted cast
// devices + the untrusted discovery inventory — choices only PREFILL), pick a
// served HLS rendition, cast. The address field is always editable: that is
// the manual `host[:port]` escape hatch for devices mDNS cannot see across
// VLANs, IPv6 bracketed first (ADR-0042). Starting POSTs
// /api/v1/cast/sessions (201); a 409/422 surfaces its RFC 9457 detail in a
// destructive toast, field problems inline at the field.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useQueryClient } from '@tanstack/react-query';
import { Link } from 'react-router-dom';

import { operationErrorMessage, startCastSession } from './api';
import {
  castStartFormToRequest,
  castTargetChoices,
  emptyCastStartForm,
  validateCastStartForm,
} from './forms';
import type { CastStartField, CastStartFormState } from './forms';
import { CAST_SESSIONS_QUERY_KEY } from './queries';
import { useDevices, useDiscoveredInventory } from '../devices/queries';
import { useOutputs } from '../resources/queries';
import type { FieldErrors } from '../resources/forms';
import { FormField, SelectField } from '../resources/FormControls';
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

/** The target-selector key for the manual-address escape hatch. */
const MANUAL_TARGET = 'manual';

const NO_ERRORS: FieldErrors<CastStartField> = {};

/** Props for {@link CastStartDialog}. */
export interface CastStartDialogProps {
  /** Whether the sheet is open. */
  readonly open: boolean;
  /** Open/close intent from the dialog chrome. */
  readonly onOpenChange: (open: boolean) => void;
}

/** The "cast to a device" sheet. */
export function CastStartDialog({ open, onOpenChange }: CastStartDialogProps): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const devices = useDevices();
  const discovered = useDiscoveredInventory();
  const outputs = useOutputs();
  const [form, setForm] = useState<CastStartFormState>(emptyCastStartForm);
  const [target, setTarget] = useState<string>(MANUAL_TARGET);
  const [errors, setErrors] = useState<FieldErrors<CastStartField>>(NO_ERRORS);
  const [pending, setPending] = useState(false);

  const choices = castTargetChoices(devices.data ?? [], discovered.data ?? []);
  // Every HLS/LL-HLS output is a castable rendition of the program canvas
  // (the delivery map the control plane builds, ADR-M011).
  const renditions = (outputs.data ?? []).filter(
    (output) => output.kind === 'hls' || output.kind === 'll-hls',
  );
  // The first served rendition is preselected; the body always names the
  // rendition explicitly so what the operator saw is what is cast.
  const effectiveOutput =
    form.output !== '' ? form.output : (renditions.at(0)?.id ?? '');

  const reset = (): void => {
    setForm(emptyCastStartForm());
    setTarget(MANUAL_TARGET);
    setErrors(NO_ERRORS);
  };

  const close = (): void => {
    reset();
    onOpenChange(false);
  };

  const submit = (): void => {
    const candidate: CastStartFormState = { ...form, output: effectiveOutput };
    const validation = validateCastStartForm(candidate);
    setErrors(validation);
    if (Object.keys(validation).length > 0) {
      return;
    }
    setPending(true);
    startCastSession(castStartFormToRequest(candidate))
      .then((session): void => {
        setPending(false);
        toast({
          title: t`Casting started`,
          description: t`The device is loading ${session.output}. Playback runs seconds behind live (Tier D).`,
        });
        void queryClient.invalidateQueries({ queryKey: CAST_SESSIONS_QUERY_KEY });
        close();
      })
      .catch((error: unknown): void => {
        setPending(false);
        toast({
          title: t`Could not start casting`,
          description: operationErrorMessage(error),
          variant: 'destructive',
        });
      });
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next): void => {
        if (!next) {
          close();
        }
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>
            <Trans>Cast to a device</Trans>
          </DialogTitle>
          <DialogDescription>
            <Trans>
              Multiview dials the device and plays an HLS rendition the engine
              already serves on its Default Media Receiver. The session is
              ephemeral until you save it as a device.
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
          <SelectField<string>
            label={t`Cast target`}
            value={target}
            options={[MANUAL_TARGET, ...choices.map((choice) => choice.key)]}
            optionLabel={(option): string =>
              option === MANUAL_TARGET
                ? t`Manual address…`
                : (choices.find((choice) => choice.key === option)?.label ?? option)
            }
            onChange={(next): void => {
              setTarget(next);
              const choice = choices.find((candidate) => candidate.key === next);
              if (choice !== undefined) {
                // A choice only PREFILLS the sheet; the address field below
                // stays the source of truth.
                setForm({ ...form, address: choice.address, name: choice.name });
              }
            }}
          />
          <FormField
            id="cast-address"
            label={t`Device address`}
            value={form.address}
            required
            placeholder="[2001:db8::20]:8009"
            error={errors.address}
            hint={
              <Trans>
                host[:port] — the port defaults to 8009; IPv6 literals go in
                brackets. Enter the address directly when mDNS discovery
                cannot see the device across VLANs.
              </Trans>
            }
            onChange={(next): void => {
              setForm({ ...form, address: next });
            }}
          />
          <FormField
            id="cast-name"
            label={t`Session name (optional)`}
            value={form.name}
            placeholder={t`e.g. Lounge TV`}
            onChange={(next): void => {
              setForm({ ...form, name: next });
            }}
          />
          {renditions.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              <Trans>
                No HLS rendition is served — declare an HLS or LL-HLS output
                first, then cast it.
              </Trans>{' '}
              <Link to="/outputs" className="underline underline-offset-2">
                <Trans>Open outputs</Trans>
              </Link>
            </p>
          ) : (
            <SelectField<string>
              label={t`Rendition`}
              value={effectiveOutput}
              options={renditions.map((rendition) => rendition.id)}
              optionLabel={(option): string => {
                const rendition = renditions.find((candidate) => candidate.id === option);
                return rendition === undefined ? option : `${rendition.name} (${rendition.id})`;
              }}
              error={errors.output}
              onChange={(next): void => {
                setForm({ ...form, output: next });
              }}
            />
          )}
          <DialogFooter>
            <Button type="button" variant="outline" onClick={close}>
              <Trans>Cancel</Trans>
            </Button>
            <Button type="submit" disabled={pending || renditions.length === 0}>
              <Trans>Cast</Trans>
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
