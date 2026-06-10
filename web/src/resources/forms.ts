// Pure form-state <-> config-body mapping for the Sources / Outputs / Overlays
// management forms.
//
// This module is framework-free (no React, no Lingui) so the mapping and the
// validation can be unit-tested in isolation. The body shapes produced here
// mirror the Rust config schema EXACTLY:
//   * `Source` / `SourceKind` / `RtspOptions` / `SourceAuth` / `ColorOverride`
//     / `CaptionSelector` / `SourceWallClock`
//     (crates/multiview-config/src/schema.rs),
//   * `Output` and its per-kind fields (schema.rs), `OutputAudio` (audio.rs),
//   * `DevicePin` (placement.rs),
//   * `Overlay` (schema.rs) with the kind params the Rust side actually
//     consumes/documents: the clock face/tz/placement read by
//     multiview-cli's `analog_clock_from_config`, and the tally_border
//     width_px/color/binding from the shipped examples + template reference.
// The control plane validates POST/PUT bodies against those types with 422
// (ADR-W015), so a wrong body is a rejected body. DO NOT add fields that do
// not exist in schema.rs.
//
// Validation returns machine codes per field; the pages translate codes into
// localized messages (Lingui stays in the components).

import type { OutputKind, OverlayKind, ProbeKind, ResourceRecord, SourceKind } from './types';

// --- shared helpers ----------------------------------------------------------

/** The GPU vendor families a `DevicePin` names (placement.rs `PinVendor`). */
export type PinVendor = 'nvidia' | 'intel' | 'amd' | 'apple';

/** All pin vendors, for building selectors. */
export const PIN_VENDORS: readonly PinVendor[] = ['nvidia', 'intel', 'amd', 'apple'];

/** `#RGB` or `#RRGGBB` hex colour (schema `Solid.color` / border colour). */
const HEX_COLOR_RE = /^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/;

/** A whole-number string (optionally signed), with no float/exponent forms. */
const INT_RE = /^-?\d+$/;

/** Minimum/maximum clock timezone offset in minutes: −12:00 … +14:00. */
export const CLOCK_TZ_MIN_MINUTES = -720;
export const CLOCK_TZ_MAX_MINUTES = 840;

/** The machine validation codes a field can fail with. */
export type FormErrorCode =
  | 'required'
  | 'url-invalid'
  | 'scheme-rtsp'
  | 'scheme-srt'
  | 'scheme-rtmp'
  | 'scheme-http'
  | 'hex-color'
  | 'int'
  | 'int-range'
  | 'positive-int'
  | 'number'
  | 'zone-extent'
  | 'mount-slash'
  | 'tracks-required';

/** Per-field validation errors keyed by form-state field name. */
export type FieldErrors<Field extends string> = Partial<Record<Field, FormErrorCode>>;

/** Parse a strict integer string, or `undefined` when not a whole number. */
function parseIntStrict(value: string): number | undefined {
  const trimmed = value.trim();
  if (!INT_RE.test(trimmed)) {
    return undefined;
  }
  const parsed = Number.parseInt(trimmed, 10);
  return Number.isSafeInteger(parsed) ? parsed : undefined;
}

/** A plain decimal number string (optionally signed; no exponent forms). */
const NUMBER_RE = /^-?\d+(\.\d+)?$/;

/** Parse a strict finite decimal string, or `undefined` when not a number. */
function parseNumberStrict(value: string): number | undefined {
  const trimmed = value.trim();
  if (!NUMBER_RE.test(trimmed)) {
    return undefined;
  }
  const parsed = Number.parseFloat(trimmed);
  return Number.isFinite(parsed) ? parsed : undefined;
}

/**
 * Whether `value` is a well-formed URL whose scheme is in `schemes` (any
 * parseable scheme when `schemes` is `undefined`) and that carries a host.
 * Bracketed IPv6 literal hosts (`rtsp://[2001:db8::1]:8554/…`) parse fine; an
 * UNbracketed IPv6 literal fails (its colons read as an invalid port), which is
 * exactly the rejection we want (conventions §10 — bracket IPv6 URL literals).
 */
export function isValidUrl(value: string, schemes?: readonly string[]): boolean {
  return urlErrorCode(value, schemes, 'url-invalid') === undefined;
}

/**
 * Validate a URL field: `required` when blank, `url-invalid` when unparseable
 * or host-less, `schemeCode` when parseable but on the wrong scheme.
 */
function urlErrorCode(
  value: string,
  schemes: readonly string[] | undefined,
  schemeCode: FormErrorCode,
): FormErrorCode | undefined {
  const trimmed = value.trim();
  if (trimmed === '') {
    return 'required';
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return 'url-invalid';
  }
  const scheme = parsed.protocol.endsWith(':')
    ? parsed.protocol.slice(0, -1).toLowerCase()
    : parsed.protocol.toLowerCase();
  if (schemes !== undefined && !schemes.includes(scheme)) {
    return schemeCode;
  }
  if (parsed.host === '') {
    return 'url-invalid';
  }
  return undefined;
}

/** Type guard: a non-null, non-array object (a plain record). */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Narrow an unknown to a plain record without an unsafe assertion. */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === 'string' ? value : undefined;
}

function asFiniteNumber(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** A finite number rendered back to its form-input string ('' when absent). */
function numberToField(value: number | undefined): string {
  return value === undefined ? '' : String(value);
}

function asPinVendor(value: unknown): PinVendor {
  return PIN_VENDORS.find((vendor) => vendor === value) ?? 'nvidia';
}

/**
 * The body keys NOT managed by a form, preserved verbatim across an edit.
 *
 * Keys land via `Object.defineProperty`, never plain assignment: a stored body
 * can carry an OWN `__proto__` key (JSON allows it), and `extra[key] = value`
 * for that key would swap the accumulator's prototype and silently drop the
 * key. `defineProperty` always creates an own data property, and the later
 * body spreads (`{ ...form.extra }`) copy it back as an own data property too
 * (spread uses CreateDataProperty, never the inherited setter).
 */
function extraOf(
  body: Record<string, unknown>,
  managedKeys: readonly string[],
): Readonly<Record<string, unknown>> {
  const extra: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(body)) {
    if (!managedKeys.includes(key)) {
      Object.defineProperty(extra, key, {
        value,
        enumerable: true,
        configurable: true,
        writable: true,
      });
    }
  }
  return extra;
}

// --- Sources ------------------------------------------------------------------

/** The user-pickable source kinds (no legacy `test` alias). */
export type SourceFormKind = Exclude<SourceKind, 'test'>;

/** The clock-face styles (schema `ClockFaceConfig`, snake_case). */
export type ClockFace = 'analog' | 'digital';

/** Both clock faces, for building selectors. */
export const CLOCK_FACES: readonly ClockFace[] = ['analog', 'digital'];

