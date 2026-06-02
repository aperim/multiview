# Mosaic codebase map (one screen)

A fast orientation for agents. Authoritative detail lives in
[`docs/architecture/conventions.md`](../architecture/conventions.md) (source of truth) and the
root [`CLAUDE.md`](../../CLAUDE.md). This page is the map, not the territory.

## Top-level layout

```
mosaic/
  CLAUDE.md            # repo-wide agent rules + 11 invariants + crate map (Claude reads this)
  AGENTS.md            # tool-agnostic agent contract (non-Claude agents)
  Cargo.toml           # workspace; 16 crates + xtask
  crates/<crate>/CLAUDE.md   # per-crate orientation, loads on demand
  crates/              # the 16 mosaic-* crates + xtask
  web/                 # React 19 + TS + Vite SPA (web/CLAUDE.md)
  xtask/               # dev automation (build-web, gen-openapi, packaging)
  examples/            # mosaic configs + layout templates
  docs/                # architecture, research briefs, ADRs, dev docs
  deploy/ .devcontainer/ .github/   # container + CI
  .mosaic-build/ target/            # git-ignored transient/build output (do NOT read or commit)
```

## Crate map + dependency direction

**`core` ← everything. No cycles.** Leaf crates depend on `core` (+ `hal`/`ffmpeg`/`events` as
needed); `engine` depends on the media crates; `control`/`preview` depend on `engine` + `events`;
`cli` depends on all.

```
          mosaic-core  (Frame, PixelFormat=NV12, ColorInfo, MediaTime, stage traits) -- no FFI
             ^
   +---------+----------------------------------------------+
   |         |              |            |          |        |
 mosaic-hal mosaic-ffmpeg  mosaic-       mosaic-    mosaic-  mosaic-
 (caps,     (libav RAII,   framestore   compositor  audio   overlay
  planner)   FFI owner)    (last-good)  (GPU color) (mix)   (libass)
   |         |              |            |          |        |
   +----+----+------+-------+------------+----+-----+--------+
        |           |                        |
   mosaic-input  mosaic-output          mosaic-engine  <-- PROTECTED OUTPUT CORE
   (pacer,       (RTSP/HLS/NDI/         (output clock, supervisor,
    jitter,       push, encode-          hot-reconfig, degradation loop)
    PTS norm)     once-mux-many)              ^
                                              |
                            mosaic-config / mosaic-events / mosaic-telemetry
                                              ^
                                +-------------+-------------+
                                |                           |
                          mosaic-control              mosaic-preview
                          (axum REST/WS/SSE,           (taps, WHEP/MJPEG,
                           OpenAPI, SQLite, SPA)        strictly isolated)
                                              ^
                                         mosaic-cli  (binary `mosaic`; wires it all)
```

| Crate | Touch it when… | Brief(s) / ADRs to read first |
|-------|----------------|-------------------------------|
| `mosaic-core` | Shared types/traits, clock, layout model, error taxonomy. No FFI. | core-engine §3–§5 |
| `mosaic-hal` | Capability detect, backend negotiation, cost model/planner. | core-engine §6, efficiency; ADR-0003/0004/E008 |
| `mosaic-ffmpeg` | Safe RAII over libav*, hwframe lifecycle, all raw FFI. | core-engine §7,§8.1,§12; ADR-0002/0004 |
| `mosaic-compositor` | GPU compositor: color convert, linear-light blend, overlays. | color-management, core-engine §8.2,§13; ADR-C001..C006, E002 |
| `mosaic-framestore` | Lock-free last-good-frame stores + tile state machine. | resilience-and-av, streaming-gotchas §1,§7; ADR-T002 |
| `mosaic-audio` | Decode/resample/mix/route + EBU R128. | resilience-and-av, streaming-gotchas §5,§7; ADR-R005/R006/T006 |
| `mosaic-overlay` | Overlay layers, text, subtitles (libass). | resilience-and-av; ADR-R007/R008 |
| `mosaic-input` | Ingest, **input pacer**, jitter, PTS normalization, reconnect. | streaming-gotchas §1–§3,§5–§7, core-engine §9.1; ADR-T003/T004/T006/T007/T008 |
| `mosaic-output` | RTSP/HLS·LL-HLS/NDI/push; encode-once-mux-many. | streaming-gotchas §4, core-engine §9.2; ADR-0006/0007/T005 |
| `mosaic-engine` | **Protected core**: output clock, supervisor, hot-reconfig, degradation. | core-engine §4–§12, resilience-and-av, streaming-gotchas §0; ADR-T001/R001/R004 |
| `mosaic-config` | Config/template schema, validation, config-as-code. | core-engine §13,§14, management-capability-matrix; ADR-0010 |
| `mosaic-events` | Realtime event types + versioned envelope. | realtime-api; ADR-RT002/RT003 |
| `mosaic-control` | axum REST/WS/SSE, OpenAPI, auth, SQLite, command bus, embedded SPA. | web-api-stack, realtime-api, management-capability-matrix; ADR-RT001..RT006, W001..W008 |
| `mosaic-preview` | Preview taps, WHEP/MJPEG, cue/pre-warm. Strictly isolated. | preview-subsystem; ADR-P001..P005 |
| `mosaic-telemetry` | `tracing` + Prometheus + health (`/livez`,`/readyz`). | core-engine §15, resilience-and-av; ADR-R009 |
| `mosaic-cli` | Binary `mosaic`: wires engine + control, config load, run/validate. | core-engine; conventions §7 (licensing) |
| `web/` | React 19 SPA: shadcn/ui, TanStack, react-konva, dnd-kit, OpenAPI client. | web-api-stack, conventions §8; ADR-W001..W008 |

## Docs tree

```
docs/
  architecture/conventions.md   # SOURCE OF TRUTH (names, flags, invariants, licensing)
  architecture/                 # overview, pipeline, color, timing-and-sync, resilience, hardware
  research/                     # 11 verification-hardened design briefs (the "why")
  decisions/                    # 89 ADRs, grouped by prefix (below)
  development/                  # this map, working-in-this-monorepo, completeness checklist
  reference/  io/  media/  templates/  operations/  glossary.md  roadmap.md
```

**ADR prefixes:** `0001+` core engine · `R*` resilience/AV · `E*` efficiency · `C*` color ·
`T*` streaming/timing · `P*` preview · `RT*` realtime API · `M*` management · `W*` web/API stack ·
`DC*` devcontainer. Indexes: [`research/README.md`](../research/README.md),
[`decisions/README.md`](../decisions/README.md).

## The 11 invariants (one line; full text in conventions.md §5, root CLAUDE.md §2)

1 output-clock · 2 last-good-frame + state machine · 3 unified timing (never float fps) ·
4 HLS pacing to wall-clock · 5 NV12-throughout · 6 decode-at-display-resolution ·
7 encode-once-mux-many · 8 fixed color pipeline order · 9 resource-adaptive degradation ·
10 isolation (control/preview never back-pressure the engine) · 11 live-apply classification.

**#1 and #10 are the heart of the product. A change that risks either: stop, write a design note.**

How to work here without exhausting context:
[`working-in-this-monorepo.md`](working-in-this-monorepo.md).
