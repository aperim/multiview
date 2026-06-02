# Changelog

All notable changes to **Mosaic** — an efficient, hardware-accelerated, Rust live video
mosaic generator — are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> **Status:** pre-release scaffold. Until the first tagged `0.1.0`, everything lives under
> [Unreleased] and the public API is considered unstable.

---

## [Unreleased]

The repository has been bootstrapped: the canonical conventions, the full design-research
corpus, the Architecture Decision Records, and the workspace skeleton are in place. No runtime
functionality ships yet — this is the documentation-and-scaffolding foundation that the
implementation will build against.

### Added

#### Repository scaffold
- Initial repository structure following the canonical [repository layout][layout]:
  `crates/`, `web/`, `docs/`, `examples/`, `deploy/`, `xtask/`, and `.github/workflows/`.
- Dual licensing under **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`).
- Toolchain pin via `rust-toolchain.toml` (Rust **edition 2021**, stable channel).
- Baseline tooling configs: `rustfmt.toml`, `.editorconfig` (with `clippy.toml` and
  `deny.toml` to follow as the workspace is wired up).

#### Architecture & conventions
- **[Canonical conventions][conventions]** — the single source of truth pinning project
  identity, repository layout, the crate map, the feature-flag taxonomy, the load-bearing
  technical invariants, the API/realtime conventions, the licensing model, and frontend +
  naming/style rules.

#### Research / design briefs
The verification-hardened deep briefs that back the implementation
(see the [research index][research]):

| Area | Brief |
|------|-------|
| Core Engine | [core-engine.md][b-core] |
| Resilience & A/V | [resilience-and-av.md][b-resilience] |
| Efficiency | [efficiency.md][b-efficiency] |
| Color | [color-management.md][b-color] |
| Streaming / Timing | [streaming-gotchas.md][b-streaming] |
| Preview | [preview-subsystem.md][b-preview] |
| Realtime API | [realtime-api.md][b-realtime] |
| Management | [management-capability-matrix.md][b-mgmt] |
| Web / API Stack | [web-api-stack.md][b-web] |

- Supporting reference material: a [bibliography][bib] and an
  [example-streams catalogue][streams].

#### Architecture Decision Records
- **89 ADRs** (status: *Proposed*) derived from the design briefs — see the
  [ADR index][adr]. Grouped by area:
  - Core Engine — `ADR-0001`…`ADR-0014`
  - Resilience & A/V — `ADR-R001`…`ADR-R009`
  - Efficiency — `ADR-E001`…`ADR-E009`
  - Color — `ADR-C001`…`ADR-C006`
  - Streaming / Timing — `ADR-T001`…`ADR-T008`
  - Preview — `ADR-P001`…`ADR-P005`
  - Realtime API — `ADR-RT001`…`ADR-RT006`
  - Management — `ADR-M001`…`ADR-M007`
  - Web / API Stack — `ADR-W001`…`ADR-W008`

  Load-bearing examples: the continuous-output guarantee ([ADR-R001][adr-r001]), the
  single internal timeline + fixed-cadence output clock ([ADR-T001][adr-t001]), the
  LGPL-clean default build ([ADR-0012][adr-0012]), and the axum web framework choice
  ([ADR-W001][adr-w001]).

#### Crate workspace skeleton
- Cargo workspace skeleton enumerating the canonical [crate map][cratemap]. All crates are
  prefixed `mosaic-` and live under `crates/`; hardware/FFI/GPU code sits behind
  **off-by-default** Cargo features so the default `cargo check` builds the pure-Rust,
  LGPL-clean trait/type layer:

  | Crate | Responsibility |
  |-------|----------------|
  | `mosaic-core` | Shared types & traits (`Frame`, `PixelFormat`, `ColorInfo`, `MediaTime`, stage traits). No FFI. |
  | `mosaic-hal` | Hardware capability detection, backend registry, negotiation + cost model/planner. |
  | `mosaic-ffmpeg` | Safe RAII wrappers over libav* (demux/decode/encode, hwframe lifecycle). |
  | `mosaic-compositor` | Custom GPU compositor (scale + place + color convert + linear-light blend + overlay). |
  | `mosaic-framestore` | Per-tile last-good-frame stores + tile state machine. |
  | `mosaic-audio` | Per-input audio decode/resample/mix/route + EBU R128 metering. |
  | `mosaic-overlay` | Overlay layers, text rendering, subtitle ingest/render. |
  | `mosaic-input` | Ingest sources, input pacer, jitter buffers, timestamp normalization, reconnect. |
  | `mosaic-output` | Output sinks/servers; encode-once-mux-many fan-out. |
  | `mosaic-engine` | Protected output core: output clock, compositor drive, supervisor, hot-reconfig. |
  | `mosaic-config` | Config & template schema, validation, config-as-code import/export. |
  | `mosaic-events` | Shared realtime event types + versioned envelope. |
  | `mosaic-control` | axum REST + WebSocket + SSE API, OpenAPI, auth, SQLite, embedded SPA. |
  | `mosaic-preview` | Preview taps, preview encoder pool, WHEP/MJPEG/snapshot endpoints. |
  | `mosaic-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). |
  | `mosaic-cli` | Binary **`mosaic`**: wires engine + control plane; run/validate subcommands. |
  | `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint). |

  **Dependency direction:** `core` ← everything; `engine` depends on the media crates;
  `control`/`preview` depend on `engine` + `events`; `cli` depends on all. No cycles.

### Notes

- **Platforms:** Linux (x86_64 + aarch64) and macOS (Apple Silicon + Intel). **No Windows.**
- **Default build is LGPL-clean and redistributable.** The `gpl-codecs` feature (x264/x265)
  makes the build GPL and is opt-in only; the proprietary `ndi` feature is off by default and
  runtime-loaded (never vendored). See the [licensing model][conventions].

---

## Versioning policy

Mosaic follows **[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html)**
(`MAJOR.MINOR.PATCH`):

- **MAJOR** — incompatible API changes (Rust public API across crates, and the
  REST/WebSocket surface under `/api/v1`).
- **MINOR** — backwards-compatible functionality.
- **PATCH** — backwards-compatible bug fixes.

Additional rules for this project:

- **Pre-1.0 (`0.y.z`):** the API is unstable; **minor** bumps may carry breaking changes,
  per the SemVer pre-release allowance. Pin exact versions if you depend on the crates.
- **Cargo feature flags are part of the contract.** Removing or repurposing a feature
  (e.g. `cuda`, `vaapi`, `ndi`, `gpl-codecs`, `webrtc`) is a breaking change.
- **REST/WebSocket API** versioning is independent of the crate versions and is carried in
  the `/api/v1` path and the realtime envelope version; bumping it is a breaking change.
- ADR status transitions (Proposed → Accepted/Superseded) are recorded in the ADRs
  themselves, not as version bumps.

---

[Unreleased]: https://github.com/aperim/mosaic/compare/HEAD

<!-- Architecture & docs -->
[conventions]: docs/architecture/conventions.md
[layout]: docs/architecture/conventions.md#2-repository-layout
[cratemap]: docs/architecture/conventions.md#3-canonical-crate-map
[research]: docs/research/README.md
[adr]: docs/decisions/README.md
[bib]: docs/reference/bibliography.md
[streams]: docs/reference/example-streams.md

<!-- Research briefs -->
[b-core]: docs/research/core-engine.md
[b-resilience]: docs/research/resilience-and-av.md
[b-efficiency]: docs/research/efficiency.md
[b-color]: docs/research/color-management.md
[b-streaming]: docs/research/streaming-gotchas.md
[b-preview]: docs/research/preview-subsystem.md
[b-realtime]: docs/research/realtime-api.md
[b-mgmt]: docs/research/management-capability-matrix.md
[b-web]: docs/research/web-api-stack.md

<!-- Selected ADRs -->
[adr-r001]: docs/decisions/ADR-R001.md
[adr-t001]: docs/decisions/ADR-T001.md
[adr-0012]: docs/decisions/ADR-0012.md
[adr-w001]: docs/decisions/ADR-W001.md