/** RTSP lower-transport options (schema `RtspOptions.transport`); '' = default. */
export type RtspTransport = '' | 'tcp' | 'udp';

/** The caption-selector modes (schema `CaptionSelector`), plus 'none' = omit. */
export type CaptionsMode =
  | 'none'
  | 'auto'
  | 'off'
  | 'teletext_page'
  | 'track'
  | 'embedded_cc'
  | 'sidecar';

/** All caption modes in display order. */
export const CAPTION_MODES: readonly CaptionsMode[] = [
  'none',
  'auto',
  'off',
  'teletext_page',
  'track',
  'embedded_cc',
  'sidecar',
];

/** The wall-clock verb (schema `WallClockUse`), plus 'default' = omit. */
export type WallClockChoice = 'default' | 'use' | 'discard';

/** The editable state behind the source form (numbers kept as input strings). */
export interface SourceFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: SourceFormKind;
  /** `url` for rtsp/hls/youtube/ts/srt/rtmp. */
  readonly url: string;
  /** NDI source name (`ndi`). */
  readonly ndiName: string;
  /** Filesystem path (`file`). */
  readonly path: string;
  /** Fill colour (`solid`). */
  readonly color: string;
  /** Clock face (`clock`). */
  readonly clockFace: ClockFace;
  /** 12-hour mode (`clock`). */
  readonly clockTwelveHour: boolean;
  /** Timezone offset in minutes (`clock`). */
  readonly clockTzMinutes: string;
  /** RTSP lower transport ('' = engine default, omitted from the body). */
  readonly rtspTransport: RtspTransport;
  /** Caption selector mode ('none' = no `captions` block). */
  readonly captionsMode: CaptionsMode;
  /** Teletext page (mode `teletext_page`). */
  readonly captionsPage: string;
  /** Track id / language tag (mode `track`). */
  readonly captionsTrack: string;
  /** CEA-608/708 field/service (mode `embedded_cc`). */
  readonly captionsField: string;
  /** Sidecar file path (mode `sidecar`). */
  readonly captionsPath: string;
  /** Whether a `color_override` block is written. */
  readonly colorOverrideEnabled: boolean;
  readonly colorPrimaries: string;
  readonly colorTransfer: string;
  readonly colorMatrix: string;
  readonly colorRange: string;
  /** Whether a `gpu_pin` block is written. */
  readonly gpuPinEnabled: boolean;
  readonly gpuPinVendor: PinVendor;
  readonly gpuPinStableId: string;
  /** Wall-clock verb ('default' = no `wallclock` block). */
  readonly wallclock: WallClockChoice;
  /** `auth.secret_ref` ('' = no `auth` block; never a plaintext secret). */
  readonly authSecretRef: string;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The source-form fields that can carry a validation error. */
export type SourceField =
  | 'id'
  | 'name'
  | 'url'
  | 'ndiName'
  | 'path'
  | 'color'
  | 'clockTzMinutes'
  | 'captionsPage'
  | 'captionsTrack'
  | 'captionsField'
  | 'captionsPath'
  | 'gpuPinStableId';

/** Default fill colour for a fresh `solid` source. */
export const SOLID_DEFAULT_COLOR = '#101014';

/** A fresh, empty source form. */
export function emptySourceForm(): SourceFormState {
  return {
    id: '',
    name: '',
    kind: 'rtsp',
    url: '',
    ndiName: '',
    path: '',
    color: SOLID_DEFAULT_COLOR,
    clockFace: 'analog',
    clockTwelveHour: false,
    clockTzMinutes: '0',
    rtspTransport: '',
    captionsMode: 'none',
    captionsPage: '',
    captionsTrack: '',
    captionsField: '',
    captionsPath: '',
    colorOverrideEnabled: false,
    colorPrimaries: 'auto',
    colorTransfer: 'auto',
    colorMatrix: 'auto',
    colorRange: 'auto',
    gpuPinEnabled: false,
    gpuPinVendor: 'nvidia',
    gpuPinStableId: '',
    wallclock: 'default',
    authSecretRef: '',
    extra: {},
  };
}

/** The body keys the source form manages (everything else is preserved). */
const SOURCE_MANAGED_KEYS: readonly string[] = [
  'id',
  'display_name',
  'kind',
  'url',
  'name',
  'path',
  'color',
  'face',
  'twelve_hour',
  'tz_offset_minutes',
  'rtsp',
  'auth',
  'color_override',
  'captions',
  'gpu_pin',
  'wallclock',
];

/** Whether a source kind's locator is its `url`. */
export function sourceKindHasUrl(kind: SourceFormKind): boolean {
  return (
    kind === 'rtsp' ||
    kind === 'hls' ||
    kind === 'youtube' ||
    kind === 'ts' ||
    kind === 'srt' ||
    kind === 'rtmp'
  );
}

/**
 * Switch a source form to a new kind. The shared locator value is kept when
 * moving between url kinds (a UX nicety); kind-scoped options reset so a stale
 * payload can never leak into the body (the body writer is kind-exact anyway).
 */
export function withSourceKind(form: SourceFormState, kind: SourceFormKind): SourceFormState {
  if (kind === form.kind) {
    return form;
  }
  return {
    ...form,
    kind,
    rtspTransport: '',
    color: form.color.trim() === '' ? SOLID_DEFAULT_COLOR : form.color,
  };
}

/** Build the exact config `Source` body from a valid form. */
export function sourceFormToBody(form: SourceFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  const displayName = form.name.trim();
  if (displayName !== '') {
    body.display_name = displayName;
  }
  body.kind = form.kind;
  switch (form.kind) {
    case 'bars':
      break;
    case 'solid':
      body.color = form.color.trim();
      break;
    case 'clock':
      body.face = form.clockFace;
      body.twelve_hour = form.clockTwelveHour;
      body.tz_offset_minutes = parseIntStrict(form.clockTzMinutes) ?? 0;
      break;
    case 'rtsp':
      body.url = form.url.trim();
      if (form.rtspTransport !== '') {
        body.rtsp = { transport: form.rtspTransport };
      }
      break;
    case 'hls':
    case 'youtube':
    case 'ts':
    case 'srt':
    case 'rtmp':
      body.url = form.url.trim();
      break;
    case 'ndi':
      body.name = form.ndiName.trim();
      break;
    case 'file':
      body.path = form.path.trim();
      break;
  }
  if (form.authSecretRef.trim() !== '') {
    body.auth = { secret_ref: form.authSecretRef.trim() };
  }
  if (form.colorOverrideEnabled) {
    body.color_override = {
      primaries: form.colorPrimaries.trim() === '' ? 'auto' : form.colorPrimaries.trim(),
      transfer: form.colorTransfer.trim() === '' ? 'auto' : form.colorTransfer.trim(),
      matrix: form.colorMatrix.trim() === '' ? 'auto' : form.colorMatrix.trim(),
      range: form.colorRange.trim() === '' ? 'auto' : form.colorRange.trim(),
    };
  }
  const captions = captionsBody(form);
  if (captions !== undefined) {
    body.captions = captions;
  }
  if (form.gpuPinEnabled) {
    body.gpu_pin = { vendor: form.gpuPinVendor, stable_id: form.gpuPinStableId.trim() };
  }
  if (form.wallclock !== 'default') {
    body.wallclock = { use: form.wallclock };
  }
  return body;
}

