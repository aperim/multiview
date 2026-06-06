# Multiview — Work Schedule & Fanout Plan

Every remaining incomplete / stubbed / future item, planned to completion with acceptance
criteria, then sequenced for a **dependency‑ and conflict‑aware parallel fanout**. Generated
2026‑06‑05 from an 8‑stream architecture pass (`docs/decisions/` + the live scaffold).

**How to mark off:** flip the box in **Part 2** (`[ ]` → `[x]`) and set the item's `Status:` in
Part 3 when its *Acceptance* criteria are met (tests written + green, invariants re‑asserted).
48 items across 8 work‑streams: **AUD** audio · **OUT** output servers · **IN** inputs ·
**CTL** control→engine · **PRV** preview/WebRTC · **ENG** engine timing/resilience ·
**GPU** compositor/efficiency/hardware · **SUR** captions/NMOS/web‑codegen.

---

## Part 1 — Execution plan (dependencies → waves → lanes)

### 1a. Logical dependency waves (topological by `deps:` — "cannot *start* until")

A later wave's items depend only on earlier waves. This is the *logical* order; the *parallel*
order is constrained further by file conflicts (1b).

| Wave | Items (can start once the prior wave's deps land) |
|---|---|
| **0** (19) | AUD‑1, CTL‑1, CTL‑3, ENG‑1, ENG‑2, ENG‑4, ENG‑6, GPU‑1, GPU‑2, GPU‑3, GPU‑5, IN‑1, IN‑4, OUT‑1, OUT‑3, PRV‑1, SUR‑1, SUR‑3, SUR‑4 |
| **1** (14) | AUD‑2, CTL‑2, CTL‑4, CTL‑5, GPU‑4, GPU‑6, IN‑2, IN‑5, IN‑6, OUT‑2, OUT‑4, PRV‑2, SUR‑2, SUR‑5 |
| **2** (4) | AUD‑3, CTL‑6, IN‑3, PRV‑3 |
| **3** (6) | AUD‑4, AUD‑5, AUD‑6, IN‑7, PRV‑4, PRV‑5 |
| **4** (1) | AUD‑7 |
| **5** (1) | AUD‑8 |
| **6** (3) | ENG‑3, ENG‑5, SUR‑6 *(deferred polish; ENG‑3 needs ENG‑5's syscall seam)* |

**Critical path (longest logical chain):** `AUD‑1 → AUD‑2 → AUD‑3 → AUD‑4 → AUD‑7 → AUD‑8`
(M→L→L→XL→M→L). The audio‑output spine is the single longest dependency chain and gates `tone`
(AUD‑5) and the audio UI (AUD‑8).

### 1b. File‑conflict map (mutual exclusion — "cannot run *concurrently* in worktrees")

Two items that edit the same file will collide if run as parallel worktree agents. The hotspots:

| Touched by | File | Items |
|---|---|---|
| **9** | `crates/multiview-cli/src/pipeline.rs` | CTL‑6, ENG‑1, ENG‑2, GPU‑1, IN‑3, IN‑5, OUT‑1, OUT‑2, OUT‑4 |
| 4 | `crates/multiview-config/src/schema.rs` | AUD‑7, IN‑4, OUT‑1, OUT‑3 |
| 4 | `crates/multiview-cli/src/control.rs` | CTL‑1, CTL‑2, CTL‑3, CTL‑6 |
| 3 | `crates/multiview-control/src/openapi.rs` | CTL‑4, SUR‑4, SUR‑6 |
| 3 | `crates/multiview-control/src/routes/mod.rs` | CTL‑4, PRV‑2, SUR‑4 |
| 2 | `multiview-output/src/sink.rs` | AUD‑4, OUT‑1 |
| 2 | `multiview-events/src/event.rs` | CTL‑1, CTL‑6 |
| 2 | `cli/src/run.rs` | CTL‑1, GPU‑2 |
| 2 | `control/src/state.rs` | CTL‑3, PRV‑2 |
| 2 | `control/routes/sources.rs` | CTL‑2, SUR‑4 |
| 2 | `cli/.../command.rs` | CTL‑2, CTL‑6 |
| 2 | `multiview-hal/src/load.rs` | ENG‑4, GPU‑5 |
| 2 | `multiview-output/Cargo.toml` | OUT‑2, OUT‑3 |
| 2 | `multiview-preview/src/focus.rs`, `hal/degradation.rs` | PRV‑3, PRV‑4 |

**Headline:** `pipeline.rs` (the data‑plane drive loop) is edited by **9 items from 6 different
streams**. A naive "one worktree per stream" fanout will thrash on it. The data‑plane core
(`pipeline.rs` + `sink.rs` + `run.rs` + `control.rs` + `events/event.rs` + `command.rs` +
`config/schema.rs`) is a tightly‑coupled monolith and must be evolved **serially by one owner**.

### 1c. Recommended fanout — 1 serial integrator + 6–7 parallel lanes

Partition by **file territory** (not by stream), so each lane owns a disjoint set of files and runs
as its own worktree; items *within* a lane are serial in `deps:` order.

- **LANE‑CORE (serial — the long pole, one "data‑plane integrator").** Owns `pipeline.rs`,
  `sink.rs`, `run.rs`, `control.rs`, `events/event.rs`, `command.rs`, `config/schema.rs`.
  Order: **GPU‑1** (encode‑once‑mux‑many — *do first; everything muxes through it*) → **ENG‑1**
  (teardown join) → **OUT‑1/OUT‑3** (output decision + schema) → **CTL‑1 → CTL‑3 → CTL‑2 → CTL‑5**
  (control→engine apply) → **AUD‑2 → AUD‑3 → AUD‑4 → AUD‑5 → AUD‑7** (audio spine) → **IN‑3, IN‑5**
  (NDI/YouTube ingest wiring) → **OUT‑2/OUT‑4** (RTSP/NDI sinks) → **GPU‑2** (software clock) →
  **ENG‑2** (PTS normalizer) → **CTL‑6** (Class‑2). *This lane is the critical path for
  parallelism — keep it staffed.*
- **LANE‑IN (parallel).** `crates/multiview-input/src/{st2110,webrtc,youtube}/` — **IN‑1 → IN‑2 →
  IN‑6**, **IN‑4**, then **IN‑7** (CI). Only IN‑3/IN‑5 (which touch `pipeline.rs`) hand off to CORE.
- **LANE‑PRV (parallel).** `crates/multiview-preview/*` — **PRV‑1 → PRV‑3 → PRV‑4 → PRV‑5**.
  *Coordinate PRV‑2* (adds routes to `control/routes/mod.rs` + `state.rs`) with LANE‑API/CORE.
- **LANE‑ENG (parallel).** `engine/src/{ptp,ha}.rs`, `hal/src/load.rs`, `cli/wallclock.rs` —
  **ENG‑6**, **ENG‑5 → ENG‑3**, **ENG‑4** *(shares `hal/load.rs` with GPU‑5 — merge with LANE‑GPU
  or let one owner hold `load.rs`)*.
- **LANE‑GPU (parallel).** `compositor/*`, `ffmpeg/hwframe.rs`, `hal/select.rs` — **GPU‑3 → GPU‑4**,
  **GPU‑5**, **GPU‑6**. (GPU‑1/GPU‑2 are in CORE.)
- **LANE‑BCAST (parallel).** `control/src/{nmos,is07}.rs` — **SUR‑1**, **SUR‑2**; plus **SUR‑3**
  (captions, `ffmpeg`/`input` — independent).
- **LANE‑API (parallel, but the *cross‑lane coordination hub*).** Owns `control/openapi.rs` +
  `routes/mod.rs` route registration + the codegen — **SUR‑4 → SUR‑5**, **SUR‑6**, and **CTL‑4**
  (ApplyLayout route). Because PRV‑2, CTL‑4, SUR‑4, SUR‑6 all touch `openapi.rs`/`routes/mod.rs`,
  **one owner registers all new routes + OpenAPI annotations** to avoid churn; other lanes file
  their handler bodies and let LANE‑API wire the router.
- **LANE‑WEB (parallel).** `web/src/` — **AUD‑8** (audio matrix UI) after AUD‑7; **SUR‑5** (generated
  client) after SUR‑4.

**Three coordination points to watch in any fanout:** (i) `control/routes/mod.rs` + `openapi.rs`
(CTL‑4 · PRV‑2 · SUR‑4 · SUR‑6) → LANE‑API owns route/spec registration; (ii) `hal/load.rs`
(ENG‑4 · GPU‑5) → one owner; (iii) `control/state.rs` (CTL‑3 · PRV‑2) → agree the `AppState` shape
first.

**So a "complete fanout" = ~7 concurrent worktrees** (CORE + IN + PRV + ENG + GPU + BCAST + API/WEB),
with CORE as the staffed critical path and the three coordination points pre‑agreed. Expect CORE to
dominate wall‑clock; the six parallel lanes finish well before it.

---

## Part 2 — Master checklist


### AUD — Audio pipeline + tone

- [x] **AUD-1** `M` — Logical audio-codec selector + license-aware resolution  ·  _deps: —_
- [ ] **AUD-2** `L` — Per-source runtime audio decode thread (peer of video ingest)  ·  _deps: AUD-1_
- [ ] **AUD-3** `L` — Program-bus mix + per-tick sample budget on the output clock  ·  _deps: AUD-2_
- [ ] **AUD-4** `XL` — Audio encode + dual-stream mux in the output sinks (the core gap)  ·  _deps: AUD-1, AUD-3_
- [ ] **AUD-5** `M` — `bars` synthetic-source 1 kHz tone companion  ·  _deps: AUD-2, AUD-3_
- [ ] **AUD-6** `L` — EBU R128 loudness normalisation on the program bus  ·  _deps: AUD-3_
- [ ] **AUD-7** `M` — Audio routing config schema + capability validation  ·  _deps: AUD-3, AUD-4_
- [ ] **AUD-8** `L` — WebUI audio routing matrix + loudness meters  ·  _deps: AUD-6, AUD-7_

### OUT — Output servers (RTSP, NDI)

- [ ] **OUT-1** `M` — RTSP egress decision spike + sidecar baseline (MediaMTX) wired as a Push target  ·  _deps: —_
- [ ] **OUT-2** `XL` — In-process RTSP server via `gst-rtsp-server`, fed pre-encoded NALs (`PacketSink`)  ·  _deps: OUT-1_
- [ ] **OUT-3** `L` — NDI dynamic-load backend (`NDIlib_v6_load`) + feature/license scaffolding  ·  _deps: —_
- [ ] **OUT-4** `L` — NDI output Sender wired as a Sink (host-memory copy from canvas)  ·  _deps: OUT-3_

### IN — Inputs (NDI · ST 2110 · WebRTC · YouTube)

- [x] **IN-1** `M` — ST 2110 receive: frame assembler over the depacketizers  ·  _deps: —_  · _pure SRD/line reassembler (marker/seq/timestamp) tested; pgroup→NV12 unpack + `ProducedFrame` adaptation belong with IN-2_
- [x] **IN-2** `L` — ST 2110 receive: wire `RtpReceiver`/`DualPathReceiver` into a `FrameProducer` + PTP timing  ·  _deps: IN-1_  · _`549b971`: `St2110Producer: FrameProducer` over the IN-1 assembler + 90 kHz→ns rebase (`WrapBits::Rtp32`, float-free, monotonic) + 2022-7 `DualPathPacketSource` dedup + bounded async→sync bridge; injected-packet tested, live NIC/PTP gated. cli wiring = IN-3_
- [ ] **IN-3** `XL` — NDI ingest: runtime-loaded SDK → `FrameProducer` + CLI wiring  ·  _deps: IN-2_
- [x] **IN-4** `M` — YouTube live: pure resolver core over `yt-dlp -J`  ·  _deps: —_
- [ ] **IN-5** `L` — YouTube live: wire to HLS ingest + re-resolution loop  ·  _deps: IN-4_
- [~] **IN-6** `XL` — WebRTC ingest: ICE/DTLS/SRTP transport behind an application-layer media engine  ·  _deps: IN-1_  · _`831e3af`: testable core landed behind `webrtc` — session lifecycle + `MediaEngine` seam + RFC-6184 H264 depacketizer (keyframe-gated, bounded) + `WebRtcProducer: FrameProducer`, fake-driven tests; live ICE/DTLS/SRTP engine (str0m) + cli wiring + Opus/VP8 = **IN-6b**_
- [x] **IN-7** `S` — CI strategy: feature-gated compile + integration gating for the wired transports  ·  _deps: IN-2, IN-3, IN-6_  · _`871db84`: `.github/workflows/ci.yml` `feature-clippy` matrix (9 legs: st2110/webrtc/youtube/ptp/cluster/ntp/is07-mqtt/nmos/i915-pmu, all -D) + `asyncapi-validate` job (SUR-6b CI tail); commands verified locally. NDI(IN-3) excluded (no feature yet — needs an SDK-fetch Docker lane)_

### CTL — Control plane → engine

- [ ] **CTL-1** `L` — Drain-apply every accepted command + emit outcome events  ·  _deps: —_
- [ ] **CTL-2** `L` — Apply Source/Output/Overlay CRUD to the running engine via the command bus  ·  _deps: CTL-1, CTL-3_
- [ ] **CTL-3** `M` — Mirror `multiview run` config into the resource store at startup + on change  ·  _deps: —_
- [ ] **CTL-4** `S` — `ApplyLayout` HTTP route  ·  _deps: CTL-1_
- [ ] **CTL-5** `M` — Salvo/Start/Stop outcome events on the realtime stream (corr-correlated)  ·  _deps: CTL-1_
- [ ] **CTL-6** `XL` — Class-2 parallel-output (make-before-break) migration  ·  _deps: CTL-1, CTL-2, CTL-4, CTL-5_

### PRV — Preview & WebRTC transport

- [~] **PRV-1** `XL` — Native ICE/DTLS/SRTP transport behind a `WhepTransport` seam (str0m, in-process default)  ·  _deps: —_  · _SEAM landed `befefb2` (trait + SDP offer/answer glue + session lifecycle + bounded drop-oldest feed, fake-transport tested); native str0m ICE/DTLS/SRTP impl is **PRV-1b** below_
- [x] **PRV-2** `L` — Wire WHEP focus routes into `multiview-control` (POST/DELETE per scope) with token-gated Focus + transport seam  ·  _deps: PRV-1_  · _`daedd1d`: POST/DELETE `/preview/{program,inputs/{id},outputs/{id}}/whep` over a codec-free `WhepProvider` seam (offer→201+sdp answer; 401/403 token-gated; 404/415/503-fallback RFC9457); openapi-documented; 7 route tests. Binary adapter bridging to preview's `WhepTransport` = PRV-1b glue_
- [x] **PRV-3** `M` — Concurrent-focus session caps + isolation enforcement (the `FocusGate`)  ·  _deps: PRV-2_  · _`a67dc24`: `FocusGate` (global + per-scope caps, fail-closed, `FocusLease` Drop frees) + `GatedWhep` decorator admitting before delegate → existing `503 fallback` shed shape; 5+4 tests. HAL cost-budget hook = PRV-4_
- [x] **PRV-4** `M` — Make preview the topmost (cheapest-to-shed) degradation rung  ·  _deps: PRV-3_  · _`a6023db`: 5 preview rungs prepended ABOVE every tile/program rung in `multiview-hal::degradation` (13-rung ladder, `affects_preview()`/`first_program_level()`) + `FocusGate::suspend()/resume()` hook refusing new focus while degraded (503-fallback); 14 hal + preview tests prove preview is fully shed before any program lever. The cli degradation-loop glue that observes `Hysteresis` and calls `suspend()/resume()` (+ tracing) is **PRV-4b**_
- [ ] **PRV-5** `L` — Sub-second WebRTC OUTPUT (program) focus: program-canvas tap → preview encode → WHEP  ·  _deps: PRV-1, PRV-2, PRV-3_

### ENG — Engine timing & resilience

- [x] **ENG-1** `M` — Bounded teardown join for a wedged sink (task #50)  ·  _deps: —_  · _`5ca8eab`: `StreamEgress::join` waits ≤`SINK_WEDGE_GRACE` (2s) per sink via `is_finished()`, then reports+detaches a wedged sink (never a `join()` that can't return); consumer fan-out uses a bounded `send_bounded` (try_send poll) so a never-draining sink can't stall it either. Watchdog-thread TDD test (RED hung at 8s → GREEN ~2s)_
- [ ] **ENG-2** `XL` — Input PTS normalizer + pacer reroute (ADR-0021 points 1-3)  ·  _deps: —_
- [~] **ENG-3** `M` — NTP/PTP lock auto-detect for the wall-clock badge (task #37)  ·  _deps: ENG-5_  · _`d87b69d`: `sysref` classifier + `ReferenceSelector` (NTP/SYS vs PTP) pure-tested + new `multiview-ntpsys` FFI leaf crate owning the one read-only `adjtimex` unsafe (deny+SAFETY, libc, `ntp` feature) + gated live read; wiring it into the cli `SystemWallClock::reference()` badge = **ENG-3b**_
- [x] **ENG-4** `L` — Linux i915/amdgpu GPU load probe  ·  _deps: —_  · _sysfs busy%+VRAM probe (`SysfsLoadProbe`, PCI-bus keyed) + pure parsers tested; per-engine enc/dec via `/proc/pid/fdinfo` walk + i915 PMU (needs unsafe) are follow-up slices_
- [x] **ENG-5** `L` — PTP / ST 2059 PHC NIC binding (`ptp` feature)  ·  _deps: ENG-3_  · _`9cb742b`: lock-state machine + offset servo (pure, tested) + Linux `/dev/ptpN` read via `rustix` (no unsafe), live test gated_
- [x] **ENG-6** `L` — HA cluster peer transport (`cluster` feature)  ·  _deps: —_  · _`UdpClusterTransport` + failover/replication over loopback-tested; true multi-host partition is hardware-tier (gated)_

### GPU — Compositor, efficiency & hardware

- [x] **GPU-1** `L` — Hoist the single encoder into the bake consumer; fan packets to mux-only sinks  ·  _deps: —_  · _DONE via `357d52b` (`ProgramEncoder`) + `a1af76c` (GPU-1b wiring): the consumer encodes ONCE, fans `EncodedPacket`s to N `PacketMuxSink`s. Verified end-to-end — one run encoded 50 frames once and fanned them to program.ts (50) + HLS (seg0 25 + seg1 25), all decodable mpeg2video; streaming inv-#1/#10 suite still 4/4_
- [ ] **GPU-2** `M` — Converge the SOFTWARE engine onto `synth::generator_loop` so a clock source animates  ·  _deps: —_
- [x] **GPU-3** `S` — GPU `describe_*` metadata trait methods: wire or remove  ·  _deps: —_
- [~] **GPU-4** `L` — Overlay IMAGE-primitive GPU texture upload (the wgpu shader branch)  ·  _deps: GPU-3_  · _`8fd5d01`: WGSL `KIND_IMAGE` premultiplied blit + upload-once content-keyed texture cache + packing/bind-entry (CPU seams tested, naga-validated); runtime dispatch + SSIM/PSNR parity need a GPU runner → **GPU-4b** below_
- [~] **GPU-5** `XL` — Multi-GPU PLACEMENT decision engine: closed-loop controller + config + telemetry  ·  _deps: —_  · _PARTIAL: HAL deliberate-split decision (`split.rs`: `plan_split`/`CutPoint`/`CrossGpuCopy`) landed `c995341`; the engine controller (sustained-overload SHED-vs-MIGRATE), config policy fields + telemetry counters REMAIN_
- [ ] **GPU-6** `XL` — Hardware backend real decode/encode/composite PATHS (cuda/vaapi/qsv/metal)  ·  _deps: GPU-1, GPU-3_

### SUR — Captions · NMOS · web codegen

- [x] **SUR-1** `M` — IS-05 scheduled activation (absolute + relative)  ·  _deps: —_
- [x] **SUR-2** `L` — IS-07 MQTT broker transport  ·  _deps: SUR-1_  · _codec+topics+bounded drop-oldest queue (always-built) + live `rumqttc` client behind `is07-mqtt`; round-trip exercised against an in-process `rumqttd` broker_
- [ ] **SUR-3** `XL` — Caption ingest Phase 2/3: broaden native decode beyond HLS WebVTT  ·  _deps: —_
- [x] **SUR-4** `M` — OpenAPI: annotate the layout/resource write ops so they enter the spec  ·  _deps: —_
- [x] **SUR-5** `M` — Web: replace the hand-written layouts wrapper with the generated client + wire deferred routes  ·  _deps: SUR-4_  · _generated openapi-fetch client; create/update/delete wired; tsc+eslint+build+76 tests green_
- [~] **SUR-6** `XL` — AsyncAPI generation + generated realtime envelope types (replace the hand-modelled envelope)  ·  _deps: SUR-4_  · _`bd1bd68`: AsyncAPI 3.0 generator + `xtask gen-asyncapi` + generated TS types (additive, idempotent, tested); envelope SWAP + serve `/asyncapi.json` + CI AsyncAPI-CLI validation are **SUR-6b** below_

#### Discovered follow-on slices (added during shipping — keep the plan complete)
- [x] **GPU-4b** `M` — Wire the overlay-image GPU dispatch into the compositor `composite()` (upload image cache layers + bind group + `OverlaySubpass` between composite and encode) + the GPU-vs-CPU SSIM/PSNR parity test (GPU-tagged runner)  ·  _deps: GPU-4_  · _`0cdd9bd`: `composite_with_overlays` uploads-once to a persistent Rgba8Unorm layer-array, binds binding-5, dispatches `OverlaySubpass` (premultiplied-over in linear, inv #8); `plan_image_uploads` seam + 4 CPU tests; SSIM/PSNR parity test `#[ignore]`-gated (Y SSIM≥0.98/PSNR≥38dB, GPU-runner-only). no-overlay path byte-unchanged. (GPU-4 stays [~] until real-GPU parity runs)_

#### Discovered follow-on slices (batch-8)
- [ ] **PRV-4b** `S` — cli degradation-loop glue: observe `Hysteresis`/the ladder and call `FocusGate::suspend()/resume()` on the preview rungs + `tracing` each preview adaptation (ADR-P003)  ·  _deps: PRV-4_
- [ ] **PRV-1b** `XL` — Native str0m `WhepTransport` impl: real ICE/DTLS/SRTP behind a `webrtc-native` feature + env-gated DTLS-SRTP loopback test + ffprobe egress check; add str0m/ring to `deny.toml`  ·  _deps: PRV-1_
- [x] **SUR-6b** `M` — Swap web realtime consumers (`connection.ts`/`useEngineEvents.ts`) onto the generated envelope types + serve `/asyncapi.json` on the axum router  ·  _deps: SUR-6_  · _`7b9fd90`: GET /asyncapi.json served (embedded, route-tested) + web `LifecycleState`/`TileState` now from generated-types (91 web tests); the AsyncAPI-CLI **CI** validation step is a separate `.github` follow-on (fold into IN-7's CI work)_
- [ ] **IN-6b** `XL` — WebRTC ingest native engine: a concrete str0m-backed ICE/DTLS/SRTP `MediaEngine` behind a `webrtc-native` feature + env-gated loopback test + wire `WebRtcProducer` into the cli; Opus/VP8 depacketizers; add str0m/ring to `deny.toml`  ·  _deps: IN-6_
- [ ] **ENG-3b** `S` — Wire `SystemRefTracker`/`ReferenceSelector` (ENG-3) into the cli `SystemWallClock::reference()` so the rendered wall-clock badge reflects the measured NTP/PTP status; cli `ntp` feature passthrough + update the badge render test  ·  _deps: ENG-3_
- [x] **GPU-5b** `L` — GPU-5 remainder: off-hot-path placement controller in `multiview-engine` (EWMA sustained-overload SHED-vs-MIGRATE, make-before-break) + config policy fields + telemetry counters  ·  _deps: GPU-5_  · _`250034b`: `PlacementController::observe→{Hold,Shed,Migrate,Split}` (pure, reuses `Hysteresis`, anti-storm cooldown/budget/min-gain, 11 tests) + `multiview-config` `PlacementConfig`/`DevicePin`/`gpu_pin` + `multiview-telemetry` placement counters. Wiring proposals into the supervisor execution (make-before-break swap) = **GPU-5c**_
- [x] **ENG-4b** `M` — GPU load probe remainder: live `/proc/<pid>/fdinfo` enc/dec-util walk (sum own PIDs) + telemetry gauge registration  ·  _deps: ENG-4_  · _`4152955`: `FdinfoMediaTracker` two-snapshot diff → `DeviceLoad.enc/dec_util_frac` (pure parser tested on fixtures, live `/proc` walk gated); the existing ENG-4 gauges consume it. i915 PMU `perf_event_open` (needs unsafe → FFI shim) = **ENG-4c**_
- [ ] **GPU-5c** `L` — Wire `PlacementController` (GPU-5b) into the engine runtime: a `LoadPoller` poll thread publishing the `arc_swap` `Vec<DeviceLoad>` snapshot, execute `Migrate`/`Split` proposals through the make-before-break supervisor + scene-swap, call `PlacementCounters::record_*`, resolve config `DevicePin`→`multiview_hal::DeviceId`. Touches the protected output core.  ·  _deps: GPU-5b_
- [x] **ENG-4c** `S` — i915 PMU `perf_event_open` GPU-busy path behind a tiny FFI-shim leaf crate (deny+SAFETY, like `multiview-ntpsys`), feeding the same `DeviceLoad` enc/dec fields on Intel where fdinfo is unavailable  ·  _deps: ENG-4b_  · _`4a45c46`: NEW `multiview-i915pmu` FFI crate — 3 `// SAFETY:` `unsafe` syscalls (`perf_event_open`/`read`/`close`) + a hand-rolled correct `PERF_ATTR_SIZE_VER1` (72-byte) `perf_event_attr`; hal `i915-pmu` feature folds busy-ns→frac into `DeviceLoad`. Pure diff tested; live `perf_event_open` gated. Poller call = ENG-4 infra_
- [x] **GPU-1b** `L` — Wire the `ProgramEncoder` into the cli bake consumer (encode-once-mux-many)  ·  _deps: GPU-1_  · _`a1af76c`: consumer owns one `ProgramEncoder`; `RunnableOutput`/`build_outputs`/`run_one_output`→`PacketMuxSink`; `StreamingPacketSource`; test seam evolved frame→packet (no assertion weakened); per-sink encoders retired; aspirational comments fixed. Completes GPU-1 / inv #7_


---

## Part 3 — Detailed work items

_Each item: Goal · Touches · Approach · Acceptance · Risks · Read‑first. Flip the box above and set Status here as items land._


## AUD — Audio pipeline + tone

**Grounding summary.** The runtime pipeline (`crates/multiview-cli/src/pipeline.rs`) is **video-only** today: `Pipeline::build` (pipeline.rs:483) builds per-source video `IngestPlan`s, the engine emits one NV12 canvas per tick, and the egress (`StreamEgress::spawn`, pipeline.rs:1570) fans each `Arc<Nv12Image>` to sinks whose `run()` registers exactly **one** video stream (`FileSink::run` sink.rs:301, `drive_to_single_muxer` sink.rs:402). Verified: `demo-output/clocks/program.ts` has one `mpeg2video` stream, no audio. The pure-Rust building blocks already exist and are untouched by the runtime: `Mixer` (mixer.rs — program bus + discrete tracks + silence-fill), `LoudnessMeter` (loudness.rs — full BS.1770-4 M/S/I/LRA/dBTP), `AudioFileDecoder`/`Resampler` (decode.rs / multiview-ffmpeg resample.rs, 48k fltp canonical), and `AudioEncoder`/`AudioEncodeTarget` (multiview-ffmpeg encode.rs:151 — opened-and-ready but never wired to a muxer). `Muxer::add_stream` (mux.rs:91) already accepts a second stream. The synth `generator_loop` (synth.rs:307) produces video frames only. There is **no** `AudioCodec` logical selector mirroring `VideoCodec` (codec.rs) — that is the first gap.

---

### `[ ]` AUD-1 — Logical audio-codec selector + license-aware resolution · effort: M · deps: none
- **Goal:** Add an `AudioCodec` logical type and `select_audio_encoder` to `multiview-ffmpeg` so the pipeline can resolve AAC/Opus/MP2 the same license-aware way video already resolves via `select_encoder`, keeping the default build LGPL-clean.
- **Touches:** `crates/multiview-ffmpeg/src/codec.rs` (mirror `VideoCodec`/`select_encoder` codec.rs:40-157); `crates/multiview-ffmpeg/src/encode.rs:308` (`static_codec_name` already lists `"aac"`); `crates/multiview-ffmpeg/src/lib.rs` re-exports.
- **Approach:**
  1. Add `enum AudioCodec { Aac, Opus, Mp2 }` with `lgpl_software_encoder()` returning `"aac"` (FFmpeg's native AAC is LGPL), `"libopus"` (LGPL), `"mp2"` (LGPL) — all default-buildable; reserve any GPL/`libfdk_aac` (nonfree) behind a gate exactly as `gpl_software_encoder()` does for video (codec.rs:73).
  2. Add `candidate_encoders(AudioCodec)` + `select_audio_encoder(AudioCodec) -> Option<&'static str>` walking the same fixed-policy order.
  3. Extend `static_codec_name` (encode.rs:308) for `"libopus"`/`"mp2"`.
- **Acceptance (done when):** unit tests in codec.rs assert `AudioCodec::Aac.lgpl_software_encoder() == Some("aac")`, Opus→`libopus`, Mp2→`mp2`, and that `libfdk_aac` is never returned in a default build (mirror the H.264-returns-None tests codec.rs:183). `cargo test -p multiview-ffmpeg` green; guardrail: no `unwrap`/`as` in non-test code.
- **Risks/notes:** Native `aac`/`libopus`/`mp2` are all LGPL — keep them in the default candidate list; only `libfdk_aac` is nonfree and must stay gated. CI has FFmpeg present (video tests already encode), so the audio encoders should `find_by_name` fine; if a build lacks `libopus`, `select_audio_encoder` must fall through (don't `expect`).
- **Read first:** ADR-R005 (decode→re-encode normalized, capability matrix); `crates/multiview-ffmpeg/src/codec.rs` header doc.

### `[ ]` AUD-2 — Per-source runtime audio decode thread (peer of video ingest) · effort: L · deps: AUD-1
- **Goal:** Decode each file/URL source's audio on its own thread into a per-source lock-free audio store (48k fltp), so the output clock can *sample* audio per tick exactly as it samples video tiles — never pacing or stalling the engine (#1/#10).
- **Touches:** new `crates/multiview-cli/src/audio.rs` (peer of `synth.rs`); `IngestPlan` (pipeline.rs:421) gains an optional audio route; `IngestSupervisor::start` (pipeline.rs:930) spawns the audio thread alongside the video decode thread. Reuse `multiview_audio::AudioFileDecoder` (decode.rs:48) and `multiview_ffmpeg::Resampler` (resample.rs:54).
- **Approach:**
  1. Build an `AudioTileStore` analogous to the video `TileStore` (a bounded SPSC ring of `AudioBlock`s keyed by source id, drop-oldest) — or reuse `multiview_framestore` generically if its store is type-parametric; keep it lock-free and read-only on the engine side per ADR-R006.
  2. In a new `audio_decode_loop` (model on `synth::generator_loop` synth.rs:307: spawn, `stop: &AtomicBool`, prompt teardown), pull `DecodedBlock`s, PTS-rebase to the program clock, and on EOF/dropout publish `AudioBlock::silence` (mixer.rs already silence-fills, but the *store* must also never gap — load-bearing for invariant A per ADR-R005 §4.1).
  3. Wire into `IngestSupervisor::start` so audio threads are torn down on `supervisor.shutdown()` (pipeline.rs:1073) exactly like video.
- **Acceptance (done when):** new test feeds a short fixture clip (multiview-ffmpeg `test_fixtures.rs`) and asserts the store yields 48k fltp blocks then silence past EOF; a "dead source" test asserts the audio thread never blocks join past one chunk (reuse the synth `sleep_until` teardown pattern). Invariant re-assert: engine loop only *samples* the audio store (#10 no back-pressure). Guardrail: `unsafe_code = forbid` preserved (multiview-audio touches libav only via the safe seam, decode.rs header).
- **Risks/notes:** Live URL/NDI/audio-free sources contribute no decoder → ride silence-fill (the build-time meter timelines already handle this absence, pipeline.rs:384). CI: needs an audio-bearing fixture; generate a tone clip rather than relying on network. Resample drift handled later (ADR-T006 is the soft-resample story; v1 may use fixed 48k and note the drift gap).
- **Read first:** streaming-gotchas §5 (long-run drift) + §7 (A/V sync/jitter); ADR-R005 §4.1; core-engine §9.2 audio bullet.

### `[ ]` AUD-3 — Program-bus mix + per-tick sample budget on the output clock · effort: L · deps: AUD-2
- **Goal:** At each output tick, pull exactly `samples_per_tick = 48000·den/num` samples per track, mix the program bus via the existing `Mixer`, and carry the audio alongside the canvas through the egress — making the output truly encode-once-mux-many on the audio side.
- **Touches:** `StreamItem` (pipeline.rs:1397) gains an `audio: ProgramAudio` field (program-bus `AudioBlock` + per-discrete-track blocks); the hot-loop projection `state_of` (pipeline.rs:979) samples the audio stores and runs `Mixer::mix_program` (mixer.rs:148); `StreamEgress`/`StreamingFrameSource` (pipeline.rs:1570, run_one_output:1729) carry the audio to sinks.
- **Approach:**
  1. Construct one `Mixer` (mixer.rs:60) per run at the canonical 48k/stereo format; `add_input` + `route_to_program` per source from the routing config (AUD-7).
  2. In `state_of`, after sampling video, pull `samples_per_tick` from each audio store, `Mixer::submit` each, call `mix_program()` → the program `AudioBlock`; attach to `StreamItem`. The mix is cheap and pure; it runs on the hot loop but does no I/O and cannot block (the stores are non-blocking) — assert this is within tick budget.
  3. Extend the egress channel payload from `Arc<Nv12Image>` to a `(Arc<Nv12Image>, ProgramAudio)` (or a small struct) so each sink receives matched A/V per tick. Honour the same drop-on-overload/block-for-exact `SendPolicy` (pipeline.rs:122) — audio rides the same bounded queue, dropped *with* its frame so A/V stay locked.
- **Acceptance (done when):** test drives N ticks and asserts `total_audio_samples ≈ ticks · samples_per_tick` (continuity — the audio analogue of "output-clock never stalls"); a dropped-source test asserts the program bus is gap-free silence, never absent. Invariant #1 re-assert: audio sample count is a pure function of tick count; #10: mixing never back-pressures the engine. ffprobe check deferred to AUD-4.
- **Risks/notes:** `samples_per_tick` is fractional for 1001-denominator rates (e.g. 30000/1001 → 1601.6 samples/tick) — accumulate the remainder across ticks (1601/1602 alternation) so the long-run sample count stays exact (this is the audio side of the drift invariant). Guardrail: no float `as` truncation on the sample count — use rational accumulation.
- **Read first:** streaming-gotchas §0 (unified timing, input-PTS→output-frame-index) + §7; ADR-R005 §4.1.

### `[ ]` AUD-4 — Audio encode + dual-stream mux in the output sinks (the core gap) · effort: XL · deps: AUD-1, AUD-3
- **Goal:** Register a *second* (audio) stream on every muxer and interleave encoded audio packets with video — so `program.ts`/HLS/RTMP/SRT carry video **and** audio, completing encode-once-mux-many. This is the load-bearing change that turns the verification (`program.ts` one video stream) into two streams.
- **Touches:** `crates/multiview-output/src/sink.rs` — `EncodeConfig` gains an audio half (codec_name/sample_rate/layout/bitrate); `FileSink::run` (sink.rs:301), `drive_to_single_muxer` (sink.rs:402), `SegmentSink::run` (sink.rs:506), `PushSink::run` (sink.rs:632); a new `AudioFrameSource` trait paralleling `VideoFrameSource` (sink.rs:102); reuse `multiview_ffmpeg::AudioEncoder` (encode.rs:172) and `Muxer::add_stream` (mux.rs:91).
- **Approach:**
  1. Add an `AudioEncodeConfig` to `EncodeConfig`; in `run`, after `muxer.add_stream(video…)`, build `AudioEncoder::new(target)` (encode.rs:178) and `muxer.add_stream(audio.as_codec_context(), audio.time_base())` → audio `stream_index`. Write header once with both streams registered (it is immutable for the session — the pinning rule, ADR-R005/brief §3.3).
  2. Generalise `drive_to_single_muxer` to interleave: each tick, encode the video frame (existing path) **and** feed the tick's `AudioBlock` to `AudioEncoder::send_frame` (chunking to the encoder's `frame_size`, encode.rs:217, e.g. AAC's 1024). Re-stamp audio PTS from a sample counter (`audio_pts = Σ samples`), the audio analogue of `out_pts = tick` (#3 — never forward input PTS). Drain both encoders' packets and `muxer.write_packet` on the matching stream index; libav's interleaved writer orders by DTS.
  3. Carry the audio through `StreamingFrameSource`/`run_one_output` (pipeline.rs:1729) and the packet-fan path (`drive_packets_to_single_muxer` sink.rs:439) for the multi-sink case, so the *same* encoded audio packets fan to file+HLS+push (invariant #7 — audio is encoded once, not per output).
- **Acceptance (done when):** new integration test runs a `bars`+tone config and ffprobes the output: **two** streams — `video` + `aac` (or configured codec) — and `silencedetect` confirms the program is *not* silent (content-aware, not a byte hash). HLS segments each carry audio; `tsp`/ffprobe confirm continuity. Invariant #1: audio packet count tracks tick count with no gaps; #7: assert audio is encoded once even with 2+ sinks (count `AudioEncoder` builds). Name tests `program_ts_has_video_and_audio_streams`, `hls_segments_carry_audio`.
- **Risks/notes:** AAC frame_size (1024) ≠ samples_per_tick → maintain a per-sink sample ring that buffers across ticks and flushes whole encoder frames; flush remainder on EOF. Push/HLS interleaving must keep audio non-back-pressuring (#10) — a wedged audio encoder drops with its frame under live policy. Default codec MP2 or AAC (both LGPL); document MP2 as the most-compatible-LGPL-in-TS default. Guardrail: this crate already maps libav errors via `ff()` (sink.rs:137) — no `unwrap`.
- **Read first:** `crates/multiview-output/CLAUDE.md` (encode-once-mux-many, inv #3/#7); ADR-R005 §4.2 capability matrix (TS=N PIDs, HLS=renditions); brief §1.2 rule 5 (muxer continuity config).

### `[ ]` AUD-5 — `bars` synthetic-source 1 kHz tone companion · effort: M · deps: AUD-2, AUD-3
- **Goal:** Give the `bars` synthetic source a 1 kHz sine on its audio bus (the line-up tone companion to colour bars), so the synthetic line-up signal is audible and the audio path is exercisable without any external media.
- **Touches:** `crates/multiview-cli/src/synth.rs` (add a tone generator peer to `generator_loop` synth.rs:307); `SyntheticKind::Bars` (synth.rs:46); the audio store from AUD-2. Tone math is pure Rust → can live in `multiview-audio` (new `tone.rs`) so it is unit-testable and reusable, with synth.rs as the publish loop.
- **Approach:**
  1. Add `fn tone_block(format, freq_hz, phase, frames) -> (AudioBlock, next_phase)` generating a phase-continuous 1 kHz sine at a calibrated line-up level (−18 dBFS / −20 dBFS EBU alignment — make it a const) into `AudioBlock::from_interleaved` (format.rs:103). Phase carried across ticks so there is **no click** at tick boundaries.
  2. In synth, when `SyntheticKind::Bars` (and only bars — `solid`/`clock` stay silent), run a `tone_loop` peer publishing `samples_per_tick` into the source's audio store at cadence, mirroring `generator_loop`'s stop/teardown.
  3. Route the bars tone to the program bus by default (it is a real source's audio per ADR-R005 fan-out).
- **Acceptance (done when):** unit test asserts `tone_block` is phase-continuous across a block boundary (last sample of block N and first of N+1 are within one sample-step of the sine) and that an integrated `LoudnessMeter::momentary` (loudness.rs:217) on the tone reads the expected level ±0.5 LU; integration: a `bars`-only run's `program.ts` ffprobes a ~1 kHz tone (FFT bin or `astats`) — content-aware. Invariant #1: tone never gaps. Guardrail: sine via `f64::sin`, sample narrowing only through the clamped helper (mixer.rs:182 pattern), no raw `as`.
- **Risks/notes:** Keep tone generation pure/deterministic so it runs identically on CI without hardware. 1 kHz at 48k is exactly 48 samples/cycle → phase bookkeeping is clean; still carry phase as f64 for non-integer-divisor freqs.
- **Read first:** synth.rs header (synthetic source = peer of decode thread); ADR-0027 (synthetic sources, referenced in synth.rs:1); resilience-and-av §4.1.

### `[ ]` AUD-6 — EBU R128 loudness normalisation on the program bus · effort: L · deps: AUD-3
- **Goal:** Normalise *only* the program bus toward a target LUFS (−23 broadcast / −16 web) with a true-peak ceiling, reusing the existing `LoudnessMeter` math, while leaving discrete tracks unaltered (authenticity guarantee, ADR-R005/R006).
- **Touches:** new `crates/multiview-audio/src/loudnorm.rs` (a normaliser built on `loudness.rs` measurement, loudness.rs:97); applied in the pipeline mix step (AUD-3, pipeline.rs `state_of`) between `mix_program` and the encoder.
- **Approach:**
  1. Implement a single-pass/live `loudnorm` (brief §4.1: dynamic mode, live tolerance ±1 LU, gate at −70 LUFS so a lost input's silence doesn't drag the target): drive gain from the running integrated/short-term loudness off `LoudnessMeter` (loudness.rs:223/266), smoothed, with a true-peak limiter clamping to −1.5 dBTP using `true_peak_dbtp` (loudness.rs:355).
  2. Run the meter **read-only** per ADR-R006 — but here it measures the *program bus we are about to emit*, so it is on the audio path, not the engine hot path; ensure it cannot stall the output (drop-and-continue, never block).
  3. Apply gain to the program `AudioBlock` only; discrete tracks bypass entirely (mixer.rs `discrete_track` mixer.rs:131 stays clean).
- **Acceptance (done when):** test feeds a known-loudness signal (the AUD-5 tone, or a louder/quieter fixture) and asserts the normalised program-bus integrated loudness converges to target within ±1 LU and true-peak never exceeds −1.5 dBTP; a discrete-track test asserts bytes are byte-identical to input (unaltered). Invariant: −70 LUFS gate excludes a silenced input. Re-assert #1 (normaliser can't stall the clock) and #10. Name `program_bus_converges_to_target_lufs`, `discrete_tracks_unaltered`.
- **Risks/notes:** True-peak is the expensive metric (ADR-R006: ~2.5-3× cost, worse on ARM) — only run the 4× oversampled TP detector on the program bus (one track), which AUD-6 already scopes. Live single-pass loudnorm cannot match file-mode ±0.2 LU — document ±1 LU. Guardrail: gain math in f64, narrow via the clamp helper.
- **Read first:** ADR-R006 (read-only metering, normalize only the bus, true-peak gating); resilience-and-av §4.1 + §5.

### `[ ]` AUD-7 — Audio routing config schema + capability validation · effort: M · deps: AUD-3, AUD-4
- **Goal:** Add the `{input_id, channels, target_track, language, title, include_in_program_bus, gain, mute}` routing model to config and validate it against the per-output capability matrix (TS=N, HLS=select-one, RTMP=endpoint-gated, NDI=channel-map), so routing is declarative and impossible selections are rejected at config time.
- **Touches:** `crates/multiview-config/src/schema.rs` (new `AudioRoute`/audio block on `Source` or top-level, alongside `Output` schema.rs:508 which currently carries only a *video* `codec`); `crates/multiview-config/src/error.rs` (new validation errors); the `Mixer` wiring in AUD-3 reads these routes.
- **Approach:**
  1. Add an `audio` section: per-source `AudioRoute` fields per ADR-R005 §4.1; add an `audio_codec` token to each `Output` variant (schema.rs:512) resolved via AUD-1.
  2. Implement a machine-readable `OutputCapability` matrix (brief §10 "first-class data structure, not scattered conditionals") and a validator that rejects e.g. N discrete tracks on legacy RTMP, maps NDI to channel-map, and flags HLS as select-one.
  3. Feed routes into `Mixer::add_input`/`route_to_program`/gain (mixer.rs:76-95) and per-track mute in AUD-3.
- **Acceptance (done when):** config tests parse a multi-track routing doc and assert the `Mixer` is wired with the right gains/program membership; capability tests assert "N tracks on RTMP" is rejected (or degraded with an explicit warning) while "N PIDs on TS" passes — the designed-in asymmetry from brief §9.4. Round-trip serde test (the schema is `Serialize+Deserialize`, schema.rs:509). Guardrail: no panic on malformed routes — return `ConfigError`.
- **Risks/notes:** Keep the capability matrix machine-readable for reuse by the WebUI (AUD-8). Don't over-build NDI/RTMP multitrack now (those sinks aren't runnable yet, pipeline.rs:2719) — validate + degrade honestly.
- **Read first:** ADR-R005 (capability matrix, routing data model §4.1-4.2); resilience-and-av §10 data-model anchors (`AudioRoute`, `OutputCapability`).

### `[ ]` AUD-8 — WebUI audio routing matrix + loudness meters · effort: L · deps: AUD-7, AUD-6
- **Goal:** Expose the routing matrix (program bus + discrete tracks, per-input gain/mute/include) and live program-bus loudness compliance in the SPA, with the capability-aware validator greying out impossible selections per output.
- **Touches:** `web/src/pages/` (new `AudioPage.tsx` peer of `MonitoringPage.tsx`); `web/src/resources/api.ts` (extend the Outputs bindings api.ts:295 with audio routes/codec); `web/src/realtime/` (loudness over the existing engine WebSocket, useEngineEvents.ts) reusing `multiview_audio::meterdata`/`Conflator` (loudness/meterdata, ~30 Hz) per ADR-R006.
- **Approach:**
  1. Add the routing-matrix editor (input × output-track grid) driven by AUD-7's capability matrix — disable cells the matrix forbids (brief §8: "greys out N-track audio on a legacy-RTMP endpoint, shows channel-map for NDI").
  2. Wire program-bus M/S/I/LRA/dBTP + clip flags over the existing realtime WebSocket at 10-25 Hz, binary/numeric, ballistics applied client-side (ADR-R006 wire-to-browser). Reuse the existing meter-timeline infrastructure already feeding per-tile overlays (pipeline.rs:384).
  3. Follow the existing page/i18n/test conventions (the pages have `.test.tsx` peers, e.g. `AlarmsPage.test.tsx`).
- **Acceptance (done when):** component test renders the matrix and asserts a forbidden cell (RTMP multitrack) is disabled and NDI shows "channel-map"; a meter component test asserts LUFS/dBTP render from a mock WS frame. Existing `web` test suite (vitest) green. No new engine-path code — UI only consumes telemetry (#10).
- **Risks/notes:** Telemetry must stay numeric-only over WS (never audio) — bandwidth + ADR-R006. Defer simultaneous multi-track *monitoring* on HLS (it is select-one) to a track *selector* per the brief.
- **Read first:** resilience-and-av §8 (web surface: audio routing matrix, metering) + §5 (wire-to-browser); ADR-R006.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-output/src/sink.rs
- /workspaces/mosaic/crates/multiview-audio/src/mixer.rs
- /workspaces/mosaic/crates/multiview-ffmpeg/src/encode.rs
- /workspaces/mosaic/crates/multiview-cli/src/synth.rs


## OUT — Output servers (RTSP, NDI)


### `[ ]` OUT-1 — RTSP egress decision spike + sidecar baseline (MediaMTX) wired as a Push target · effort: M · deps: none
- **Goal:** Land a *working* RTSP egress immediately via the ADR-0006 sidecar fallback (publish the existing encoded stream to MediaMTX over libav RTSP), and lock the in-process-vs-sidecar decision before committing to the GStreamer C-stack, so RTSP output stops being a no-op without dragging GLib into the lean default build.
- **Touches:** `crates/multiview-cli/src/pipeline.rs:2713` (replace the warn-and-skip arm); `crates/multiview-output/src/sink.rs:550-573` (`PushProtocol::Rtsp` already maps to the `rtsp` muxer — reuse it); `crates/multiview-config/src/schema.rs:514` (`Output::RtspServer { mount, codec, latency_profile }`); a new `docs/decisions/ADR-0006` follow-up note (read-only here — propose, don't write).
- **Approach:**
  1. Confirm `PushProtocol::Rtsp` → `muxer_name() == "rtsp"` (sink.rs:570) drives `Muxer::create_as(rtsp://host:8554/<mount>, "rtsp")` (mux.rs:70) — i.e. libav RTSP ANNOUNCE/RECORD to a listening MediaMTX. This is the *publish hop* ADR-0006 calls the sidecar baseline; it needs **zero new sink code**, only routing.
  2. In `build_outputs` (pipeline.rs:2683), replace the `Output::RtspServer { .. }` skip arm with a `RunnableOutput::Push { sink: PushSink::new(cfg, PushProtocol::Rtsp, rtsp_publish_url(mount)), label: "rtsp" }`, deriving the publish URL from a `[system.rtsp] publish_base` config (default the MediaMTX `rtsp://127.0.0.1:8554`). Keep the connect-failure tolerance already in `run_push_output` (pipeline.rs:1789) so a missing sidecar is reported, never fatal (#1/#10).
  3. Add a `deploy/` MediaMTX sidecar manifest entry (there is a `deploy/` dir already) so `rtsp` output has a peer; gate behind config, not a Cargo feature (no native dep).
  4. Spike-document: prototype `appsrc ! h264parse ! rtph264pay` caps against real NVENC output is the in-process path (OUT-2); record the go/no-go in the ADR follow-up.
- **Acceptance (done when):** new test `crates/multiview-output/tests/push_rtsp_ffprobe.rs` (model on `push_udp_ffprobe.rs`) that spawns a local RTSP listener (MediaMTX in CI service container, or `ffmpeg -rtsp_flags listen` as the loopback peer), runs the `Rtsp` `PushSink`, then `ffprobe rtsp://…` re-reads the stream and asserts codec/geometry + exact frame count; assert `PushProtocol::Rtsp.muxer_name() == "rtsp"` (pure, always-CI). Invariant re-assert: the RTSP push reuses `drive_to_single_muxer` so it is the *same* one-encode stream (#7); a dropped/absent RTSP peer is logged and dropped, never back-pressures the engine (#10) and never stalls the output clock (#1) — proven by the existing tolerant `run_push_output` path.
- **Risks/notes:** Sidecar needs a MediaMTX binary/service in CI (network + extra process) — guard the live test behind a `requires-mediamtx` cfg like the existing RTMP/SRT tests already self-exclude. LGPL-clean: no GPL codec, no GLib pulled — this baseline keeps the default build native-light. No `unwrap`/`as`/indexing in non-test (the URL builder must use checked formatting).
- **Read first:** ADR-0006; core-engine §9.2 (RTSP server bullets); streaming-gotchas §4 (pacing — never flush a backlog to a server).

### `[ ]` OUT-2 — In-process RTSP server via `gst-rtsp-server`, fed pre-encoded NALs (`PacketSink`) · effort: XL · deps: OUT-1
- **Goal:** Deliver the ADR-0006 *primary* path — an in-process RTSP server that fans the already-encoded canvas (encode-once-mux-many) to RTSP clients with **no GStreamer re-encode** — so RTSP is a first-class low-latency egress without a Go sidecar hop.
- **Touches:** new `crates/multiview-output/src/rtsp/` module behind a new `rtsp` Cargo feature in `crates/multiview-output/Cargo.toml:features` (mirror the `ndi`/`ffmpeg` gating pattern at lines 62-72); implement `fanout::PacketSink` (fanout.rs:105) so it slots into the existing `PacketRouter`; new dep `gstreamer-rtsp-server` (not yet in `Cargo.lock`); `crates/multiview-cli/src/pipeline.rs:2713` to register the server sink when `--features rtsp`.
- **Approach:**
  1. Add the `rtsp` feature pulling `gstreamer` + `gstreamer-app` + `gstreamer-rtsp-server` (LGPL-2.1, dynamic-link). Run the GLib main loop on its own thread bridged to Tokio (ADR-0006 consequence).
  2. Build the factory pipeline exactly as core-engine §9.2 specifies: `appsrc name=src is-live=true format=TIME ! h264parse ! rtph264pay name=pay0 config-interval=-1` (and `h265parse ! rtph265pay` for HEVC), `factory.set_shared(true)` so one encode fans to all clients.
  3. Implement `RtspServerSink: PacketSink` whose `deliver(&Arc<EncodedPacket>)` (fanout.rs:111) pushes the **pre-encoded** NAL bytes (`EncodedPacket.data`, fanout.rs:97) into `appsrc` with the packet's already-tick-stamped `pts`/`duration` (fanout.rs:89) — strictly non-blocking, into a bounded drop-oldest buffer (the module contract, fanout.rs:16-21).
  4. Resolve the upstream-feed mismatch: the CLI currently fans **baked NV12 frames** to per-sink encoders (`StreamingFrameSource`, pipeline.rs:2797), not encoded packets. For RTSP we must feed *encoded* packets. Two options grounded in existing code: (a) register the server under `PacketRouter` fed from the CLI's single encoder's packet stream (the encode-once path `PacketMuxSink` already consumes via `PacketSource`, sink.rs:782), or (b) front the server with one `PushSink`-style encoder. Choose (a) to honour #7 (one encode). This requires the CLI to expose its encoder's packet stream to the router — coordinate with the engine stream.
  5. Prototype the exact `appsrc` caps (stream-format/alignment/SPS-PPS, `config-interval=-1` for late joiners) against real NVENC/VideoToolbox byte-stream output — ADR-0006 flags this as the must-validate risk.
- **Acceptance (done when):** `tests/rtsp_server_playout.rs` (feature `rtsp`) starts the server, a client (`ffprobe`/`gst-launch rtspsrc`) pulls `rtsp://127.0.0.1:8554/<mount>` and asserts codec/geometry + a monotonic, gap-free frame run; a second simultaneous client proves `set_shared(true)` fan-out from one encode (#7); a deliberately-slow/abandoned client must not stall the others or the producer (#1/#10) — assert the bounded drop-oldest buffer sheds rather than blocks `deliver`. Unit test: `PacketSink::deliver` returns without blocking under a full buffer.
- **Risks/notes:** Pulls the GStreamer/GLib C stack + a GLib main loop — **must** stay behind the `rtsp` feature so the default build is unaffected (ADR-0006 consequence; lean-static-binary tension). CI needs GStreamer base+good plugins (`rtph264pay`, `h264parse`) installed — gate the live test on their presence. LGPL-clean: gst-rtsp-server is LGPL-2.1 and dynamic-linked (clean); the H.264 *encode* stays in our LGPL/gpl-codecs-gated encoder, GStreamer only *payloads*. Guardrails: the FFI surface is GStreamer's safe Rust bindings — keep `unsafe_code = forbid` in this crate, no `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0006; core-engine §9.2 (the exact `appsrc`/`rtph264pay`/`set_shared` recipe); `multiview-output/CLAUDE.md` (#7/#10 contract); streaming-gotchas §4.

### `[ ]` OUT-3 — NDI dynamic-load backend (`NDIlib_v6_load`) + feature/license scaffolding · effort: L · deps: none
- **Goal:** Stand up the ADR-0008 runtime-loaded NDI backend — feature-gated, never-vendored, dynamically loaded via `NDIlib_v6_load()` with mandatory attribution and a runtime license-acceptance gate — so the default open-source build carries zero proprietary code/obligations and NDI stays inert until an operator accepts.
- **Touches:** `crates/multiview-output/Cargo.toml` (the `ndi` feature is currently an empty stub at line 72 — wire it to `grafton-ndi` + `libloading`); new `crates/multiview-output/src/ndi/` module; `crates/multiview-config/src/schema.rs:550` (`Output::Ndi { name }`) plus a `[system.ndi] accept_license` setting (ADR-0008 runtime gate — add to the system schema); attribution surfaced in `NOTICE`/`README` (read-only here — propose).
- **Approach:**
  1. Wire the `ndi` feature to `grafton-ndi` (build-links the dylib) **and** provide the `NDIlib_v6_load()` libloading path so the SDK is resolved at *runtime*, not build time (ADR-0008: grafton-ndi's `build.rs` panics without the SDK; the dynamic-load backend keeps the default build SDK-free). Headers MIT-vendored, runtime never vendored.
  2. Implement the runtime license gate: an `NdiLicense` guard that refuses to construct any NDI sender/receiver until `[system.ndi] accept_license = true` (audited who/when) — no NDI I/O starts otherwise (ADR-0008 hard requirement). Surface a clear refused-status string, never a panic.
  3. Add the mandatory attribution constants (`"NDI® is a registered trademark of Vizrt NDI AB"`, ndi.video link) exposed for the About/NOTICE surfaces.
  4. Keep the whole module behind `#[cfg(feature = "ndi")]`; the default `cargo test`/`cargo deny` (all-features=false) never sees it — matching the `ffmpeg`-feature isolation already used (Cargo.toml comments lines 16-31).
- **Acceptance (done when):** `tests/ndi_license_gate.rs` (feature `ndi`) asserts that with `accept_license = false` no sender is constructed and a typed refusal is returned (never a panic, never a started sender); a unit test asserts the attribution string constants are present; `cargo deny check` with all-features=false shows **no** new dependency entered the default graph (the LGPL-clean baseline is intact). Loader test: `NDIlib_v6_load()` resolution failure (no runtime present) surfaces a typed error, not a crash.
- **Risks/notes:** NDI SDK is proprietary/royalty-free — CI/Docker for the `ndi` feature must *fetch* the SDK (not vendored); the default CI must **not**. Runtime needs a resolvable dylib or the dynamic-load path. Attribution + no-"NDI"-in-product-name obligations are mandatory (ADR-0008). Guardrails: libloading is `unsafe` FFI — confine all `unsafe` to a thin `multiview-ffmpeg`-style boundary or to grafton-ndi; keep `multiview-output` `unsafe_code = forbid` by isolating the loader. No `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0008; core-engine §10 (NDI integration & licensing); `docs/io/ndi.md`.

### `[ ]` OUT-4 — NDI output Sender wired as a Sink (host-memory copy from canvas) · effort: L · deps: OUT-3
- **Goal:** Publish the composited multiview as a single NDI source (one `NDIlib` Sender) fed from the canvas, wired into the CLI's sink fan-out exactly like the file/HLS/push sinks, so NDI output is a real, runnable egress (gated + license-accepted).
- **Touches:** `crates/multiview-cli/src/pipeline.rs:2719` (replace the warn-and-skip `Output::Ndi` arm); a new `RunnableOutput::Ndi` variant (pipeline.rs:326) + its `run_one_output` arm (pipeline.rs:1733); `crates/multiview-output/src/ndi/` Sender from OUT-3; reuses `StreamingFrameSource`'s baked `Arc<Nv12Image>` fan-out (pipeline.rs:2785).
- **Approach:**
  1. Unlike RTSP/HLS/push, NDI takes **uncompressed** frames, so feed it from the *baked NV12* fan-out (`StreamingFrameSource`, pipeline.rs:2797) — not the encoded packet router. This is the one sink that is correctly frame-fed, not packet-fed; #7 still holds (composite once; NDI is a separate uncompressed rendition, explicitly allowed when the codec differs).
  2. Convert NV12 → NDI `UYVY` (`color_format_fastest`, core-engine §9.2/§10) at the NDI boundary — ADR-0008 notes NDI frames are host-memory, so the one host↔GPU copy lives here; reuse the existing plane-copy discipline (`copy_plane`, pipeline.rs:2856).
  3. Add `RunnableOutput::Ndi { sink, name }`; in `build_outputs` push it (gated on `cfg(feature = "ndi")` *and* license-accepted), else keep the honest skip. Its sink runner (`run_one_output`) pulls baked frames off the bounded fan-out channel and `send_video` — a slow/absent NDI receiver paces only this consumer (#10), never the engine.
  4. Re-stamp NDI frame timecode from the tick counter, never raw input PTS (#3), consistent with every other sink (sink.rs:23-28).
- **Acceptance (done when):** `tests/ndi_output_roundtrip.rs` (feature `ndi`, license-accepted) creates a Sender named from config and an NDI Finder/Receiver in-process, then asserts the advertised source appears and a received frame matches the canvas geometry/format and frame count; a content-aware check on the pixel values (a known ramp survives NV12→UYVY). Invariant re-assert: NDI sink runs off the hot path on its own thread (pipeline.rs:1581 model), so a stalled receiver cannot back-pressure the engine (#10) or stall the output clock (#1); timecode is tick-derived (#3).
- **Risks/notes:** Needs a resolvable NDI runtime + network in CI for the live roundtrip — gate it behind the `ndi` feature *and* a `requires-ndi-runtime` cfg (like the RTMP/SRT live tests self-exclude); the construction/license-gate path stays always-testable. NDI is proprietary/opt-in — never in the default build. Full NDI is ~125–250 Mbps/1080p60 and CPU-decoded downstream (core-engine §10 density note) — document the network budget. Guardrails: the NV12→UYVY conversion and host-copy must be checked-indexing only (mirror `copy_plane`), no `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0008; core-engine §9.2 (NDI out bullet) + §10; `docs/io/ndi.md`.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs (skip points at :2713 RTSP, :2719 NDI; `build_outputs` :2683; `RunnableOutput` :326; `run_one_output` :1729; sink fan-out `StreamEgress` :1562)
- /workspaces/mosaic/crates/multiview-output/src/sink.rs (`PushSink`/`PushProtocol::Rtsp` :550-655; `drive_to_single_muxer` :402; `PacketMuxSink`/`PacketSource` :720-808)
- /workspaces/mosaic/crates/multiview-output/src/fanout.rs (`PacketSink` :105, `PacketRouter` :117, `EncodedPacket` :84 — the encode-once-mux-many seam the RTSP server plugs into)
- /workspaces/mosaic/crates/multiview-output/Cargo.toml (`ndi` empty-stub feature :72; add `rtsp` feature; gating pattern :62-72)
- /workspaces/mosaic/crates/multiview-config/src/schema.rs (`Output::RtspServer` :514, `Output::Ndi` :550; add `[system.ndi] accept_license`)


## IN — Inputs (NDI · ST 2110 · WebRTC · YouTube)

Grounded in: `crates/multiview-input/src/{source.rs,st2110/transport.rs,webrtc/transport.rs,error.rs}`, `crates/multiview-cli/src/pipeline.rs` (ingest_plan_for @2909, ingest_loop @3225, open_and_stream @3285, SourceLocation @3071), `crates/multiview-config/src/schema.rs` (SourceKind @211), ADR-0008, ADR-0015, docs/io/{ndi,youtube-live}.md, docs/research/streaming-gotchas.md §0/§5/§7.

Key architectural fact established by the scaffold: every ingest path must produce frames into a `TileStore` via a `source::FrameProducer` (`crates/multiview-input/src/source.rs:104`) driven by an `IngestPump`, OR via the CLI's direct decode loop (`open_and_stream`). The pump is *sampled, never pacing* (invariants #1/#10). All four items converge on emitting `ProducedFrame`s through that trait or the CLI store, never blocking the engine.

---

### `[ ]` IN-1 — ST 2110 receive: frame assembler over the depacketizers · effort: M · deps: none
- **Goal:** Add a pure, testable per-frame assembler that turns the stream of `V20Payload` SRD segments + RTP marker bits into a single `ProducedFrame`, so the `st2110` transport has something to feed the `IngestPump` (the current `RtpReceiver` yields raw packets with no reassembly).
- **Touches:** `crates/multiview-input/src/st2110/` — new `assembler` submodule alongside `v20.rs`/`v30.rs`/`v40.rs`; consumes `RtpHeader.marker`/`.timestamp` (`rtp.rs:73/81`) and `SrdSegment` (`v20.rs:77`). Pure, always-compiled (mirrors how depacketizers are always compiled per `Cargo.toml` `st2110` feature comment).
- **Approach:**
  1. Define `FrameAssembler` keyed by RTP timestamp: accumulate `V20Payload` segments into a line-addressed pixel buffer until the RTP `marker` bit (RFC 4175 end-of-frame) flips, then emit a complete raster.
  2. Map the 90 kHz RTP `timestamp` (`rtp.rs:81`) to `raw_pts` in the producer's timebase; set `wrap_bits` = 32-bit RTP (the `WrapBits` enum in `normalize.rs` already has an RTP case — confirm and reuse). Surface `discontinuity` on a sequence gap reported by the ST 2022-7 reconstructor.
  3. Emit `ProducedFrame { pixels, raw_pts, discontinuity, meta }` with `meta.format` per the -20 sampling (8/10-bit 4:2:2 → the canonical NV12/P010 the compositor expects; document the conversion as a follow-up if the pgroup unpack isn't trivial — keep this item to luma+chroma plane assembly).
  4. Errors → `Error::St2110` (`error.rs:53`).
- **Acceptance (done when):** new test `crates/multiview-input/tests/st2110_assemble.rs` (with the mandatory `#![allow(clippy::unwrap_used,…)]` header per AGENTS.md) builds golden multi-packet frames (marker on the last) and asserts one `ProducedFrame` per marker with correct geometry and monotonic `raw_pts`; a property test asserts no panic on truncated/out-of-order/duplicate sequences. Re-assert #1 (assembler never blocks; an incomplete frame at EOS is dropped, never awaited). No `unwrap`/`as`/indexing in non-test (the codebase uses `read_u16`/`saturating_add` helpers — follow that).
- **Risks/notes:** pure logic, runs in the default LGPL-clean build (no feature gate, no NIC). Watch `as_conversions` ban on the 90 kHz→ns rebase — push the float-free math into `multiview_core::time::Rational`.
- **Read first:** docs/research/streaming-gotchas.md §0 (PTS pipeline) + §7 (RTP reorder/jitter); ADR-T003.

### `[ ]` IN-2 — ST 2110 receive: wire `RtpReceiver`/`DualPathReceiver` into a `FrameProducer` + PTP timing · effort: L · deps: IN-1
- **Goal:** Make the compile-only `st2110::transport` an actual ingest `Source` by driving the sockets → assembler → `IngestPump`, with receive timing anchored to a PTP/ST 2059 reference (or wall-clock fallback) per the timing model.
- **Touches:** `crates/multiview-input/src/st2110/transport.rs` (current `RtpReceiver::recv_rtp` @~95, `DualPathReceiver::recv_merged` @~140); a new `St2110Producer` implementing `source::FrameProducer`; `crates/multiview-input/Cargo.toml` `st2110` feature (already `["tokio/net"]`).
- **Approach:**
  1. Build an async receive task: loop `DualPathReceiver::recv_merged`, extracting `V20Payload`/`V30Payload`/`V40Payload` (the `extract` closure already exists), feeding IN-1's assembler; completed frames go through a bounded `tokio::sync` channel to a `FrameProducer::next_frame` adapter (the `IngestPump` is sync — bridge with a non-blocking `try_recv`, newest-wins, never block: invariant #10).
  2. Add `St2110Config` (local addr, optional multicast group + interface, optional path-B for 2022-7, declared fps/timebase) and a `bind`/`spawn` entry. Reuse `join_multicast_v4` (@~58).
  3. PTP timing: per streaming-gotchas §5 the master clock stays `CLOCK_MONOTONIC`; do **not** slave the output to the input. Use the RTP 90 kHz media clock for per-input PTS only; expose a hook for a future PTP epoch but anchor first frame to `master_now` exactly like `IngestPump::pump_one` already does. (Document that full ST 2059 lock is out of scope; cite the "free-run the rest" guidance.)
  4. Keep everything `#[cfg(feature = "st2110")]`; the producer adapter trait impl can be pure.
- **Acceptance (done when):** loopback integration test (gated `#[cfg_attr(not(feature="st2110"), ignore)]` and behind a `--ignored`/env guard) binds two `UdpSocket`s on `127.0.0.1`, sends hand-built golden -20 datagrams from a second socket through both paths, and asserts the `TileStore` receives the reassembled frame with merged/de-duped sequences; `cargo check --features st2110` and `cargo clippy --features st2110 -- -D warnings` are green (today's CI clippy job at ci.yml:32 runs **default features only**, so the gated transport is currently never clippy-checked — see IN-6). Re-assert #10 (channel is bounded, drop-oldest; a stalled reader never back-pressures the receive task).
- **Risks/notes:** needs real NICs/PTP for production; CI uses loopback unicast only (no multicast, no PTP) — gate the integration test. No FFI, stays `unsafe_code = forbid`. The pgroup→NV12 unpack may need a real conversion path; if heavy, defer to a sub-task and assemble planes first.
- **Read first:** docs/research/streaming-gotchas.md §5 (master clock) + §7; ADR-0009 (data vs IO plane); `crates/multiview-input/src/st2022_7.rs` (reconstructor contract).

### `[ ]` IN-3 — NDI ingest: runtime-loaded SDK → `FrameProducer` + CLI wiring · effort: XL · deps: none (but shares the FrameProducer→CLI bridge with IN-2)
- **Goal:** Replace the hard error at `crates/multiview-cli/src/pipeline.rs:2930` ("NDI ingest is not wired") with a real NDI receive source, runtime-loaded behind the `ndi` feature with operator license acceptance, feeding tiles like any other source.
- **Touches:** new `crates/multiview-input/src/ndi/` module (per ADR-0015 consequences: "no new crate"); `multiview-input/Cargo.toml` `ndi` feature (currently empty `"ndi" = []` — add `grafton-ndi` + `libloading`, both already in `Cargo.lock`); `crates/multiview-cli/src/pipeline.rs` `ingest_plan_for` (@2909) + `SourceLocation` enum (@3071, add `Ndi { name }`) + `ingest_loop`/`open_and_stream` (@3225/@3285, add an NDI branch that bypasses libav); `SourceKind::Ndi { name }` already exists (`schema.rs:266`).
- **Approach:**
  1. Implement the two-path model from ADR-0008/docs/io/ndi.md §2: `NDIlib_v6_load()` dynamic-load backend via `libloading` (default, no SDK to build) and an optional build-time `grafton-ndi` link. Probe at runtime → if unresolved, report capability absent (never crash).
  2. Add the **runtime license gate** (ADR-0008): NDI source refuses to start until `[system.ndi] accept_license` is set; surface a clear status, audited who/when. Wire this check in `ingest_plan_for` so an unaccepted NDI source produces a degraded tile, not a hung thread.
  3. Implement `NdiProducer: source::FrameProducer`: NDI `Receiver` → host-memory UYVY/P216/BGRA frame → `ProducedFrame` (host→GPU upload happens later in the compositor; this is the acknowledged copy boundary, ADR-0004). Wrap in NDI **FrameSync** per docs (per-source timing). Audio (FLTP) rebased like any source.
  4. CLI: change line 2930 from `Err(...)` to `Ok(IngestPlan{ location: SourceLocation::Ndi{name}, live: true, … })`; in `ingest_loop` route `SourceLocation::Ndi` to an NDI-specific drive loop (using the supervised-reconnect bracket already in `ingest_loop`) instead of `open_and_stream`.
- **Acceptance (done when):** with the runtime absent, `CapabilityReport`-style probe returns unavailable and an NDI source degrades its tile (LIVE→STALE→NO_SIGNAL) — assert via a unit test that `ingest_plan_for` no longer errors and that a missing-runtime NDI source never extends `PRIME_WAIT_BUDGET` (invariant #1, hard upper bound at pipeline.rs:~3083). A gated integration test (real runtime present, `#[ignore]` in CI) receives frames from a local NDI sender. `cargo deny check` on **default** features stays green (the proprietary deps must be fully behind `ndi`). Mandatory `ndi.video` attribution present in About/docs.
- **Risks/notes:** proprietary SDK — must never leak into the default build or `--all-features` deny job (ci.yml:69 runs default-only by design; verify `ndi` is excluded from any `full` preset). No real NDI network in CI → receive path is gated/ignored. `grafton-ndi` is `unsafe`/FFI — but it's an external crate; `multiview-input` stays `unsafe_code = forbid` (the FFI lives in grafton-ndi, not our code). License acceptance is load-bearing and audited.
- **Read first:** ADR-0008 (full); docs/io/ndi.md §2 (two code paths) + §3 (frame formats); ADR-0004 (copy boundary); ADR-M007 (CapabilityReport).

### `[ ]` IN-4 — YouTube live: pure resolver core over `yt-dlp -J` · effort: M · deps: none
- **Goal:** Add a `youtube` module that spawns `yt-dlp`, parses its JSON info-dict, classifies `live_status`, and extracts the HLS `manifest_url` + `expire` deadline — pure and fixture-testable (ADR-0015 phase P0).
- **Touches:** new `crates/multiview-input/src/youtube/` module (ADR-0015: "no new crate"); `multiview-input/Cargo.toml` new off-by-default `youtube` feature (per docs: `youtube` requires `ffmpeg`; add `serde_json` already in workspace Cargo.toml:15); `crates/multiview-config/src/schema.rs` (new `SourceKind::Youtube { url }` — the enum is `#[non_exhaustive]` @209 so additive).
- **Approach:**
  1. Pure parse layer: `fn parse_info_dict(json: &str) -> Result<ResolvedHls>` reading `streamingData.hlsManifestUrl` equivalent from yt-dlp's `manifest_url`, `is_live`/`live_status`, and the `expire` Unix-timestamp query param. No network, no subprocess in the parse function (the spawn is a thin separate fn).
  2. Subprocess wrapper: spawn `yt-dlp -J --no-warnings <url>` via `tokio::process` with an argument vector (no shell), hard timeout, captured+redacted stderr (ADR-0015 security). Pin `--extractor-args "youtube:player_client=web_safari"`; avoid `ios`.
  3. `expire` parsing → a TTL/deadline type for the re-resolution loop (IN-5).
- **Acceptance (done when):** `crates/multiview-input/tests/youtube_resolve.rs` over **recorded yt-dlp JSON fixtures** (no network) asserts manifest extraction, `live_status` classification (live/upcoming/post-live-DVR), and `expire` parsing; a property test asserts no panic on malformed JSON. Capability probe (`yt-dlp --version`) returns unavailable cleanly when the binary is absent. No `unwrap`/`as`/indexing in non-test.
- **Risks/notes:** `yt-dlp` is runtime-discovered, never vendored (LGPL-clean by construction, Unlicense subprocess boundary). YouTube's player surface moves — ADR-0015 says cited yt-dlp line refs + ~6 h TTL must be re-verified at implementation time. Cookies are secret-ref only (ADR-M006).
- **Read first:** ADR-0015 (full) + docs/io/youtube-live.md §1 + §10 (phases P0–P1); ADR-M006 (secrets).

### `[ ]` IN-5 — YouTube live: wire to HLS ingest + re-resolution loop · effort: L · deps: IN-4
- **Goal:** Feed the resolved googlevideo HLS URL into the existing `hls`/libav ingest path and run a control-plane re-resolution loop that refreshes before `expire`, so a YouTube tile survives the ~6 h manifest expiry (ADR-0015 phases P2–P4).
- **Touches:** `crates/multiview-cli/src/pipeline.rs` `ingest_plan_for` (@2909 — map `SourceKind::Youtube` to `SourceLocation::Url(resolved)`, `live: true`, reusing the exact rtsp/hls/ts/srt/rtmp branch @2927); the re-resolution task in `crates/multiview-input/src/youtube/`; reuse `reconnect.rs` backoff and the `PtsWallClock`/`open_and_stream` HLS path unchanged.
- **Approach:**
  1. At plan build, resolve once (IN-4) → `SourceLocation::Url`; a resolve failure must degrade the tile, never fail the build (the @2945 comment: "must never fail the *build* of a never-ending live source").
  2. Re-resolution loop on the control/IO plane (ADR-0009): parse `expire`, refresh at lead-time, do a make-before-break URL swap (ADR-R004/M005 Class-1 style), and re-resolve immediately on a sustained 403 burst. Run as a supervised subtask with hard timeout + bounded backoff (`reconnect.rs`); a hung `yt-dlp` is killed, not awaited (invariant #10).
  3. Surface staleness/extraction-failure alarm + resolver version via telemetry.
- **Acceptance (done when):** unit test asserts the swap is make-before-break (old URL stays live until new one primes) and that a resolve failure yields a degraded tile while the **output clock never stalls** (invariant #1) and never back-pressures (#10). A long soak test (gated/manual, real network) spans ≥1 expiry boundary with no tile loss. ffprobe the resolved `*.googlevideo.com` URL is a readable HLS master in the manual test.
- **Risks/notes:** real network + a current `yt-dlp` (ideally a JS runtime like Deno for n-sig) needed — gate the live test, keep CI on fixtures. Extraction breakage is *expected and handled*, not a hard failure.
- **Read first:** ADR-0015 consequences ("re-resolution is load-bearing"); ADR-T004 (HLS pacing) + docs/io/inputs.md §3 (input pacer); ADR-R003/R004.

### `[ ]` IN-6 — WebRTC ingest: ICE/DTLS/SRTP transport behind an application-layer media engine · effort: XL · deps: IN-1 (shares the RTP→assembler→FrameProducer bridge)
- **Goal:** Turn the compile-only `webrtc::transport::WebRtcSession` shell into a real receive session, driving the pure SDP negotiation (`negotiate_answer`, mod.rs:266) through ICE/DTLS/SRTP into RTP → the IN-1 assembler → `IngestPump`.
- **Touches:** `crates/multiview-input/src/webrtc/transport.rs` (current shell holds only `NegotiatedSession`); `webrtc/Cargo.toml` feature (currently `"webrtc" = []`); a `WebRtcProducer: source::FrameProducer`. Per the module doc, the concrete media-engine binding "is supplied by the application layer" — so define the trait/seam here, keep the native WebRTC crate out of `multiview-input` to preserve the pure/LGPL-clean default.
- **Approach:**
  1. Define a `MediaEngine` trait in `transport.rs` (the application-layer seam): `start(negotiated) -> rtp packet stream`. `multiview-input` provides the SDP negotiation + the RTP→frame adapter; the ICE/DTLS/SRTP engine (e.g. `str0m`, sans-IO, pure-Rust — verify license before adopting) is wired at the application layer behind the `webrtc` feature so the default build stays pure.
  2. `WebRtcSession::start` consumes `NegotiatedSession` (already held), drives the engine, and routes decrypted RTP payloads (matching the negotiated `Codec`/payload type from `mod.rs`) into IN-1's assembler for video (H264/VP8 keyframe-gated) and the audio rebaser.
  3. Bridge to `FrameProducer` exactly as IN-2 (bounded channel, drop-oldest, never block — invariant #10). RTP timestamp → `raw_pts`, 32-bit wrap.
- **Acceptance (done when):** `crates/multiview-input/tests/webrtc_sdp.rs` stays green (pure negotiation unchanged); a new gated integration test (`#[ignore]` in CI, needs a peer) negotiates an answer and receives RTP through a fake/loopback `MediaEngine`, asserting frames reach the `TileStore`. `cargo check --features webrtc` + `cargo clippy --features webrtc -- -D warnings` green. Re-assert #1/#10 (sampled, never pacing).
- **Risks/notes:** no ICE peer/TURN/real network in CI → engine path is gated/ignored; the sans-IO engine choice must be license-vetted (LGPL-clean default) and kept at the application layer, not pulled into `multiview-input` (which stays `unsafe_code = forbid`, no native WebRTC lib). Highest-effort item; consider phasing (negotiation seam + adapter first, full engine second).
- **Read first:** the `webrtc::transport` module doc (compile-only rationale) + `webrtc/mod.rs` `negotiate_answer`; docs/research/streaming-gotchas.md §7 (RTCP SR lip-sync, jitter); ADR-0009.

### `[ ]` IN-7 — CI strategy: feature-gated compile + integration gating for the wired transports · effort: S · deps: IN-2, IN-3, IN-6
- **Goal:** Ensure the four new gated paths are clippy/check-verified without requiring NICs/peers/SDKs in CI, closing the gap that today's clippy job (ci.yml:32) runs **default features only** and never lints `st2110`/`webrtc`/`ndi`/`youtube`.
- **Touches:** `.github/workflows/ci.yml` (the `fmt + clippy` job @21 and `check + test` job @34).
- **Approach:**
  1. Add a `cargo check`/`cargo clippy -- -D warnings` matrix over `--features st2110`, `--features webrtc`, `--features youtube` (all NIC/network-free to *compile*). Keep `ndi` compile-checked via the dynamic-load backend only (no SDK fetch); the build-time `grafton-ndi` link stays in a separate Docker/CI lane that fetches the SDK (ADR-0008 consequences).
  2. Mark every real-hardware test `#[ignore]` (or `#[cfg_attr(not(feature=…), ignore)]`) so `cargo test --workspace` stays green on shared runners; document a manual/self-hosted lane for loopback ST 2110, an NDI sender, and a YouTube/WebRTC live soak.
  3. Keep `cargo deny check` on **default** features only (ci.yml:69) so the proprietary `ndi` deps never reach the license allowlist.
- **Acceptance (done when):** CI compiles+clippies all four features green with zero native deps for the pure paths; `cargo test --workspace` excludes hardware tests; `cargo deny check` still passes default-only. No new `-D warnings` violations introduced by the gated code.
- **Risks/notes:** the `ndi` build-time-link lane needs the proprietary SDK fetched in Docker (ADR-0008) — keep it off the default LGPL-clean matrix. Don't enable `--all-features` in any deny/license job (would pull `gpl-codecs` + proprietary `ndi` by design).
- **Read first:** ci.yml (current jobs); ADR-0008 consequences (CI/Docker SDK fetch); ADR-0012 (LGPL-clean default).

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-input/src/source.rs
- /workspaces/mosaic/crates/multiview-input/src/st2110/transport.rs
- /workspaces/mosaic/crates/multiview-input/src/webrtc/transport.rs
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-config/src/schema.rs


## CTL — Control plane → engine

Grounding summary (verified by reading the scaffold): the engine's per-tick control hook is `command_drain` at `crates/multiview-cli/src/control.rs:140`, invoked from `EngineRuntime::run_*_with_control` at `crates/multiview-engine/src/runtime.rs:420` (line 420, before `compose`). The drain currently handles only `Command::SwapSource`; all other accepted commands fall into the `_ => false` arm (`control.rs:150`). HTTP submit is non-blocking via `submit_accepted` → `CommandSender::try_submit` (`routes/mod.rs:181`, `command.rs:242`). The shared `EnginePublisher<EngineStateSnapshot, Event>` is `Clone` and `publish_event` is a non-blocking drop-oldest broadcast (`isolation.rs:347`) — so the drain can emit outcome events directly without any new engine channel and without violating inv #10. Sources/Outputs/Overlays CRUD persist opaque `serde_json::Value` bodies to `ResourceRepository` (`resource_store.rs`, `routes/sources.rs`) but never touch the engine. `MultiviewConfig.{sources,cells,overlays,outputs}` are typed (`multiview-config/src/lib.rs:91–100`). `CompositorDrive` exposes `set_layout` and `insert_store` (`drive.rs:127,136`); there is no canvas/output reconfig (Class-2 territory per ADR-R004).

The central design decision threaded through every item: **give the drain closure a cloned `Arc<EnginePublisher<…, Event>>`** so commands apply *and* emit their outcome on the realtime stream from the one place that already runs at the frame boundary, all non-blocking.

---

### `[ ]` CTL-1 — Drain-apply every accepted command + emit outcome events  · effort: L · deps: none
- **Goal:** Make the engine actually apply Start/Stop/ApplyLayout/Arm/Take/CancelSalvo/SetTallyOverride at the frame boundary (today they 202 then no-op), and emit each one's outcome on the event stream, so the WebUI's accepted commands take effect and are observable.
- **Touches:** `crates/multiview-cli/src/control.rs:140` (`command_drain`), `crates/multiview-cli/src/main.rs:206,312` and `:328` (drain construction + `run_until_stopped_with_control` call), `crates/multiview-cli/src/run.rs:393` (signature already threads `publisher`), `crates/multiview-engine/src/isolation.rs:347` (`publish_event`), `crates/multiview-events/src/event.rs` (`SalvoEvent`/`SalvoPhase`/`OutputStatus`/`OutputRunState`).
- **Approach:**
  1. Change `command_drain`'s signature to also take `publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>` (clone of the one already built at `main.rs:179`/`:281`); store it in the returned closure. This is read-share of a `Clone` publisher whose `publish_event` is non-blocking — no new channel, no engine change.
  2. Introduce an internal program-running flag (`bool`) captured in the closure. `Command::Start`/`Stop` flip it; emit an `Event::OutputStatus { state: Running/Stopped, .. }` (`event.rs:107`) correlated to `op`. Wire the realtime layer to set `Envelope::corr` from the op id (see CTL-5 — this item just emits; CTL-5 carries `corr` to the wire).
  3. For `Command::ApplyLayout { layout, .. }`: resolve the named layout from the working config/resource store and `drive.set_layout(Arc::new(solved))` (reuse the existing `solve_layout` + `set_layout` path already proven for `SwapSource` at `control.rs:156`). Emit success/failure outcome.
  4. For salvo commands (`ArmSalvo`/`TakeSalvo`/`CancelSalvo`): a salvo is a named layout+binding recall. Arm stages a solved layout in the closure's state; Take calls `set_layout`; Cancel discards. Emit `Event::SalvoArmed/Taken/Cancelled(SalvoEvent::new(salvo, phase).with_head(head))` (`event.rs:250`). (Salvo body resolution detail shared with CTL-5; full multi-head semantics can be a follow-up — land arm/take/cancel of the salvo's layout here.)
  5. `SetTallyOverride` is the engine's tally arbiter's concern; if the arbiter isn't wired into the software engine yet, keep it accepted and emit a `TallyState` echo, leaving arbitration to the tally stream. Do **not** silently no-op without an event.
  6. Keep every arm panic-free: no `unwrap`/indexing; an unknown layout/salvo logs `tracing::warn!` and emits a failure outcome (mirroring the existing `solve_layout` error handling at `control.rs:164`).
- **Acceptance (done when):** New unit tests in `control.rs` `mod tests`: `start_then_stop_emits_output_status`, `apply_layout_swaps_active_layout` (assert `drive.layout()` id changed), `unknown_layout_emits_failure_not_panic`, `salvo_take_applies_armed_layout`. A drain-loop test asserts that submitting N commands and draining is O(pending) and never awaits. Invariant to re-assert: **#1 output-clock never stalls** and **#10 no engine back-pressure** — add/extend a soak test driving `run_for_with_control` while flooding the bus, asserting `RunReport.faltered == false` and `frames == ticks`. ffprobe is not applicable (no new output here); content check is the composited layout actually changing post-ApplyLayout.
- **Risks/notes:** The drain must stay allocation-light on the hot loop (runtime.rs:413 comment is explicit). `publish_event` is drop-oldest, so an outcome event *can* be dropped under a slow consumer — that is correct per inv #10; never block to guarantee delivery. LGPL-clean (pure Rust, no codec/NDI). No hardware/network needed in CI. Guardrail: no `unwrap`/`as`/indexing in the closure.
- **Read first:** ADR-W013 (path #2 + #3, slice A3), `docs/research/management-capability-matrix.md` §1.3, ADR-W008 referenced for the 202+corr contract.

### `[ ]` CTL-4 — `ApplyLayout` HTTP route  · effort: S · deps: CTL-1
- **Goal:** Add the missing `POST /api/v1/commands/apply-layout` (or `POST /api/v1/program:take`) route so the WebUI can request a layout change — the command enum variant and the drain handler exist, but no HTTP entry point does.
- **Touches:** `crates/multiview-control/src/routes/mod.rs` (alongside `cmd_start`/`cmd_stop`/`cmd_swap` at `:220–265`), the router registration in `crates/multiview-control/src/router/` (where `/commands/*` are mounted), `crates/multiview-control/src/openapi.rs` (path registration).
- **Approach:**
  1. Add a `ApplyLayoutRequest { layout: String }` body type (mirror `SwapRequest` at `routes/mod.rs:244`).
  2. Add `async fn cmd_apply_layout` following `cmd_swap` exactly: `require(Action::Write)`, optional `authorize_object`, then `submit_accepted(&state, &idem, |op| Command::ApplyLayout { op, layout })` (`routes/mod.rs:181`), then `state.audit(.., AuditAction::Command, "layout", &layout, ..)`.
  3. Register the route in the same table as `/commands/start`; add the `#[utoipa::path]` annotation + OpenAPI registration so `/docs` shows it.
- **Acceptance (done when):** New route test (mirroring existing command-route tests): `apply_layout_returns_202_with_op_id`, `apply_layout_requires_write_role` (403 for viewer), `apply_layout_sheds_503_when_bus_full`. Re-assert inv #10: the handler only `try_submit`s (never blocks). OpenAPI doc test asserts the path appears in `/api/v1/openapi.json`.
- **Risks/notes:** Idempotency-Key handling is free via `submit_accepted` (note the release-on-shed logic at `routes/mod.rs:206`). No licensing/hardware concerns. Guardrail: reuse `submit_accepted`; do not hand-roll a second submit path.

### `[ ]` CTL-3 — Mirror `multiview run` config into the resource store at startup + on change  · effort: M · deps: none
- **Goal:** Seed the Sources/Outputs/Overlays (and layouts) resource stores from the loaded `MultiviewConfig` when `bind_and_serve` starts, so those WebUI pages are non-empty under a live run, and keep them in sync as commands mutate the working config.
- **Touches:** `crates/multiview-cli/src/control.rs:78` (`AppState::new` construction in `bind_and_serve`), `crates/multiview-control/src/state.rs:135` (`AppState`/`with_*_store` builders at `:240–257`), `crates/multiview-control/src/resource_store.rs:94` (`ResourceInput` body), `crates/multiview-config/src/lib.rs:91–100` (typed `sources`/`cells`/`overlays`/`outputs`).
- **Approach:**
  1. In `bind_and_serve`, accept the `&MultiviewConfig` (the caller at `main.rs:192`/`:298` already has it). Before constructing `AppState`, build in-memory stores and `create(&id, ResourceInput { body })` one resource per `config.sources` / `config.outputs` / `config.overlays`, with `body = serde_json::to_value(source)?` (each config type is serde-typed). Seed layouts/cells into the layout repository similarly.
  2. Pass the seeded stores into `AppState` via the existing `with_sources_store`/`with_outputs_store`/`with_overlays_store` builders (`state.rs:240–257`) instead of the default empty `InMemory*Store`.
  3. For "on change": when CTL-2 applies a CRUD change to the engine, write the same body back so the store stays authoritative; conversely when CTL-1 applies a `SwapSource`/`ApplyLayout`, update the mirrored cell/layout body. Keep the resource store the single source of truth that the drain reads from.
- **Acceptance (done when):** Integration test: build a config with 3 sources + 2 outputs + 1 overlay, call `bind_and_serve`, then `GET /api/v1/sources` returns 3 id-sorted resources (currently empty). `mirror_roundtrips_source_body` asserts `serde_json::from_value::<Source>(body)` equals the config source. Re-assert inv #10: seeding happens once at bind, off the engine hot loop. No ffprobe applicable.
- **Risks/notes:** Body is opaque JSON by design (`resource_store.rs` doc + `sources.rs:9`), so engine-side validation stays at apply time — don't tighten the store schema. Watch for id collisions (config validation already enforces unique source ids at `config/src/lib.rs:409`). LGPL-clean. Guardrail: `serde_json::to_value` returns `Result` — handle, don't `unwrap`.

### `[ ]` CTL-2 — Apply Source/Output/Overlay CRUD to the running engine via the command bus  · effort: L · deps: CTL-1, CTL-3
- **Goal:** Make a successful `POST/PUT/DELETE` on `/api/v1/sources|outputs|overlays` actually reconfigure the running engine (today it only persists to SQLite), so editing a source/overlay in the WebUI changes the live composite.
- **Touches:** `crates/multiview-control/src/routes/sources.rs:74–135` (create/update/delete), `routes/outputs.rs`, `routes/overlays.rs`, `crates/multiview-control/src/command.rs:81` (the `#[non_exhaustive]` `Command` enum — add variants), `crates/multiview-cli/src/control.rs:140` (drain handler), `crates/multiview-engine/src/drive.rs:136` (`insert_store`).
- **Approach:**
  1. Extend `Command` with `UpsertSource { op, id, body }` / `RemoveSource { op, id }` and overlay equivalents (enum is already `#[non_exhaustive]`, and `kind()`/`operation_id()` at `command.rs:154,169` just need new arms). Outputs that change pinned params are Class-2 (CTL-6); a hot output edit (e.g. bitrate) is a separate non-layout command.
  2. In each mutating handler (after the store write + audit, e.g. `sources.rs:82`), `submit_accepted`-style `try_submit` the corresponding command so the change reaches the drain. Keep the store write authoritative; the bus submit is best-effort (shed→503 only if you choose to gate; for CRUD prefer: persist always, submit non-blocking, surface bus-full as a warning, not a failed write).
  3. In `command_drain`, handle the new variants: deserialize `body` into the typed `Source`/`Overlay`, rebuild the working `MultiviewConfig`, re-solve, and `set_layout` (overlay/cell changes) or build+`insert_store` a new `TileStore` (source add/replace) — `insert_store` already exists at `drive.rs:136`. Emit an outcome event.
  4. Classify each per the capability matrix: most source/overlay edits are **Class-1 (Hot)** (matrix §2.1, §2.6); surface the class via the existing plan/dry-run surface if present, else default Hot.
- **Acceptance (done when):** Tests: `update_source_reaches_engine` (after PUT, drain applies and `drive` reflects the new store/binding), `delete_overlay_removes_layer`, `crud_write_persists_even_when_bus_full` (store write succeeds, command shed logged). Re-assert **#10** (handlers never block; drain non-blocking) and **#1** (soak: continuous CRUD churn, `faltered == false`). Content check: composited frame changes after a solid-source color edit (compare `y_plane`, as `run.rs` tests do at `:704`).
- **Risks/notes:** Deserializing the opaque body can fail — the drain must log+emit-failure, never panic (no `unwrap`). A source add allocates a `TileStore` + synthetic frame; keep that off the tick (build it in the handler or a sibling task, hand the ready `Arc<TileStore>` to the drain) to honour the hot-loop allocation rule. LGPL-clean for synthetic kinds; real-decoder sources need the `ffmpeg` pipeline path (`main.rs:173`), not the software engine — gate accordingly. Guardrail: bound the body size; no indexing.

### `[ ]` CTL-5 — Salvo/Start/Stop outcome events on the realtime stream (corr-correlated)  · effort: M · deps: CTL-1
- **Goal:** Carry each command's outcome to the WebUI on the realtime stream correlated by its operation id, so a 202'd Start/Stop/Salvo shows its eventual result (the ADR-W008 contract: result arrives on the stream, not the HTTP body).
- **Touches:** `crates/multiview-control/src/realtime.rs:158` (`next_delta` — sets the envelope), `:204` (`event_scope_id`), `:213` (`topic_for_event` — already maps `SalvoArmed/Taken/Cancelled`→`Tally`, `OutputStatus`→`Outputs`), `crates/multiview-events/src/envelope.rs:102` (`with_corr`), and CTL-1's emission site.
- **Approach:**
  1. Thread the `OperationId` from the command into the emitted event so the drain can stamp `corr`. Simplest grounded path: carry the op id alongside the event from the drain (e.g. emit events that already know their op) and set `Envelope::with_corr(op)` in `next_delta` — extend `event_scope_id`-style with an `event_corr_id(&event)` helper, OR (cleaner) have the drain publish via a small wrapper that records corr. Pick the helper approach to avoid changing the `Event` enum shape.
  2. Confirm `topic_for_event` already routes the salvo/output events correctly (it does — `realtime.rs:218,228`); just ensure `corr` is populated on those frames.
  3. Verify the SSE path (`sse_handler`, `realtime.rs:385`) and WS path (`run_ws_session`, `:342`) both carry `corr` (they share `SessionStream::next_delta`, so one change covers both).
- **Acceptance (done when):** Tests: `salvo_take_outcome_carries_corr` (drive a take, assert the streamed `Envelope.corr == op`), `output_status_event_on_topic_outputs`. A `SessionStream`-level unit test asserting lagged-skip still holds (re-assert inv #10: a slow client resubscribes, `realtime.rs:191`, never back-pressures). No ffprobe.
- **Risks/notes:** `corr` is `Option<String>` (`envelope.rs:71`) so non-command events stay `corr: None`. Outcome events ride the same drop-oldest broadcast — a lagged client may miss one and re-baseline; that's acceptable and tested. LGPL-clean. Guardrail: no `unwrap` in the projection.

### `[ ]` CTL-6 — Class-2 parallel-output (make-before-break) migration  · effort: XL · deps: CTL-1, CTL-2, CTL-4, CTL-5
- **Goal:** Implement controlled-reset migration for pinned-param changes (codec, geometry-beyond-max, pixel format, GOP structure, canvas fps/resolution) as a new parallel output spun up and cut over while the original keeps running — the only correct way to change pinned params without a downstream-visible falter (ADR-R004).
- **Touches:** new logic in `crates/multiview-cli/src/control.rs` (drain) + `crates/multiview-cli/src/pipeline.rs` (the `ffmpeg` output path — `main.rs:173`), `crates/multiview-output/*` (output session lifecycle), `crates/multiview-control/src/command.rs` (a `MigrateOutput { op, id, new_config, cutover }` command per matrix §2.3 `POST .../migrate`), `routes/outputs.rs` (the migrate route + `POST .../plan` dry-run surfacing `reset_required`), `crates/multiview-events/src/event.rs` (`OutputRunState::Migrating` already exists at `:100`).
- **Approach:**
  1. Add the `plan`/dry-run surface first: a handler that classifies a proposed output change as Class-1/reset-lite/Class-2 per the pinned-param list in ADR-R004 and matrix §1.3, returning `reset_required` **before** apply (this is the inv #11 contract). Pure function over old-vs-new config; no engine change.
  2. Add `Command::MigrateOutput`; in the drain (or a sibling task it signals — migration is heavyweight and must NOT run on the tick loop), stand up a second output session with the new pinned config while the original keeps emitting (`OutputRunState::Migrating`), then atomically cut consumers over and tear down the original (make-before-break, ADR-R004 decision).
  3. Because building/tearing an encoder session is expensive and may block, do it on a **sibling task**, not in `command_drain` — the drain only flips the active-output pointer at a frame boundary once the new session reports ready. This preserves inv #1/#10 (the tick loop never builds an encoder).
  4. Emit `OutputStatus { state: Migrating → Running }` outcomes (CTL-5 carries corr).
- **Acceptance (done when):** Tests: `plan_classifies_codec_change_as_class2`, `plan_classifies_bitrate_change_as_hot`, and a migration integration test asserting the original output never gaps during cutover (the load-bearing claim). ffprobe check: on the **real** output (`ffmpeg` feature), probe both old and new streams across the cutover and assert continuous PTS / no `EXT-X-DISCONTINUITY` on the surviving consumer until cutover, new SPS/PPS only on the new session. Re-assert **#1** (output-clock never stalls during migration) and **#10** (encoder build is off-loop). 
- **Risks/notes:** This is the largest item and the only one needing the `ffmpeg` pipeline + real encoder; software-engine has no output session to migrate, so the end-to-end ffprobe test must run under `--features ffmpeg` (+`gpl-codecs` for software H.264/H.265) — keep the default LGPL-clean build green by feature-gating the migration encoder path and testing the *classifier* (pure Rust) unconditionally. CI without an encoder can run the plan/classifier tests; the cutover ffprobe test needs an `ffmpeg`-featured runner. Guardrail: no encoder construction on the hot loop; no `unwrap` anywhere.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/control.rs
- /workspaces/mosaic/crates/multiview-control/src/command.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/mod.rs
- /workspaces/mosaic/crates/multiview-control/src/realtime.rs
- /workspaces/mosaic/crates/multiview-cli/src/main.rs


## PRV — Preview & WebRTC transport

Grounded against `docs/research/preview-subsystem.md`, ADR-P001..P005, ADR-0006, and the current scaffold: `crates/multiview-preview/src/{whep.rs,tap.rs,encode.rs,framing.rs,token.rs}`, the engine isolation primitive (`crates/multiview-engine/src/isolation.rs:141` `EventStream`/`EventSubscription`, drop-oldest broadcast), the output fan-out (`crates/multiview-output/src/fanout.rs:105` `PacketSink`, `:117` `PacketRouter`), the LL-HLS segmenter (`crates/multiview-output/src/hls/media.rs`), the control seam (`crates/multiview-control/src/preview.rs:21` `PreviewProvider`, `routes/preview.rs`), and the HAL ladder (`crates/multiview-hal/src/degradation.rs:49`).

Key facts that shape the plan: WHEP today is **pure SDP/codec negotiation only** (`whep.rs:111` `WhepSession::negotiate`); there is **no native dep** in the workspace (no str0m/webrtc-rs/gstreamer) and the `webrtc` feature pulls none by design. The `PreviewProvider` trait in control is JPEG-only — there is no WHEP/SDP route wired in control yet. The degradation ladder (`degradation.rs:49`) has **no preview rung** despite ADR-P001/P005 mandating preview be shed first.

---

### `[ ]` PRV-1 — Native ICE/DTLS/SRTP transport behind a `WhepTransport` seam (str0m, in-process default)  · effort: XL · deps: none
- **Goal:** Turn the WHEP SDP scaffold into a working sub-250ms focus media path by adding a transport seam and a native (str0m) implementation, so `negotiate()` produces an answer that actually carries SRTP video — without coupling preview to the engine.
- **Touches:** `crates/multiview-preview/src/whep.rs` (extend `WhepSession`, `whep.rs:91`), new `crates/multiview-preview/src/whep/transport.rs`; `crates/multiview-preview/Cargo.toml` (new optional `str0m` dep gated under a *new* `webrtc-native` feature, kept separate from the existing pure `webrtc` feature so the negotiation-only build stays dep-free per `lib.rs:38`); `Cargo.toml` workspace `[workspace.dependencies]`; `deny.toml` (license/advisory entries for str0m + ring/openssl chain).
- **Approach:**
  1. Define a `trait WhepTransport { fn accept(&self, offer:&str, codec:PreviewCodec, media: PreviewMediaSource) -> Result<TransportAnswer>; fn close(&self, session_id); }` in `transport.rs`. `WhepSession::negotiate` keeps doing pure codec selection (`select_codec`, `whep.rs:199`); the transport fills ICE ufrag/pwd, DTLS fingerprint, bundle/mid into the answer that `build_answer_sdp` (`whep.rs:209`) currently leaves as `0.0.0.0` placeholders — refactor `build_answer_sdp` to accept transport-supplied attributes rather than hard-coding.
  2. Add `Str0mTransport` under `#[cfg(feature = "webrtc-native")]`: str0m is sans-IO (pure Rust, no C, no openssl — preferred for LGPL-clean default and the lean-binary goal), drive its `Rtc` state machine on a dedicated tokio task in **Tier A** (per ADR-P001), feeding it RTP from the preview encoder (PRV depends on the encoder pool — wire to `PreviewMediaSource` which pulls from a `TapLease`, `tap.rs:202`).
  3. The media source reads NV12 from a `TapLease::recv` (`tap.rs:224`) → preview H.264 encode → packetize → str0m `write`. Encoder selection already returns `PreviewCodec` (`whep.rs:127`); H.264 baseline preferred.
  4. Keep MediaMTX sidecar as an *alternative* `WhepTransport` impl stub (republish path) per ADR-0006 "Sidecar reuse" — document it as the v1 fallback terminator; do not build it fully here, just leave the seam.
- **Acceptance (done when):** New `tests/whep_transport.rs` (TDD): (a) `negotiate` + `Str0mTransport::accept` yields an answer with non-placeholder ICE ufrag/pwd + DTLS fingerprint + `a=candidate`; (b) a loopback integration test (feature-gated, behind `#[ignore]` unless a runtime env flag is set, since UDP/STUN in CI is unreliable) establishes DTLS-SRTP and decodes ≥1 H.264 NAL via a str0m client. ffprobe check: pipe the egress to ffprobe and assert `codec_name=h264` + resolution = the preview thumbnail size. Re-assert **inv #10**: the media source only holds a `TapLease` (drop-oldest); add an assertion that the transport task holds no `EventStream` publish handle and never awaits the engine.
- **Risks/notes:** Prefer str0m to keep the default build LGPL-clean and C-free; webrtc-rs pulls a heavier stack. Gate strictly behind `webrtc-native` (off by default) so `cargo deny`/the pure build stay green (`lib.rs:43` `forbid(unsafe_code)` must hold — str0m is safe Rust). UDP/STUN/TURN availability in CI is unreliable → loopback-only, env-gated. No `unwrap`/`as`/indexing in non-test (RRTP seq/timestamp arithmetic must use checked/`wrapping_*`).
- **Read first:** preview-subsystem.md §4 (transport table) + §8 "Sidecar reuse"; ADR-P002; ADR-0006.

### `[ ]` PRV-2 — Wire WHEP focus routes into `multiview-control` (POST/DELETE per scope) with token-gated Focus + transport seam  · effort: L · deps: PRV-1
- **Goal:** Expose the WHEP focus endpoints the brief §5 specifies so the SPA can actually open a focus session, enforcing `AccessScope::Focus` and the focus cap at the HTTP edge.
- **Touches:** `crates/multiview-control/src/routes/preview.rs` (add handlers alongside `program_jpeg`, `routes/preview.rs:35`), `crates/multiview-control/src/routes/mod.rs` (route registration), `crates/multiview-control/src/preview.rs` (extend `PreviewProvider` seam, `preview.rs:21`, with a `whep_negotiate`/`whep_close` capability OR a sibling `WhepProvider` trait so control stays codec-free), `crates/multiview-control/src/state.rs` (hold the new provider in `AppState`).
- **Approach:**
  1. Add `POST /api/v1/preview/program/whep`, `POST /api/v1/preview/inputs/{id}/whep`, `POST /api/v1/preview/outputs/{id}/whep` (+ matching `DELETE …/{session_id}`) per brief §5. Each: SDP offer body in (`Content-Type: application/sdp`), `201 Created` + `Location:` resource URL + answer SDP body out; `503` with `application/problem+json` (RFC 9457 per control CLAUDE.md) + `fallback: ws-jpeg|llhls` hint when cap hit or HW budget unavailable.
  2. Verify `AccessScope::Focus` via the existing `TokenIssuer::verify` (`token.rs:270`) — control must map its `Principal`/`Action` (`routes/preview.rs:13`) to a minted preview token scoped to the exact `TapKey`. Reuse the `WhepError::AccessDenied` → `403` mapping.
  3. Delegate the actual negotiation+transport to a `WhepProvider` the binary implements (same isolation discipline as `PreviewProvider`); control never links str0m.
- **Acceptance (done when):** `tests/preview_whep_routes.rs` in control (TDD): View token → `403`; malformed SDP → `400`; valid Focus offer → `201` + `Location` + `application/sdp` answer; `DELETE` of the resource URL → `204` and frees the session (assert subscriber count via descriptor drops to 0). OpenAPI (`openapi.rs`) regenerates with the new paths. Inv #10: assert the handler path holds only a `TapLease`/`WhepProvider` handle, never the engine.
- **Risks/notes:** Auth/SSRF: cue/whep schemes must stay allowlisted (ADR-P004) — not in scope here but the route must reject non-allowlisted ids. Keep control free of native deps (the seam is a trait object). RFC 9457 problem+json for all error bodies. No `unwrap` in handlers.
- **Read first:** preview-subsystem.md §5 (API tables) + §2; ADR-P002; control CLAUDE.md API conventions.

### `[ ]` PRV-3 — Concurrent-focus session caps + isolation enforcement (the `FocusGate`)  · effort: M · deps: PRV-2
- **Goal:** Bound worst-case preview load deterministically — hard caps on concurrent WHEP focus sessions (per-operator and server-wide) with "open second focus demotes the first," and admit focus encode sessions only from leftover budget after program reserves first.
- **Touches:** new `crates/multiview-preview/src/focus.rs` (a `FocusGate` admission gate); `crates/multiview-preview/src/lib.rs` (re-export, `lib.rs:57`); integrate with `crates/multiview-hal/src/cost.rs` (`CostBudget`, `cost.rs:24`) and `crates/multiview-hal/src/degradation.rs` for shed-first; `crates/multiview-preview/src/token.rs` (the `AccessScope::Focus` cap is "only enforceable when Focus is granted explicitly," `token.rs:106` — the gate is where that promise is kept).
- **Approach:**
  1. `FocusGate` holds an atomic/Mutex map of active focus sessions keyed by operator + a server-wide counter, both with configured caps (default e.g. 1/operator per ADR-P002, N server-wide). `try_acquire(operator, key) -> Result<FocusLease, AdmissionDenied>`: enforce per-operator cap by **demoting** (closing) the operator's prior focus lease, and the server-wide cap by returning `AdmissionDenied { fallback: WsJpeg|LlHls }` (drives PRV-2's `503` hint).
  2. Admission against HAL: before granting, check a preview-encode session is available from leftover budget (program sessions reserved first per ADR-P001 §8). Reuse `CostBudget` (`cost.rs:24`); do **not** invent a parallel resource model.
  3. `FocusLease` Drop releases the slot (mirrors `TapLease` Drop, `tap.rs:245`), so a dropped/timed-out session frees the cap.
- **Acceptance (done when):** `tests/focus.rs` (TDD): (a) opening a 2nd focus for one operator returns a lease and the 1st lease is closed/demoted; (b) server-wide cap+1 returns `AdmissionDenied` carrying a `fallback`; (c) dropping all leases returns active count to 0 (idle-cost invariant, ADR-P003); (d) a property test that the live focus count never exceeds the cap under concurrent acquire/release. Inv #10: the gate uses only a short-lived `Mutex` the engine never touches (same pattern as `TapRegistry`, `tap.rs:26`).
- **Risks/notes:** Base Apple silicon = 1 encode engine (brief §4) → cap WHEP to 1 and prefer JPEG; make caps config-driven, never hard-coded (probe via HAL). No `unwrap`/indexing; saturating arithmetic on counters.
- **Read first:** preview-subsystem.md §3 ("CAP CONCURRENCY") + §8 ("Shared budgets"); ADR-P003; ADR-P002.

### `[ ]` PRV-4 — Make preview the topmost (cheapest-to-shed) degradation rung  · effort: M · deps: PRV-3
- **Goal:** Honor the non-negotiable ADR-P001 guarantee that preview is shed *before any program lever moves*, by extending the HAL ladder so preview suspension precedes rung 0.
- **Touches:** `crates/multiview-hal/src/degradation.rs` (`DegradationAction` enum `:27`, `LADDER` `:49`, `rung` `:62`, `affects_program` `:81`); `crates/multiview-preview/src/focus.rs` (a `suspend()`/`resume()` driven by the ladder); the binary glue that observes `Hysteresis` (`degradation.rs:202`).
- **Approach:**
  1. Prepend new cheapest rungs to the ladder *above* `DropTileResolution`: `ShedFocusWhep` → `DropPreviewGridFps` → `DropPreviewGridRes` → `DropOffAirCueDecoders` → `SuspendPreviewEntirely`, matching brief §8's "topmost rung" list. Update `LADDER`, `rung()`, `MAX_LEVEL` (`:87`), and crucially `affects_program()` (`:81`) so the program-affecting boundary shifts to *after* all preview rungs.
  2. Wire the planner's `Hysteresis::observe` (`degradation.rs:251`) consumer to call `FocusGate::suspend()` (PRV-3) on the preview rungs, returning `503`/`fallback` to in-flight focus opens. Log every preview adaptation like any other shed action (operator trust, ADR-P003).
- **Acceptance (done when):** `degradation.rs` tests extended (TDD): assert preview rungs occupy levels below the first program rung, `affects_program()` is `false` for all preview rungs and `true` only at the first tile/output lever, and a test that climbing pressure sheds *all* preview rungs before any `DropTile*`/`LowerOutput*`. The PRV-1/PRV-2 no-back-pressure chaos test must still pass with preview suspended. Inv #9 (cheapest-impact-first) + inv #10 re-asserted.
- **Risks/notes:** This changes a load-bearing enum in the protected-core HAL — coordinate with the engine/HAL work-stream; the change is additive (prepending rungs) but `MAX_LEVEL` shifts, so any persisted level mapping must be migrated. No behavior change to existing rungs' relative order.
- **Read first:** preview-subsystem.md §3 + §8 ("First on the degradation ladder"); ADR-P001; multiview-engine CLAUDE.md inv #9/#10.

### `[ ]` PRV-5 — Sub-second WebRTC OUTPUT (program) focus: program-canvas tap → preview encode → WHEP  · effort: L · deps: PRV-1, PRV-2, PRV-3
- **Goal:** Deliver sub-second program (and per-output) focus over WebRTC — the "OUTPUT/PROGRAM focus" rows of brief §4 — built on the same transport, gate, and tap seams as input focus, with mandatory real-vs-approx labeling for outputs.
- **Touches:** `crates/multiview-preview/src/tap.rs` (a program-canvas `TapKey` of `TapScope::Program` and per-output `TapScope::Output`, `token.rs:35`); `crates/multiview-output/src/fanout.rs` (register a *separate* preview `PacketSink` on the existing `PacketRouter::register`, `fanout.rs:133`, depth-1-3 drop-oldest per ADR-P001 — never the encoder readback ring); the preview encode source in `whep/transport.rs` (PRV-1); a `preview/source` descriptor (`routes/preview.rs`) reporting `REAL ENCODED OUTPUT (tap:<proto>)` vs `PRE-ENCODE CANVAS APPROX` per ADR-P005.
- **Approach:**
  1. PROGRAM focus: append a GPU downscale blit into a dedicated preview ring (own `EventStream`, `isolation.rs:188`), skipped entirely when subscriber count is 0 (ADR-P003 conditional tap). The `TapLease` feeds the PRV-1 H.264 preview encode → str0m egress. Label always `PRE-ENCODE CANVAS APPROX`.
  2. OUTPUT focus: register a preview `PacketSink` (`fanout.rs:105`) on the target `RenditionId` that O(1)-clones `Arc<EncodedPacket>` into a depth-1-3 drop-oldest ring, decode-back at reduced res (`skip_frame=nokey`), re-encode small, egress. Label `REAL ENCODED OUTPUT (tap:<protocol>)`. For HLS-family outputs, prefer redirect to the published playlist (zero extra encode, ADR-P005) instead of a WHEP encode.
  3. Reuse PRV-3 `FocusGate` for caps and PRV-4 shed-first.
- **Acceptance (done when):** `tests/program_output_whep.rs`: PROGRAM focus with 0 subscribers performs **no blit** (assert the conditional-skip), first subscriber starts it, last-leave + linger stops it. OUTPUT focus registers exactly one extra `PacketSink` and the `route()` count (`fanout.rs:168`) increases by 1 (encode-once preserved — no second encode of the canvas). ffprobe the OUTPUT WHEP egress and assert its color/resolution matches the *tapped rendition*, not the pre-encode canvas. `GET …/preview/source` returns the correct label. Chaos: SIGKILL/stall the program-focus consumer and assert byte-for-byte-unchanged program output + zero added frame-interval jitter (ADR-P001 hard gate, inv #1 + #10).
- **Risks/notes:** Output decode-back adds decode-engine load (worst on Intel/AMD/CPU, ADR-P005) → reduced-res + I-frames-only + capped; HLS outputs take the zero-decode published-playlist path. The program downscale blit needs GPU; in CI without a GPU, gate the blit test on a software/headless path or `#[ignore]`. Never share the encoder NV12 readback ring (ADR-P001 — explicit audit). gpl-codecs/ndi stay opt-in; NDI output preview is a convention-tagged host-frame tap, not a VUI-tagged bitstream (ADR-P005).
- **Read first:** preview-subsystem.md §1 (three scopes) + §2 (isolation table) + §7 (mermaid taps); ADR-P005; ADR-P001.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-preview/src/whep.rs
- /workspaces/mosaic/crates/multiview-preview/src/tap.rs
- /workspaces/mosaic/crates/multiview-engine/src/isolation.rs
- /workspaces/mosaic/crates/multiview-output/src/fanout.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/preview.rs


## ENG — Engine timing & resilience


### `[ ]` ENG-1 — Bounded teardown join for a wedged sink (task #50) · effort: M · deps: none
- **Goal:** Make `drive_streaming` always return on stop even when an output sink thread is wedged in a blocking muxer/network write, so no infinite hang on teardown — without touching inv #1 (the engine already emits all N ticks past a wedged sink).
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `StreamEgress::join` (`pipeline.rs:1629`), the teardown sequence at `pipeline.rs:1088` (`egress.join()`), and the existing precedent `IngestSupervisor::join_all` (`pipeline.rs:2269`) + `INGEST_JOIN_GRACE` (`pipeline.rs:3128`). New test alongside `crates/multiview-cli/tests/streaming_encode.rs`.
- **Approach:**
  1. Mirror `IngestSupervisor::join_all`'s bounded-detach loop into `StreamEgress::join`: replace the unconditional `self.consumer.join()` and per-sink `handle.join()` with a `JoinHandle::is_finished()` poll loop against a deadline `Instant::now() + EGRESS_JOIN_GRACE` (add the const next to `INGEST_JOIN_GRACE`, value ~2 s as in ADR-0026 §5 "mirroring `INGEST_JOIN_GRACE`").
  2. Join the consumer first (it drops sink senders on exit, unblocking each sink's `rx.recv()` with `Err` — the exact mechanism the existing `streaming_encode` blocked-runner test relies on, lines 117-123). A sink still unfinished after the grace window is **detached + logged** (`tracing::warn!`), and `join` returns a partial `EgressOutcome` rather than blocking. Detach is safe: sinks own their own libav muxer state freed in `Drop`, reaped at process exit (same justification as the ingest detach comment, `pipeline.rs:2264`).
  3. Preserve the finalize-on-error contract: a sink that *does* finish within grace still has its trailer written / playlist flushed (`pipeline.rs:1640`).
- **Acceptance (done when):**
  - New `#[tokio::test]` `wedged_sink_teardown_returns_within_grace` (peer of `live_blocked_sink_stays_bounded_and_never_stalls`): a runner that pulls one frame then blocks on a recv that *never* sees disconnect (e.g. holds its own clone of the sender, or `loop { park }`) — assert `drive_streaming_for_test` returns `Ok` and wall-time-to-return < grace + margin; assert a detach was logged.
  - Re-assert **inv #1** unchanged: `result.report.frames == TICKS` still holds (the engine loop already returned before teardown). Re-assert **inv #10**: `peak_occupancy <= capacity + 1` unchanged.
  - Existing `streaming_encode.rs` tests stay green (the well-behaved detach-on-disconnect path must not regress to a premature detach).
- **Risks/notes:** No new deps, no unsafe, no licensing change. Guardrail: the detach path must not `unwrap`/panic on a poisoned handle — match and log. Risk: detaching a sink mid-trailer yields a truncated artifact; that is the correct trade (a wedged sink can't produce a valid trailer anyway) and must be reported, not hidden (ADR-0025 honest-falter). This is the standalone slice of ADR-0026 §5; land it independently of the encoder hoist.
- **Read first:** ADR-0026 §5 (bounded teardown folds in #50); resilience-and-av §2.2 ("hard timeout + process kill — a worker wedged in FFI never observes the token").

---

### `[ ]` ENG-2 — Input PTS normalizer + pacer reroute (ADR-0021 points 1-3) · effort: XL · deps: none
- **Goal:** Wire the existing-but-unused `multiview-input::PtsNormalizer` + `Pacer` + `ReorderBuffer` into the file/VOD + live ingest loop so wrap/discontinuity/jitter classes are handled (latch-on-tick already fixed the race/freeze; this adds the *additional* class ADR-0021 §"Not built" tracks), retiring the ad-hoc `PtsWallClock`.
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `ingest_loop` / `open_and_stream` path and the `PtsWallClock` read-ahead throttle; `crates/multiview-input/src/normalize.rs` (`PtsNormalizer`), `src/pacer.rs` (`Pacer`), `src/jitter.rs` (`ReorderBuffer`) — confirm these exist and their unit tests; `crates/multiview-ffmpeg::StreamVideoDecoder` (currently keeps the cadence-derived genpts via `with_declared_fps` per the As-built note — decide whether genpts moves to `PtsNormalizer` or stays).
- **Approach:**
  1. First read `crates/multiview-input/src/normalize.rs` + `pacer.rs` + `jitter.rs` and their tests to confirm the exact `normalize(raw, master_now_ns)` / `submit(pts, now_ns)` signatures (ADR-0021 §Consequences says they are pure functions returning emitted `MediaTime` / `Release` decisions).
  2. Route each decoded frame's `best_effort_timestamp`-or-`pts` (+ the stream `r_frame_rate`) through `PtsNormalizer` (33-bit/32-bit delta-unwrap into i64, genpts from declared cadence, discontinuity re-anchor on `EXT-X-DISCONTINUITY`/TS indicator/`|jump|>~10 s`, first-frame anchor, strict-monotonic guard) before stamping the `TileStore` publish, replacing the source-relative `pts − first_pts` stamping the As-built fix shipped.
  3. Keep the latch-on-tick sampling as the *primary* correctness mechanism (do **not** regress `framestore::read_at`); the pacer/normalizer is the ingest-gate refinement. Decide genpts ownership: ADR-0021 §18 wants it in `PtsNormalizer` (cadence from per-input `r_frame_rate`), but the As-built note says the decoder's `with_declared_fps` already meets cadence-correctness — pick one to avoid double genpts and document it.
- **Acceptance (done when):**
  - The ADR-0021 §Consequences "adversarial matrix" is covered by deterministic tests (injected clock, no sleeping): CFR 24/25/30, VFR, B-frames/non-monotonic-received, no-PTS, mpegts ~1.44 s offset, mid-stream discontinuity, PTS gap, **33-bit/32-bit wrap boundary**, off-output-fps 24/25/29.97/30/50/60. The decisive guard from §Consequences: *no-PTS-at-25 + 24 fps output emits a smooth measured-cadence schedule* (must fail the old 29.97 constant).
  - **Content-aware end-to-end** (not part of the flake-free unit gate, but the verification owner): overlays-OFF `tblend=difference`→`signalstats` YAVG vs a ground-truth ffmpeg encode across a synthetic wrap boundary — render real-time, rendered motion ≈ ground-truth (the §53 correction mandates content-aware, never overlay-laden).
  - Re-assert **inv #1**: the pacer gates *ingest only*; the output `out_pts = f(tick)` is untouched (no test may show output cadence changing with a bursting/wrapping source). Re-assert **inv #10**: ingest feeds the lock-free store, no back-pressure to the engine.
- **Risks/notes:** Largest item; the wrap-boundary class is exactly the "ran fine for an hour, fails overnight" trap (ADR-T003 §Consequences) — synthetic-timestamp tests are mandatory, real soak is separate (≥72 h zero-gap, ADR-0021 §"Soak/GPU tier"). No new deps, no unsafe. Guardrail: all time stays i64 ns / exact rationals (inv #3); no `unwrap`/`as` on the unwrap arithmetic — use checked/`i128` intermediates as `PtpServo` does.
- **Read first:** ADR-0021 (esp. §As-built + §"Not built"), ADR-T003 (the unwrap/genpts/monotonic decision), streaming-gotchas §0.

---

### `[ ]` ENG-3 — NTP/PTP lock auto-detect for the wall-clock badge (task #37) · effort: M · deps: ENG-5 (shares the syscall binding)
- **Goal:** Replace the *assumed* `RefStatus::Locked` on the on-screen clock badge with a *measured* kernel lock-state (Linux `adjtimex`, macOS `ntp_adjtime`), so the overlay clock honestly shows Locked / Holdover / Freerun (`RefStatus` already has all three — `crates/multiview-overlay/src/clock.rs:281`).
- **Touches:** `crates/multiview-cli/src/wallclock.rs` — `SystemWallClock::reference()` (`wallclock.rs:118`) currently returns the static `status` field; `RefStatus` enum (`multiview-overlay/src/clock.rs:281`, `.is_locked()` at :322 covers Locked|Holdover). The binding cannot live in `multiview-cli` (`#![forbid(unsafe_code)]`, `wallclock.rs:33`).
- **Approach:**
  1. Add a tiny, sampled lock-status reader behind the same syscall binding ENG-5 introduces (a `nix`-style safe wrapper, or a `deny(unsafe_code)` FFI shim sub-crate). On Linux call `adjtimex`/`clock_adjtime` and map: `STA_UNSYNC` set ⇒ `Freerun`; synced + within tolerance ⇒ `Locked`; synced but flagged stale/holdover ⇒ `Holdover`. macOS `ntp_adjtime` analog.
  2. Make `SystemWallClock` sample this **at draw time** through the existing injectable `WallClock::reference()` seam — keep it a pure read, off the hot path, never pacing (the module doc's anti-drift contract, `wallclock.rs:39-45`).
  3. Default-safe: if the syscall is unavailable (container without the cap, unknown platform), fall back to the current assumed status rather than panicking — the `FakeClock` test seam (`wallclock.rs:182`) already proves injectability.
- **Acceptance (done when):**
  - New tests asserting the mapping (injected raw `adjtimex` result → expected `RefStatus`), including the `STA_UNSYNC`→`Freerun` and unavailable→fallback arms; the existing `system_clock_default_reports_sys_locked` test must be updated to reflect measured status (or kept for the fallback path).
  - The badge renders text+glyph for all three states (a11y: never colour alone — `clock.rs:275`), verified by the overlay render test.
  - Re-assert **inv #1**: lock status is *sampled at draw*, never pacing; advancing the injected clock still advances displayed time-of-day independent of lock state.
- **Risks/notes:** CI has no real PTP/NTP grandmaster — the syscall returns a value (usually `Freerun`/`STA_UNSYNC` in a container), so the *real* read is exercised but the *Locked* assertion must use the injected mapping, not a live grandmaster. Unsafe is confined to ENG-5's binding. Licensing: `nix`/`rustix` are MIT — LGPL-clean.
- **Read first:** `crates/multiview-cli/src/wallclock.rs` module docs (§"Reference status — honest about what we can detect"); ADR-T003/T001 for the timing-reference posture.

---

### `[ ]` ENG-4 — Linux i915/amdgpu GPU load probe · effort: L · deps: none
- **Goal:** Implement the real `SysfsLoadProbe` (currently returns `LoadSample::Unavailable { reason: "Linux sysfs/i915 PMU load probe not yet implemented" }`, `load.rs:630`) so the scheduler gets live AMD/Intel `DeviceLoad` snapshots — sampled off the hot path at 1-4 Hz, never pacing (ADR-0017).
- **Touches:** `crates/multiview-hal/src/load.rs` — `linux_sysfs` module (`load.rs:600-633`), gated behind `vaapi`/`qsv`; mirrors the `NvmlLoadProbe` reference impl (`load.rs:491+`). Telemetry gauges in `multiview-telemetry` (ADR-0017 §Consequences: register only known metrics).
- **Approach:**
  1. `devices()`: enumerate `/sys/class/drm/card*/device/` render nodes, read `vendor`/`device` PCI ids to classify AMD vs Intel and build a stable `DeviceId` from the **PCI bus id** (not the enumeration index — `load.rs:61` identity rule).
  2. AMD `sample()`: read `gpu_busy_percent` (sysfs) → `gpu_busy_frac`; `mem_info_vram_used`/`mem_info_vram_total` → VRAM bytes; treat VCN4+ enc/dec as the **merged "Media engine"** figure (ADR-0017 §Rationale) — populate `enc_util_frac`/`dec_util_frac` only if a per-engine source exists, else leave `None` (honest unknown, never fabricated zero, `load.rs:140`).
  3. Intel `sample()`: MVP via DRM **fdinfo** per-engine `drm-engine-*` counters (plain file reads, **no unsafe**); defer the i915 **PMU** `perf_event_open` path (that one needs `unsafe`) to a follow-up — sysfs/fdinfo is the safe-first landing the scaffold comment already promises.
  4. All reads are plain `std::fs::read_to_string` + parse with checked conversions; any missing/garbled file ⇒ `LoadSample::Unavailable` (graceful, `load.rs:628`), never a panic.
- **Acceptance (done when):**
  - Unit tests over **fixture sysfs/fdinfo text** (golden files, injected path root) asserting parse → `DeviceLoad` fields, including the malformed-file → `Unavailable` arm and the VCN4 merged-media arm. The `vram_frac` clamp tests (`load.rs:678`) already model the over-total guard — reuse.
  - The `select` policy tests still pass with a blind-field probe (ADR-0017 §Consequences: "a blind-vendor probe falls back to VRAM + overall-busy without blocking placement").
  - Re-assert **inv #1 (chaos gate)**: "the probe can never stall the engine" — a hung/slow sysfs read inside `LoadPoller::poll` (called on the engine's dedicated blocking thread, `load.rs:395`) must be bounded; selection happens at admission/reconfig only, never per-frame.
- **Risks/notes:** CI has **no AMD/Intel GPU** — real-hardware sampling is unverifiable here, so tests run against captured fixtures (the same posture as NVML's graceful-init); a hardware soak is a separate tier. The fdinfo walk is a §5-risk-7 cost on tiny boxes — keep it inside the clamped `PollInterval` (1-4 Hz, `load.rs:438`). No unsafe for the sysfs/fdinfo MVP; PMU deferred. Licensing: pure `std::fs`, no new deps — LGPL-clean.
- **Read first:** ADR-0017 §Decision + §Rationale (vendor-asymmetric metric matrix); gpu-monitoring-and-scheduling §1-§2.5.

---

### `[ ]` ENG-5 — PTP / ST 2059 PHC NIC binding (`ptp` feature) · effort: L · deps: none (blocks ENG-3's syscall shim)
- **Goal:** Bind the disciplined-reference servo (`PtpServo`, fully tested, `ptp.rs`) to a real PTP Hardware Clock: read the host PHC and feed `(offset, delay)` samples into the servo, behind the off-by-default `ptp` feature — sampled, **never pacing** the output clock.
- **Touches:** `crates/multiview-engine/src/ptp.rs` — the `phc` module (`ptp.rs:263-306`), currently compile-only wrapping `DisciplinedReference`. Feature `"ptp" = []` (`engine/Cargo.toml:44`). Engine forbids unsafe (`lib.rs:87`), so the raw `clock_gettime(dynamic_clock_from_fd)`/`clock_adjtime` calls need a binding boundary.
- **Approach:**
  1. Add `nix` (or `rustix`) as an **optional dep pulled in only by `ptp`** to get safe wrappers for opening `/dev/ptpN`, `clock_gettime` on the dynamic clock id (`FD_TO_CLOCKID`), and `PTP_SYS_OFFSET*` ioctls — avoiding a local `unsafe` override in the engine. If a needed ioctl isn't wrapped, isolate it in a thin `deny(unsafe_code)` + `// SAFETY:` sub-module gated by `ptp` (the workspace lint posture, `Cargo.toml:43`).
  2. Implement a `PhcReader` that, per sample tick, reads PHC-vs-system offset and path delay and constructs `PtpSample::new(offset_ns, delay_ns)` (the existing constructor clamps negative delay, `ptp.rs:58`), feeding `DisciplinedReference::apply` (`ptp.rs:290`).
  3. Run the reader on a dedicated sampled thread (like `LoadPoller`), publishing the servo's `offset_ns`/`frequency_ppb` into a wait-free `LatestState`/`ArcSwapOption` snapshot (`isolation.rs:56`) — the engine *reads* the estimate, never gates a tick on it.
- **Acceptance (done when):**
  - `cargo build -p multiview-engine --features ptp` and `clippy` clean (compile-verified — this env has no PTP NIC, as the module doc states `ptp.rs:18-19`).
  - The servo math stays under its existing `ptp_servo.rs` property tests (unchanged); add a `PhcReader` test with an **injected fake PHC source** (offset/delay it controls) proving samples flow to the servo and the published snapshot tracks `servo.offset_ns()`.
  - Re-assert **inv #1 — the load-bearing one for this stream**: a test proves the output `OutputClock` tick count is identical with the PTP reader producing wild/absent samples vs. with it off; the servo disciplines only the *separate reference estimate* (`ptp.rs:21-35`). PHC is **sampled, never pacing**.
- **Risks/notes:** CI/devcontainer has **no PTP-capable NIC** — runtime-verified only via the injected fake; real verification is a hardware tier. `ptp` is opt-in (default LGPL-clean build never links it). The `unsafe` boundary must be minimal and `// SAFETY:`-documented (engine is `forbid(unsafe_code)` — the override is local to the gated module only). Guardrail: integer ns / ppb only (inv #3), no float fps.
- **Read first:** `ptp.rs` module docs (§"Invariant #1 is preserved"); ADR-T003; core-engine §11 ("no genlock/PTP over arbitrary RTSP/HLS — PTP only for ST 2110 uncompressed-over-IP").

---

### `[ ]` ENG-6 — HA cluster peer transport (`cluster` feature) · effort: L · deps: none
- **Goal:** Implement a real `ClusterTransport` (peer heartbeat sockets + replication snapshot/delta wire I/O) behind the off-by-default `cluster` feature, driving the already-tested `HaRunner`/`HaStateMachine`/`ReplicaApplier` — best-effort, drop-oldest, never back-pressuring the active engine.
- **Touches:** `crates/multiview-engine/src/ha/transport.rs` — the `ClusterTransport` trait (`transport.rs:35`) and `HaRunner` (`transport.rs:90`) are done and pure; only a concrete socket impl is missing. `repl.rs` `EngineSnapshot`/`ReplicationDelta` are already serde-tagged (`repl.rs:117`). Feature `"cluster" = []` (`engine/Cargo.toml:52`).
- **Approach:**
  1. Implement a `UdpClusterTransport` (or a small TCP variant) satisfying `ClusterTransport`: `publish_heartbeat` serialises a `Heartbeat` and does a non-blocking `send` (drop on `WouldBlock` — the trait contract is "must never block or back-pressure", `transport.rs:36-43`); `poll_heartbeat` is a non-blocking `recv` returning `None` when empty (`transport.rs:48`).
  2. `publish_snapshot`/`publish_delta` serialise via the existing serde models (`repl.rs:73,117`) — JSON for cross-node wire per the repl doc (`repl.rs:71`); `poll_replication` deserialises into `ReplicationMessage` (`transport.rs:74`). Bound the inbound queue (drop-oldest) so a flooding peer can't grow memory.
  3. Wire under an off-hot HA thread that calls `HaRunner::pump_heartbeats`/`pump_replication`/`beat` (`transport.rs:130,142,156`) on the injected `MediaTime` — the runner already makes every *decision* via the pure model; the socket layer only moves bytes.
- **Acceptance (done when):**
  - `cargo build -p multiview-engine --features cluster` + clippy clean (compile-verified — no live multi-node cluster here, `transport.rs:1-8`).
  - A **loopback two-instance test** (two transports on localhost): node B promotes when node A stops beating past the miss deadline, and a `LayoutSwap` delta replicates A→B and applies contiguously (reusing `ha_failover.rs`/`ha_replication.rs` model tests over the real transport). A dropped delta surfaces `ApplyError::VersionGap` and triggers a snapshot re-request (`repl.rs:285`), proving no silent divergence.
  - Re-assert **inv #1 + #10**: a partitioned/flapping/slow link changes only *when a standby decides to promote*, never the active's `out_pts = f(tick)` or its send path — a chaos test with a black-holed socket shows the active's tick count unaffected (`mod.rs:23-44`).
- **Risks/notes:** Loopback is testable in CI; true multi-host failover (split-brain across a real partition) is a hardware/network tier. `cluster` is opt-in (default build never links it). Use `std`/`tokio` UDP — no GPL deps (LGPL-clean). Guardrail: no `unwrap` on socket/serde results; a malformed datagram is dropped + logged, never a panic; sends are non-blocking so the publisher never stalls (inv #10).
- **Read first:** `crates/multiview-engine/src/ha/mod.rs` + `transport.rs` + `repl.rs` module docs (the isolation contract); resilience-and-av §2 (supervision, no-split-brain), §1 (output guarantee).

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-engine/src/ptp.rs
- /workspaces/mosaic/crates/multiview-engine/src/ha/transport.rs
- /workspaces/mosaic/crates/multiview-hal/src/load.rs
- /workspaces/mosaic/crates/multiview-cli/src/wallclock.rs


## GPU — Compositor, efficiency & hardware


### `[ ]` GPU-1 — Hoist the single encoder into the bake consumer; fan packets to mux-only sinks · effort: L · deps: none
- **Goal:** Make inv #7 actually hold across outputs: a file + HLS run at one rendition must encode the canvas once, not twice, by moving the lone `VideoEncoder` into `consumer_main` and fanning `EncodedPacket`s to `PacketMuxSink`s.
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `consumer_main` (1682), `StreamEgress::spawn` (1568), `SinkRunner` type (1448), `run_one_output` (1729), `RunnableOutput` enum (~328), `StreamingFrameSource` (~1735). Uses the existing `multiview_output::PacketMuxSink`/`PacketSource` (`sink.rs:720`) and `multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder}`. The `EncodeConfig` already lives on the `Pipeline` (`pipeline.rs:565`).
- **Approach:**
  1. In `consumer_main`, after `StreamBaker::new`, build **one** `VideoEncoder` from the pipeline's resolved `EncodeConfig.target()` (the same config `FileSink`/`SegmentSink` consume today), and snapshot `StreamCodecParameters::from_encoder` + `encoder.time_base()` once.
  2. Change the per-sink fan-out channel element from `Arc<Nv12Image>` to `EncodedPacket` (the `sync_channel` in `spawn`, `SINK_QUEUE_CAP` unchanged — packets are ≪ NV12 so the bound is still cheap). Per baked frame: run the existing `FrameConverter` NV12→`yuv420p` + tick-PTS re-stamp (lift the converter logic out of `sink.rs:230` into the consumer, or reuse it), `encoder.send_frame`, drain `receive_packet`, wrap each in `EncodedPacket`, and **`clone()` once per live sink** (`EncodedPacket::Clone` is `av_packet_ref`, `packet.rs:108`) before the blocking `tx.send`.
  3. Replace each `SinkRunner` body: `run_one_output` builds a `PacketMuxSink::file`/`::segment`/`::push` and drives `PacketMuxSink::run(&mut source, &codec_params, time_base)` over a new `PacketStreamSource` (the `EncodedPacket`-fed twin of `StreamingFrameSource`, draining the `Receiver<EncodedPacket>`). The `StreamCodecParameters` + `time_base` are cloned into each runner closure at spawn (both are `Send`, independent copies — `packet.rs:170`).
  4. On encoder-build failure inside the consumer, return `PipelineError::Engine` so the egress join surfaces it (the existing `drop(sink_txs)` finalizes partial sinks).
  5. Fold #50 bounded teardown: the existing `egress.join()` already joins consumer-first then sinks; add a grace-deadline join for a wedged `PacketMuxSink` (mirror the ingest `INGEST_JOIN_GRACE` constant) so `drive_streaming` always returns, detaching + logging a still-unfinished sink.
- **Acceptance (done when):** TDD: (a) a unit/integration test on the cli streaming seam (`drive_streaming_for_test`, `pipeline.rs:789`) with **two** outputs at one rendition asserts exactly **one** `VideoEncoder` is constructed — add a `#[cfg(test)]` `ENCODER_BUILDS` counter mirroring `SEED_ENCODER_BUILDS` (`sink.rs:61`); (b) an `ffprobe` content check that the file output and the HLS segments carry the **same** coded stream (identical packet count / keyframe positions); (c) re-assert inv #1 (output clock never stalls — the existing `faltered=false` / `frames==ticks` check) and inv #10 (the hot loop still only `try_send`s; a slow `PacketMuxSink` paces the consumer, never the engine — extend the existing slow-sink test). Invariant #7 verified by the encoder-count == 1.
- **Risks/notes:** LGPL-clean — the single encoder still selects via `resolve_encoder` (`pipeline.rs:299`), GPL `x264`/`x265` stay behind `gpl-codecs`; nothing new linked. No hardware/network needed (mpeg2video default, CI has ffmpeg). Guardrails: the per-sink clone must be `EncodedPacket::clone` then `into_owned_packet` inside the sink (so `write_packet`'s in-place rescale is sound — already the `PacketMuxSink` contract); no `unwrap`/`as`/indexing in the consumer loop. Watch the per-GOP seed wart: the segment path already builds its seed **once** (the `SEED_ENCODER_BUILDS==1` test, `sink.rs:1289`) — the packet-fed `PacketMuxSink::segment` seeds each segment muxer from the one `StreamCodecParameters` snapshot (`sink.rs:802`), so the wart is structurally gone; assert it stays gone.
- **Read first:** ADR-0026 (the exact 5-step decision), efficiency §2.3, ADR-E004; `multiview-output/CLAUDE.md`.

---

### `[ ]` GPU-2 — Converge the SOFTWARE engine onto `synth::generator_loop` so a clock source animates · effort: M · deps: none
- **Goal:** A clock source in an overlay-built **software** run animates (one bake/sec) instead of showing a static placeholder card, by spawning the existing `generator_loop` per animated source rather than priming a single static frame.
- **Touches:** `crates/multiview-cli/src/run.rs` — `SoftwareEngine` (145), `build` (185), `prime_stores` (462), `software_source_frame` (610), the `run_*` entry points (289/342/376). Reuses `crate::synth::{SyntheticKind, generator_loop}` (`synth.rs`) and `multiview_framestore::TileStore` (already the store type, `run.rs:151`).
- **Approach:**
  1. In `build`, classify each source via `SyntheticKind::from_source_kind` (`synth.rs:72`). For a **static** kind (`Bars`/`Solid`, `animated()==false`) keep the current prime-once path. For an **animated** kind (`Clock`), record it for a generator thread instead of baking a placeholder.
  2. Add a generator-supervisor to the software run mirroring the ffmpeg pipeline's pattern (`pipeline.rs:3229`): for each animated source spawn a thread running `generator_loop(kind, &store, w, h, canvas_color, cadence, &stop)`; own a shared `Arc<AtomicBool> stop` and join all generators on run teardown (the `sleep_until` chunked stop, `synth.rs:348`, makes teardown prompt).
  3. Thread the existing `StopSignal` from `run_until_stopped*` into that `AtomicBool` (or bridge it) so Ctrl-C tears generators down; for the bounded `run_for`/`run_for_realtime` paths, raise the stop after the tick budget.
  4. Replace the `_ =>` placeholder arm in `software_source_frame` for animated kinds with the honest fallback only when `overlay` is off (the generator returns `OverlayRequired`, `synth.rs:260`) — keep the per-index card for genuinely undecodable kinds (rtsp/hls/etc.), preserving the `a_decoded_kind_does_not_masquerade_as_bars` test (`run.rs:725`).
- **Acceptance (done when):** TDD: a software-run test under `feature = "overlay"` with a `clock` source asserts the tile's `TileStore` content **changes across a second boundary** (content-aware: sample the program canvas at displayed-second N vs N+1, assert `y_plane` differs — the same shape as `analog_clock_renders_and_animates`, `synth.rs:464`). Re-assert inv #1: the existing `RunReport.faltered == false` / `frames == ticks` (`run.rs:555`) still holds with generators running. A non-overlay build still produces a valid slate per tick (inv #1/#2) and the static `bars`/`solid` tests (`run.rs:699/711`) are unchanged.
- **Risks/notes:** No hardware. `clock` rendering needs `feature = "overlay"`; gate the generator spawn behind it and keep the GPU-free CPU bake (`apply_overlays_to_nv12`, `synth.rs:247`). Guardrails: generator threads must never block the engine (they publish into the lock-free `TileStore` the engine only samples — inv #10) and a render failure holds last-good (`synth.rs:332`), never panics. Don't double-publish: an animated source must not also be primed by `prime_stores`.
- **Read first:** core-engine §13 (output-core/isolation), ADR-0027 (synthetic sources), efficiency §2.6 (fps harmonization / re-render only on change).

---

### `[ ]` GPU-3 — GPU `describe_*` metadata trait methods: wire or remove · effort: S · deps: none
- **Goal:** Resolve the dead `NotImplemented` `describe_output` on `GpuCompositor` (and the analogous `Decoder::describe_next`/`Encoder::describe_input` defaults) — either return real `FrameMeta` or remove the override so the scaffold is honest.
- **Touches:** `crates/multiview-compositor/src/gpu/compositor.rs:525` (`GpuCompositor::describe_output`), `crates/multiview-core/src/traits.rs:98-149` (the three trait defaults + `FrameMeta`). No real decoder/encoder currently implements the core `Decoder`/`Encoder` traits (the ffmpeg crate has its own concrete types), so those two are default-only.
- **Approach:**
  1. Decide per the brief: the `GpuCompositor` already knows its output geometry/color at construction — `describe_output` should return a real `FrameMeta` (NV12 canonical pixel format, inv #5; the canvas color tag, inv #8) instead of `Err(NotImplemented)`. Wire it: store the output `FrameMeta` on the compositor (or derive from the per-`composite` canvas spec) and return `Ok(meta)`.
  2. For `Decoder::describe_next` / `Encoder::describe_input`: confirm via grep that nothing calls them (no impls beyond the default). If genuinely unused, **remove the methods** from the traits rather than ship a permanent `NotImplemented` (cleaner core surface per `multiview-core/CLAUDE.md`); if a caller is planned, leave them but document the contract.
  3. Keep `multiview-core` FFI-free (its CLAUDE.md rule): `FrameMeta` construction stays pure.
- **Acceptance (done when):** TDD under `feature = "wgpu"`: a unit test builds a `GpuCompositor` and asserts `describe_output()` returns `Ok` with the expected width/height, `PixelFormat::Nv12`, and the canvas color tag (no `NotImplemented`). If the Decoder/Encoder defaults are removed: `cargo check --workspace` is green and no caller breaks. Re-assert inv #5 (the described format is NV12).
- **Risks/notes:** `wgpu` is off by default / no CI adapter — the `describe_output` test must be pure metadata (no dispatch), so it runs GPU-free like the existing `validate_overlay_shader` tests. Guardrails: no `unwrap`/`as`; build `FrameMeta` via its constructors.
- **Read first:** core-engine §3–§5 (trait surface), color-management (the output tag axes for `FrameMeta`).

---

### `[ ]` GPU-4 — Overlay IMAGE-primitive GPU texture upload (the wgpu shader branch) · effort: L · deps: GPU-3
- **Goal:** Make the `KIND_IMAGE` branch in `overlay.wgsl` a real premultiplied-RGBA blit (DVB-sub / bitmap caption burn-in on GPU) instead of the transparent no-op, matching the CPU reference `blend_image` within SSIM/PSNR thresholds.
- **Touches:** `crates/multiview-compositor/src/gpu/shaders/overlay.wgsl:142` (the `KIND_IMAGE` no-op), `crates/multiview-compositor/src/overlay/gpu_subpass.rs` — `OverlayPrimGpu::pack` Image arm (181), `bind_group_layout` (350), `OverlaySubpass` (271); the CPU reference `blend_image` (`overlay/subpass.rs:701`) is the ground truth. Likely needs the GPU compositor to actually **dispatch** the overlay subpass (currently `gpu/compositor.rs` only runs composite+encode) and a new image-atlas/texture-array binding.
- **Approach:**
  1. Add an image-texture binding to the overlay bind-group layout (`gpu_subpass.rs:350`) — an `Rgba8Unorm` texture (or texture-array, layer per Image primitive) holding the premultiplied bitmap(s), mirroring the existing R8 glyph-atlas binding 2. Pack the layer index + atlas offset into the Image primitive's `kind_meta` (today zeroed, `gpu_subpass.rs:181`).
  2. In `overlay.wgsl`, replace the `coverage = 0.0` no-op (line 145) with a `textureLoad` of the image texel at `(dest + col,row)`, contributing its premultiplied RGBA into the same `over` accumulator (lines 155-159) — the bitmap is already premultiplied, so feed it as `src` directly (no `* coverage`), matching `blend_image`.
  3. Wire the upload: a `write_texture` of each Image cue's premultiplied bytes before dispatch (cap by `MAX_OVERLAY_PRIMS` / a bounded image-atlas size, no per-frame unbounded allocation — ADR-E005 bounded-memory rule the file already cites at line 26).
  4. Ensure the GPU compositor dispatches `OverlaySubpass` between composite and encode (the file header's stated position) — alias the overlay output with the encode-pass input so no extra readback (T10).
- **Acceptance (done when):** TDD: a content-aware GPU-vs-CPU parity test — render an Image primitive through both the GPU subpass and `blend_image`, assert SSIM/PSNR above threshold (**never bit-exact**, per `multiview-compositor/CLAUDE.md` and the work-stream rule). The naga-validation test (`validate_overlay_shader`) stays green. Re-assert inv #5 (NV12-throughout: the image blits in linear-RGBA in the subpass, output stays NV12; no per-tile RGBA materialization) and inv #8 (color pipeline order unchanged — blend in linear, premultiplied).
- **Risks/notes:** `wgpu` off by default, **no GPU adapter in CI** — the parity/SSIM test must run on a GPU-tagged self-hosted runner (per AGENTS §"Real GPU … run on GPU-tagged self-hosted runners"); keep the WGSL naga-validating and the CPU reference path the CI default. Guardrails: no `unwrap`/`as`/indexing in the pack/upload (the file already hand-rolls as-free float→int helpers, `gpu_subpass.rs:198-264` — follow that style). LGPL-clean (no new deps).
- **Read first:** ADR-0016 §4.1 (overlay subpass position), color-management (premultiplied linear blend), efficiency §2.2 (NV12 policy); the in-file TODO(gpu-image) at `overlay.wgsl:29`.

---

### `[~]` GPU-5 — Multi-GPU PLACEMENT decision engine: closed-loop controller + config + telemetry · effort: XL · deps: none
> **PARTIAL (2026-06-05):** the HAL deliberate-split decision shipped — `crates/multiview-hal/src/split.rs` (`plan_split` + `CutPoint`/`CrossGpuCopy`/`SplitPlan`/`SplitReject`, 8 tests), commit `c995341`. Step 1 (split completeness in `select.rs`) is done. **Remaining:** step 2 (the off-hot-path controller in `multiview-engine` — EWMA/sustained-overload SHED-vs-MIGRATE, make-before-break), step 3 (config policy fields), step 4 (telemetry counters). Track as a follow-on.
- **Goal:** Turn the pure `multiview-hal::select` placement policy (already built) into a live system: an off-hot-path placement controller in `multiview-engine` that senses `DeviceLoad`, detects sustained overload, and proposes SHED-vs-MIGRATE — plus the config policy and telemetry counters ADR-0018 specifies.
- **Touches:** `crates/multiview-hal/src/select.rs` (the pure `select_device` exists — extend with `SplitPlan`/`CutPoint` if absent), `crates/multiview-hal/src/load.rs` (`DeviceLoad`/`LoadPoller` exist, 403). New: a placement controller in `crates/multiview-engine/` (beside the existing `ControlLoop`/`Hysteresis` — confirm names in `engine/src/`), config policy fields in `crates/multiview-config/`, and per-GPU/migration counters in `crates/multiview-telemetry/`.
- **Approach:**
  1. Audit `select.rs` for completeness vs ADR-0018 §2 (pins → hard gates → DRF+Tetris score → tie-break); add the deliberate-split `SplitPlan`/`CutPoint` + `CrossGpuCopy` cost if not yet present.
  2. In `multiview-engine`, add an **off-hot-path** controller that reads the wait-free `Vec<DeviceLoad>` snapshot (published by the engine's poll thread driving `LoadPoller::poll`, `load.rs:433`), runs an EWMA over the existing `Hysteresis` to detect **sustained** overload (transients ignored), and emits a proposal (SHED via the degradation ladder, or MIGRATE via make-before-break at an IDR boundary) — it **only proposes**, never `.await`s the engine (inv #10). Anti-storm damping: cooldown, per-GPU migration budget, min-gain gate.
  3. Add config policy as data (`multiview-config`): per-source/per-output GPU pin (stable `DeviceId`), reserve-headroom, scoring weights (`LoadWeights` already exists, `select.rs:83`), migration cooldown/budget/min-gain — conservative defaults.
  4. Telemetry: reuse the ADR-0017 per-GPU gauges, add placement/migration/split counters; log every adaptation.
- **Acceptance (done when):** TDD (all pure / no hardware — the heart is `select.rs` + a deterministic controller over injected `DeviceLoad`): assert a running pipeline is never fragmented unless no single GPU fits; a split is taken only after degrade-to-fit fails and its copy is cost-accounted; a **transient** spike never migrates while a **sustained** overload does; a migration is IDR-aligned make-before-break dropping zero output frames; anti-storm damps bound migration frequency; a pin always wins; a blind-vendor (all-`None`) probe falls back to VRAM + cost-model without blocking; the controller can never stall the engine (chaos gate). On single-GPU hosts the engine adds zero behaviour. Re-assert inv #1 + inv #9.
- **Risks/notes:** No hardware in CI — the controller and `select` are pure and unit-testable with synthetic `DeviceLoad`s (the `FakeProbe` pattern, `load.rs:716`); real NVML/sysfs probes (`load.rs:495`/`600`) stay feature-gated. Guardrails: no `unwrap`/`as` — `select.rs` already hand-rolls as-free arithmetic; follow it. Largest item; land `select` completeness first, then controller, then config/telemetry, each gate-green.
- **Read first:** ADR-0018 (the whole decision), gpu-placement-engine brief, ADR-0017 (the load model + ranking), ADR-R004 (make-before-break), efficiency §3.

---

### `[ ]` GPU-6 — Hardware backend real decode/encode/composite PATHS (cuda/vaapi/qsv/metal) · effort: XL · deps: GPU-3, GPU-1
- **Goal:** Promote the hardware backends from detection/capability-only to real device-resident `decode → composite → encode` islands, bound to the `AVHWFramesContext` scaffold, NV12-throughout, with the single encoder of GPU-1.
- **Touches:** `crates/multiview-ffmpeg/src/hwframe.rs` (the RAII `AVHWDeviceContext`/`AVHWFramesContext` scaffold, the crate's only FFI module, 1-90), `crates/multiview-ffmpeg/src/decode.rs`/`decode_stream.rs`/`encode.rs`/`codec.rs` (bind a real `*_cuvid`/`vaapi`/`qsv`/`videotoolbox` decoder+encoder to the hw frames ctx), `crates/multiview-compositor/src/gpu/` (the native composite fast paths the compositor CLAUDE.md mentions as opt-in), `crates/multiview-hal/src/probe.rs` + `capability.rs` (already detect; now negotiate). Features `cuda`/`vaapi`/`qsv`/`metal` (all currently empty stubs, `compositor/Cargo.toml`).
- **Approach (per backend, NVIDIA first as the reference island):**
  1. Bind a real `get_format`-style decoder callback to the `AVHWFramesContext` (the `hwframe.rs` header flags this as the documented future call site, lines 22-23): `catch_unwind` at the FFI boundary, allocation-light, NV12/P010 sw-format. Decode device-resident (`-hwaccel cuda -hwaccel_output_format cuda`), use NVDEC `cuvid -resize` for decode-at-display-resolution (efficiency §2.1) where one tile size suffices.
  2. Keep the island whole on one device (ADR-0004 zero-copy island; the placement engine GPU-5 decides where): decode→composite→encode all device-resident, **no `hwdownload`/`hwupload` round-trip** — add the telemetry that fails loudly on an inserted scale/convert (ADR-E002 consequence, efficiency §2.5).
  3. Encode through the single hoisted encoder (GPU-1): NVENC P4-P5 low-latency, fixed closed GOP, probe the per-system NVENC session ceiling at runtime (never hard-coded — efficiency §3.2). Mirror per backend: VAAPI/QSV media filters staying in the media stack, VideoToolbox + IOSurface unified-memory zero-copy on Apple.
  4. wgpu interop reality (efficiency §2.5): no stable external-texture import — for non-Vulkan-Video decoders accept the GPU→CPU→GPU round trip until the native path lands; gate the zero-copy claim per backend.
- **Acceptance (done when):** TDD + content-aware: per backend, an integration test on a **GPU-tagged self-hosted runner** decodes a real clip, composites, and encodes once, validated by SSIM/PSNR vs the CPU reference (never bit-exact). Assert inv #5 (NV12 sw-format throughout — `ffprobe`/format-trace shows no inserted RGBA or `hwdownload`), inv #7 (one encode, GPU-1's single-encoder count holds on the hw path), inv #1 (output clock never stalls under decode/encode load), inv #10 (no engine back-pressure). A no-GPU box / default build still compiles and the feature-off arms report graceful absence (the existing `probe`/`load` discipline, `load.rs:495`).
- **Risks/notes:** Heavy hardware + CI dependency — **none of this runs on shared CI**; everything must compile GPU-free (the `hwframe.rs` scaffold already does) and only the real paths run on the self-hosted matrix (AGENTS §CI). Licensing: NVENC/VAAPI/QSV/VideoToolbox are not GPL — LGPL-default stays clean; `gpl-codecs`/`ndi` remain opt-in. Guardrails: `hwframe.rs` is the **only** module allowed `unsafe` (crate is `deny`, not `forbid`) and every block needs `// SAFETY:` (lines 25-30); the `get_format` callback must `catch_unwind`. Sequence after GPU-1 (single encoder) and GPU-3 (describe_output metadata) so the hw encoder slots into the proven egress.
- **Read first:** core-engine §7/§8.1/§12 (hwaccel island), efficiency §2.1/§2.5/§3.2, ADR-E002/E003 (NV12 + encoder selection), ADR-0004 (zero-copy island), color-management; `multiview-ffmpeg/CLAUDE.md` (safety contract).

---

## Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs (consumer_main / StreamEgress / SinkRunner — GPU-1, and synth wiring reference for GPU-2)
- /workspaces/mosaic/crates/multiview-output/src/sink.rs (PacketMuxSink/PacketSource — already built, the GPU-1 target API)
- /workspaces/mosaic/crates/multiview-cli/src/run.rs + /workspaces/mosaic/crates/multiview-cli/src/synth.rs (SoftwareEngine vs generator_loop — GPU-2)
- /workspaces/mosaic/crates/multiview-compositor/src/gpu/gpu_subpass.rs + /workspaces/mosaic/crates/multiview-compositor/src/gpu/shaders/overlay.wgsl (KIND_IMAGE branch — GPU-4; describe_output in gpu/compositor.rs — GPU-3)
- /workspaces/mosaic/crates/multiview-hal/src/select.rs + /workspaces/mosaic/crates/multiview-hal/src/load.rs (placement policy + load model — GPU-5; and /workspaces/mosaic/crates/multiview-ffmpeg/src/hwframe.rs for GPU-6)


## SUR — Captions · NMOS · web codegen

Grounded against: ADR-0016/ADR-0019 + `docs/io/captions.md`; `crates/multiview-control/src/nmos/is05.rs`, `is07.rs`, `nmos/mod.rs`, `nmos/transport.rs`; ADR-RT006 + `docs/api/realtime.md`; `crates/multiview-events/src/envelope.rs`; `web/src/api/layouts.ts`, `web/src/realtime/envelope.ts`, `web/src/app/router.tsx`, `xtask/src/main.rs`, `crates/multiview-control/src/openapi.rs`, `routes/mod.rs`.

Cross-cutting reality check found during exploration (load-bearing for sequencing):
- Layout **write handlers already exist** (`create_layout`/`update_layout`/`delete_layout`/`get_layout`, `routes/mod.rs:96–172`) but carry **plain doc-comments, no `#[utoipa::path]`** → absent from the spec → that is exactly why `layouts.ts` hand-wraps them. Same shape for sources/outputs/overlays (`routes/sources.rs:62–117`).
- `multiview-events` has **no `schemars`/`utoipa` derives** (`envelope.rs:56`) and there is **no AsyncAPI route, no xtask `gen-asyncapi`, no `/asyncapi.json`** anywhere — RT006 is greenfield.
- `is07-mqtt` and `nmos` are **empty feature flags** (`Cargo.toml:106/111`), no broker/timing deps wired.

---

### `[ ]` SUR-1 — IS-05 scheduled activation (absolute + relative) · effort: M · deps: none
- **Goal:** Promote a staged IS-05 connection at a scheduled TAI time so a facility controller can pre-load a 2110 receiver swap, completing the activation modes already modelled but deferred.
- **Touches:** `crates/multiview-control/src/nmos/is05.rs` (`ActivationMode` is05.rs:29, `Activation.requested_time` is05.rs:47, `ConnectionState::activate_if_immediate` is05.rs:138); `crates/multiview-control/src/nmos/mod.rs` (`NmosRegistry::stage_connection` mod.rs:156, `RegistryInner` mod.rs:71).
- **Approach:**
  1. Add a total `parse_tai(&str) -> Option<TaiTime>` helper in `is05.rs` over the `<seconds>:<nanoseconds>` form (mirror the focused-parser discipline of `parse_sdp_transport` is05.rs:164 — no `unwrap`, no full RFC clock dep).
  2. Generalise `activate_if_immediate` into `activate_due(now: TaiTime) -> bool` that: applies `ActivateImmediate` unconditionally; for `ActivateScheduledAbsolute` applies when `now >= requested_time`; for `ActivateScheduledRelative` resolves the relative offset against a `staged_at` stamp captured at `stage()` time (add `staged_at: Option<String>` to `ConnectionState`). Keep `activate_if_immediate` as a thin `activate_due` caller so existing call sites/tests stay green.
  3. In `mod.rs`, store the pending scheduled change in `RegistryInner` and add `NmosRegistry::tick_scheduled(now)` that walks `connections` and calls `activate_due`. Drive it from the control plane's existing clock seam (the same boundary that stamps IS-07 `creation_timestamp`, `is07.rs:104` doc) — never from an input PTS (invariant #1).
  4. Surface the staged-vs-active distinction unchanged through `patch_staged` (mod.rs:307) and `get_active` (mod.rs:273); a scheduled stage returns `200` with `staged` populated and `active` unchanged until the tick fires.
- **Acceptance (done when):** new unit tests in `is05.rs` — `scheduled_absolute_activates_at_or_after_requested_time`, `scheduled_relative_activates_after_offset`, `scheduled_does_not_activate_before_due`, `parse_tai_rejects_garbage_without_panicking`; existing `scheduled_activation_is_not_treated_as_immediate` (is05.rs:264) flips to assert it *does* activate once `tick_scheduled` is past due; a `mod.rs` registry test asserts `tick_scheduled` promotes exactly the due receiver. Invariant re-asserted: **#1 output-clock never stalls / #10 no back-pressure** — `tick_scheduled` runs on the control-plane clock, holds only the registry `Mutex` (mod.rs:88) the engine never takes.
- **Risks/notes:** Pure model stays in the always-compiled, CI-green default build (no `nmos` feature, no PTP NIC). No `chrono`/`hifitime` dep needed — TAI seconds:nanos is integer math; guard against overflow with checked arithmetic (no `as`). No `unwrap`/indexing in non-test code.
- **Read first:** `docs/decisions/ADR-… (IS-05 brief, broadcast-multiviewer §6/§8 referenced in is05.rs:1)`; the module doc block at `is05.rs:38–49`.

---

### `[ ]` SUR-2 — IS-07 MQTT broker transport · effort: L · deps: none (parallel with SUR-1)
- **Goal:** Carry IS-07 event/tally messages over an MQTT broker so Multiview interoperates with MQTT-native NMOS facilities, completing the transport whose message model + topic seam already exist.
- **Touches:** `crates/multiview-control/src/is07.rs` (`mod mqtt` is07.rs:300, `topic_for_source`/`topic_for_message` is07.rs:317/324, `Is07Message` is07.rs:118); `crates/multiview-control/Cargo.toml` (`"is07-mqtt" = []` Cargo.toml:106); `deny.toml` (opt-in native dep allowance, deny.toml:28).
- **Approach:**
  1. Wire a broker client behind the feature: `"is07-mqtt" = ["dep:rumqttc"]` (async, MIT/Apache — verify against `deny.toml` allowlist; it is *not* a `gpl-codecs`/`ndi`-class dep, so it joins the LGPL-clean opt-in set, but keep it feature-gated out of the default build per the existing comment Cargo.toml:101–105).
  2. In `is07::mqtt`, add a `Publisher` that connects (`rumqttc::AsyncClient`), serialises an `Is07Message` to JSON (reuse the existing tagged serde, no new wire model), and publishes to `topic_for_message(&msg)` (is07.rs:324) at QoS per NMOS guidance. Add a `Subscriber` that subscribes to `x-nmos/events/v1.0/sources/+` and decodes inbound frames back into `Is07Message`, feeding the existing `GpiEvent::from_is07` / `tally_color_from_is07` converters (is07.rs:196/284).
  3. Keep the live socket strictly feature-gated; the **pure topic/codec seam stays always-compiled** (it already is — is07.rs:300 is the only gated block today). Run publish on a detached tokio task off the realtime fan-out so a slow/dead broker cannot back-pressure (invariant #10) — drop-oldest on a bounded channel, identical posture to the WS fan-out (`realtime-api.md §7`).
- **Acceptance (done when):** TDD: pure tests (always run) for round-trip `Is07Message → JSON → topic → decode` already partly exist (`mqtt_topic_follows_the_nmos_convention` is07.rs:493) — extend with a serialise/deserialise-over-the-wire-shape test. Feature-gated integration test behind `#[cfg(feature = "is07-mqtt")]` against an **embedded/in-process MQTT broker** (e.g. `rumqttd` as a dev-dependency) — no external broker in CI; assert a published Boolean state is received and decodes to the right `GpiEvent`. Invariant re-asserted: **#10 no engine back-pressure** — publisher uses a bounded drop-oldest queue, never `.await`ed by the engine; **#1** untouched (control-plane only).
- **Risks/notes:** Network/broker availability in CI — solve with an in-process broker dev-dep, never a live broker. Licensing: confirm `rumqttc`/`rumqttd` license rows are added to `deny.toml` allow-list; default build must stay green with the feature off. No `unwrap`/`as` in the transport task; decode failures degrade to a dropped frame, never a panic.
- **Read first:** `docs/decisions/ADR-RT006.md` (typed-events posture) and the IS-07 module doc `is07.rs:1–28` (transport section is07.rs:19–28).

---

### `[ ]` SUR-3 — Caption ingest Phase 2/3: broaden native decode beyond HLS WebVTT · effort: XL · deps: none (Rust, independent of web/NMOS)
- **Goal:** Decode CEA-608/708 (embedded), DVB-teletext, and the HLS native-master rendition in-pipeline per ADR-0019 so captions arrive from the source stream itself, not only from a sidecar/HLS-WebVTT path — degrading to "no cue" and never pacing output.
- **Touches:** `crates/multiview-ffmpeg/` (caption decoder FFI behind `ffmpeg` feature — captions.md §7 table); `crates/multiview-input/` (PMT-walk discovery, A53 sniff, HLS `SUBTITLES`-group resolve, per-tile cue store — captions.md §7; `unsafe = forbid`); `crates/multiview-overlay::subtitle` (`Cue { start, end, lines }` extended with optional `region`, captions.md §1) + `caption_probe` (captions.md §7); `multiview-cli/src/pipeline.rs` (the one `av_read_frame` loop — captions.md §4).
- **Approach:** First **audit what is already real vs missing** (the task explicitly asks this):
  1. Inventory existing surface — sidecar SRT/VTT parser (`multiview-overlay::subtitle::CueTrack`, captions.md §7 "already committed"), the caption-presence probe (already committed), and the HLS-WebVTT path (Phase 1). Confirm the unified `CaptionCue` Text|Bitmap model (captions.md §1) and `CaptionTrackInfo`/selector enum (captions.md §6) exist or scaffold them in `multiview-core`/`multiview-overlay`.
  2. **CEA-608/708 (form 3):** sniff `AV_FRAME_DATA_A53_CC` side data on the already-decoded video frame and feed `cc_dec` (captions.md §3 row 3) — zero extra demux, runs only when `embedded_cc` selector active (captions.md §4 "CEA special case").
  3. **DVB-teletext (form 1) + DVB-sub (form 2):** walk the PMT for `teletext_descriptor`(0x56)/`subtitling_descriptor`(0x59) (captions.md §2 table), instantiate `libzvbi_teletextdec`/`dvbsub` only for the selected page/PID — teletext → Text cue, DVB-sub → Bitmap cue (premultiplied RGBA, captions.md §1).
  4. **HLS native master fetch (form 4 hardening):** resolve the master `#EXT-X-MEDIA:TYPE=SUBTITLES` group and open the rendition's own WebVTT segment list as a *second isolated reader* (captions.md §3 note, §4 diagram).
  5. All forms rebase cue PTS onto the source ns timeline (captions.md §5) and write into the bounded per-tile `CueStore`; the compositor *samples* `active_cue(out_pts)` non-blocking (captions.md §4/§5).
- **Acceptance (done when):** TDD on **synthesised** fixtures built with the FFmpeg CLI (captions.md §9 — never live broadcast): mux known SRT → DVB-sub/teletext TS and assert decoded `CaptionCue`s match; `-a53cc 1` H.264 fixture and assert `cc_dec` recovers captions on the right field/service; generated HLS master+`SUBTITLES` group resolves and cues decode. **ffprobe/content-aware check:** verify the synthesised TS carries the expected subtitle `stream_type`/descriptor before decoding (so the test asserts on real demux, not a tautology). Pure state-machine property tests for cue-store expiry / no-cue gaps / wrong-page-empty / intermittency / rebasing driven by injected `MediaTime` (captions.md §9, ADR-G002). Invariants re-asserted: **#1 output-clock never stalls** (decode on input thread, compositor only reads), **#2 last-good** (no-cue is graceful), **#3 timing** (ns rebasing, no float fps), **#10 isolation** (stalled/wrong-page caption cannot back-pressure; bounded drop-oldest).
- **Risks/notes:** FFI decoders are the only feature-gated part (`ffmpeg` feature) — libzvbi/dvbsub/cc_dec are **already linked** in the FFmpeg 7.1 build, so **no new `cargo deny`-relevant dependency** (captions.md §3). Pure model (cue store, selector, parsers) always compiled+tested with no FFmpeg/GPU. All raw libav FFI stays in `multiview-ffmpeg` with `// SAFETY:` (captions.md §7); `multiview-input` is `unsafe = forbid`. No `unwrap`/`as`/indexing on the decode path. ASS styled rendering (form 6) stays behind the opt-in `libass` feature (ADR-R007) — out of scope here.
- **Read first:** `docs/io/captions.md` (whole doc; §3 decoder table, §4 in-pipeline diagram, §5 timing, §9 tests) and ADR-0019 / ADR-R007 / ADR-R008.

---

### `[ ]` SUR-4 — OpenAPI: annotate the layout/resource write ops so they enter the spec · effort: M · deps: none
- **Goal:** Add the `POST`/`PUT`/`DELETE`/`GET-by-id` layout (and sources/outputs/overlays) operations to the generated OpenAPI spec so the web client can call them fully-typed, eliminating the hand-written wrapper's reason to exist.
- **Touches:** `crates/multiview-control/src/routes/mod.rs` (`get_layout` mod.rs:96, `create_layout` mod.rs:108, `update_layout` mod.rs:129, `delete_layout` mod.rs:154 — all currently un-annotated); `crates/multiview-control/src/routes/sources.rs` (`get/create/update/delete_source` sources.rs:62–117); `crates/multiview-control/src/openapi.rs` (`paths(...)` openapi.rs:26, `rest_routes` table openapi.rs:127); `docs/api/openapi.json` (regenerated artifact).
- **Approach:**
  1. Add `#[cfg_attr(feature = "openapi", utoipa::path(...))]` to each write handler — mirror the exact shape of the annotated `list_layouts` (routes/mod.rs:70) and the NMOS `patch_staged` (mod.rs:291): `request_body = LayoutInput`, responses incl. `201`/`200`/`204`, `404`, `412` (`If-Match`), `application/problem+json` body = `Problem`.
  2. Register every new operation in `openapi.rs paths(...)` (openapi.rs:26). The `rest_routes()` table (openapi.rs:129–133) already *lists* `POST/PUT/DELETE /layouts/{id}` — make the actual spec match that asserted truth.
  3. Regenerate: `cargo xtask gen-openapi` (`xtask/src/main.rs:29` → writes `docs/api/openapi.json`).
- **Acceptance (done when):** `crates/multiview-control/tests/openapi.rs` extended to assert the spec now contains `POST/PUT/DELETE /api/v1/layouts/{id}` operations with `LayoutInput` request bodies and `412` responses (it already asserts the route table at openapi.rs:127 — close the gap so the table and the emitted `paths` agree). `cargo xtask gen-openapi` produces a `docs/api/openapi.json` diff containing the new path ops; CI diff-gate (RT006 posture) stays green after commit. Invariant: control-plane only — **#10** unaffected (no new engine interaction).
- **Risks/notes:** Pure annotation work, no behaviour change. Guardrail: keep `Problem` (RFC 9457) as the error body so the generated TS error type is correct. No `unwrap`/`as`.
- **Read first:** `docs/decisions/ADR-W002.md` (utoipa code-first spec) and the existing annotated handler `routes/mod.rs:67–93`.

---

### `[ ]` SUR-5 — Web: replace the hand-written layouts wrapper with the generated client + wire deferred routes · effort: M · deps: SUR-4
- **Goal:** Delete `writeLayout`/`deleteLayout` and call the generated `openapi-fetch` client directly, so layout CRUD is compile-checked against the spec, and route the remaining `PlaceholderPage` screens to real surfaces.
- **Touches:** `web/src/api/layouts.ts` (whole file — the `TODO(api-schema)` at layouts.ts:15–17 is the trigger), `web/src/api/schema.ts` (regenerated), `web/src/api/client.ts`; `web/src/app/router.tsx` (route table router.tsx:30); `web/src/pages/PlaceholderPage.tsx` consumers (any route still pointing at it); `web/package.json` (`generate:api` script package.json:9).
- **Approach:**
  1. Regenerate the TS schema from the SUR-4 spec: `npm --prefix web run generate:api` (→ `openapi-typescript ../docs/api/openapi.json -o src/api/schema.ts`, package.json:9). The new `paths` now declare the write ops the wrapper documented as missing (layouts.ts:7–8).
  2. Per the file's own `TODO(api-schema)` (layouts.ts:16): delete `writeLayout`/`deleteLayout`/`LayoutWriteOptions`/`headersFor`/`urlFor`/`readProblem` and call `client.POST/PUT/DELETE('/api/v1/layouts/{id}', …)` through the typed client (`client.ts`), keeping `If-Match`/`ETag` (layouts.ts:88, conventions §6) via the typed `header` params. Keep the exported `Layout`/`LayoutInput`/`Problem` aliases (layouts.ts:25–31) — they already come from generated `components['schemas']`.
  3. Audit which `router.tsx` routes still resolve to `PlaceholderPage`/un-wired screens; wire each deferred `PlaceholderPage` (PlaceholderPage.tsx) to its real page (the CRUD scaffold pattern in `SimplePages.tsx` is the template for resource screens). Update any nav that points at a stub.
- **Acceptance (done when):** Vitest: layouts CRUD tests call through the typed client with no `as`-casts and no bespoke `fetch` (extend the pattern in `web/src/api/queries.test.tsx`); `tsc --noEmit` (part of `npm run build`, package.json:10) passes under strict TS with `writeLayout` removed; `eslint . --max-warnings=0` (package.json:11) clean. Accessibility: each newly-wired route keeps a single `<h1>` + landmark (the `PageHeader` contract PlaceholderPage.tsx:24) and meets **WCAG 2.1 AA** (status by text+glyph, not colour — SimplePages.tsx header comment). Invariant: UI is best-effort, tolerates dropped realtime events (**#10**, web/CLAUDE.md).
- **Risks/notes:** Hard dependency on SUR-4 landing first (the generated schema must contain the write ops, else the client call won't typecheck). Strict TS: no `any`, no non-null assertions; preserve the defensive problem-parsing behaviour the wrapper had. No new hand-written fetch calls (web/CLAUDE.md "do not hand-write fetch calls or types").
- **Read first:** `docs/decisions/ADR-W002.md` / `ADR-W003.md` (generated-client mandate) and `web/CLAUDE.md` (codegen rule).

---

### `[ ]` SUR-6 — AsyncAPI generation + generated realtime envelope types (replace the hand-modelled envelope) · effort: XL · deps: none for the schemars derives; the web swap deps SUR-6a
- **Goal:** Implement ADR-RT006: derive AsyncAPI 3.0 from the shared `multiview-events` types, serve `/asyncapi.json`, generate TS models, and replace the hand-modelled `web/src/realtime/envelope.ts` (its `TODO(api-schema)` envelope.ts:3) with codegen — closing the realtime drift gap.
- **Touches:** `crates/multiview-events/` (`envelope.rs:56`, `event.rs`, `subscription.rs`, `topic.rs`, `seq.rs` — add `schemars`/`utoipa` derives; `Cargo.toml` has neither today); `crates/multiview-control/src/openapi.rs` / a new `asyncapi.rs` + route (no `/asyncapi.json` exists yet); `xtask/src/main.rs` (new `gen-asyncapi` task beside `gen-openapi` xtask/src/main.rs:29 — the help text at main.rs:26 already advertises it as "coming soon"); `web/src/realtime/envelope.ts` (+ `connection.ts`, `useEngineEvents.ts`); `web/package.json` (new `generate:events` script).
- **Approach:**
  1. **(SUR-6 Rust)** Add `schemars` (and/or reuse `utoipa::ToSchema`) derives to the `multiview-events` wire types so one set of Rust types feeds both specs (RT006 "single source of truth"). Build the AsyncAPI 3.0 doc in an `asyncapi.rs` (channels = topics, messages = the envelope `oneOf` by `t`), with the documented `serde_json` post-process to inject WS bindings the generator lacks (RT006 Consequences). Serve it at `/asyncapi.json` and add a `gen-asyncapi` xtask that writes `docs/api/asyncapi.json` (mirror `gen_openapi` xtask/src/main.rs:49).
  2. **(SUR-6a web)** Add a `generate:events` script (Modelina or `openapi-typescript`-equivalent for AsyncAPI message schemas) producing typed envelope/payload models; replace the hand-written `Envelope`, `TileState`, `TileSnapshotEntry`, `TileStateDeltaData` and their parsers (envelope.ts:22–213) with generated types behind the thin hand-written runtime (resume/conflation stays hand-owned per RT006 — `connection.ts`/`useEngineEvents.ts`). Keep `parseEnvelope` defensive (tolerate unknown `t`/major, envelope.ts:8) — RT006 keeps the runtime hand-written precisely for this.
- **Acceptance (done when):** TDD Rust: `multiview-control` test asserts `/asyncapi.json` validates against the AsyncAPI 3.0 schema (AsyncAPI CLI in CI) and contains the envelope channels/messages; a serde round-trip test proves the derived schema matches the wire JSON in `docs/api/realtime.md §2`. xtask: `cargo xtask gen-asyncapi` emits `docs/api/asyncapi.json`; **CI regenerates OpenAPI+AsyncAPI+TS and fails on any git diff** (RT006 Decision). Web: Vitest envelope tests pass against generated types; `tsc --noEmit` + `eslint --max-warnings=0` clean. Invariant re-asserted: **#10** — realtime stays best-effort, generated types do not change the no-back-pressure runtime (`realtime-api.md §7`); **#1** untouched.
- **Risks/notes:** RT006 flags `asyncapi-rust` (v0.2) as young and **lacking WS bindings** → the post-process step is mandatory and must itself be tested (schema-assembly fallback from `schema_for!(T)`). Verify `@asyncapi/react-component` renders the exact 3.0 features (keep the WS "Try it" console separate so a docs regression can't break it). Binary meter frames need explicit `contentType` documentation. All-permissive deps (schemars MIT/Apache) keep the default build LGPL-clean. No `unwrap`/`as` in the xtask/generator.
- **Read first:** `docs/decisions/ADR-RT006.md` and `docs/api/realtime.md §2` (envelope) + `docs/research/realtime-api.md`.

---

## Suggested order
SUR-4 → SUR-5 (web layouts swap) form the shortest grounded win. SUR-1 and SUR-2 are independent NMOS items runnable in parallel. SUR-3 (captions XL) and SUR-6 (AsyncAPI XL) are the two largest, each independent at the Rust layer; SUR-6's web envelope swap gates on the SUR-6 Rust spec landing.

## Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-control/src/nmos/is05.rs
- /workspaces/mosaic/crates/multiview-control/src/is07.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/mod.rs
- /workspaces/mosaic/web/src/api/layouts.ts
- /workspaces/mosaic/web/src/realtime/envelope.ts
