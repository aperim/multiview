// Typed view-models for the resource surfaces the SPA shows alongside layouts.
//
// SCHEMA STATUS (read me)
// -----------------------
// The control plane will expose Sources, Outputs, and Overlays as first-class
// REST resources, but those operations are NOT in the generated OpenAPI schema
// yet (only `GET /api/v1/layouts` is). These interfaces are deliberately-marked
// view-models that mirror the documented `mosaic-config` shapes
// (crates/mosaic-config/src/schema.rs: `Source`, `Overlay`, output sinks). They
// are NOT fake `as` casts of an untyped body — they are honest placeholders the
// read views render until the API ships.
//
// TODO(api-schema): once `cargo xtask gen-openapi` emits Sources/Outputs/Overlays
// operations, derive these from `components['schemas'][…]` and replace the stub
// queries in `./queries.ts` with the typed client calls.

/** The transport kinds Mosaic can ingest (config `[[sources]]` `kind`). */
export type SourceKind =
  | 'rtsp'
  | 'hls'
  | 'srt'
  | 'rtmp'
  | 'ndi'
  | 'file'
  | 'test';

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