/** The internally-tagged `CaptionSelector` payload, or `undefined` to omit. */
function captionsBody(form: SourceFormState): Record<string, unknown> | undefined {
  switch (form.captionsMode) {
    case 'none':
      return undefined;
    case 'auto':
    case 'off':
      return { mode: form.captionsMode };
    case 'teletext_page':
      return { mode: 'teletext_page', page: parseIntStrict(form.captionsPage) ?? 0 };
    case 'track':
      return { mode: 'track', id: form.captionsTrack.trim() };
    case 'embedded_cc':
      return { mode: 'embedded_cc', field: form.captionsField.trim() };
    case 'sidecar':
      return { mode: 'sidecar', path: form.captionsPath.trim() };
  }
}

/**
 * Parse a stored source kind tag onto the editable form kind. The legacy
 * `test` alias canonicalizes to `bars` (the schema does the same); any OTHER
 * unknown/absent tag returns `undefined` — an explicit refusal, never a fold
 * (folding would silently rewrite the authored document on the next save).
 */
export function parseSourceFormKind(tag: string | undefined): SourceFormKind | undefined {
  if (tag === 'test') {
    return 'bars';
  }
  return (
    [
      'bars',
      'solid',
      'clock',
      'rtsp',
      'hls',
      'youtube',
      'ts',
      'srt',
      'rtmp',
      'ndi',
      'file',
    ] as const
  ).find((k) => k === tag);
}

