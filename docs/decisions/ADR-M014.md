# ADR-M014: SA-1+ vendor-caps deep probe — rich per-device/host capability telemetry (extends ADR-W030)

- **Status:** Proposed <!-- flips to Accepted when the #180-A code lands (repo #97 pattern) -->
- **Area:** Management (HAL / Control / Web)
- **Date:** 2026-07-11
- **Source:** task #180 (the SA-1+ deep-probe half deferred from #9/#176 by [ADR-W030](ADR-W030.md));
  [management-capability-matrix](../research/management-capability-matrix.md) §3.4; [ADR-M007](ADR-M007.md)
  (CapabilityReport gate); [ADR-0035](ADR-0035.md) (self-aware placement, SA-1+); read-only design pass
  (agent session, 2026-07-11); operator direction (design-first, rule 3)

## Context

[ADR-W030](ADR-W030.md) shipped the **honest default-build** capability surface —
`GET /api/v1/system/capabilities` returning `multiview_control::system::SystemCapabilities`
(codec backends, compositor class, effective licence, NDI attribution) — and **deliberately
omitted** the rich [§3.4](../research/management-capability-matrix.md) telemetry (per-device codec
profiles/levels, NVENC session budget, VRAM, engine topology, host cgroup/PSI). W030 named that
omission **task #180**, "blocked by this" (W030 §"the default-vs-SA-1+ boundary"), because it needs
feature-gated backend-crate code and **GPU-hardware validation** (rule 26). This ADR is the design
record for #180.

**The load-bearing reframe (verified read-only, 2026-07-11): #180 is mostly a *surfacing* task,
not a from-scratch probe.** The expensive, hardware-validated *live* half already exists and is
already trusted by the placement path:

- [ADR-0017](ADR-0017.md)'s `DeviceLoad` (`multiview-hal/src/load.rs`) carries VRAM used/total,
  per-engine encode/decode utilisation, and the NVENC concurrent-session count, read through the
  **runtime-loaded NVML poller** (`nvml-wrapper`, feature-gated `cuda`/`vaapi`/`qsv`) on a ~1 s
  **off-hot-path** tick (`multiview-cli/src/placement.rs`) and consumed by `select.rs` for
  affinity-first placement — but it is **never surfaced on the API**.

The genuinely-**new** work is therefore small and bounded: **(a)** the **static** L2 vendor caps
that `EnvProbe` explicitly defers (`multiview-hal/src/probe.rs`: the "conservative baseline
`DeviceCaps` the feature-gated backend crate later refines with true vendor caps queries") —
device model, driver version, per-codec profiles/levels/bit-depth/B-frame support, engine
topology; and **(b)** a **host** block (OS/arch/cores/RAM, cgroup limits, PSI/thermal-sensor
*presence*).

Binding constraints: invariant **#1** (output clock never blocked), invariant **#10** (the control
plane must not couple to / back-pressure the engine — and control keeps **zero** dependency on
`multiview-hal`, the #263 / W030 boundary), **rule 6** (no modelled-but-unfilled fields; a seam
with only mock impls is a scaffold), **rule 26** (vendor queries validate on real GPU hardware),
**rule 27** (no aspirational reporting), and the LGPL-clean licensing model
([AGENTS.md §G](../../AGENTS.md) / [ADR-0012](ADR-0012.md)).

## Decision

Extend the capability surface additively and assemble it, as W030 did, from what the running
binary actually knows — never a serialized phantom type.

### 1. The invariant-#10 static-vs-live boundary (the whole safety story)

The caps DTO carries **static / semi-static** fields **only**:

- **static:** device vendor/stable-id/PCI-bus-id, `vram_total`, engine topology (NVDEC/NVENC
  counts), device model, driver version, unified-memory flag, per-codec profiles/levels/bit-depth;
  host OS/arch/CPU-cores/`available_parallelism`/`total_ram`, cgroup `cpu.max` / `memory.max`,
  **PSI availability** (is `/proc/pressure` present) and **thermal-sensor presence**.
