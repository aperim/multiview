# Architecture Decision Records

These ADRs capture the load-bearing decisions for the Multiview engine. 99 ADRs total. Most are **Proposed** — derived from the design briefs in [../research](../research/). The [Implementation Build-out](#implementation-build-out) series (`ADR-I*`) records decisions **Accepted** during the foundation build-out (the as-built state, which may deliberately and temporarily diverge from a Proposed ADR or from [conventions](../architecture/conventions.md) with a tracked follow-up).

## Core Engine

- [ADR-0001](ADR-0001.md) — Hybrid media engine: FFmpeg/libav for demux/decode/encode + custom Rust for compositing and serving
- [ADR-0002](ADR-0002.md) — rsmpeg as the primary libav binding
- [ADR-0003](ADR-0003.md) — Per-stage HAL with runtime auto-negotiation and zero-copy-island preference
- [ADR-0004](ADR-0004.md) — Zero-copy islands per vendor; explicit copy at every cross-vendor/NDI/CPU boundary
- [ADR-0005](ADR-0005.md) — Custom GPU-native compositor (CUDA / Metal / Vulkan·wgpu·libplacebo), not FFmpeg filters
- [ADR-0006](ADR-0006.md) — RTSP serving via in-process gst-rtsp-server, with MediaMTX as optional sidecar
- [ADR-0007](ADR-0007.md) — CMAF-first HLS with custom-built Apple LL-HLS
- [ADR-0008](ADR-0008.md) — NDI first-class but feature-gated, dynamically loaded, with attribution
- [ADR-0009](ADR-0009.md) — Hybrid concurrency: dedicated-thread data plane + Tokio control/IO plane
- [ADR-0010](ADR-0010.md) — Declarative layered config: TOML/JSON + serde adjacently-tagged enums + JSON Schema
- [ADR-0011](ADR-0011.md) — Cross-platform targets: Linux containers (NVIDIA + VAAPI) and macOS native universal2
- [ADR-0012](ADR-0012.md) — LGPL-clean default build; GPL/nonfree/NDI strictly opt-in; project dual MIT OR Apache-2.0
- [ADR-0013](ADR-0013.md) — Deadline-driven compositor with per-tile FrameSync and continuous drift correction
- [ADR-0014](ADR-0014.md) — Encode the multiview once per output; size density to physical NVENC chips
- [ADR-0015](ADR-0015.md) — YouTube live ingest via an external runtime-discovered resolver (yt-dlp) *(Proposed)*
- [ADR-0016](ADR-0016.md) — Efficient overlay rendering: GPU glyph-atlas text + libass subtitles + cached/dirty-region compositing *(Proposed)*
- [ADR-0019](ADR-0019.md) — Native multi-form caption ingest: unified cue model decoded in-demux into a per-tile sampled cue store *(Proposed)*
- [ADR-0024](ADR-0024.md) — Native caption master-fetch over libav avio (no `curl`, no HTTP dep) + bounded retry; fetcher seam for offline tests *(Accepted)*

## Resilience & A/V

- [ADR-R001](ADR-R001.md) — Continuous-output guarantee via inverted control flow, output clock, and per-tile last-good-frame stores
- [ADR-R002](ADR-R002.md) — Three-tier fault isolation: process-isolate FFI ingest and encoder; protect the output core
- [ADR-R003](ADR-R003.md) — Supervision, backoff, circuit breakers, watchdogs, and bounded memory
- [ADR-R004](ADR-R004.md) — Pin output session params; seamless edits via atomic scene-graph swap; Class-2 changes via parallel-output migration
- [ADR-R005](ADR-R005.md) — Discrete per-input audio routing + program bus, with a verified per-output capability matrix and explicit degradation
- [ADR-R006](ADR-R006.md) — In-process EBU R128 metering, read-only and non-blocking, two cadences, true-peak gated
- [ADR-R007](ADR-R007.md) — Subtitle ingest -> libass burn-in (off hot path) + format-aware discrete passthrough
- [ADR-R008](ADR-R008.md) — Overlay rendering: serializable layer stack, glyphon/Vello+SDF, premultiplied alpha, dirty-region uploads, input-decoupled
- [ADR-R009](ADR-R009.md) — Resilience testing: always-on output-validity probe as SLO arbiter, layered chaos, soak/fuzz, GPU-less CI

## Efficiency

- [ADR-E001](ADR-E001.md) — Decode each tile at (or near) its display resolution, per-backend negotiated
- [ADR-E002](ADR-E002.md) — NV12-throughout pixel-format policy; YUV->RGB only in-shader
- [ADR-E003](ADR-E003.md) — Composite once, encode the canvas once per rendition
- [ADR-E004](ADR-E004.md) — Encode-once, mux-many fan-out (tee semantics); ladder is a separate cost
- [ADR-E005](ADR-E005.md) — Reference-counted frame pool with bounded, drop-oldest working set
- [ADR-E006](ADR-E006.md) — Adaptive dirty-region recompositing & frame-rate harmonization (opt-in)
- [ADR-E007](ADR-E007.md) — Resource-adaptive degradation & admission control without breaking output
- [ADR-E008](ADR-E008.md) — Cost-model-driven planner with a capability+cost registry
- [ADR-E009](ADR-E009.md) — Per-tier efficiency budgets enforced by perf-regression CI
- [ADR-0017](ADR-0017.md) — GPU/CPU utilisation monitoring + affinity-aware least-loaded multi-GPU work placement *(Proposed)*
- [ADR-0018](ADR-0018.md) — Adaptive affinity-first GPU work-placement with deliberate split + closed-loop re-placement *(Proposed)*
- [ADR-0022](ADR-0022.md) — Real-time software compositor: parallelize + LUT the per-pixel colour pipeline; content-aware smoothness gate *(Accepted)*
- [ADR-0023](ADR-0023.md) — Region-limited overlay bake: per-primitive dirty rects + exact passthrough, end the full-canvas colour round-trip (inv #5) *(Accepted)*
- [ADR-0025](ADR-0025.md) — Streaming encode: bound memory + continuous output on run-until (off-hot-path bake consumer + bounded queue, offline-block/live-drop) *(Accepted)*
- [ADR-0026](ADR-0026.md) — Encode-once-mux-many: hoist the single encoder into the bake consumer, fan encoded packets to mux-only sinks (folds in bounded teardown) *(Proposed)*
- [ADR-0027](ADR-0027.md) — Synthetic sources are first-class: bars/solid/clock as in-process `SourceKind`s through the one uniform ingest path (`test` kept as a `bars` alias) *(Proposed)*
- [ADR-0028](ADR-0028.md) — Native NDI FFI binding data design: resolve-once flat `NdiV6` fn-pointer table, RAII safe handles, all `unsafe` confined to `multiview-ndi-sys` *(Accepted)*
- [ADR-0029](ADR-0029.md) — ACME/TLS for the control plane: DNS-01 only, rustls (no openssl), pluggable `DnsProvider` trait with Cloudflare first, fail-soft renewal off the engine path (inv #10) *(Proposed)*
- [ADR-0030](ADR-0030.md) — Multiple active programs: a `Program` actor (multiview/passthrough/transcode) under a `ProgramSet` supervisor with per-program output clocks + shared time source, decode-once-use-many source registry, admission control + per-program degradation *(Proposed)*
- [ADR-0031](ADR-0031.md) — Build our own pinned FFmpeg (reject jellyfin/PPA): LGPL-clean `--disable-everything` allowlist + separated GPL variant, reproducible multi-arch builder, FFmpeg 7.1.4 first (binding pin) then gated 8.1.1; shrink FFmpeg to codec+hwaccel only *(Proposed)*
- [ADR-0032](ADR-0032.md) — HLS/LL-HLS delivery: tier the serving boundary (static-frontable master/segments/init for any CDN vs Multiview's own async blocking-reload origin for the live LL-HLS playlist + byte-range parts); CMAF/fMP4 default container; rolling-playlist + DVR + atomic-publish + deferred-unlink-pruning foundation; configurable locations/`base_url`; Cache-Control/CORS header contract + reference nginx/traefik config; HLS-0..14 backlog *(Proposed)*
- [ADR-0033](ADR-0033.md) — AES67 / ST 2110-30 audio I/O (open-interop-first, no proprietary SDK): L16/L24 PCM-over-RTP send+receive; PTP/ST 2059-2 media-clock as a REFERENCE not a pacer (slave the audio sample clock, keep the video output clock free-running, inv #1) + boundary resampler; SDP+SAP discovery, NMOS IS-04/05 optional; receive reuses the existing st2110 depacketizer; bounded fail-honest on loss; AES67-0..N backlog *(Proposed)*
- [ADR-0034](ADR-0034.md) — Per-stream decoupled routing + instant crosspoint switching: inputs/layouts/outputs as independent resources wired by per-stream crosspoints (video→cell, audio-track→bus w/ BREAKAWAY, subtitle→layer, SCTE-35/timecode→output) over the ADR-0030 ProgramSet+SourceRegistry; StreamInventory discovery; instant warm re-point (Class-1) reusing GP-6/GP-7 restamp+splice + ADR-R004 make-before-break (Class-2); honest #11 classifier inspecting destination pinned params (incl. Reset-lite tier); atomic mixed-stream salvo; RT-0..17 backlog *(Proposed)*
- [ADR-0035](ADR-0035.md) — Self-aware placement + capability detection + actionable health warnings: probe usable backends (wgpu adapter / NVDEC / NVENC) + cross-check vs NVML hardware → detect+warn the silent CPU-fallback (the RTX-4060-idle-while-5-cores-burn bug); wire the already-built cost-model/select_device/PlacementController/degradation engine into the run (sense→plan→apply) net of co-tenant load, affinity-preserving (never fragment a pipeline); SA-0..N backlog, SA-0 = detect+warn the compositor mismatch *(Proposed)*
- [ADR-0036](ADR-0036.md) — Validated codec catalog: typed codecs (building on the existing multiview-ffmpeg VideoCodec/AudioCodec enums) constrained by a TRANSPORT×codec matrix AND a CODEC×ACCELERATION matrix; explicit per-host class CPU-only/GPU-only/BOTH/dedicated(future) from the ADR-0035 throwaway-open probe (e.g. AV1 NVENC needs Ada+; H264 CPU encode needs gpl-codecs); config codec becomes a constrained enum validated against transport∩hardware (no silent mpeg2 fallback); /capabilities/codecs API + UI dropdown w/ accel+license badges; CODEC-0..N backlog *(Proposed)*
- [ADR-0037](ADR-0037.md) — ISO (isolated per-source) + Program (composited-output) recording: faithful all-stream copy-remux tapping Demuxer::read_packet before video-selection; segmented + time/size retention (reuse the HLS rolling-window + atomic publish); disk-pressure threshold disarm; bulletproof bounded drop-oldest + capped-backoff write path that can NEVER back-pressure/stall the engine (inv #1/#10); preserves original source timestamps (archive as-is); reuses GP-7 copy path, RT-12 fan-out sink-mover, GP-6 restamp, ADR-0035 health warnings; REC-0..N backlog *(Proposed)*
- [ADR-0038](ADR-0038.md) — Per-source wall-clock trust + multi-source time-of-day sync: detect a source wall-clock + classify Trusted/Suspected/None (reuse the PTP/NTP lock-state machine), operator Use/Discard, reclock-to-house on discard (the as-built normalize.rs anchor); extract RTCP-SR/HLS-PDT/TEMI/SMPTE-timecode-SEI; per-program ENCODED-pre-decode alignment buffer (decoder runs D-behind, framestore stays bounded — ~6MB vs ~3.1GB/4K-source) with MAX-window→OFFLINE (never stall); output cadence house-locked but the wall-clock LABEL = the synced instant T=now−D; reclocked sources house-align by arrival only (never content-sync); SYNC-0..N backlog *(Proposed)*

## Color

- [ADR-C001](ADR-C001.md) — Canvas working & output color space defaults to SDR BT.709 limited, with opt-in HDR canvas
- [ADR-C002](ADR-C002.md) — Untagged-input default policy: resolution heuristic for matrix/primaries, format class for range, never auto-HDR
- [ADR-C003](ADR-C003.md) — Composite in linear light with premultiplied alpha
- [ADR-C004](ADR-C004.md) — Range handled explicitly in-shader exactly once; expand on input, compress on output
- [ADR-C005](ADR-C005.md) — HDR->SDR tone-mapping default: per-tile BT.2390 EETF anchored at 203-nit reference white
- [ADR-C006](ADR-C006.md) — Always explicitly tag output across encoder + container/protocol, then verify with ffprobe

## Streaming/Timing

- [ADR-T001](ADR-T001.md) — Single internal monotonic timeline + fixed-cadence output clock drives the compositor
- [ADR-T002](ADR-T002.md) — Per-tile frame resampling = hold-last-good + duplicate/drop (sample on output tick)
- [ADR-T003](ADR-T003.md) — Per-input timestamp normalization: unwrap, genpts fallback, monotonic guard, discontinuity re-anchor
- [ADR-T004](ADR-T004.md) — HLS ingest pacing: custom PTS-to-wall-clock pacer, not -re
- [ADR-T005](ADR-T005.md) — HLS/LL-HLS output: wall-clock-paced, GOP-aligned segments; custom origin for true LL-HLS
- [ADR-T006](ADR-T006.md) — Long-run clock drift: monotonic master + PI/dead-band loop + adaptive audio resampling
- [ADR-T007](ADR-T007.md) — Codec edge-case & decode/encode policy: one bad input never stalls the multiview
- [ADR-T008](ADR-T008.md) — A/V sync & per-input jitter-buffer model
- [ADR-T009](ADR-T009.md) — Per-tile media-time ring uses O(capacity) copy-on-write publish, not an in-place O(1) ring
- [ADR-0020](ADR-0020.md) — Layered timing: monotonic pacing + optional reference-lock + per-input frame-sync *(Proposed)*
- [ADR-0021](ADR-0021.md) — Input timing & frame-sync: best-effort PTS normalisation + wall-clock pacer + sample-at-tick *(Proposed)*

## Preview

- [ADR-P001](ADR-P001.md) — Preview isolation model: read-only taps, drop-oldest, Tier A, shed-first
- [ADR-P002](ADR-P002.md) — Default per-scope transport: cheap JPEG grids, on-demand WHEP focus, LL-HLS fallback
- [ADR-P003](ADR-P003.md) — On-demand activation + auto-stop lifecycle (cost ~zero when idle)
- [ADR-P004](ADR-P004.md) — Off-air cue mechanism = the pre-warm worker (one machinery for look + take)
- [ADR-P005](ADR-P005.md) — Output preview = tap the REAL encoded bitstream; label real-vs-approx always

## Realtime API

- [ADR-RT001](ADR-RT001.md) — WebSocket primary, SSE one-way fallback, REST for commands
- [ADR-RT002](ADR-RT002.md) — Single versioned message envelope with discriminated payloads
- [ADR-RT003](ADR-RT003.md) — Snapshot-then-delta with per-connection seq resume and re-snapshot fallback
- [ADR-RT004](ADR-RT004.md) — Structural backpressure isolation with per-topic conflation and meter sampling
- [ADR-RT005](ADR-RT005.md) — WS auth via short-lived one-time ticket (default), with subprotocol-token and same-origin-cookie alternatives
- [ADR-RT006](ADR-RT006.md) — Document the event API with AsyncAPI 3.0 from shared types, served beside Scalar, with codegen'd typed clients

## Management

- [ADR-M001](ADR-M001.md) — Unified REST+WS resource model and /api/v1 naming with explicit ownership boundaries
- [ADR-M002](ADR-M002.md) — EncodeProfile + transcode model: composite-once, scale-per-output, capability-gated backends, pinned vs hot params
- [ADR-M003](ADR-M003.md) — Color-control model: four-axis per-source override + single canvas working space + output CICP tagging with verify gate
- [ADR-M004](ADR-M004.md) — Audio track-mapping model: Source owns attributes, Output owns the cross-product mapping, capability-aware projection
- [ADR-M005](ADR-M005.md) — Live-apply vs needs-reset semantics: Class-1/reset-lite/Class-2 + listener-restart, surfaced via dry-run plan
- [ADR-M006](ADR-M006.md) — Config-as-code import/export with versioning, rollback, and reference-only secrets
- [ADR-M007](ADR-M007.md) — CapabilityReport as the single machine-readable gate for UI and validator

## Web/API Stack

- [ADR-W001](ADR-W001.md) — Rust web/API framework: axum 0.8.x
- [ADR-W002](ADR-W002.md) — OpenAPI 3.1 tooling + interactive try-it-out: utoipa + utoipa-axum + Scalar
- [ADR-W003](ADR-W003.md) — Frontend stack: React 19 + TS + Vite + shadcn/ui + TanStack Query
- [ADR-W004](ADR-W004.md) — Layout-editor library: react-konva (canvas) + dnd-kit
- [ADR-W005](ADR-W005.md) — API auth model: dual-credential (cookie sessions + API keys) with RBAC
- [ADR-W006](ADR-W006.md) — Config persistence: SQLite via sqlx + config-as-code
- [ADR-W007](ADR-W007.md) — SPA build/serve: embed in the single binary (rust-embed / axum-embed)
- [ADR-W008](ADR-W008.md) — Engine-command bus: actor + lock-free desired-state hand-off
- [ADR-W009](ADR-W009.md) — Target WCAG 2.2 AA across the management web app
- [ADR-W010](ADR-W010.md) — Canvas layout editor: accessible-equivalent non-canvas editing path
- [ADR-W011](ADR-W011.md) — Realtime status/tally/alarms: no color alone + disciplined aria-live
- [ADR-W012](ADR-W012.md) — i18n: Lingui v5 + ECMAScript Intl, client-localized errors
- [ADR-W013](ADR-W013.md) — Serve the management plane from `multiview run`: control↔engine integration (3 isolation-safe paths, Class-1/2)
- [ADR-W014](ADR-W014.md) — Control-plane access: bootstrap admin token from `MULTIVIEW_CONTROL_TOKEN` (else generated + logged once)

## Dev Container

- [ADR-DC001](ADR-DC001.md) — GPU passthrough via hostRequirements.gpu "optional" (no hardcoded --gpus / --device)
- [ADR-DC002](ADR-DC002.md) — Seed a gitignored repo-root .env from ~/.onepassword_token via host initializeCommand; inject at runtime with --env-file
- [ADR-DC003](ADR-DC003.md) — Base on mcr.microsoft.com/devcontainers/rust:1-trixie + thin Dockerfile + official Features
- [ADR-DC004](ADR-DC004.md) — Install the 1Password CLI (op) in the image; authenticate via service-account token


## Engineering Guardrails

- [ADR-G001](ADR-G001.md) — Absolute typing enforced via centralized workspace lints + TS strictTypeChecked, blocking in CI
- [ADR-G002](ADR-G002.md) — TDD-first with a mutation-testing gate and protected tests (anti-reward-hacking)
- [ADR-G003](ADR-G003.md) — Mandatory adversarial cross-vendor review in a fresh context; human is the final approver
- [ADR-G004](ADR-G004.md) — Scope discipline, no-silent-suppression, secrets, and supply-chain guardrails for agents

## Broadcast Multiviewer

- [ADR-MV001](ADR-MV001.md) — Add a content-aware monitoring/alarm engine with X.733 severity and northbound notification
- [ADR-MV002](ADR-MV002.md) — Implement TSL UMD (v3.1/4.0/5.0) ingest/egress with external tally-bus integration and arbitration
- [ADR-MV003](ADR-MV003.md) — Add loudness logging and multi-standard audio metering for compliance
- [ADR-MV004](ADR-MV004.md) — Introduce a multi-head output model and salvo/scheduled layout automation
- [ADR-MV005](ADR-MV005.md) — Adopt NMOS (IS-04/05/07/08, IS-10, IS-12) and router-control bridges (Ember+, SW-P-08) for IP-facility integration

## Implementation Build-out

Decisions **Accepted** during the foundation build-out — the as-built state of the engine, compositor, control plane, and broadcast-feature placement (some deliberately and temporarily diverge from a Proposed ADR or from conventions, with a tracked follow-up noted in the ADR).

- [ADR-I001](ADR-I001.md) — Engine isolation primitives: `arc_swap::ArcSwapOption` (wait-free latest-state) + `tokio::sync::broadcast` (drop-oldest events), replacing a hand-rolled Mutex ring (realizes invariant #10)
- [ADR-I002](ADR-I002.md) — GPU compositor: wgpu behind an off-by-default `wgpu` feature; WGSL shaders are naga-validated GPU-free and SSIM≥0.98/PSNR≥40 dB-gated at runtime (follow-up: flip `wgpu` to default per conventions §3)
- [ADR-I003](ADR-I003.md) — Control persistence: SQLite/sqlx behind an off-by-default `sqlite` feature; in-memory trait `Repository` is the tested default; scoped cargo-deny ignore of RUSTSEC-2024-0436
- [ADR-I004](ADR-I004.md) — Broadcast multiviewer (M10–M12) feature placement: modules inside the existing 16 crates (no new crates), native/hardware behind off-by-default features

## Accessibility & Internationalization

- [ADR-W009](ADR-W009.md) — Target WCAG 2.2 AA across the management web app
- [ADR-W010](ADR-W010.md) — Canvas layout editor — accessible-equivalent non-canvas editing path
- [ADR-W011](ADR-W011.md) — Realtime status/tally/alarms — no color alone + disciplined aria-live
- [ADR-W012](ADR-W012.md) — i18n — Lingui v5 + ECMAScript Intl, client-localized errors
- [ADR-W013](ADR-W013.md) — Serve the management plane from `multiview run` — control↔engine integration (3 isolation-safe paths, Class-1/2)
