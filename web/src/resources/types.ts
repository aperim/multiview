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

/** The transport kinds Multiview can ingest (config `[[sources]]` `kind`). */
export type SourceKind =
  | 'rtsp'
  | 'hls'
  | 'srt'
  | 'rtmp'
  | 'ndi'
  | 'file'
  | 'test';

/** All source kinds, for building selectors. */
export const SOURCE_KINDS: readonly SourceKind[] = [
  'rtsp',
  'hls',
  'srt',
  'rtmp',
  'ndi',
  'file',
  'test',
];

/** A managed ingest source. */
export interface SourceView {
  /** Stable source id (referenced by a cell's `input_id`). */
  readonly id: string;
  /** Operator label. */
  readonly name: string;
  /** Transport kind. */
  readonly kind: SourceKind;
  /** The configured URL/locator (redacted of credentials for display). */
  readonly url: string | undefined;
}

/** The output transport kinds (config `[[outputs]]`). */
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
  /** Whether the sink is currently enabled. */
  readonly enabled: boolean;
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
  /** Stacking order over the program. */
  readonly z: number;
}
