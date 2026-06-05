// Typed view-models for the resource surfaces the SPA manages alongside layouts.
//
// Sources, Outputs, and Overlays are first-class control-plane REST resources,
// each stored as a `{ id, name, body }` record where `body` is the opaque,
// validated config document (`multiview-config`: `Source`, `Output`, `Overlay`).
// The control plane stores and version-stamps the body without the SPA having to
// model every kind's fields. The view-models below are the small, display-facing
// projections the read views and the layout-editor palette render; they are
// derived from the opaque body with typed field guards (see `./api.ts`), never
// `as`-casts.

/** The resource collections the SPA manages (the REST path segment). */
export type ResourceKind = 'sources' | 'outputs' | 'overlays';

/**
 * A persisted resource record, exactly as the control plane returns it: a stable
 * id, an operator label, and the opaque config `body`.
 */
export interface ResourceRecord {
  /** Stable resource id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** The opaque, validated config document. */
  readonly body: Record<string, unknown>;
}

/** The create/update payload accepted by the control plane. */
export interface ResourceInput {
  /** Operator label. */
  readonly name: string;
  /** The config document to store. */
  readonly body: Record<string, unknown>;
}

/**
 * The transport kinds Multiview can ingest (config `SourceKind`, internally
 * tagged by `kind`). These tags are the literal config wire kinds (snake_case),
 * so they double as the `kind` field written into the body.
 */
export type SourceKind =
  | 'bars'
  | 'solid'
  | 'clock'
  | 'rtsp'
  | 'hls'
  | 'ts'
  | 'srt'
  | 'rtmp'
  | 'ndi'
  | 'file'
  // `test` is a legacy alias for `bars` (ADR-0027); kept in the union for
  // back-compat parsing of older bodies, but not offered in the picker.
  | 'test';

/**
 * The user-pickable source kinds, for building selectors. Excludes the legacy
 * `test` alias (folded to `bars`); `test` stays in {@link SourceKind} so older
 * bodies still parse.
 */
export const SOURCE_KINDS: readonly SourceKind[] = [
  'bars',
  'solid',
  'clock',
  'rtsp',
  'hls',
  'ts',
  'srt',
  'rtmp',
  'ndi',
  'file',
];

/** A managed ingest source. */
export interface SourceView {
  /** Stable source id (referenced by a cell's `input_id`). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Transport kind. */
  readonly kind: SourceKind;
  /**
   * The configured locator for display — the kind's key field: `url` for the
   * network kinds, the source `name` for NDI, the `path` for file. Absent for
   * the synthetic kinds (`bars`/`solid`/`clock`, and the legacy `test` alias),
   * which carry no locator.
   */
  readonly locator: string | undefined;
}

/** The output transport display kinds (config `Output`, folded for display). */
export type OutputKind = 'rtsp' | 'hls' | 'll-hls' | 'ndi' | 'rtmp' | 'srt';

/** All output kinds, for building selectors. */
export const OUTPUT_KINDS: readonly OutputKind[] = [
  'rtsp',
  'hls',
  'll-hls',
  'ndi',
  'rtmp',
  'srt',
];

/** A configured output sink/server. */
export interface OutputView {
  /** Stable output id. */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Output transport kind. */
  readonly kind: OutputKind;
  /**
   * The kind's key field for display: the RTSP `mount`, the HLS/LL-HLS `path`,
   * the RTMP/SRT `url`, or the NDI source `name`.
   */
  readonly target: string | undefined;
  /** The video codec (absent for NDI, which carries no codec). */
  readonly codec: string | undefined;
}

/** The overlay kinds (config `[[overlays]]` `kind`). */
export type OverlayKind = 'clock' | 'label' | 'tally_border' | 'image' | 'subtitle';

/** All overlay kinds, for building selectors. */
export const OVERLAY_KINDS: readonly OverlayKind[] = [
  'clock',
  'label',
  'tally_border',
  'image',
  'subtitle',
];

/** A configured overlay layer. */
export interface OverlayView {
  /** Stable overlay id (also draggable onto a cell from the palette). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Overlay kind. */
  readonly kind: OverlayKind;
  /** Attachment target (`canvas` or a cell id). */
  readonly target: string;
  /** Stacking order over the program. */
  readonly z: number;
}
