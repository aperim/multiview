// Value formatting via the ECMAScript Intl API (docs/web/internationalization.md).
//
// HARD RULE: the i18n library owns catalogs + lookup; `Intl` owns ALL value
// formatting (dates, numbers, relative time). Formatters are costly to build and
// are memoized per (locale, options). `dB`/`fps`/`Mbps` are NOT CLDR units —
// format the number and append a NON-translated literal.

type DateOptions = Intl.DateTimeFormatOptions;
type NumberOptions = Intl.NumberFormatOptions;

const dateCache = new Map<string, Intl.DateTimeFormat>();
const numberCache = new Map<string, Intl.NumberFormat>();
const relativeCache = new Map<string, Intl.RelativeTimeFormat>();

function dateFormatter(locale: string, options: DateOptions): Intl.DateTimeFormat {
  const key = `${locale}|${JSON.stringify(options)}`;
  const existing = dateCache.get(key);
  if (existing !== undefined) {
    return existing;
  }
  const created = new Intl.DateTimeFormat(locale, options);
  dateCache.set(key, created);
  return created;
}

function numberFormatter(locale: string, options: NumberOptions): Intl.NumberFormat {
  const key = `${locale}|${JSON.stringify(options)}`;
  const existing = numberCache.get(key);
  if (existing !== undefined) {
    return existing;
  }
  const created = new Intl.NumberFormat(locale, options);
  numberCache.set(key, created);
  return created;
}

function relativeFormatter(locale: string): Intl.RelativeTimeFormat {
  const existing = relativeCache.get(locale);
  if (existing !== undefined) {
    return existing;
  }
  const created = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  relativeCache.set(locale, created);
  return created;
}

/** Format a wall-clock time in a specific timezone for the active locale. */
export function formatTime(
  locale: string,
  date: Date,
  timeZone: string,
): string {
  return dateFormatter(locale, {
    timeZone,
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    timeZoneName: "short",
  }).format(date);
}

/** Format a date+time for the active locale (browser timezone). */
export function formatDateTime(locale: string, date: Date): string {
  return dateFormatter(locale, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

/** Format an integer/decimal count for the active locale. */
export function formatNumber(
  locale: string,
  value: number,
  options: NumberOptions = {},
): string {
  return numberFormatter(locale, options).format(value);
}

/** Format a decibel value: locale-formatted number + a literal, non-translated `dB`. */
export function formatDecibels(locale: string, value: number): string {
  const number = numberFormatter(locale, {
    style: "decimal",
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
    signDisplay: "exceptZero",
  }).format(value);
  return `${number} dB`;
}

/** Format a frame rate: locale-formatted number + a literal `fps`. */
export function formatFps(locale: string, value: number): string {
  const number = numberFormatter(locale, {
    style: "decimal",
    maximumFractionDigits: 2,
  }).format(value);
  return `${number} fps`;
}

/** Format a bitrate via the compact CLDR `*-per-second` unit. */
export function formatBitrate(locale: string, kilobitsPerSecond: number): string {
  return numberFormatter(locale, {
    style: "unit",
    unit: "kilobit-per-second",
    unitDisplay: "short",
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(kilobitsPerSecond);
}

/**
 * Format "N seconds ago" style strings. Picks the largest natural unit.
 */
export function formatRelativeTime(locale: string, deltaSeconds: number): string {
  const formatter = relativeFormatter(locale);
  const abs = Math.abs(deltaSeconds);
  if (abs < 60) {
    return formatter.format(Math.round(deltaSeconds), "second");
  }
  if (abs < 3600) {
    return formatter.format(Math.round(deltaSeconds / 60), "minute");
  }
  if (abs < 86400) {
    return formatter.format(Math.round(deltaSeconds / 3600), "hour");
  }
  return formatter.format(Math.round(deltaSeconds / 86400), "day");
}