- **semi-static:** the NVENC **session-cap ceiling** (a moving, host-wide, driver-derived number —
  captured in the startup snapshot as the ceiling, never the live count).

The **live** gauges — VRAM *free*, NVENC sessions *used/available*, engine *utilisation %*, host
PSI *values*, thermal *readings* — **stay on the telemetry stream** (`Event::SystemMetrics`,
[ADR-0035](ADR-0035.md) / [ADR-RT004](ADR-RT004.md)) and are **excluded** from the caps DTO.

*Why this is non-negotiable:* folding a live gauge into `/system/capabilities` forces either
**per-request hardware probing** (a read that touches the engine/device — an invariant-#10
violation) or a **snapshot that silently goes stale** (a rule-27 lie). The endpoint therefore
stays exactly what W030 built: a **static startup snapshot** installed via
`AppState::with_capabilities`, clone-on-read, **route unchanged, no engine channel**.

### 2. Schema — additive-only to `multiview_control::system` (primitives/enums, zero hal dep)

Add to `SystemCapabilities` (all `#[serde(default, skip_serializing_if = …)]`, `#[non_exhaustive]`,
`#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]`):

- `devices: Vec<DeviceCapability>` — per detected accelerator.
- `host: Option<HostInfo>` — the machine block.
- `detection: DetectionInfo` — `{ l1_ffmpeg, l2_vendor, l3_probe }` booleans: **which probe layers
  actually ran**, so the client never renders a gauge for a layer that did not execute.

New DTOs (every telemetry field `Option`/`Vec`, **"unknown is first-class, never a fabricated
zero"**):

- `DeviceCapability { vendor, id, pci_bus_id?, model?, driver_version?, unified_memory?,
  vram_total_bytes?, engines: EngineTopology, nvenc_session_cap?, codecs: Vec<CodecCapability> }`
  (`codecs` is added at **#180-B**, see §4 — not declared before a probe fills it).
- `CodecCapability { codec, stage /* decode|encode */, max_width?, max_height?,
  profiles: Vec<String>, bit_depth?, bframes? }`.
- `HostInfo { os, arch, cpu_cores?, available_parallelism?, total_ram_bytes?,
  cgroup: CgroupLimits, psi_available: bool, thermal_sensors: Vec<String> }`.

Control keeps **primitive/enum fields only — no `multiview-hal` types** (the #263 / W030 boundary).
On the software/CI build `devices` is empty and `host` is filled, so the JSON is **byte-compatible**
with today's W030 snapshot (backward-compatible extension). Requires `openapi.rs` `ToSchema`
registration + `cargo xtask gen-openapi` (regenerates `docs/api/openapi.json`) + web schema
regeneration (`web/src/api/system.ts`).

### 3. HAL API — new, pure, feature-gated (`multiview-hal`)

- `caps.rs` — a serde-free `VendorCaps` value + a `VendorCapsProbe` **seam** (trait). Feature-gated
  impls query the real vendor SDKs: `cuda` → NVENC `nvEncGetEncodeCaps` (+ NVML
  `nvmlDeviceGetName` / `nvmlSystemGetDriverVersion`); `vaapi` → `vaQueryConfigProfiles` /
  `vaQueryConfigEntrypoints` / `vaGetConfigAttributes`; `qsv` → oneVPL `MFXQueryImplsDescription`;
  `videotoolbox` → `VTCopySupportedPropertyDictionary…` (coarse — macOS telemetry is limited,
  per M007). Every field `Option` (unknown-first-class). Results **correlate to the existing
  `DeviceLoad`/`DeviceId` by PCI-bus-id / stable-id — never the NVML index** ([ADR-M007](ADR-M007.md)).
- `host.rs` — a `HostInfo` probe over `std` (`os`/`arch`/`available_parallelism`) + `/proc` +
  `/sys` + cgroup v2 (`cpu.max`, `memory.max`) + `/proc/pressure` presence. **Pure, GPU-free,
  CI-testable.**
- `probe.rs` — the deferred L2 refinement point becomes the `VendorCapsProbe` call site (no
  behaviour change to the L1 presence baseline `EnvProbe` already returns).

The **CLI** (`multiview-cli/src/system_capabilities.rs`) remains the hal→DTO **bridge** (it already
owns the hal dependency): it assembles `devices[]` by correlating the deep probe with the existing
`DeviceLoad` snapshot, and installs the result via `AppState::with_capabilities` — the same static
one-shot as W030.

### 4. The #180-A / #180-B decomposition (rule 6 + rule 26)

- **#180-A — host block + surface the already-validated `DeviceLoad` statics** (`vram_total`,
  engine topology, vendor/id, and model/driver where the NVML poller exposes them) + `detection{}`.
  **GPU-free-completable:** the host reads are pure `std` / `/proc`; the device surfacing *re-uses
  the [ADR-0017](ADR-0017.md) probe already GPU-validated in production* and is exercised in CI
  against a `DeviceLoad` **fixture** — so #180-A needs **no new GPU validation** (rule 26 met by
  reuse). Ships as the **full fanned chain** hal → control DTO → cli bridge → web, **together**,
  post-#271 (rule 6: no partial merge).
- **#180-B — the deep L2 vendor-query caps** (`codecs[]`: per-codec profiles/levels/bit-depth/
  B-frames/max-res via NVENC / VAAPI / oneVPL / VideoToolbox). **Double-gated:** **rule 6** — a
  `VendorCapsProbe` seam carrying **only mock impls** is a scaffold (the #36 modelled-but-not-wired
  class), so #180-B must ship **with real vendor impls, never as a bare seam**; **rule 26** — those
  vendor queries have **never been exercised**, so they need **GPU-runner validation** (runners
  currently = 0). #180-B therefore **groups with #198 (HAL-1 FailureLedger IMPL, likewise
  GPU-runner-blocked)** and is held until GPU runners exist. The `codecs` field is **added** to
  `DeviceCapability` only when #180-B lands (additive, not declared-empty in #180-A — mirroring
  W030's discipline of never declaring a field before a probe fills it).

