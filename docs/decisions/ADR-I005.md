# ADR-I005: One packet carrier — `multiview_ffmpeg::EncodedPacket` is the single production fan-out type; retire the `fanout::EncodedPacket` `Arc<[u8]>` carrier (RT-13)

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-10
- **Source brief:** [decoupled-routing.md](../research/decoupled-routing.md), [iso-program-recording.md](../research/iso-program-recording.md), [efficiency.md](../research/efficiency.md)
- **Realizes:** Invariant #7 (encode-once-mux-many), invariant #3 (re-stamp from the tick), invariants #1/#10 (the fan-out never stalls or back-pressures the engine); see [ADR-0026](ADR-0026.md), [ADR-E003](ADR-E003.md), [ADR-E004](ADR-E004.md), [ADR-0034](ADR-0034.md), [ADR-0037](ADR-0037.md)
- **Closes follow-up:** RT-13 (`EncodedPacket` convergence) tracked at `crates/multiview-output/src/fanout.rs` (~L38–L42), [iso-program-recording.md §7](../research/iso-program-recording.md) and the [decoupled-routing backlog](../development/decoupled-routing-backlog.md) RT-12/RT-13 output lane.

## Context

The codebase grew **two** distinct "one encoded packet" carriers, and inv #7
(encode the canvas once per rendition, fan the *same* coded packets to N
transports) was being documented twice, differently:

1. **`multiview_ffmpeg::EncodedPacket`** (`crates/multiview-ffmpeg/src/packet.rs`)
   — a `Send` wrapper around one libav `codec::packet::Packet` (an `AVPacket`),
   tagged with a `StreamKind` (`Video` / `Audio`, `#[non_exhaustive]`). It exposes
   `pts`/`dts`/`is_keyframe`/`len`, yields an **independently-writable owned copy**
   per consumer via `to_owned_packet()` (`av_packet_ref` + `av_packet_make_writable`)
   and a move-out `into_owned_packet()` for the last muxer. This is what the
   **production** path actually carries: `ProgramEncoder::encode_frame` /
   `encode_audio*` (`multiview-output/src/sink.rs`) emit `Vec<EncodedPacket>`, the
   CLI `consumer_main`/`fan_packets` (`multiview-cli/src/pipeline.rs` ~L2645–2775)
   clone one per sink across bounded `sync_channel`s, and each `PacketMuxSink`
   pulls them through `PacketSource::next_packet` → `Muxer::write_packet`
   (`set_stream` + `rescale_ts` **in place**, `mux.rs` ~L271–292).

2. **`fanout::EncodedPacket`** (`crates/multiview-output/src/fanout.rs`) — a
   pure-Rust struct `{ rendition, kind: PacketKind, pts, dts, duration, data: Arc<[u8]> }`
   handed out as one `Arc<EncodedPacket>` and fanned **by reference** through the
   `PacketRouter` / `EncodeOnceDriver` to a `PacketSink` trait. This is a
   **routing-model** type: it exists so the encode-once contract is *provable in
   `multiview-output` alone* (the call-count spy in `tests/encode_once_sink_mover.rs`),
   but as the decoupled-routing brief records, it is **dead/unwired in production**
   — its only consumer (`rtsp_server`) is skipped in `pipeline.rs`.

Having two carriers is a latent defect, not a convenience. Three gates are about
to be built that all fan packets and would each independently pick (or re-invent)
a carrier, diverging permanently if convergence is not pinned **first**:

- **AUD-4 dual-stream mux** — already routes `StreamKind::Audio` vs `Video` to
  the muxer's audio/video stream index (`stream_index_for`, `sink.rs`).
- **OUT-2 RTSP `PacketSink`** — the in-process RTSP server's egress.
- **PRV-5 program `PacketSink`** — the preview program tap.

`fanout::PacketKind` (`VideoKeyframe`/`VideoDelta`/`Audio`) and
`multiview_ffmpeg::StreamKind` (`Video`/`Audio`) are **two different taxonomies
for the same routing decision** — the keyframe bit is a *packet flag*
(`is_keyframe()`), not a stream identity, so folding it into the kind enum
conflates "which elementary stream" with "is this a segment boundary".

## Decision

