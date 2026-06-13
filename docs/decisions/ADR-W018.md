# ADR-W018: Live source apply — add / edit / remove on the running engine

- **Status:** Accepted
- **Area:** Web/API stack ↔ engine · sources management · live-apply (invariant #11)
- **Date:** 2026-06-10
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md),
  [resilience-and-av](../research/resilience-and-av.md), [streaming-gotchas §1–§3](../research/streaming-gotchas.md);
  builds on ADR-W015 (typed CRUD + `X-Multiview-Apply`), ADR-W008 (command bus), ADR-T002
  (tile stores + state machine), ADR-0027 (synthetic sources), ADR-0018/ADR-0035 (GPU placement)

## Context

`POST`/`PUT`/`DELETE /api/v1/sources/{id}` validate (ADR-W015) and store — but the running engine
never sees the change: every response says `X-Multiview-Apply: restart`. The real pipeline builds
per-source ingest **at startup only** (`Pipeline::build` → `IngestSupervisor::start`, one decode
thread per `IngestPlan`; synthetic sources ride the same supervisor via
`SourceLocation::Synthetic` → `synth::generator_loop`). The engine command drain
(`CommandDrain::apply`, run on the output-clock loop at each frame boundary) already applies
swaps/routes/layouts/salvos live, but has no notion of a source coming or going.

Invariant #11 demands management changes classify as Class-1 (hot) or Class-2 and apply
accordingly; the operator directive demands source mutations apply **instantly** where the engine
can do so, with hardware allocation re-assessed on every change.

Hard constraints:

- **Inv #1:** nothing on the output-clock thread may block, allocate per-tick, or wait on a spawn.
  Thread spawn, libav open, network I/O are all *heavy* — they must happen off the render thread.
- **Inv #10:** control may never back-pressure the engine: bounded channels, reject/drop over grow.
- **One uniform ingest:** a runtime-added source must run the *same* supervised
  ingest/generator path the startup path builds — never a second quality of ingest.
- **GPU placement principle (ADR-0018/0035):** load informs placement of NEW work only; affinity
  is the hard constraint — a runtime add never fragments or migrates the running pipeline island.

## Decision

### 1. Two new engine commands on the existing bounded bus

`Command::UpsertSource { op, source: multiview_config::Source }` and
`Command::RemoveSource { op, id }` ride the same bounded, non-blocking command bus (ADR-W008).
The control plane submits them **after** the resource-store write succeeds; the engine drains them
at the frame boundary. A full bus sheds the submit (`SubmitError::Full`) — the store write stands,
and the response then *honestly* declares `restart` (the stored doc still applies on restart).

### 2. Per-response apply classification (inv #11) — capability-driven (level 2 shipped)

The mutation response's `X-Multiview-Apply` header is computed per request **from the run's
declared capability** (`LiveSourceCapability` on the control `AppState` — the honesty keystone,
added with level 2). The binary derives the capability from the seams it actually wired:
`network = true` **iff** a real decoded-ingest spawner was handed to the live-source hub (the
full-pipeline/`ffmpeg` run path); the software run declares `synthetic_only`, and the control
crate's default is `synthetic_only` (never over-claims). The header therefore never says `live`
for a kind the running engine cannot ingest:

| Mutation | Condition | Header |
|---|---|---|
| POST/PUT | new kind is **live-appliable on this run** — synthetic (`bars`/`solid`/`clock`) on every run; network/file (`rtsp`/`hls`/`ts`/`srt`/`rtmp`/`file`) on a run with the ingest spawner — and the `UpsertSource` was enqueued | `live` |
| PUT | previous stored kind live-appliable, new kind not (e.g. `rtsp` → `ndi`) → `RemoveSource` enqueued (the running producer stops; the new doc applies on restart) | `restart` |
| POST/PUT | `ndi`/`youtube`/`aes67` (never live in this slice), or a network kind on a run without the spawner (software engine) | `restart` |
| DELETE | `RemoveSource` enqueued (any kind — teardown is a stop-flag raise + store unregister) | `live` |
| any | bus full/closed, or no engine drains (no `[control]` run) | `restart` |

