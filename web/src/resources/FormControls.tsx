// Shared, accessible form controls for the resource management dialogs.
//
// Every field renders a real <label>, and a field error renders inline in a
// dedicated element wired to the control via `aria-describedby` (+
// `aria-invalid`), so screen readers announce the problem at the field — never
// a single far-away alert. Error codes come from the pure validators in
// `./forms`; the localized message lives here (Lingui stays in components).
import { useId } from 'react';
import type { JSX, ReactNode } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { ChevronRight, Download, Info } from 'lucide-react';

import type { FormErrorCode } from './forms';
import {
  ConfigExportUnsupportedError,
  downloadConfigExport,
  fetchConfigExport,
} from './exportConfig';
import { HelpLink } from '../components/HelpLink';
import { Button } from '../components/ui/button';
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

/** The localized message for a validation code. */
export function FieldErrorMessage({ code }: { readonly code: FormErrorCode }): JSX.Element {
  switch (code) {
    case 'required':
      return <Trans>This field is required.</Trans>;
    case 'url-invalid':
      return (
        <Trans>
          Enter a full URL with a host. Wrap an IPv6 literal in brackets, e.g.
          rtsp://[2001:db8::1]:8554/cam.
        </Trans>
      );
    case 'scheme-rtsp':
      return <Trans>An RTSP source URL must start with rtsp:// (or rtsps://).</Trans>;
    case 'scheme-srt':
      return <Trans>An SRT URL must start with srt://.</Trans>;
    case 'scheme-rtmp':
      return <Trans>An RTMP URL must start with rtmp:// (or rtmps://).</Trans>;
    case 'scheme-http':
      return <Trans>This URL must start with http:// or https://.</Trans>;
    case 'hex-color':
      return <Trans>Enter a hex colour like #101014 or #abc.</Trans>;
    case 'int':
      return <Trans>Enter a whole number.</Trans>;
    case 'int-range':
      return <Trans>Enter a whole number within the allowed range.</Trans>;
    case 'positive-int':
      return <Trans>Enter a whole number greater than zero.</Trans>;
    case 'mount-slash':
      return <Trans>A mount point must start with /, e.g. /multiview.</Trans>;
    case 'tracks-required':
      return <Trans>List at least one track name (comma-separated).</Trans>;
  }
}

/** A labelled text/number input with an inline, SR-wired error. */
export function FormField({
  id,
  label,
  value,
  onChange,
  error,
  disabled,
  required,
  placeholder,
  type,
  hint,
  trailing,
}: {
  readonly id: string;
  readonly label: string;
  readonly value: string;
  readonly onChange: (next: string) => void;
  /** The active validation code for this field, if any. */
  readonly error?: FormErrorCode | undefined;
  readonly disabled?: boolean;
  readonly required?: boolean;
  readonly placeholder?: string;
  readonly type?: string;
  /** Optional supporting hint rendered under the control. */
  readonly hint?: ReactNode;
  /** Optional trailing affordance next to the label (e.g. a HelpLink). */
  readonly trailing?: ReactNode;
}): JSX.Element {
  const errorId = `${id}-error`;
  const hintId = `${id}-hint`;
  const describedBy =
    [error !== undefined ? errorId : undefined, hint !== undefined ? hintId : undefined]
      .filter((part) => part !== undefined)
      .join(' ') || undefined;
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center gap-1.5">
        <Label htmlFor={id}>{label}</Label>
        {trailing}
      </div>
      <Input
        id={id}
        type={type ?? 'text'}
        value={value}
        required={required ?? false}
        disabled={disabled ?? false}
        aria-invalid={error !== undefined}
        {...(describedBy !== undefined ? { 'aria-describedby': describedBy } : {})}
        {...(placeholder !== undefined ? { placeholder } : {})}
        onChange={(event): void => {
          onChange(event.target.value);
        }}
      />
      {hint !== undefined ? (
        <p id={hintId} className="text-xs text-muted-foreground">
          {hint}
        </p>
      ) : null}
      {error !== undefined ? (
        <p id={errorId} className="text-sm text-destructive">
          <FieldErrorMessage code={error} />
        </p>
      ) : null}
    </div>
  );
}