**There is exactly one production packet carrier: `multiview_ffmpeg::EncodedPacket`
(the `AVPacket` wrapper).** All fan-out — the existing file/HLS/push sinks, and
the to-be-built OUT-2 RTSP sink and PRV-5 program sink — consumes that type. The
two notions are converged by **promoting the libav wrapper and retiring the
`Arc<[u8]>` carrier as a production type**, not by backing the wrapper with an
`Arc<[u8]>`.

Concretely:

1. **Carrier type.** `multiview_ffmpeg::EncodedPacket { packet: codec::packet::Packet, kind: StreamKind }`
   is *the* carrier. It is `Send` (the underlying `AVPacket` carries no thread
   affinity), movable across the fan-out's bounded channels, and `Clone` is a
   ref-counted `av_packet_ref` that **preserves the `kind` tag**.

2. **Routing tag.** `StreamKind` (`Video` / `Audio`, `#[non_exhaustive]`) is the
   one routing taxonomy. The fan-out uses it to pick the destination muxer
   `stream_index` (`stream_index_for`: video→video stream, audio→registered
   audio stream, else a typed error — never a silent mis-route). The
   **keyframe / segment-boundary** decision is read from the packet flag
   (`EncodedPacket::is_keyframe()`), *not* from the kind — so a segmenter rotates
   on `kind == Video && is_keyframe()` (`drive_packets_to_single_muxer`,
   `sink.rs`). `fanout::PacketKind` is removed; its `VideoKeyframe`/`VideoDelta`
   split is expressed as `StreamKind::Video` + the keyframe flag.

3. **Zero-copy / sharing for fan-out.** The honest answer to "is it one shared
   allocation?" is **no, and it must not be**: each muxer mutates its packet in
   place (`set_stream` + `rescale_ts` rewrite the timestamps into *that* stream's
   time-base). A single shared immutable buffer cannot serve N muxers that each
   rescale differently. So the carrier shares the **coded payload** by reference
   (the `AVBufferRef` behind the `AVPacket` is reference-counted: `Clone` /
   `to_owned_packet` bump a refcount and copy only the small packet header, not
   the bytes) while giving each muxer its **own writable packet struct**. That is
   genuinely zero-copy on the payload and per-sink-private on the mutable header —
   the correct shape for inv #7, and strictly better than the `Arc<[u8]>` model,
   which would have forced every muxer to re-wrap the bytes into a fresh `AVPacket`
   anyway before libav could write them.

4. **What "encode once" means, stated once.** Inv #7's load-bearing claim is
   **the encoder runs once per rendition per tick**; the per-sink hand-off is a
   refcounted header copy over a shared payload. Both the (now sole) production
   path and the routing-model proof honour this. The `fanout.rs` module doc's
   "Two `EncodedPacket` notions" section is deleted and replaced with the single
   reconciliation above.

