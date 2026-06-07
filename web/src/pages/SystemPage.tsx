// System — the live host/GPU telemetry detail page. Streams the same `system`
// realtime topic the footer uses ({@link useSystemMetrics}) and renders the host
// summary plus a card per GPU (vendor, name, compute util, VRAM, encoder/decoder
// util, NVENC sessions/ceiling) with larger sparklines. All reads are
// best-effort and never block the engine (invariant #10). Status is conveyed by
// VALUE + a labelled meter, never colour alone (WCAG 1.4.1).
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";

import { PageHeader } from "../components/PageHeader";
import { Sparkline } from "../components/Sparkline";
import { ConnectionStatus } from "../components/ConnectionStatus";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../components/ui/card";
import { useActiveLocale } from "../i18n/I18nProvider";
import { formatBytes, formatFps, formatPercent } from "../i18n/format";
import { useSystemMetrics } from "../realtime/useSystemMetrics";
import type { GpuMetrics, GpuVendor } from "../realtime/useSystemMetrics";

/** A labelled horizontal meter. The numeric percent text carries the value;
 *  the bar is a redundant visual cue, so meaning is never colour-only. */
function Meter(props: {
  readonly label: JSX.Element;
  readonly ratio: number;
  readonly valueText: string;
}): JSX.Element {
  const pct = Math.min(100, Math.max(0, Math.round(props.ratio * 100)));
  return (
    <div>
      <div className="mb-1 flex items-center justify-between gap-2 text-sm">
        <span className="text-muted-foreground">{props.label}</span>
        <span className="font-medium tabular-nums">{props.valueText}</span>
      </div>
      <div
        role="meter"
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        className="h-2 w-full overflow-hidden rounded-full bg-muted"
      >
        <div
          className="h-full rounded-full bg-foreground/60"
          style={{ width: `${String(pct)}%` }}
        />
      </div>
    </div>
  );
}

/** Map the vendor enum to a localised display name (vendor is not colour-coded). */
function vendorLabel(vendor: GpuVendor): JSX.Element {
  switch (vendor) {
    case "nvidia":
      return <Trans>NVIDIA</Trans>;
    case "intel":
      return <Trans>Intel</Trans>;
    case "amd":
      return <Trans>AMD</Trans>;
    case "apple":
      return <Trans>Apple</Trans>;
    case "other":
      return <Trans>Other</Trans>;
  }
}

/** One GPU's detail card. */
function GpuCard(props: { readonly gpu: GpuMetrics }): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const { gpu } = props;
  const vramRatio = gpu.mem_total_bytes > 0 ? gpu.mem_used_bytes / gpu.mem_total_bytes : 0;

  return (
    <Card>
      <CardHeader className="pb-3">
        <CardTitle className="truncate text-base" lang="" dir="auto">
          {gpu.name ?? gpu.id}
        </CardTitle>
        <CardDescription>
          {vendorLabel(gpu.vendor)}
          <span aria-hidden="true"> · </span>
          <code className="text-xs" lang="" dir="auto">
            {gpu.id}
          </code>
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <Meter
          label={<Trans>Compute</Trans>}
          ratio={gpu.compute_util}
          valueText={formatPercent(locale, gpu.compute_util)}
        />
        <Meter
          label={<Trans>VRAM</Trans>}
          ratio={vramRatio}
          valueText={`${formatBytes(locale, gpu.mem_used_bytes)} / ${formatBytes(locale, gpu.mem_total_bytes)}`}
        />
        {gpu.encoder_util !== undefined ? (
          <Meter
            label={<Trans>Encoder</Trans>}
            ratio={gpu.encoder_util}
            valueText={formatPercent(locale, gpu.encoder_util)}
          />
        ) : null}
        {gpu.decoder_util !== undefined ? (
          <Meter
            label={<Trans>Decoder</Trans>}
            ratio={gpu.decoder_util}
            valueText={formatPercent(locale, gpu.decoder_util)}
          />
        ) : null}
        {gpu.encoder_sessions !== undefined ? (
          <div className="flex items-center justify-between gap-2 text-sm">
            <span className="text-muted-foreground">
              <Trans>NVENC sessions</Trans>
            </span>
            <span
              className="font-medium tabular-nums"
              aria-label={t`Active NVENC sessions out of the discovered ceiling`}
            >
              {String(gpu.encoder_sessions)}
              {" / "}
              {gpu.encoder_session_ceiling === undefined
                ? "∞"
                : String(gpu.encoder_session_ceiling)}
            </span>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** The System telemetry page. */
export function SystemPage(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const { status, current, series } = useSystemMetrics();

  return (
    <>
      <PageHeader
        title={<Trans>System</Trans>}
        description={
          <Trans>
            Live host and accelerator utilisation streaming from the engine.
          </Trans>
        }
        actions={<ConnectionStatus status={status} />}
      />

      <section aria-labelledby="host-heading">
        <h2 id="host-heading" className="mb-3 text-lg font-semibold">
          <Trans>Host</Trans>
        </h2>
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>CPU</Trans>
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-2">
              <p className="text-3xl font-semibold tabular-nums">
                {current === undefined ? "—" : formatPercent(locale, current.cpu_util)}
              </p>
              <Sparkline
                values={series.cpu}
                ariaLabel={t`CPU utilisation trend`}
                width={220}
                height={40}
                className="w-full text-foreground/70"
              />
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>Memory</Trans>
              </CardTitle>
            </CardHeader>
            <CardContent>
              {current?.mem_used_bytes !== undefined &&
              current.mem_total_bytes !== undefined ? (
                <Meter
                  label={<Trans>Used</Trans>}
                  ratio={
                    current.mem_total_bytes > 0
                      ? current.mem_used_bytes / current.mem_total_bytes
                      : 0
                  }
                  valueText={`${formatBytes(locale, current.mem_used_bytes)} / ${formatBytes(locale, current.mem_total_bytes)}`}
                />
              ) : (
                <p className="text-sm text-muted-foreground">
                  <Trans>No host-memory sample.</Trans>
                </p>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-base">
                <Trans>Program output</Trans>
              </CardTitle>
            </CardHeader>
            <CardContent className="space-y-2">
              <p className="text-3xl font-semibold tabular-nums">
                {current?.program_fps === undefined
                  ? "—"
                  : formatFps(locale, current.program_fps)}
              </p>
              <Sparkline
                values={series.fps}
                max={Math.max(1, ...series.fps)}
                ariaLabel={t`Program output frame rate trend`}
                width={220}
                height={40}
                className="w-full text-foreground/70"
              />
            </CardContent>
          </Card>
        </div>
      </section>

      <section aria-labelledby="gpus-heading" className="mt-8">
        <h2 id="gpus-heading" className="mb-3 text-lg font-semibold">
          <Trans>GPUs</Trans>
        </h2>
        {current === undefined ? (
          <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
            <Trans>Waiting for the first metrics sample…</Trans>
          </p>
        ) : current.gpus.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            <Trans>No GPUs are reported on this host.</Trans>
          </p>
        ) : (
          <ul className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {current.gpus.map((gpu) => (
              <li key={gpu.id}>
                <GpuCard gpu={gpu} />
              </li>
            ))}
          </ul>
        )}
      </section>
    </>
  );
}