`SourceKind::is_synthetic()` and `SourceKind::is_network_media()` (both on `multiview-config`)
are the two classification points; `LiveSourceCapability::is_live(kind)` combines them with the
run's declared truth. The SPA's saved toast echoes the per-response header value (the page can
not know the build flavour; the response is the source of truth).

Two honesty bounds on the header: **(a)** a submit that succeeds but is never drained (the engine
stops before the next frame boundary) converges on restart semantics — the stored document remains
the durable truth and applies at the next start, so a `live` answer followed by an engine stop
never strands state; **(b)** a `clock` live-add on a build without the `overlay` feature returns
`live` (the control plane cannot see the engine build's features) but the hub cannot render it: it
registers the store, warns once, and the tile rides the slate until a restart of an
overlay-enabled build.

### 3. Frame-boundary registration; heavy work on a hub worker (inv #1/#10)

The split is **register-on-the-drain, produce-on-the-hub**:

- **`CommandDrain` (output-clock loop, frame boundary)** does only cheap binding mutations:
  - `UpsertSource`: create **or reuse** the per-source `TileStore` (reuse on edit — see §5),
    `CompositorDrive::insert_store`, register the route key
    (`RouteResolution::set_video_store_key`), mirror the source into the working config (so
    `ApplyLayout`/swap validation and config export see it), then `try_send` a spawn request to
    the **LiveSourceHub** (bounded; a full hub queue drops + warns — the tile rides the slate,
    the clock never waits).
  - `RemoveSource`: `CompositorDrive::remove_store` (new engine API) — cells bound to the id
    composite their **on_loss failover slate** from the next tick (`sample_cell` already treats a
    missing store as `NoSignal`); mirror the removal out of the working config; `try_send` a
    teardown request to the hub.
  - Registering a store before its producer has published is the normal resilience model
    (ADR-T002): the tile rides `NO_SIGNAL`/slate until the first frame arrives, then goes `LIVE`.
- **`LiveSourceHub`** (new `multiview-cli::live_sources`): one worker thread owning a bounded
  request channel. It performs every heavy/blocking step: spawn the producer thread, tear down
  (raise the per-source stop flag, join with a bounded grace, detach a wedged thread — the same
  policy `IngestSupervisor::join_all` uses), and maintain the **shared preview store map**
  (`Arc<ArcSwap<HashMap<id, Arc<TileStore>>>>`, RCU per mutation) the `CliPreviewProvider` reads —
  so live-added sources appear as preview inputs without any lock on the render thread.

### 4. One uniform producer path + per-source stop flags

A hub-spawned synthetic producer runs the **same** `synth::generator_loop` the startup path runs
(same cadence pacing, same lock-free `TileStore` publish, same chunked stop-poll). A hub-spawned
**network/file** producer (level 2, shipped) runs the **same** `ingest_loop` (same supervised
reconnect with jittered backoff, jitter handling, PTS normalization, rw-timeout) — never a
parallel ingest. The single per-source construction is shared by extraction:
`ingest_plan_for` builds the plan and `spawn_ingest_producer` (one fn) creates + registers the
per-source stop flag and spawns the named thread — `IngestSupervisor::start` (startup) and the
hub's `IngestSpawner` (`Pipeline::live_ingest_spawner`, the `LiveIngestSpawner`) both call it.

Two as-built notes on the level-2 spawn:

- **Tile geometry** comes from the *startup* solved layout (`cell_pixel_size`, canvas fallback) —
  a freshly added source is typically unbound until the follow-up route, so it decodes at canvas
  size, exactly like an unbound startup source (decode-at-display-resolution is an efficiency
  lever; the compositor scales either way).
  *Hardware-validation note (2026-06-11):* this geometry made runtime adds the first real
  exerciser of the **identity** tile-scale (decoded size == tile size ⇒ an NV12→NV12 same-geometry
  swscale request), and libswscale's no-op converter on FFmpeg 7/8 copies the luma plane but
  leaves the interleaved NV12 chroma plane zeroed — the live-added tile rendered saturated
  green/magenta while bound startup sources (a real resize) were correct. Fixed in
  `multiview-ffmpeg::Scaler::run` (an identity request is now a deep plane copy, never
  `sws_scale`) plus a direct read-out in the CLI's `TileScaler`; pinned by
  `scale_resample::identity_nv12_to_nv12_preserves_the_chroma_plane` and the end-to-end
  `live_decode_chroma` U/V-statistics parity test (live-added decode == startup decode of the
  same clip).
- **Registration pattern (decided):** the drain registers the store and bindings immediately at
  the frame boundary and the tile rides `NO_SIGNAL → LIVE` as the hub-spawned ingest comes up —
  the same pattern the synthetic flow shipped with. No "hub reports ready" handshake exists or is
  needed: ADR-T002's state machine covers the warm-up window, and a spawn that ultimately fails
  leaves an honest slate.
- **Captions gap (honest):** a live-added network source gets **no** native caption reader or
  burn-in until restart — the run's `SubtitleRouter`/baker layer set is built at drive start, so
  spawning a reader would publish cues nothing samples. Caption-reader *teardown* on remove/edit
  is wired (the `{id}/`-prefixed flags); live caption *attach* is a follow-up that needs a
  runtime-extensible subtitle router.

Teardown needs per-source granularity, so the startup supervisors move from one shared stop flag
to **one stop flag per producer thread**, registered in a shared **stop registry**
(`Arc<Mutex<HashMap<id, Arc<AtomicBool>>>>`, touched only off the hot path: at spawn, by the hub
worker, and at run teardown — never by the drain or the clock):

- `run.rs GeneratorSupervisor` and `pipeline.rs IngestSupervisor` register each spawned producer's
  flag; their `shutdown` raises all of their flags and joins as before.
- A live **remove**/**edit** raises the target id's flag via the registry on the hub worker — this
  tears down a *startup* producer (generator or, on the ffmpeg path, a network ingest thread)
  exactly like a hub-spawned one. A thread wedged in a blocking libav call is bounded by the
  existing `rw_timeout` + join-grace/detach policy.

### 5. Edit = upsert under the same id, reusing the store

`UpsertSource` for an existing id **keeps the registered `TileStore`** and swaps the producer
behind it (hub: teardown old → spawn new into the same store). The bound tile holds last-good and
rides the `LIVE → STALE → (RECONNECTING)` ladder while the producer swaps — it never falters and
never flashes the slate unless the new producer genuinely fails to produce (then the honest
`NO_SIGNAL` path applies). A kind change synthetic→network (until level 2) is a remove (the old
picture stopping is more honest than a stale generator pretending to be the new URL).

**Bounded old-frame window:** the old producer stops *cooperatively* (its stop flag is observed
between publishes — within one read/render cadence for a generator, one packet-read/`rw_timeout`
for an ingest thread), so on an edit a handful of the old producer's frames can still land in the
reused store after the new content is requested, interleaved with the new producer's first frames.
This is wrong-*content* for a bounded moment, never a falter or a stall, and is the same class of
window any source switch has. To shrink it, the drain submits the hub teardown/spawn request
**before** mutating the drive bindings, giving the hub a head start on raising the old producer's
flags. The teardown also raises every `{id}/`-prefixed companion flag — notably `{id}/captions` —
so a replaced network source's caption reader stops with it instead of burning the stale URL's
cues over the replacement picture.

### 6. Removal semantics

Removal is a deliberate operator act, not a signal loss: the cell transitions to its `on_loss`
failover slate at the **next frame boundary** (the terminal state of the loss ladder) rather than
sitting in last-good/STALE limbo pretending the source still exists. Producer teardown, preview
unregistration, and memory release happen asynchronously and bounded on the hub. A cell whose
*config* still binds the removed id keeps compositing the slate; a subsequent `ApplyLayout`
re-solve will refuse (config validation requires declared bindings) with a warning until the
operator re-routes that cell — held, never a panic, never a falter.

### 7. Hardware re-assessment on every change

- **Synthetic add/edit/remove (this slice):** zero decode/GPU demand — there is nothing for the
  placement engine to place (CPU raster into a shared frame, one bake/s for a clock). Consulting
  the planner would be a fabricated no-op; we state that instead of pretending.
- **Network add (level 2 — SHIPPED):** the hub consults the **same** scorer the startup
  admission uses (`multiview_hal::select_device`, the exact `select_admission_pick` path in
  `pipeline.rs` — implemented as `select_live_decode_placement`), with two changes honouring the
  placement principle: the **candidate set is exactly the running island's device** — a runtime
  add may never fragment or migrate the island — and the demand is the island's current tile set
  **plus** the new decode (`TileLoad::new(Decode, …)`), re-polling NVML at decision time. The
  outcome is an explicit tri-state on the `IngestPlan` (`DecodePlacement`): an admit stamps
  `Pinned(cuda_ordinal)` (NVDEC co-located, ADR-0035 Tier-1/Tier-2 affinity + perf-class
  budget); a reject (budget/headroom/island absent from the snapshot) stamps `SoftwareOnly`,
  which the single decoder-open gate (`decoder_open_args`) turns into a **forced software
  open** for that source only, with a loud warning — the island is never overcommitted and the
  output never falters.

  *Amendments where reality diverged from the sketch above:*
  - The sketch named `Pins::pin_pipeline(island_device)`. As built, the consult **restricts the
    candidate set to the island device with no pin** instead: a hal pin deliberately *bypasses*
    the headroom ceiling (operator-pins-always-win, by design), which would have defeated this
    section's reject → software-decode ladder. The single-candidate set preserves the
    never-fragment guarantee identically (the scorer physically cannot name another GPU) while
    keeping the headroom gate live.
  - The live demand sets `opens_encode_session = false`: a decode-only add opens no NEW NVENC
    session, and the island's running session is already in the *measured* snapshot — modelling
    it again would double-count it against the session ceiling. (The demand still carries the
    island's composite + encode tile loads, as startup models them; the budget gate sees the
    same shape.)
  - A reject is the explicit `DecodePlacement::SoftwareOnly`, **not** "no ordinal". The first
    build encoded a reject as `cuda_ordinal: None` on the plan — but the decoder open computed
    its hardware preference independently, and *hardware-wanted with no ordinal* opens NVDEC on
    libav's **default** CUDA device: on a single-GPU host that is the over-headroom island
    itself (the reject would overcommit it anyway), and on a multi-GPU host it may be a
    *different* GPU (silent island fragmentation, forbidden by ADR-0018). `Option<ordinal>`
    cannot distinguish *no placement decision* from *placement rejected*, so the plan carries
    the tri-state `DecodePlacement::{Default, Pinned(ordinal), SoftwareOnly}` and one gate
    (`decoder_open_args`) maps it to the decoder open — `SoftwareOnly` forces a software open
    even when NVDEC is compiled, present, and not env-disabled (the operator's
    `MULTIVIEW_DISABLE_NVDEC` opt-out still wins over a pin). The island-vanished consult
    (device absent from the load snapshot) is `SoftwareOnly` for the same reason.
  - The island identity (`LiveIsland`: device id + CUDA ordinal + startup tile count) is
    published by `drive_streaming` into a lock-free slot (`ArcSwapOption`) after the decide-once
    admission pick; the spawner reads it per spawn. No pinned island (GPU-free host, no NVML, a
    startup scorer rejection) ⇒ no consult and `DecodePlacement::Default` — in lockstep with the
    startup plans, exactly the degrade ladder above.
  - The demand's "current tile set" is the **startup** tile count + the one new decode.
    Previously live-added decodes are not re-modelled in the demand: they are already in the
    *measured* NVML load the scorer reads (re-modelling them would double-count). This is the
    same measured-load stance as removal, below. **Staleness bound:** a live layout apply
    (ADR-W019) can grow/shrink the cell set after the island is published, and the consult's
    modelled demand keeps the *startup* tile count — the canvas itself cannot change live
    (a canvas mismatch is a held Class-2), so the drift is bounded by the cell-count delta,
    and every live-applied cell's real composite cost is already in the *measured* NVML
    snapshot the same consult re-polls. The modelled tile set only skews the budget estimate
    by that delta; the measured headroom ceiling still gates an actually-loaded island.
- **Removal returns its budget implicitly:** the startup path books nothing — placement decisions
  read *measured* NVML load per decision, so a removed source's NVDEC/VRAM consumption disappears
  from the next decision's inputs when its decoder closes. There is **no allocation ledger** to
  credit; per ADR-0035 the per-GPU `CostBudget`/perf-class table gates *demand*, not bookings.
  Gap (noted, not built here): a booking ledger would let admission reason about in-flight
  spawns that have not yet hit NVML counters.

### 8. Proof obligations (gates)

- **Soak/chaos (inv #1/#10):** extend the flooded-command-bus soak: continuous
  `UpsertSource`/`RouteVideo`/`RemoveSource` churn against a bounded run must yield exactly N
  frames for N ticks, never faltered. A realtime run must show a live-added source's tile reach
  `LIVE` (observed via `tile.state` events) and a removed source's cells return to slate.
- **Engine unit gates:** `remove_store` → bound cell composites slate next tick; `rebind_cell` to
  a removed source is an honest held error.
- **Route gates:** header truth-table above, pinned by integration tests over the real router.

## Scope shipped vs designed

Shipped (vertical, end-to-end, soak-gated): **level 1** (live add, synthetic kinds), **level 2**
(live add/edit of network/file kinds — rtsp/hls/ts/srt/rtmp/file — via the hub-spawned
`ingest_loop` + the island-pinned placement consult above, on the full-pipeline run; the
`LiveSourceCapability` signal keeps the software run's header at `restart`), **level 3** (live
remove, any kind whose producer is registry-known; composition-plane removal for all kinds),
**level 4** (live edit via store-reuse upsert — synthetic↔synthetic, network↔network, and
synthetic↔network on a network-capable run). The UI copy, the per-save toast (echoing the
response header) and OpenAPI descriptions state exactly this per-kind split.

Not shipped (header stays `restart` — truthful, with the per-kind reason):

- `ndi` — bypasses libav entirely (runtime-loaded host-memory receive; the SDK-backed receiver
  bind is itself still the deferred live-only half of NDI ingest);
- `youtube` — needs the external `yt-dlp` resolver behind the off-by-default `youtube` feature;
  wiring its capability honestly needs a per-feature signal + resolver-availability check;
- `aes67` — audio-only; the CLI pipeline has no video ingest plan for it at all;
- the placement **booking ledger** (in-flight spawns not yet visible to NVML counters);
- live caption attach for runtime-added sources (see §4 — the subtitle router is built at drive
  start; teardown of caption readers **is** shipped via the `{id}/`-rooted flags).

## Alternatives considered

- **Spawn producers on the drain (render thread)** — rejected: thread spawn + libav open are
  unbounded-latency; violates inv #1.
- **Engine-side spawner driven by a second channel from the route handler (bypassing the drain)** —
  rejected: registration must happen at a frame boundary with `&mut CompositorDrive` (the drain is
  the only seam with that access), and a single ordered command stream keeps upsert→route→remove
  sequences coherent (FIFO on one bus).
- **Tear down by removing the store but leaving the producer running** — rejected: leaks a thread
  + render work per removed source; per-source stop flags are cheap and uniform.
- **Ride the STALE→NO_SIGNAL ladder on remove (keep the store until it ages out)** — rejected:
  startup stores use `NoSignalPolicy::HoldForever`, so a removed source would freeze its last
  frame forever — dishonest. Deliberate removal cuts to the on_loss slate at the next boundary.
- **A separate live-apply REST surface (`/sources/{id}/apply`)** — rejected: inv #11 wants every
  mutation to declare how it applies; a second endpoint invites stored-vs-live drift.
- **Re-running the whole island admission pick on every add** — rejected: could name a *different*
  GPU and imply migration; the placement principle pins runtime adds to the island device.

## Consequences

- The command bus now carries config payloads (`Source` is `Clone + PartialEq`); the drain mirrors
  them into its working config, so export/`ApplyLayout` stay coherent with live state.
- `CompositorDrive` gains `remove_store`/`store` accessors; removal makes a bound cell slate —
  pinned by engine tests.
- The preview provider's input set becomes dynamic (shared `ArcSwap` map) — `input_ids()` reflects
  live adds/removes.
- Startup supervisors carry one stop flag per producer (same join semantics; `shutdown` raises
  all), enabling targeted teardown forever after.
- Level 2 flipped the header to `live` for network kinds with **no path change** — exactly the
  per-class flip ADR-W015 anticipated — and introduced the `LiveSourceCapability` run signal:
  any future kind that gains a live spawn path flips its header by widening the capability the
  binary derives from its wired seams, never by re-classifying in the control plane.