### 5. Territory + build order

| Lane | Files | Status |
| ---- | ----- | ------ |
| **LANE-GOV** | `docs/decisions/ADR-M014.md` (this) + README index | disjoint, **independently shippable now** (this PR) |
| **LANE-ENG** (`multiview-hal`) | `caps.rs`, `host.rs`, `probe.rs` | disjoint from #271; `host.rs` + the seam authorable now, #180-B real impls gated on GPU runners |
| **LANE-API** (`multiview-control`) | `system.rs` (DTO extension), `openapi.rs`, `docs/api/openapi.json` | **#271-locked** (frees when #271 merges) |
| **LANE-CLI** (`multiview-cli`) | `system_capabilities.rs` (bridge) | disjoint file, but compile-depends on the control DTO → sequences **after** LANE-API |
| **LANE-WEB** | `CapabilitiesPage` (DevicesPanel/HostPanel), `web/src/api/system.ts` | after API (schema regen) |

**rule 6:** #180-A lands as **one** fanned PR (hal → control → cli → web) post-#271; #180-B lands
as a **second** fanned PR once GPU runners exist. No partial merge in either.

### 6. TDD plan (red-first, rule 18)

- **hal:** mock `VendorCapsProbe` (present / absent / partial arms) — assert correlation-by-`DeviceId`
  and unknown-first-class (all-`None` → honest empty, no fabricated zero); `HostInfo` fixture tests
  (parse `/proc` + cgroup fixtures; missing files → `None`). Runs **GPU-free** → CI-green (rule 26).
- **control:** additive-DTO serde round-trip + **backward-compat** (empty `devices` + absent `host`
  serialize byte-identically to the W030 snapshot).
- **cli:** bridge correlation with a mock `DeviceLoad` + mock `VendorCaps` → assembled `devices[]`;
  assert no fabricated zero.