/** A labelled <Select> over fixed string options, with optional display text. */
export function SelectField<Option extends string>({
  label,
  value,
  options,
  onChange,
  optionLabel,
  trailing,
}: {
  readonly label: string;
  readonly value: Option;
  readonly options: readonly Option[];
  readonly onChange: (next: Option) => void;
  /** Optional display text per option (defaults to the option itself). */
  readonly optionLabel?: (option: Option) => ReactNode;
  /** Optional trailing affordance next to the label (e.g. a HelpLink). */
  readonly trailing?: ReactNode;
}): JSX.Element {
  const labelId = useId();
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center gap-1.5">
        <Label id={labelId}>{label}</Label>
        {trailing}
      </div>
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
              {optionLabel !== undefined ? optionLabel(option) : option}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}

/** A labelled checkbox row. */
export function CheckboxField({
  id,
  label,
  checked,
  onChange,
}: {
  readonly id: string;
  readonly label: string;
  readonly checked: boolean;
  readonly onChange: (next: boolean) => void;
}): JSX.Element {
  return (
    <div className="flex items-center gap-2">
      <Input
        id={id}
        type="checkbox"
        className="h-4 w-4"
        checked={checked}
        onChange={(event): void => {
          onChange(event.target.checked);
        }}
      />
      <Label htmlFor={id}>{label}</Label>
    </div>
  );
}

/**
 * A keyboard-native collapsible "Advanced" disclosure (a real
 * <details>/<summary>, so it needs no extra ARIA wiring).
 */
export function AdvancedSection({
  summary,
  children,
}: {
  readonly summary: string;
  readonly children: ReactNode;
}): JSX.Element {
  return (
    <details className="group rounded-md border p-3" data-testid="advanced-section">
      <summary
        className="flex cursor-pointer list-none items-center gap-1 text-sm font-medium [&::-webkit-details-marker]:hidden"
        data-testid="advanced-toggle"
      >
        <ChevronRight
          className="size-4 transition-transform group-open:rotate-90"
          aria-hidden="true"
        />
        {summary}
      </summary>
      <div className="mt-3 flex flex-col gap-4">{children}</div>
    </details>
  );
}

/**
 * The honest apply-semantics callout (ADR-W015 §4, ADR-W018): each page states
 * exactly how its saved edits reach the running engine. The default copy is
 * the stored-resource text (true for outputs); the sources page overrides it
 * with the per-kind live/restart truth. Text + icon, never colour alone.
 */
export function ApplySemanticsCallout({
  helpTo,
  helpLabel,
  message,
}: {
  readonly helpTo: string;
  readonly helpLabel: string;
  /** Page-specific apply-semantics copy (defaults to the stored-resource text). */
  readonly message?: ReactNode;
}): JSX.Element {
  return (
    <div
      role="note"
      className="mb-4 flex items-start gap-2 rounded-md border bg-muted/40 p-3 text-sm"
    >
      <Info className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
      <p>
        {message ?? (
          <Trans>
            Changes saved here are stored, not yet live: the engine does not
            hot-add outputs. Apply them by exporting the configuration and
            restarting Multiview. Layout apply, source swap, routing, and
            salvos act on the running engine immediately.
          </Trans>
        )}{' '}
        <HelpLink to={helpTo} label={helpLabel} />
      </p>
    </div>
  );
}

/**
 * "Export configuration" — downloads `GET /api/v1/config/export` as
 * multiview.toml. Degrades to an explanatory toast when the backend does not
 * serve the route yet (404/501).
 */
export function ExportConfigButton({
  compact = false,
}: {
  /** Outline/secondary styling for page headers. */
  readonly compact?: boolean;
}): JSX.Element {
  const { t } = useLingui();
  const exportNow = (): void => {
    fetchConfigExport()
      .then((result): void => {
        downloadConfigExport(result);
        toast({
          title: t`Configuration exported`,
          description: t`Saved as ${result.filename}. Restart Multiview with this file to apply stored changes.`,
        });
      })
      .catch((error: unknown): void => {
        if (error instanceof ConfigExportUnsupportedError) {
          toast({
            title: t`Export not available`,
            description: t`This control plane does not serve config export yet. Update the server to use it.`,
            variant: 'destructive',
          });
          return;
        }
        toast({
          title: t`Could not export configuration`,
          description: error instanceof Error ? error.message : String(error),
          variant: 'destructive',
        });
      });
  };
  return (
    <Button variant={compact ? 'outline' : 'default'} onClick={exportNow}>
      <Download aria-hidden="true" />
      <Trans>Export configuration</Trans>
    </Button>
  );
}