/**
 * Project a stored record back onto the editable source form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function sourceFormFromRecord(record: ResourceRecord): SourceFormState | undefined {
  const body = record.body;
  const empty = emptySourceForm();
  const kind = parseSourceFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }

  const rtsp = asRecord(body.rtsp);
  const transport = asString(rtsp?.transport);
  const auth = asRecord(body.auth);
  const colorOverride = asRecord(body.color_override);
  const captions = asRecord(body.captions);
  const gpuPin = asRecord(body.gpu_pin);
  const wallclock = asRecord(body.wallclock);
  const wallclockUse = asString(wallclock?.use);

  const captionsMode: CaptionsMode =
    captions === undefined
      ? 'none'
      : (CAPTION_MODES.find((mode) => mode === asString(captions.mode)) ?? 'none');

  return {
    ...empty,
    id: record.id,
    // An authored `display_name` wins over the store name: the form writes its
    // Name back as `display_name`, so seeding from the store name would
    // clobber a differing authored value on the first save.
    name: asString(body.display_name) ?? record.name,
    kind,
    url: asString(body.url) ?? '',
    ndiName: kind === 'ndi' ? (asString(body.name) ?? '') : '',
    path: asString(body.path) ?? '',
    color: asString(body.color) ?? SOLID_DEFAULT_COLOR,
    clockFace: CLOCK_FACES.find((face) => face === asString(body.face)) ?? 'analog',
    clockTwelveHour: body.twelve_hour === true,
    clockTzMinutes: numberToField(asFiniteNumber(body.tz_offset_minutes)) || '0',
    rtspTransport: transport === 'tcp' || transport === 'udp' ? transport : '',
    captionsMode,
    captionsPage: numberToField(asFiniteNumber(captions?.page)),
    captionsTrack: asString(captions?.id) ?? '',
    captionsField: asString(captions?.field) ?? '',
    captionsPath: asString(captions?.path) ?? '',
    colorOverrideEnabled: colorOverride !== undefined,
    colorPrimaries: asString(colorOverride?.primaries) ?? 'auto',
    colorTransfer: asString(colorOverride?.transfer) ?? 'auto',
    colorMatrix: asString(colorOverride?.matrix) ?? 'auto',
    colorRange: asString(colorOverride?.range) ?? 'auto',
    gpuPinEnabled: gpuPin !== undefined,
    gpuPinVendor: asPinVendor(gpuPin?.vendor),
    gpuPinStableId: asString(gpuPin?.stable_id) ?? '',
    wallclock: wallclockUse === 'use' || wallclockUse === 'discard' ? wallclockUse : 'default',
    authSecretRef: asString(auth?.secret_ref) ?? '',
    extra: extraOf(body, SOURCE_MANAGED_KEYS),
  };
}

/** Validate a source form, returning per-field machine codes. */
export function validateSourceForm(
  form: SourceFormState,
  creating: boolean,
): FieldErrors<SourceField> {
  const errors: FieldErrors<SourceField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  switch (form.kind) {
    case 'rtsp': {
      const code = urlErrorCode(form.url, ['rtsp', 'rtsps'], 'scheme-rtsp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'srt': {
      const code = urlErrorCode(form.url, ['srt'], 'scheme-srt');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'rtmp': {
      const code = urlErrorCode(form.url, ['rtmp', 'rtmps'], 'scheme-rtmp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'hls':
    case 'youtube': {
      const code = urlErrorCode(form.url, ['http', 'https'], 'scheme-http');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'ts': {
      // MPEG-TS arrives over many transports (udp/rtp/http/…): any parseable
      // URL with a host is accepted — the schema field is an open url string.
      const code = urlErrorCode(form.url, undefined, 'url-invalid');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'ndi':
      if (form.ndiName.trim() === '') {
        errors.ndiName = 'required';
      }
      break;
    case 'file':
      if (form.path.trim() === '') {
        errors.path = 'required';
      }
      break;
    case 'solid':
      if (!HEX_COLOR_RE.test(form.color.trim())) {
        errors.color = 'hex-color';
      }
      break;
    case 'clock': {
      const tz = parseIntStrict(form.clockTzMinutes);
      if (tz === undefined || tz < CLOCK_TZ_MIN_MINUTES || tz > CLOCK_TZ_MAX_MINUTES) {
        errors.clockTzMinutes = 'int-range';
      }
      break;
    }
    case 'bars':
      break;
  }
  switch (form.captionsMode) {
    case 'teletext_page': {
      const page = parseIntStrict(form.captionsPage);
      if (page === undefined || page < 100 || page > 899) {
        errors.captionsPage = 'int-range';
      }
      break;
    }
    case 'track':
      if (form.captionsTrack.trim() === '') {
        errors.captionsTrack = 'required';
      }
      break;
    case 'embedded_cc':
      if (form.captionsField.trim() === '') {
        errors.captionsField = 'required';
      }
      break;
    case 'sidecar':
      if (form.captionsPath.trim() === '') {
        errors.captionsPath = 'required';
      }
      break;
    default:
      break;
  }
  if (form.gpuPinEnabled && form.gpuPinStableId.trim() === '') {
    errors.gpuPinStableId = 'required';
  }
  return errors;
}

// --- Outputs --------------------------------------------------------------------

/** Per-output audio selection ('default' = no `audio` block). */
export type OutputAudioChoice = 'default' | 'program' | 'tracks';

/** The editable state behind the output form. */
export interface OutputFormState {
  readonly id: string;
  readonly name: string;
  /** Display kind ('rtsp' ⇒ wire `rtsp_server`, 'll-hls' ⇒ `ll_hls`). */
  readonly kind: OutputKind;
  /** Mount point (`rtsp_server`). */
  readonly mount: string;
  /** Output path (`hls` / `ll_hls`). */
  readonly path: string;
  /** Destination URL (`rtmp` / `srt`). */
  readonly url: string;
  /** Advertised NDI source name (`ndi`). */
  readonly ndiName: string;
  /** Video codec (all kinds except `ndi`). An open schema string. */
  readonly codec: string;
  /** Latency profile hint (`rtsp_server`, optional). */
  readonly latencyProfile: string;
  /** LL-HLS part target in ms ('' = omit). */
  readonly partTargetMs: string;
  /** Segment duration in ms ('' = omit; `hls` + `ll_hls`). */
  readonly segmentMs: string;
  /** GOP duration in ms ('' = omit; `ll_hls`). */
  readonly gopMs: string;
  /** Audio selection ('default' = the engine's program-bus default). */
  readonly audioMode: OutputAudioChoice;
  /** Comma-separated selectable track names (mode `tracks`). */
  readonly audioTracks: string;
  /** Whether a `gpu_pin` block is written. */
  readonly gpuPinEnabled: boolean;
  readonly gpuPinVendor: PinVendor;
  readonly gpuPinStableId: string;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The output-form fields that can carry a validation error. */
export type OutputField =
  | 'id'
  | 'name'
  | 'mount'
  | 'path'
  | 'url'
  | 'ndiName'
  | 'codec'
  | 'partTargetMs'
  | 'segmentMs'
  | 'gopMs'
  | 'audioTracks'
  | 'gpuPinStableId';

/**
 * Which output kinds the CLI run path can actually serve today. Mirrors
 * `build_outputs` in crates/multiview-cli/src/pipeline.rs: hls / ll_hls /
 * rtmp / srt build runnable sinks; rtsp_server and ndi are accepted by the
 * config schema but warned + skipped ("not yet runnable in this build").
 */
export const OUTPUT_RUNNABLE: Readonly<Record<OutputKind, boolean>> = {
  rtsp: false,
  hls: true,
  'll-hls': true,
  ndi: false,
  rtmp: true,
  srt: true,
};

/** Map a display kind onto the config wire tag. */
export function outputWireKind(kind: OutputKind): string {
  switch (kind) {
    case 'rtsp':
      return 'rtsp_server';
    case 'll-hls':
      return 'll_hls';
    default:
      return kind;
  }
}

/** A fresh, empty output form. */
export function emptyOutputForm(): OutputFormState {
  return {
    id: '',
    name: '',
    kind: 'hls',
    mount: '',
    path: '',
    url: '',
    ndiName: '',
    codec: 'h264',
    latencyProfile: '',
    partTargetMs: '',
    segmentMs: '',
    gopMs: '',
    audioMode: 'default',
    audioTracks: '',
    gpuPinEnabled: false,
    gpuPinVendor: 'nvidia',
    gpuPinStableId: '',
    extra: {},
  };
}

/**
 * The body keys the output form manages (everything else is preserved).
 *
 * Deliberately NOT here: `id`. The output config-level `id` is OPTIONAL,
 * label-derived when absent (ADR-0034), and lives in a DIFFERENT namespace
 * from the resource/store id (seeded stores use `output-0..n`): crosspoints /
 * `OutputRef`s bind to the config id, so the form never writes it — an
 * authored `id` rides the extra-preservation path verbatim instead.
 */
const OUTPUT_MANAGED_KEYS: readonly string[] = [
  'kind',
  'mount',
  'path',
  'url',
  'name',
  'codec',
  'latency_profile',
  'part_target_ms',
  'segment_ms',
  'gop_ms',
  'gpu_pin',
  'audio',
];

/**
 * Build the exact config `Output` body from a valid form. The routable
 * config-level `id` is never derived from the form/store id (see
 * {@link OUTPUT_MANAGED_KEYS}); an authored one is re-emitted via `extra`.
 */
export function outputFormToBody(form: OutputFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.kind = outputWireKind(form.kind);
  switch (form.kind) {
    case 'rtsp':
      body.mount = form.mount.trim();
      body.codec = form.codec.trim();
      if (form.latencyProfile.trim() !== '') {
        body.latency_profile = form.latencyProfile.trim();
      }
      break;
    case 'll-hls': {
      body.path = form.path.trim();
      body.codec = form.codec.trim();
      const part = parseIntStrict(form.partTargetMs);
      if (part !== undefined) {
        body.part_target_ms = part;
      }
      const segment = parseIntStrict(form.segmentMs);
      if (segment !== undefined) {
        body.segment_ms = segment;
      }
      const gop = parseIntStrict(form.gopMs);
      if (gop !== undefined) {
        body.gop_ms = gop;
      }
      break;
    }
    case 'hls': {
      body.path = form.path.trim();
      body.codec = form.codec.trim();
      const segment = parseIntStrict(form.segmentMs);
      if (segment !== undefined) {
        body.segment_ms = segment;
      }
      break;
    }
    case 'ndi':
      body.name = form.ndiName.trim();
      break;
    case 'rtmp':
    case 'srt':
      body.url = form.url.trim();
      body.codec = form.codec.trim();
      break;
  }
  if (form.audioMode !== 'default') {
    body.audio = {
      mode: form.audioMode,
      tracks: form.audioMode === 'tracks' ? parseTrackList(form.audioTracks) : [],
    };
  }
  if (form.gpuPinEnabled) {
    body.gpu_pin = { vendor: form.gpuPinVendor, stable_id: form.gpuPinStableId.trim() };
  }
  return body;
}

/** Split a comma-separated track list into trimmed, non-empty names. */
export function parseTrackList(value: string): string[] {
  return value
    .split(',')
    .map((track) => track.trim())
    .filter((track) => track !== '');
}

/**
 * Parse a stored output wire kind onto the display kind, or `undefined` for a
 * kind this UI cannot edit — an explicit refusal, never a fold.
 */
export function parseOutputFormKind(tag: string | undefined): OutputKind | undefined {
  if (tag === 'rtsp_server') {
    return 'rtsp';
  }
  if (tag === 'll_hls') {
    return 'll-hls';
  }
  return (['hls', 'ndi', 'rtmp', 'srt'] as const).find((k) => k === tag);
}

/**
 * Project a stored record back onto the editable output form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function outputFormFromRecord(record: ResourceRecord): OutputFormState | undefined {
  const body = record.body;
  const empty = emptyOutputForm();
  const kind = parseOutputFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  const audio = asRecord(body.audio);
  const audioMode = asString(audio?.mode);
  const rawTracks = audio?.tracks;
  const tracks = Array.isArray(rawTracks)
    ? rawTracks.filter((track): track is string => typeof track === 'string')
    : [];
  const gpuPin = asRecord(body.gpu_pin);
  return {
    ...empty,
    id: record.id,
    name: record.name,
    kind,
    mount: asString(body.mount) ?? '',
    path: asString(body.path) ?? '',
    url: asString(body.url) ?? '',
    ndiName: kind === 'ndi' ? (asString(body.name) ?? '') : '',
    codec: asString(body.codec) ?? (kind === 'ndi' ? '' : 'h264'),
    latencyProfile: asString(body.latency_profile) ?? '',
    partTargetMs: numberToField(asFiniteNumber(body.part_target_ms)),
    segmentMs: numberToField(asFiniteNumber(body.segment_ms)),
    gopMs: numberToField(asFiniteNumber(body.gop_ms)),
    audioMode:
      audioMode === 'program' || audioMode === 'tracks' ? audioMode : 'default',
    audioTracks: tracks.join(', '),
    gpuPinEnabled: gpuPin !== undefined,
    gpuPinVendor: asPinVendor(gpuPin?.vendor),
    gpuPinStableId: asString(gpuPin?.stable_id) ?? '',
    extra: extraOf(body, OUTPUT_MANAGED_KEYS),
  };
}

/** Validate a positive-integer-or-blank duration field. */
function durationError(value: string): FormErrorCode | undefined {
  if (value.trim() === '') {
    return undefined;
  }
  const parsed = parseIntStrict(value);
  return parsed === undefined || parsed <= 0 ? 'positive-int' : undefined;
}

/** Validate an output form, returning per-field machine codes. */
export function validateOutputForm(
  form: OutputFormState,
  creating: boolean,
): FieldErrors<OutputField> {
  const errors: FieldErrors<OutputField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  switch (form.kind) {
    case 'rtsp':
      if (form.mount.trim() === '') {
        errors.mount = 'required';
      } else if (!form.mount.trim().startsWith('/')) {
        errors.mount = 'mount-slash';
      }
      break;
    case 'hls':
    case 'll-hls':
      if (form.path.trim() === '') {
        errors.path = 'required';
      }
      break;
    case 'ndi':
      if (form.ndiName.trim() === '') {
        errors.ndiName = 'required';
      }
      break;
    case 'rtmp': {
      const code = urlErrorCode(form.url, ['rtmp', 'rtmps'], 'scheme-rtmp');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
    case 'srt': {
      const code = urlErrorCode(form.url, ['srt'], 'scheme-srt');
      if (code !== undefined) {
        errors.url = code;
      }
      break;
    }
  }
  if (form.kind !== 'ndi' && form.codec.trim() === '') {
    errors.codec = 'required';
  }
  if (form.kind === 'll-hls') {
    const part = durationError(form.partTargetMs);
    if (part !== undefined) {
      errors.partTargetMs = part;
    }
    const gop = durationError(form.gopMs);
    if (gop !== undefined) {
      errors.gopMs = gop;
    }
  }
  if (form.kind === 'hls' || form.kind === 'll-hls') {
    const segment = durationError(form.segmentMs);
    if (segment !== undefined) {
      errors.segmentMs = segment;
    }
  }
  if (form.audioMode === 'tracks' && parseTrackList(form.audioTracks).length === 0) {
    errors.audioTracks = 'tracks-required';
  }
  if (form.gpuPinEnabled && form.gpuPinStableId.trim() === '') {
    errors.gpuPinStableId = 'required';
  }
  return errors;
}

// --- Overlays -------------------------------------------------------------------

/** The editable state behind the overlay form. */
export interface OverlayFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: OverlayKind;
  /** Attachment target (`canvas` or a cell id). */
  readonly target: string;
  /** Stacking order (an integer string). */
  readonly z: string;
  /** Clock face param (`clock`; the run path renders the analog face). */
  readonly clockFace: ClockFace;
  /** Clock timezone offset minutes (`clock`, '' = omit ⇒ UTC). */
  readonly clockTzMinutes: string;
  /** Clock centre x in canvas pixels (`clock`, '' = auto placement). */
  readonly clockX: string;
  /** Clock centre y in canvas pixels (`clock`, '' = auto placement). */
  readonly clockY: string;
  /** Clock face radius in pixels (`clock`, '' = auto size). */
  readonly clockRadius: string;
  /** Border width in pixels (`tally_border`, '' = default). */
  readonly tallyWidthPx: string;
  /** Border colour hex (`tally_border`, '' = default). */
  readonly tallyColor: string;
  /** Tally binding URI (`tally_border`, e.g. `tally://cell_big`; '' = omit). */
  readonly tallyBinding: string;
  /** Unmanaged params preserved verbatim while the kind is unchanged. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The overlay-form fields that can carry a validation error. */
export type OverlayField =
  | 'id'
  | 'name'
  | 'target'
  | 'z'
  | 'clockTzMinutes'
  | 'clockX'
  | 'clockY'
  | 'clockRadius'
  | 'tallyWidthPx'
  | 'tallyColor';

/** A fresh, empty overlay form. */
export function emptyOverlayForm(): OverlayFormState {
  return {
    id: '',
    name: '',
    kind: 'clock',
    target: 'canvas',
    z: '0',
    clockFace: 'analog',
    clockTzMinutes: '',
    clockX: '',
    clockY: '',
    clockRadius: '',
    tallyWidthPx: '',
    tallyColor: '',
    tallyBinding: '',
    extra: {},
  };
}

/** The body keys every overlay kind's writer re-emits. */
const OVERLAY_BASE_KEYS: readonly string[] = ['id', 'kind', 'target', 'z'];

/**
 * The flattened param keys the overlay form manages for ONE kind — exactly
 * the keys that kind's writer re-emits. Stripping must be kind-scoped: a
 * `label` overlay carrying e.g. `color` or `x` (param names another kind also
 * uses) must keep them via the extra-preservation path, because the label
 * writer would never re-emit them.
 */
function overlayManagedKeys(kind: OverlayKind): readonly string[] {
  switch (kind) {
    case 'clock':
      return [...OVERLAY_BASE_KEYS, 'face', 'tz_minutes', 'x', 'y', 'radius'];
    case 'tally_border':
      return [...OVERLAY_BASE_KEYS, 'width_px', 'color', 'binding'];
    case 'label':
    case 'image':
    case 'subtitle':
      return OVERLAY_BASE_KEYS;
  }
}

/**
 * Switch an overlay form to a new kind. Params are kind-scoped (the config
 * flattens them verbatim), so the previous kind's params — including the
 * preserved unmanaged extras — are dropped rather than leaked into the body.
 */
export function withOverlayKind(form: OverlayFormState, kind: OverlayKind): OverlayFormState {
  if (kind === form.kind) {
    return form;
  }
  const empty = emptyOverlayForm();
  return {
    ...empty,
    id: form.id,
    name: form.name,
    kind,
    target: form.target,
    z: form.z,
  };
}

/** Build the exact config `Overlay` body from a valid form. */
export function overlayFormToBody(form: OverlayFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  body.kind = form.kind;
  body.target = form.target.trim();
  body.z = parseIntStrict(form.z) ?? 0;
  if (form.kind === 'clock') {
    body.face = form.clockFace;
    const tz = parseIntStrict(form.clockTzMinutes);
    if (tz !== undefined) {
      body.tz_minutes = tz;
    }
    const x = parseIntStrict(form.clockX);
    if (x !== undefined) {
      body.x = x;
    }
    const y = parseIntStrict(form.clockY);
    if (y !== undefined) {
      body.y = y;
    }
    const radius = parseIntStrict(form.clockRadius);
    if (radius !== undefined) {
      body.radius = radius;
    }
  }
  if (form.kind === 'tally_border') {
    const width = parseIntStrict(form.tallyWidthPx);
    if (width !== undefined) {
      body.width_px = width;
    }
    if (form.tallyColor.trim() !== '') {
      body.color = form.tallyColor.trim();
    }
    if (form.tallyBinding.trim() !== '') {
      body.binding = form.tallyBinding.trim();
    }
  }
  // label / image / subtitle carry no kind-specific params in this build: the
  // Rust side defines none for them (schema.rs `Overlay` keeps params verbatim
  // and the run path consumes only the clock params) — nothing is invented.
  return body;
}

/**
 * Parse a stored overlay kind tag, or `undefined` for a kind this UI cannot
 * edit — an explicit refusal, never a fold.
 */
export function parseOverlayFormKind(tag: string | undefined): OverlayKind | undefined {
  return (['clock', 'label', 'tally_border', 'image', 'subtitle'] as const).find(
    (k) => k === tag,
  );
}

/**
 * Project a stored record back onto the editable overlay form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function overlayFormFromRecord(record: ResourceRecord): OverlayFormState | undefined {
  const body = record.body;
  const empty = emptyOverlayForm();
  const kind = parseOverlayFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  return {
    ...empty,
    id: record.id,
    name: record.name,
    kind,
    target: asString(body.target) ?? 'canvas',
    z: numberToField(asFiniteNumber(body.z)) || '0',
    clockFace: CLOCK_FACES.find((face) => face === asString(body.face)) ?? 'analog',
    clockTzMinutes: numberToField(asFiniteNumber(body.tz_minutes)),
    clockX: numberToField(asFiniteNumber(body.x)),
    clockY: numberToField(asFiniteNumber(body.y)),
    clockRadius: numberToField(asFiniteNumber(body.radius)),
    tallyWidthPx: numberToField(asFiniteNumber(body.width_px)),
    tallyColor: asString(body.color) ?? '',
    tallyBinding: asString(body.binding) ?? '',
    extra: extraOf(body, overlayManagedKeys(kind)),
  };
}

/** Validate an optional integer field ('' = fine). */
function optionalIntError(value: string): FormErrorCode | undefined {
  if (value.trim() === '') {
    return undefined;
  }
  return parseIntStrict(value) === undefined ? 'int' : undefined;
}

/** Validate an overlay form, returning per-field machine codes. */
export function validateOverlayForm(
  form: OverlayFormState,
  creating: boolean,
): FieldErrors<OverlayField> {
  const errors: FieldErrors<OverlayField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  if (form.target.trim() === '') {
    errors.target = 'required';
  }
  if (parseIntStrict(form.z) === undefined) {
    errors.z = 'int';
  }
  if (form.kind === 'clock') {
    const tz = optionalIntError(form.clockTzMinutes);
    if (tz !== undefined) {
      errors.clockTzMinutes = tz;
    }
    const x = optionalIntError(form.clockX);
    if (x !== undefined) {
      errors.clockX = x;
    }
    const y = optionalIntError(form.clockY);
    if (y !== undefined) {
      errors.clockY = y;
    }
    if (form.clockRadius.trim() !== '') {
      const radius = parseIntStrict(form.clockRadius);
      if (radius === undefined || radius <= 0) {
        errors.clockRadius = 'positive-int';
      }
    }
  }
  if (form.kind === 'tally_border') {
    if (form.tallyWidthPx.trim() !== '') {
      const width = parseIntStrict(form.tallyWidthPx);
      if (width === undefined || width <= 0) {
        errors.tallyWidthPx = 'positive-int';
      }
    }
    if (form.tallyColor.trim() !== '' && !HEX_COLOR_RE.test(form.tallyColor.trim())) {
      errors.tallyColor = 'hex-color';
    }
  }
  return errors;
}

// --- Probes ---------------------------------------------------------------------

/**
 * The X.733 perceived-severity wire values (core `PerceivedSeverity`,
 * PascalCase variant names), in ascending order of urgency.
 */
export type ProbeSeverity =
  | 'Cleared'
  | 'Indeterminate'
  | 'Warning'
  | 'Minor'
  | 'Major'
  | 'Critical';

/** All severities in ascending urgency, for building selectors. */
export const PROBE_SEVERITIES: readonly ProbeSeverity[] = [
  'Cleared',
  'Indeterminate',
  'Warning',
  'Minor',
  'Major',
  'Critical',
];

/** The loudness compliance standards (config `LoudnessTarget` kind tags). */
export type LoudnessStandard = 'r128' | 'a85';

/** Both loudness standards, for building selectors. */
export const LOUDNESS_STANDARDS: readonly LoudnessStandard[] = ['r128', 'a85'];

/** The standard's default integrated-loudness target (LUFS/LKFS, as a field). */
function loudnessTargetDefault(standard: LoudnessStandard): string {
  return standard === 'a85' ? '-24' : '-23';
}

/** The standard's default max true-peak ceiling (dBTP, as a field). */
function loudnessTruePeakDefault(standard: LoudnessStandard): string {
  return standard === 'a85' ? '-2' : '-1';
}

/** The editable state behind the probe form (numbers kept as input strings). */
export interface ProbeFormState {
  readonly id: string;
  readonly name: string;
  readonly kind: ProbeKind;
  /** The cell id this probe watches. */
  readonly cell: string;
  /** Luma ceiling, 8-bit `0..=255` (`black`). */
  readonly lumaThreshold: string;
  /** Inter-frame difference floor, per-mille `0..=1000` (`freeze`). */
  readonly differenceThreshold: string;
  /** Whether a `zone` block is written (`black`/`freeze`; off = full frame). */
  readonly zoneEnabled: boolean;
  /** Zone left edge, fraction of tile width (`0..=1`). */
  readonly zoneX: string;
  /** Zone top edge, fraction of tile height (`0..=1`). */
  readonly zoneY: string;
  /** Zone width, fraction of tile width (`0..=1`). */
  readonly zoneW: string;
  /** Zone height, fraction of tile height (`0..=1`). */
  readonly zoneH: string;
  /** Level ceiling in dBFS at or below which audio is silent (`silence`). */
  readonly levelDbfs: string;
  /** Loudness compliance standard (`loudness`). */
  readonly loudnessStandard: LoudnessStandard;
  /** Integrated-loudness target in LUFS/LKFS (`loudness`). */
  readonly loudnessTarget: string;
  /** Max permitted true-peak in dBTP (`loudness`). */
  readonly loudnessTruePeak: string;
  /** Milliseconds the condition must persist before the alarm raises. */
  readonly dwellUpMs: string;
  /** Milliseconds the condition must clear before the alarm clears. */
  readonly dwellDownMs: string;
  /** The X.733 severity asserted when the probe fires. */
  readonly severity: ProbeSeverity;
  /** Whether the alarm latches (held until explicitly reset). */
  readonly latched: boolean;
  /** Unmanaged body fields preserved verbatim across an edit. */
  readonly extra: Readonly<Record<string, unknown>>;
}

/** The probe-form fields that can carry a validation error. */
export type ProbeField =
  | 'id'
  | 'name'
  | 'cell'
  | 'lumaThreshold'
  | 'differenceThreshold'
  | 'zoneX'
  | 'zoneY'
  | 'zoneW'
  | 'zoneH'
  | 'levelDbfs'
  | 'loudnessTarget'
  | 'loudnessTruePeak'
  | 'dwellUpMs'
  | 'dwellDownMs';

/** A fresh, empty probe form (schema-aligned defaults). */
export function emptyProbeForm(): ProbeFormState {
  return {
    id: '',
    name: '',
    kind: 'black',
    cell: '',
    lumaThreshold: '16',
    differenceThreshold: '5',
    zoneEnabled: false,
    zoneX: '0',
    zoneY: '0',
    zoneW: '1',
    zoneH: '1',
    levelDbfs: '-60',
    loudnessStandard: 'r128',
    loudnessTarget: loudnessTargetDefault('r128'),
    loudnessTruePeak: loudnessTruePeakDefault('r128'),
    dwellUpMs: '1000',
    dwellDownMs: '1000',
    severity: 'Warning',
    latched: false,
    extra: {},
  };
}

/** The body keys the probe form manages (everything else is preserved). */
const PROBE_MANAGED_KEYS: readonly string[] = [
  'id',
  'cell',
  'kind',
  'luma_threshold',
  'difference_threshold',
  'zone',
  'level_dbfs',
  'target',
  'dwell',
  'severity',
  'latched',
];

/**
 * Switch a probe form to a new kind. The shared identity/policy fields (cell,
 * dwell, severity, latched) are kept; kind-scoped parameters reset to their
 * defaults so a stale payload can never leak into the body.
 */
export function withProbeKind(form: ProbeFormState, kind: ProbeKind): ProbeFormState {
  if (kind === form.kind) {
    return form;
  }
  const empty = emptyProbeForm();
  return {
    ...form,
    kind,
    lumaThreshold: empty.lumaThreshold,
    differenceThreshold: empty.differenceThreshold,
    zoneEnabled: empty.zoneEnabled,
    zoneX: empty.zoneX,
    zoneY: empty.zoneY,
    zoneW: empty.zoneW,
    zoneH: empty.zoneH,
    levelDbfs: empty.levelDbfs,
    loudnessStandard: empty.loudnessStandard,
    loudnessTarget: empty.loudnessTarget,
    loudnessTruePeak: empty.loudnessTruePeak,
  };
}

/**
 * Switch the loudness standard, resetting the target/true-peak fields to the
 * new standard's defaults (R128 −23 LUFS / −1 dBTP; A/85 −24 LKFS / −2 dBTP).
 */
export function withProbeLoudnessStandard(
  form: ProbeFormState,
  standard: LoudnessStandard,
): ProbeFormState {
  if (standard === form.loudnessStandard) {
    return form;
  }
  return {
    ...form,
    loudnessStandard: standard,
    loudnessTarget: loudnessTargetDefault(standard),
    loudnessTruePeak: loudnessTruePeakDefault(standard),
  };
}

/** Build the exact config `Probe` body from a valid form. */
export function probeFormToBody(form: ProbeFormState): Record<string, unknown> {
  const body: Record<string, unknown> = { ...form.extra };
  body.id = form.id.trim();
  body.cell = form.cell.trim();
  body.kind = form.kind;
  switch (form.kind) {
    case 'black':
      body.luma_threshold = parseIntStrict(form.lumaThreshold) ?? 0;
      break;
    case 'freeze':
      body.difference_threshold = parseIntStrict(form.differenceThreshold) ?? 0;
      break;
    case 'silence':
      body.level_dbfs = parseNumberStrict(form.levelDbfs) ?? 0;
      break;
    case 'loudness': {
      const target = parseNumberStrict(form.loudnessTarget) ?? 0;
      const peak = parseNumberStrict(form.loudnessTruePeak) ?? 0;
      body.target =
        form.loudnessStandard === 'a85'
          ? { kind: 'a85', target_lkfs: target, max_true_peak_dbtp: peak }
          : { kind: 'r128', target_lufs: target, max_true_peak_dbtp: peak };
      break;
    }
  }
  if ((form.kind === 'black' || form.kind === 'freeze') && form.zoneEnabled) {
    body.zone = {
      x: parseNumberStrict(form.zoneX) ?? 0,
      y: parseNumberStrict(form.zoneY) ?? 0,
      w: parseNumberStrict(form.zoneW) ?? 1,
      h: parseNumberStrict(form.zoneH) ?? 1,
    };
  }
  // Dwell, severity, and latching are policy the operator always authors here;
  // the explicit values match the schema defaults on a fresh form.
  body.dwell = {
    up_ms: parseIntStrict(form.dwellUpMs) ?? 1000,
    down_ms: parseIntStrict(form.dwellDownMs) ?? 1000,
  };
  body.severity = form.severity;
  body.latched = form.latched;
  return body;
}

/**
 * Parse a stored probe kind tag, or `undefined` for a kind this UI cannot
 * edit — an explicit refusal, never a fold.
 */
export function parseProbeFormKind(tag: string | undefined): ProbeKind | undefined {
  return (['black', 'freeze', 'silence', 'loudness'] as const).find((k) => k === tag);
}

/**
 * Project a stored record back onto the editable probe form, or `undefined`
 * when the body's kind is not one this UI can edit (the page disables Edit;
 * the document is preserved as authored).
 */
export function probeFormFromRecord(record: ResourceRecord): ProbeFormState | undefined {
  const body = record.body;
  const empty = emptyProbeForm();
  const kind = parseProbeFormKind(asString(body.kind));
  if (kind === undefined) {
    return undefined;
  }
  const zone = asRecord(body.zone);
  const dwell = asRecord(body.dwell);
  const target = asRecord(body.target);
  const standard: LoudnessStandard = asString(target?.kind) === 'a85' ? 'a85' : 'r128';
  const targetValue =
    standard === 'a85' ? asFiniteNumber(target?.target_lkfs) : asFiniteNumber(target?.target_lufs);
  return {
    ...empty,
    id: record.id,
    name: record.name,
    kind,
    cell: asString(body.cell) ?? '',
    lumaThreshold: numberToField(asFiniteNumber(body.luma_threshold)) || empty.lumaThreshold,
    differenceThreshold:
      numberToField(asFiniteNumber(body.difference_threshold)) || empty.differenceThreshold,
    zoneEnabled: zone !== undefined,
    zoneX: zone === undefined ? empty.zoneX : numberToField(asFiniteNumber(zone.x)) || '0',
    zoneY: zone === undefined ? empty.zoneY : numberToField(asFiniteNumber(zone.y)) || '0',
    zoneW: zone === undefined ? empty.zoneW : numberToField(asFiniteNumber(zone.w)) || '1',
    zoneH: zone === undefined ? empty.zoneH : numberToField(asFiniteNumber(zone.h)) || '1',
    levelDbfs: numberToField(asFiniteNumber(body.level_dbfs)) || empty.levelDbfs,
    loudnessStandard: standard,
    loudnessTarget: numberToField(targetValue) || loudnessTargetDefault(standard),
    loudnessTruePeak:
      numberToField(asFiniteNumber(target?.max_true_peak_dbtp)) ||
      loudnessTruePeakDefault(standard),
    dwellUpMs: numberToField(asFiniteNumber(dwell?.up_ms)) || empty.dwellUpMs,
    dwellDownMs: numberToField(asFiniteNumber(dwell?.down_ms)) || empty.dwellDownMs,
    severity: PROBE_SEVERITIES.find((s) => s === asString(body.severity)) ?? 'Cleared',
    latched: body.latched === true,
    extra: extraOf(body, PROBE_MANAGED_KEYS),
  };
}

/** Validate a bounded-integer field (`min..=max`), returning a code or none. */
function intRangeError(value: string, min: number, max: number): FormErrorCode | undefined {
  const parsed = parseIntStrict(value);
  return parsed === undefined || parsed < min || parsed > max ? 'int-range' : undefined;
}

/** Validate the detection-zone geometry into per-field error codes. */
function validateZone(form: ProbeFormState, errors: FieldErrors<ProbeField>): void {
  const x = parseNumberStrict(form.zoneX);
  const y = parseNumberStrict(form.zoneY);
  const w = parseNumberStrict(form.zoneW);
  const h = parseNumberStrict(form.zoneH);
  if (x === undefined) {
    errors.zoneX = 'number';
  }
  if (y === undefined) {
    errors.zoneY = 'number';
  }
  if (w === undefined) {
    errors.zoneW = 'number';
  }
  if (h === undefined) {
    errors.zoneH = 'number';
  }
  if (x === undefined || y === undefined || w === undefined || h === undefined) {
    return;
  }
  // Mirrors DetectionZone::validate: positive extent, origin within the unit
  // square, and no overhang past 1.0 on either axis.
  if (x < 0) {
    errors.zoneX = 'zone-extent';
  }
  if (y < 0) {
    errors.zoneY = 'zone-extent';
  }
  if (w <= 0 || x + w > 1) {
    errors.zoneW = 'zone-extent';
  }
  if (h <= 0 || y + h > 1) {
    errors.zoneH = 'zone-extent';
  }
}

/** Validate a probe form, returning per-field machine codes. */
export function validateProbeForm(
  form: ProbeFormState,
  creating: boolean,
): FieldErrors<ProbeField> {
  const errors: FieldErrors<ProbeField> = {};
  if (creating && form.id.trim() === '') {
    errors.id = 'required';
  }
  if (form.name.trim() === '') {
    errors.name = 'required';
  }
  if (form.cell.trim() === '') {
    errors.cell = 'required';
  }
  switch (form.kind) {
    case 'black': {
      const code = intRangeError(form.lumaThreshold, 0, 255);
      if (code !== undefined) {
        errors.lumaThreshold = code;
      }
      break;
    }
    case 'freeze': {
      const code = intRangeError(form.differenceThreshold, 0, 1000);
      if (code !== undefined) {
        errors.differenceThreshold = code;
      }
      break;
    }
    case 'silence':
      if (parseNumberStrict(form.levelDbfs) === undefined) {
        errors.levelDbfs = 'number';
      }
      break;
    case 'loudness':
      if (parseNumberStrict(form.loudnessTarget) === undefined) {
        errors.loudnessTarget = 'number';
      }
      if (parseNumberStrict(form.loudnessTruePeak) === undefined) {
        errors.loudnessTruePeak = 'number';
      }
      break;
  }
  if ((form.kind === 'black' || form.kind === 'freeze') && form.zoneEnabled) {
    validateZone(form, errors);
  }
  const up = intRangeError(form.dwellUpMs, 0, Number.MAX_SAFE_INTEGER);
  if (up !== undefined) {
    errors.dwellUpMs = up;
  }
  const down = intRangeError(form.dwellDownMs, 0, Number.MAX_SAFE_INTEGER);
  if (down !== undefined) {
    errors.dwellDownMs = down;
  }
  return errors;
}

/**
 * Collect the cell ids offered by the layout documents, for the probe form's
 * cell picker: cells of canvas-bearing (working) layouts first, then draft
 * cells, deduplicated in encounter order. Malformed bodies are skipped — the
 * picker degrades to a free-text field when nothing is found.
 */
export function cellIdsFromLayouts(
  layouts: readonly { readonly body: unknown }[],
): readonly string[] {
  const fromWorking: string[] = [];
  const fromDrafts: string[] = [];
  for (const layout of layouts) {
    const body = asRecord(layout.body);
    if (body === undefined) {
      continue;
    }
    const cells = Array.isArray(body.cells) ? body.cells : [];
    const bucket = body.canvas === undefined ? fromDrafts : fromWorking;
    for (const cell of cells) {
      const id = asString(asRecord(cell)?.id);
      if (id !== undefined && id !== '') {
        bucket.push(id);
      }
    }
  }
  const out: string[] = [];
  for (const id of [...fromWorking, ...fromDrafts]) {
    if (!out.includes(id)) {
      out.push(id);
    }
  }
  return out;
}
