// The live system-metrics footer: a sticky, desktop-only status bar that shows
// the host/GPU utilisation streaming over the `system` realtime topic. Each cell
// pairs a tiny label, a {@link Sparkline} of the recent series, and the current
// numeric value. The whole bar is a button that navigates to the System detail
// page. Status is conveyed by VALUE + SHAPE (a filled vs hollow live dot, plus
// text), never colour alone (WCAG 1.4.1). All visible words are localised.
import type { JSX } from "react";
import { Trans, useLingui } from "@lingui/react/macro";
import { useNavigate } from "react-router-dom";

import { useActiveLocale } from "../i18n/I18nProvider";
import { formatBytes, formatFps, formatPercent } from "../i18n/format";
import { useSystemMetrics } from "../realtime/useSystemMetrics";
import type { SystemMetricsSeries } from "../realtime/useSystemMetrics";
import type { RealtimeStatus } from "../realtime/connection";
import { Sparkline } from "./Sparkline";
import { cn } from "../lib/utils";

/** A live indicator whose shape (filled vs hollow) carries meaning, not colour. */
function LiveDot({ status }: { readonly status: RealtimeStatus }): JSX.Element {
  const { t } = useLingui();
  const open = status === "open";
  const label = open ? t`Live` : t`Offline`;
  return (
    <span className="flex items-center gap-1" role="status" aria-live="polite">
      <span
        aria-hidden="true"
        className={cn(
          "size-2 rounded-full border",
          open
            ? "border-status-live bg-status-live"
            : "border-muted-foreground bg-transparent",
        )}
      />
      <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
    </span>
  );
}

/** One footer cell: a label, a sparkline of the series, and the current value. */
interface CellProps {
  readonly label: JSX.Element;
  readonly series: number[];
  readonly value: string;
  /** The sparkline normalisation ceiling (default 1, i.e. a 0..1 ratio). */
  readonly max?: number;
  /** Accessible name for the sparkline graphic. */
  readonly ariaLabel: string;
}

function Cell({ label, series, value, max = 1, ariaLabel }: CellProps): JSX.Element {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <div className="flex flex-col leading-tight">
        <span className="text-[10px] uppercase tracking-wide text-muted-foreground">
          {label}
        </span>
        <span className="text-xs font-semibold tabular-nums">{value}</span>
      </div>
      <Sparkline
        values={series}
        max={max}
        ariaLabel={ariaLabel}
        width={48}
        height={18}
        className="text-foreground/70"
      />
    </div>
  );
}

/** The peak of a series (for an auto-scaled sparkline), or `1` when flat/empty. */
function seriesMax(series: number[]): number {
  const peak = series.reduce((acc, v) => Math.max(acc, v), 0);
  return peak > 0 ? peak : 1;
}

/**
 * The sticky system-metrics footer. Desktop-only (`hidden md:flex`); on smaller
 * viewports the System page carries the same data. Rendering is best-effort: an
 * empty state shows dashes until the first sample arrives, and the bar never
 * blocks (engine isolation, inv #10).
 */
export function SystemFooter(): JSX.Element {
  const { t } = useLingui();
  const locale = useActiveLocale();
  const navigate = useNavigate();
  const { status, current, series } = useSystemMetrics();

  const gpu0 = current?.gpus[0];
  const cpuValue =
    current === undefined ? "—" : formatPercent(locale, current.cpu_util);
  const gpuValue =
    gpu0 === undefined ? "—" : formatPercent(locale, gpu0.compute_util);
  const vramValue =
    gpu0 === undefined
      ? "—"
      : `${formatBytes(locale, gpu0.mem_used_bytes)} / ${formatBytes(locale, gpu0.mem_total_bytes)}`;
  const nvencValue =
    gpu0?.encoder_sessions === undefined
      ? "—"
      : `${String(gpu0.encoder_sessions)} / ${gpu0.encoder_session_ceiling === undefined ? "∞" : String(gpu0.encoder_session_ceiling)}`;
  const decValue =
    gpu0?.decoder_util === undefined
      ? "—"
      : formatPercent(locale, gpu0.decoder_util);
  const fpsValue =
    current?.program_fps === undefined
      ? "—"
      : formatFps(locale, current.program_fps);

  const nvencSeries: SystemMetricsSeries["nvenc"] = series.nvenc;

  return (
    <footer className="sticky bottom-0 z-30 hidden border-t bg-card md:block">
      <button
        type="button"
        onClick={(): void => {
          void navigate("/system");
        }}
        aria-label={t`Open the system metrics page`}
        className="flex w-full items-center gap-5 overflow-x-auto px-4 py-1.5 text-start hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <LiveDot status={status} />
        <Cell
          label={<Trans>CPU</Trans>}
          series={series.cpu}
          value={cpuValue}
          ariaLabel={t`CPU utilisation trend`}
        />
        <Cell
          label={<Trans>GPU0</Trans>}
          series={series.gpu0Util}
          value={gpuValue}
          ariaLabel={t`GPU 0 compute utilisation trend`}
        />
        <Cell
          label={<Trans>VRAM</Trans>}
          series={series.vram}
          value={vramValue}
          ariaLabel={t`GPU 0 VRAM usage trend`}
        />
        <Cell
          label={<Trans>NVENC</Trans>}
          series={nvencSeries}
          value={nvencValue}
          max={seriesMax(nvencSeries)}
          ariaLabel={t`NVENC encode sessions trend`}
        />
        <Cell
          label={<Trans>DEC</Trans>}
          series={series.dec}
          value={decValue}
          ariaLabel={t`GPU 0 decoder utilisation trend`}
        />
        <Cell
          label={<Trans>PROG</Trans>}
          series={series.fps}
          value={fpsValue}
          max={seriesMax(series.fps)}
          ariaLabel={t`Program output frame rate trend`}
        />
      </button>
    </footer>
  );
}