5. **Routing-model role, retained but demoted.** `PacketRouter` /
   `EncodeOnceDriver` / `RenditionEncoder` / the `PacketSink` trait stay as the
   **in-crate, GPU-free encode-once *proof harness*** (the call-count test is
   load-bearing for inv #7 and runs on CI without libav). They are explicitly
   **not** a second production carrier. The `fanout::EncodedPacket` struct is
   either (a) removed in favour of the harness operating over a lightweight
   `PacketMeta { rendition, kind: StreamKind, pts, dts, duration, is_keyframe }`
   descriptor (no payload — the proof only needs the *count* and the routing
   decision), or (b) kept as a test-only type behind `#[cfg(test)]`. The runtime
   sink-mover (`PacketRouter::register`/`deregister`/`move_sink`) keeps operating
   on `Arc<dyn PacketSink>` route entries — a routing-table operation that is
   independent of which packet type flows — so RT-12's sink-mover and RT-13's
   convergence compose cleanly.

## Migration

Docs-only ADR; this pins the design the three gates build against. The mechanical
migration (separate, test-first PRs, no behaviour change to the production path
which **already** carries `multiview_ffmpeg::EncodedPacket`):

1. **Delete the divergence doc.** Replace `fanout.rs`'s "Two `EncodedPacket`
   notions" module-doc + the RT-13 follow-up note (L15–L42) with the single
   reconciliation (decision §4) and a link to this ADR.
2. **Collapse `PacketKind` → `StreamKind` + keyframe flag.** Re-express the
   routing-model harness over `StreamKind` (re-exported from `multiview-ffmpeg`,
   which `multiview-output` already depends on under the `ffmpeg` feature) and a
   `is_keyframe: bool`, removing `fanout::PacketKind`. Update the call-count test
   to assert the keyframe-boundary path off the flag, not the enum variant.
3. **Drop `fanout::EncodedPacket`'s `Arc<[u8]>` payload** for the proof harness
   (decision §5a) or gate it `#[cfg(test)]` (§5b) — pick §5a unless a non-test
   consumer materialises; either way it is no longer a production carrier.
4. **OUT-2 / PRV-5 build against the pinned carrier.** Both new sinks implement
   the established `PacketSource` → `Muxer` (or RTP packetiser) seam over
   `multiview_ffmpeg::EncodedPacket`, taking owned copies via `to_owned_packet()`
   and routing by `kind()`. No new carrier type is introduced by either gate.
5. **`cargo build --workspace` stays green** at every step (the production path is
   unchanged; only the unwired routing-model type is reshaped).

## Alternatives considered

- **Back `multiview_ffmpeg::EncodedPacket` with `fanout::EncodedPacket`'s
  `Arc<[u8]>`** (the RT-12 backlog's first option) — **rejected**. The muxer
  rescales timestamps **in place** per stream, so a shared immutable byte buffer
  cannot serve N muxers; each would have to copy the bytes into a fresh `AVPacket`
  before libav could write them, *adding* a full-payload copy per sink that the
  `AVBufferRef` refcount already avoids. It also pushes a libav concern (`AVPacket`
  construction) into `multiview-output`, which is deliberately
  `unsafe_code = forbid` and never names a raw libav packet type.
- **Keep both carriers, document the split honestly** (the RT-12 fallback, the
  current state) — **rejected as the end state**. It is exactly the divergence
  these three gates would entrench; "honest documentation of a follow-up" is the
  deferred-debt anti-pattern, and convergence is now unblocked.
- **Define a third, neutral carrier in `multiview-core`** (an `Arc`-wrapped coded
  buffer + `StreamKind` + timestamps, libav-agnostic) and adapt both ends —
  **rejected**: every production sink ultimately hands bytes to a libav muxer, so
  a neutral type buys nothing but an extra wrap/unwrap at the boundary and a third
  type to keep in sync. The `AVPacket` wrapper already *is* the neutral, `Send`,
  refcounted carrier the system needs.
- **Promote `fanout::PacketKind` (with the keyframe variant) as the tag** —
  **rejected**: it conflates "which elementary stream" (a stable routing
  identity) with "is this a segment boundary" (a per-packet flag), and would force
  every audio packet to be re-classified and every video packet to carry its GOP
  position in its *kind*. `StreamKind` + `is_keyframe()` keeps the two orthogonal.

## Consequences

- **One carrier, one taxonomy.** AUD-4, OUT-2, and PRV-5 all fan
  `multiview_ffmpeg::EncodedPacket` and route on `StreamKind`; no gate re-invents a
  carrier, so they cannot diverge.
- **Inv #7 is stated once, correctly.** "Encode once per rendition per tick; fan a
  refcounted header copy over a shared payload, one writable packet per muxer." The
  duplicate/contradictory `fanout.rs` framing is gone.
- **Payload is zero-copy across sinks; the mutable header is per-sink-private** —
  the only shape compatible with in-place per-stream `rescale_ts`, and cheaper than
  the `Arc<[u8]>` model it replaces.
- **`multiview-output` stays `unsafe_code = forbid`** and still never names a raw
  libav packet type — encoded packets flow `receive_packet` → carrier →
  `write_packet` with the libav type owned entirely inside `multiview-ffmpeg`.
- **The routing-model harness keeps proving inv #7 GPU-free on CI** (call-count
  spy), now without masquerading as a second production type.
- **RT-12 (sink-mover) and RT-13 (this convergence) compose**: the sink-mover is a
  routing-table re-key over `Arc<dyn PacketSink>` entries, independent of the
  packet type that flows; converging the carrier does not touch it. ADR-0037 (ISO
  recording, which reuses the RT-12 fan-out sink-mover and the GP-7 copy path) and
  ADR-0034's egress lane inherit the single carrier.
- **No behaviour change to the running pipeline** — production already carries
  this type; the migration reshapes only the unwired routing-model type and the
  module docs.