- **web:** DevicesPanel/HostPanel render + **empty-state** (no devices → honest "no accelerators
  detected", never an aspirational gauge); vitest.
- **route:** `GET` returns the snapshot under the viewer/read gate (no engine touch).

Each slice commits its failing test before implementation.

## Rationale

- **Surfacing beats re-probing.** The costly, hardware-validated live probe (ADR-0017 `DeviceLoad`
  via NVML) already exists and is already trusted by placement; #180 mostly plumbs it to the API and
  adds the static vendor L2 + host block. Cheaper, lower-risk, and — for the live half — already
  hardware-proven. One probe feeds both placement and the API (no second source of truth, per M007).
- **The static/live split is the only invariant-#10-safe design.** Static facts belong in a startup
  snapshot; live gauges already have a bounded drop-oldest telemetry stream. Mixing them re-introduces
  the exact coupling W030's snapshot design removed.
- **Additive-only + all-`Option` + `detection{}` = rule-6/27 honesty.** The client is told which
  layers ran; no field exists until a real probe fills it; an **empty-because-probed-absent**
  collection (honest) is distinct from an **empty-because-unbuilt** placeholder (forbidden).
- **The A/B split makes the GPU-runner gate real, not a whole-feature blocker.** #180-A delivers
  useful telemetry GPU-free today's-CI-green; #180-B waits for runners alongside #198.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Re-implement a fresh live probe for the API | Duplicates ADR-0017's validated `DeviceLoad`/NVML poller → two sources of truth (M007 forbids). |
| Fold the live gauges (VRAM free, sessions used, util, PSI values) into the caps DTO | Forces per-request hardware probing (inv #10) or a silently-stale snapshot (rule 27); live telemetry already rides the drop-oldest stream (ADR-0035/RT004). |
| Ship the `VendorCapsProbe` seam now with mock-only impls (defer real queries) | A seam with no real impl is a rule-6 scaffold (the #36 class); real vendor queries need GPU validation (rule 26). #180-B ships with impls or not at all. |
| Put the new DTOs / deep-probe types in `multiview-hal` with `Serialize` | Couples hal to serde/wire and control→hal; breaks the serde-free planner layer and the #263/W030 zero-hal-dep control boundary. The CLI owns the hal→DTO map. |
| Declare `codecs[]` / profile fields now, fill them in #180-B | Modelled-but-unfilled (rule 6); W030's discipline adds a field only when a probe fills it. `codecs[]` is additive at the #180-B land. |
| One monolithic #180 PR (A + B together) | B is GPU-runner-blocked (runners = 0); bundling parks A's GPU-free-shippable telemetry behind B indefinitely. |

## Consequences

- **New surface:** hal `caps.rs` (`VendorCaps`, `VendorCapsProbe`) + `host.rs` (`HostInfo`);
  additive `multiview_control::system` DTOs; OpenAPI + web schema regeneration. New schemas must be
  registered or codegen drifts (the W030 lesson).
- **Committed to maintaining the static/live boundary:** any future "add X to capabilities" must
  classify X static-vs-live and route a live X to telemetry, never the caps DTO.
- **Invariants #1/#10 preserved:** a static startup snapshot, no engine channel, control stays
  hal-free by construction (the CLI owns the map). The engine is untouched.
- **rule 26:** #180-A validates GPU-free (fixture/mock); #180-B carries a GPU-runner validation gate
  (grouped with #198). CI stays green on the default/software build (empty `devices`, `host` filled).
- **Licensing (LGPL-clean, deny-clean):** NVML via `nvml-wrapper` is **runtime-loaded** (no
  link-time escalation); VAAPI (`libva`, MIT), oneVPL (MIT), and VideoToolbox (Apple system
  framework) vendor queries are permissive/LGPL-clean; NVENC caps use the MIT `nv-codec-headers`
  already in the default build (ADR-0012). No `gpl-codecs`, no NDI. `cargo deny` stays clean.
- **Blocked by:** #271 (frees LANE-API for #180-A's control DTO) for #180-A; **GPU runners**
  (with #198) for #180-B. This ADR flips **Proposed → Accepted** when the #180-A code lands (repo
  #97 pattern).
