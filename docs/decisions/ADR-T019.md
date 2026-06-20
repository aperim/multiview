# ADR-T019: Media-player audio-on-loop — a sample-exact buffer-and-replay loop deck with an equal-power crossfade at the seam, sample-locked to the video wrap

- **Status:** Proposed
- **Area:** Streaming/Timing / Audio
- **Date:** 2026-06-20
- **Source:** operator task #135 (the stacked follow-on to the video vamp/loop
  player, PR #204) — "a looping media player loops its audio on the SAME wrap
  instant as video, with an equal-power crossfade at the seam to kill the
  loop-point click"
- **Extends:** [ADR-0097](ADR-0097.md) §8 (the audio-on-loop scope boundary — the
  video player ships silent by design; audio-on-loop is the committed follow-on
  `multiview-audio` integration slice), [ADR-0057](ADR-0057.md) Decision 6
  (embedded audio rides the per-source `AudioStore` + decode thread)
- **Relates to:** [ADR-0059](ADR-0059.md) (switcher audio — the BUILT
  `GainRamp`/`ProgramBus`/`SampleClock` primitives this reuses),
  [ADR-T015](ADR-T015.md) §5 (AFV ramp sample budgets derive from
  `SampleClock::total_at`, never a per-tick average), [ADR-T003](ADR-T003.md) /
  [ADR-T001](ADR-T001.md) (exact rationals, never float fps/sample-rate; the
  output clock samples inputs, never paced — inv #1/#3), [ADR-R005](ADR-R005.md) /
  [ADR-R006](ADR-R006.md) (per-source store + program-bus mix + R128 loudnorm),
  [ADR-0009](ADR-0009.md) (the data-plane OS-thread vs Tokio split — the audio
  decode/loop runs on a dedicated thread, never a Tokio worker)

## Context

The video vamp/loop media player shipped in PR #204 ([ADR-0097](ADR-0097.md)):
a pre-declared player channel loops a clip over `[in_point, out_point)`, vamps
the `[vamp_in, vamp_out)` sub-window with a clean armed exit, frame-accurate and
API-drivable, driven by the pure `MediaPlayer` transport core
(`crates/multiview-cli/src/player/transport.rs`) executed by the `ffmpeg`-gated
`stream_player` loop (`crates/multiview-cli/src/pipeline.rs:9337`). [ADR-0097](ADR-0097.md)
§8 deliberately scoped that as the **video tile** only: a boot-spawned player is
**silent by design**, and the embedded audio joining the program bus and
*looping on the same wrap instant* is the committed follow-on slice — this ADR
and its implementation.

What is **BUILT and verified** today (every path checked in the worktree at this
ADR's base, `main` @7047b080):

- The **video player wraps on its source-frame index**, not a wall clock:
  `MediaPlayer::on_decoded(source_frame)` returns `SeekFlushTo { frame }` exactly
  when the decoded frame index reaches `vamp_out` (vamping) or `out_point`
  (looping), against the `PlayoutGeometry` integer-frame window
  (`transport.rs:370-407`). Stamping is output-anchored
  (`publish_at(k) = anchor + k × frame_period`, `transport.rs:438-470`), anchored
  at output media time **ZERO** at boot (`handle.rs:55`, `pipeline.rs:9361-9367`).
- The **per-source audio path** is BUILT: every libav-openable source gets an
  `Arc<AudioStore>` (`crates/multiview-cli/src/audio.rs:63`) filled by a decode
  thread (`audio_ingest_loop` → `multiview_audio::store::audio_decode_loop`,
  `audio.rs:100`), routed onto the `ProgramBus` at unity gain
  (`pipeline.rs:2988`, `ProgramBus::add_source`, `program.rs:136`); the bus is
  **owned by the bake-consumer thread** and ticked by `tick_to(tick+1)`
  (`pipeline.rs:4233`) — never on the output-clock loop (inv #1/#10).
- The **equal-power crossfade primitive** is BUILT: `GainRamp::up`/`down` and
  `GainRamp::envelope_at` (sin/cos, `sin²+cos²=1`, arbitrary endpoints,
  `crates/multiview-audio/src/mixer.rs:35-112`) — the per-sample equal-power
  envelope the switcher AFV uses.
- The **exact-rational sample clock** is BUILT: `SampleClock::total_at(tick)`
  gives the exact cumulative sample count at an output tick by integer remainder
  accumulation (the NTSC 1601/1602 alternation at 30000/1001 @ 48 kHz, no float;
  `crates/multiview-audio/src/cadence.rs:80`).
- The `AudioStore` write/read primitives: `publish(block)` (append, drop-oldest),
  `read(frames)` (always returns exactly `frames`, silence-filling gaps),
  `read_cursor()` (the bus's pull position), `seek_to_frame`-style cursor control
  (`crates/multiview-audio/src/store.rs`). `tone_publish_loop`
  (`audio.rs:190`) is the proven pattern for a thread that keeps a store filled a
  bounded lead **ahead of the bus's read cursor**, gap-free, off the hot path.

What does **NOT** exist today, and is therefore the gap this ADR closes:

- **The boot path builds no audio store/plan for a media-player channel.** The
  per-source audio loop (`pipeline.rs:1980-2048`) iterates `config.sources`;
  media players come from `config.media_players` and are boot-spawned separately
  in `build_media_player_boot` (`pipeline.rs:7433`), which today builds only the
  **video** `IngestPlan` + `TileStore`. A player therefore contributes silence to
  the bus.
- **The audio file decoder has no seek and no flush.**
  `multiview_ffmpeg::AudioFileDecoder` (`crates/multiview-ffmpeg/src/audio_file.rs`)
  exposes `open`/`next_block`/`rate`/`channels`/`frame_count` only — verified, no
  `seek`/`flush`. Unlike video, audio containers have **no IDR**: a container
  audio seek lands on a packet boundary, which is *sample-imprecise*. A
  sample-imprecise loop point produces a few-ms gap or overlap **every lap** — a
  click — which is the exact failure this feature exists to kill (rule 26: bad/seam
  inputs producing clean output is the whole point).
- **No loop-aware audio driver.** `audio_decode_loop` is file→store-to-EOS; it has
  no concept of a `[vamp_in, vamp_out)` replay window, a seam crossfade, or a
  transport mailbox.

## Decision

The player's audio loops by **sample-exact buffer-and-replay**, not a decoder
seek: decode the asset's `[in_point, out_point)` window **once** into a bounded
in-memory PCM buffer, then replay the `[vamp_in, vamp_out)` segment as an
**overlap-add loop** whose hop (period) is exactly the **decoded 48 kHz sample
count of the `[vamp_in, vamp_out)` time range** — which *is* the audio duration of
the `vamp_len` video frames, drift-free at any cadence — with a
**correlation-adaptive crossfade** (equal-power for a decorrelated seam, linear
for a correlated one) across a bounded seam window. The crossfade reuses the
existing `GainRamp` equal-power curve (and a linear sibling) applied to the
buffered samples — no new decoder, no decoder seek, no new FFI surface.

### 1. A new pure `LoopDeck` core (`crates/multiview-audio/src/loopdeck.rs`)

A pure, deterministic, libav-free state machine — the **audio analogue of the
video `MediaPlayer`** — feature-independent (CI-green default build),
unit-and-property-tested with zero hardware.

**The loop length `L` is a sample count, not a tick count.** `L` is the number of
decoded 48 kHz frames in the `[vamp_in, vamp_out)` *time range* of the asset. The
deck never feeds frame indices into `SampleClock::total_at` (whose argument is an
output-**tick** index — conflating the two only happens to align today because the
boot path retimes a player to one frame per output tick, `pipeline.rs:2056`, and
would drift the moment a 24 fps asset played on a 60 fps canvas). Because `L` is
the *audio duration of `vamp_len` asset frames*, the audio lap and the video lap
have **identical media-time length for any cadence** — that is the sample-lock,
established by what `L` *is*, not by an arithmetic coincidence (§3).

**The deck is an overlap-add loop, positioned by the absolute output frame.**
`read_at(abs_frame, frames) -> AudioBlock` returns exactly `frames` frames of the
looped stream as a **pure function of `abs_frame`** (so a forced cursor realign —
the bus's read-cursor catch-up under load, §2 — always lands inside a correctly
faded seam, never a skipped/un-crossfaded one). The deck holds:

- `body`: the `[vamp_in, vamp_out)` content, exactly `L` frames.
- `lapover`: `W` frames of **real content immediately past the loop point**
  (`[vamp_out, vamp_out + W)` when it exists, else the wrapped head `[vamp_in,
  vamp_in + W)` = `body[0..W)`), `W = min(W_target, L/2)`.

For absolute frame `f`, let `m = f mod L`:
- `m ∈ [W, L)` (clean middle): `out = body[m]`.
- `m ∈ [0, W)` (seam): overlap-add the **previous lap's tail** (`lapover[m]`,
  fading out) with **this lap's head** (`body[m]`, fading in):
  `out = win_out(m)·lapover[m] + win_in(m)·body[m]`.

The hop is exactly `L`, so the period is exactly `L` for any number of laps
(sample-lock). The overlap of complementary windows is C0-continuous, so there is
**no hard wrap** anywhere (a single baked-in crossfade region leaves a hard wrap
at the period boundary — rejected; overlap-add is the only seamless construction).

**The fade law is chosen by seam correlation, once at build.** The normalized
cross-correlation `ρ` between `lapover[0..W)` and `body[0..W)` is computed once:
- `ρ ≥ ρ_corr` (≈ 0.5 — a sustained tone / drone / musical pad continuing across
  the loop, where the two legs are near-identical): **linear** (constant-amplitude)
  `win_in(m) = (m+½)/W`, `win_out(m) = 1 − win_in(m)`. For correlated legs the
  amplitude sums to a flat 1 — no level transient. (Equal-power here would swell
  `cos+sin = √2 ≈ +3 dB` at the seam.)
- `ρ < ρ_corr` (decorrelated — a hard scene change at the loop point, different
  musical moments): **equal-power** `win_in(m) = sin((m+½)/W·π/2)`,
  `win_out(m) = cos(…)` (the BUILT `GainRamp` curve, `mixer.rs:89`). For
  decorrelated legs the *power* sums flat (`tail²cos² + head²sin² = σ²`) — no
  dip. (Linear here would dip ~−3 dB.)

Both laws are C0-continuous (click-free); the correlation choice removes the
*level* transient. This is the honest DSP answer the cross-vendor review forced:
"equal-power kills the click" is only true for a decorrelated seam.

- Transport: `vamp()` (loop the segment — the default), `arm_exit()` /
  `take_exit()` / `cancel_exit()` (a vamping deck with the exit armed plays the
  current lap out to the seam, then **settles to silence** — a short cosine
  fade-to-silence over the seam window so the bus contribution ends click-free,
  whatever the EOF policy: the *video* tile applies hold/black/auto-off; the audio
  bus simply stops contributing), `play()` (one-shot forward play then settle),
  `pause()` (contribute silence — not a frozen DC sample, which would click on
  resume), `stop()` (re-cue to the head). The exit latch is **consumed exactly
  once** at the first seam at-or-after the arm — the same exactly-once boundary
  contract the video player proves ([ADR-0097](ADR-0097.md) adversarial
  self-review).
- It never blocks and never reads a wall clock; it returns samples, it does not
  pace (inv #1). All loop/seam math is integer frame counts; the only floats are
  the fade gains and the one-time `ρ` (genuinely continuous quantities), never
  time (inv #3).

### 2. The `ffmpeg`-gated CLI driver (`crate::audio::player_audio_loop`)

The audio twin of `stream_player`, on a **dedicated OS decode thread** ([ADR-0009](ADR-0009.md),
never a Tokio worker):

1. **Prime once.** Open the asset with `AudioFileDecoder` (48 kHz / bus layout /
   `f32`), decode forward, and capture **only** the `[vamp_in, vamp_out + W)`
   frames into the `LoopDeck` (the `[vamp_in, vamp_out)` loop body + the `W`
   lap-over frames; the pre-vamp head and post-tail are *not* buffered — the MVP
   loops the vamp segment). Decoding is bounded by the asset's declared
   `out_point_frames` **and** a hard cap `MAX_LOOP_SECONDS` (§5); the buffer is
   allocated once, never per-sample (inv #5 / rule 22). A decode that yields
   nothing (no audio stream, truncated, in-point past clip end) leaves the deck
   **empty** → the source rides **silence** (the store silence-fills), never an
   error, never a stall.
2. **Replay — the deck position is the bus's absolute read cursor.** Keep the
   `AudioStore` filled a bounded lead **ahead of the bus's read cursor** (the
   `tone_publish_loop` pattern, `audio.rs:190`): each wakeup, read
   `store.read_cursor()` (the absolute frame the bus will pull next); the next
   frame to publish is `published` (the absolute write head). If a catch-up moved
   the read cursor past the write head, set `published = read_cursor` (never
   back-fill an evicted span). Then publish `deck.read_at(published, n)` for a
   bounded `n` and advance `published`. Because the published samples are
   `deck.read_at(abs_frame, …)` — a **pure function of the absolute frame** — a
   forced realign across a seam still emits the correctly faded seam for that
   absolute position (no skipped/un-crossfaded seam under load; the defect a
   stateful "deck cursor" would have, rule 26). Bounded bursts, mostly asleep, off
   the hot path.
3. **Transport.** Drain the **same shared `Arc<TransportMailbox>`** the video
   thread drains, applying each `TransportVerb` to the `LoopDeck` (`Vamp`,
   `ArmExit`, `TakeExit`, `CancelExit`, `Pause`, `Stop`, …). Because both threads
   read the same mailbox and both wrap on the same `PlayoutGeometry` window, the
   armed exit and every transport change land on the **same boundary** for audio
   and video.

### 3. Sample-lock to the video wrap — the alignment guarantee

The audio wrap is sample-locked to the video frame wrap because of **what `L`
is**, not an arithmetic coincidence: `L` is the decoded 48 kHz sample count of the
`[vamp_in, vamp_out)` *time range*, i.e. the audio that plays during the
`vamp_len` video frames. So one audio lap and one video lap are the **same
media-time duration at any asset/output cadence** — a 24 fps clip on a 60 fps
canvas, a 25 fps clip on 30000/1001, all hold. Both rails start at output media
time ZERO at boot (the video player anchors at `MediaTime::ZERO`, `handle.rs:57`;
the audio deck is positioned by the bus's absolute read cursor, which begins at
frame 0 and is driven by `drive_audio_for_item(bus, tick_index)`,
`pipeline.rs:4233`). The output clock samples both stores at the same tick, so the
two rails advance together and lap `n`'s audio seam is the media-time image of the
video's `n·vamp_len`-frame wrap — one instant on the output timeline (the
shared-anchor lip-sync guarantee, [ADR-R005](ADR-R005.md): all audio PTS-locked to
the same program clock as video). No `SampleClock::total_at`, no float, no per-tick
average, no wall clock — never the ~3.6 s/hour float-fps drift (inv #3).

> **Note (the unit trap the review caught).** It is tempting to write `L =
> SampleClock::total_at(vamp_out) − total_at(vamp_in)`, mirroring
> [ADR-T015](ADR-T015.md) §5's AFV-ramp formula. That is **wrong here**:
> [ADR-T015](ADR-T015.md) §5 feeds `total_at` **output-tick** indices (`t₀`,
> `t₀+N`), whereas `vamp_in`/`vamp_out` are **asset source-frame** indices. The two
> are equal only when the asset cadence equals the output cadence and the player
> emits one frame per tick — true today by how the boot path retimes a player, but
> a latent drift the moment that does not hold. Deriving `L` from the decoded
> sample count of the time range is correct for every cadence.

### 4. Boot wiring (`build_media_player_boot`, additive)

`build_media_player_boot` (`pipeline.rs:7433`), for a player whose default asset
declares an out-point, additionally:

- builds an `Arc<AudioStore>` at the canonical format and registers it in
  `audio_stores` (so the existing bus-routing at `pipeline.rs:2988` picks it up at
  unity gain — the same path every source's audio takes), and
- queues a **player audio plan** carrying the asset path, the `PlayoutGeometry`
  (so the deck's loop window matches the video's), the EOF policy, and the shared
  mailbox `Arc`. The plan spawns a `player_audio_loop` thread alongside the
  existing audio/tone threads (`pipeline.rs:5497`), under the same per-`{id}`
  stop-flag registration so a live remove stops it too.

A player whose asset is **silent** (no audio stream) contributes silence exactly
as a silent tile does — *not* a half-built feature.

### 5. Bounded memory — the `MAX_LOOP_SECONDS` cap

The loop body is held in RAM for the channel's life, so it is **bounded by an
explicit cap**, not by operator restraint (safety §5). At the canonical 48 kHz
stereo `f32` the segment costs `48000 × 2 × 4 = 384_000 B/s ≈ 0.366 MiB/s`. The
prime decode stops at `MAX_LOOP_SECONDS` (default 600 s ≈ 220 MiB/channel); a
declared vamp window longer than the cap is **refused for audio** — the player
loops video normally and its audio rides silence with a one-line warning (never an
OOM, never a silent truncation that would shift the loop point). The cap is a
`const` in `multiview-audio` consulted at prime, surfaced in config validation so
the operator sees the limit ([ADR-0057](ADR-0057.md) §8 player-count bounding is
the sibling memory bound).

### 6. Invariant posture (explicit)

- **inv #1 (output clock):** the deck/loop thread only ever **writes** the
  lock-free `AudioStore`; the bus *samples* it per tick (silence-filling any
  un-written span). A slow/wedged/stalled decode can neither pace nor stall the
  output clock. The decode/loop runs on a dedicated OS thread, never a Tokio
  worker ([ADR-0009](ADR-0009.md)).
- **inv #3 (exact time):** the loop length `L` is an integer **sample** count (the
  decoded frames of the time range — *not* a `SampleClock::total_at` tick delta,
  §3); the only floats are the fade gains and the one-time correlation `ρ`
  (genuinely continuous quantities), never time.
- **inv #10 (isolation):** the transport mailbox is the existing bounded
  drop-oldest seam the engine never awaits; the audio thread adds no channel into
  the engine.
- **Hold-last-good / never off-air:** an empty deck (no audio stream, decode
  failure, or over-cap window) rides silence; the bus's `read` always returns a
  full block. No `unwrap`/`expect`/`panic` on the loop path — a fault logs and the
  source rides silence (rule 26 / safety §7).

## Rationale

- **Sample-exact is the whole point.** A loop seam that is even one packet off
  clicks every lap; the operator asked for *no click*. Buffer-and-replay places
  the loop point on an exact sample (the decoded sample count of the time range),
  and the **correlation-adaptive** crossfade keeps the *level* flat through the
  seam for both correlated (linear → flat amplitude) and decorrelated (equal-power
  → flat power) material — reusing the BUILT `GainRamp` curve for the equal-power
  case. A decoder seek cannot offer sample accuracy on a container with no audio
  IDR; the literal equal-power-everywhere law swells +3 dB on a sustained-tone
  seam (the cross-vendor review's catch), so the law is chosen by `ρ`.
- **Reuse, don't rebuild** ([ADR-0059](ADR-0059.md) rationale): the equal-power
  curve, the exact-rational sample clock, the per-source store, the bus routing,
  and the bounded-lead publish pattern are all BUILT and tested. The net-new
  surface is one pure `LoopDeck` core + one CLI driver + four lines of boot
  wiring — no new DSP, no new FFI, no new engine channel, no new invariant
  exposure.
- **The bounded buffer is cheap and bounded.** At 48 kHz stereo `f32` the segment
  is `0.366 MiB/s`, allocated once at prime, never per-sample — squarely inside the
  data-plane bounded-memory rule. The decode is bounded by the asset's declared
  `out_point_frames` **and** the explicit `MAX_LOOP_SECONDS` cap (§5), so the cost
  is a real ceiling, not operator restraint.
- **Same mailbox, same geometry ⇒ same instant.** Deriving both rails' wrap from
  one `PlayoutGeometry` and one mailbox is the single-source-of-truth that gives
  sample-exact A/V wrap alignment, exactly as [ADR-0059](ADR-0059.md) derives AFV
  from the same switcher state machine that drives video rather than a second
  follower.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **Decoder container-seek + flush at the wrap (mirror `stream_player` literally).** | Audio containers have no IDR; a container audio seek lands on a packet boundary = sample-imprecise → a few-ms gap/overlap **every lap** = a click, the exact defect this feature kills (rule 26). Also requires net-new `seek`/`flush` FFI on `AudioFileDecoder` (none today) for a strictly worse result than buffer-replay. The "same seek-back-on-wrap" wording of [ADR-0097](ADR-0097.md) §8 is the *contract* (same wrap instant + equal-power crossfade + glitch-free), not a mandate for the seek *mechanism* — the same refinement [ADR-0097](ADR-0097.md) §5 already made for the video player (synthesize the timeline rather than route raw PTS through `PtsNormalizer`). |
| **`ProgramBus::repoint_crossfade` at the seam** (re-point the route A→B). | `repoint_crossfade` (`program.rs:217`) crossfades a **store swap** (the switcher's A→B breakaway/AFV): it spins up a temporary outgoing strip on the *old* store and fades onto a *new* store. A loop seam is one store crossfading **with itself** — overlap-adding its own tail and head inside the player's buffer is simpler, needs no second store, no temp strip, and keeps the bus route identity stable. (The bus path also can't express "fade to my own head W samples from now" sample-exactly.) |
| **Hard cut at the loop point (no crossfade).** | A *discontinuous* loop point (the common case — the waveform jumps) is an audible click/pop every lap, the literal symptom [streaming-gotchas](../research/streaming-gotchas.md) §7 lists. (A *properly trimmed* seamless loop is already waveform-continuous and a hard cut would not click there — but the player cannot assume the operator hand-trimmed to a zero-crossing, so the crossfade is the safe default; the correlation-adaptive law degrades to near-transparent for an already-continuous seam.) |
| **Equal-power crossfade for *every* seam (the obvious "constant-power" choice).** | Equal-power (`cos+sin`) is power-flat only for **decorrelated** legs; for a **correlated** seam (a sustained tone/drone/pad continuing across the loop — the canonical music-loop case) the in-phase legs sum to `√2 ≈ +3 dB`, an audible level swell every lap (the cross-vendor review's catch). The law must be chosen by the seam's measured correlation `ρ`: linear for correlated (flat amplitude), equal-power for decorrelated (flat power). |
| **Float sample-rate / `N × samples_per_tick` for the loop period.** | Inv #3 / [ADR-T015](ADR-T015.md) §5: at 30000/1001 @ 48 kHz the per-tick budget alternates 1601/1602 (1601.6 exact); a float/average product drifts the audio loop off the video edge (~3.6 s/hour). The loop length is the exact decoded **sample count of the `[vamp_in, vamp_out)` time range** — an exact integer, drift-free at any cadence. |
| **`L = SampleClock::total_at(vamp_out) − total_at(vamp_in)`.** | `total_at`'s argument is an output-**tick** index; `vamp_in`/`vamp_out` are asset source-**frame** indices (§3 Note). Equal only when asset cadence ≡ output cadence and the player emits one frame per tick — a latent drift otherwise. The decoded sample count of the time range is correct for every cadence. |
| **Resample/stretch the segment to a whole number of ticks.** | Retiming the audio detunes it and adds a resampler on the loop path for no benefit — the decoded sample count of the `[vamp_in, vamp_out)` time range is already an exact integer at the bus rate; the segment loops at its native pitch sample-exactly. |
| **Mix the loop on the engine/output thread.** | Puts decode/mix on the output-clock loop — inv #1/#10 violation. The bus mix already lives on the bake-consumer thread; the deck/loop is a *producer* into the same `AudioStore` the bus samples. |

## Consequences

- **Positive.** A looping/vamping media player now loops its audio sample-exactly
  on the same instant as video, click-free, with the armed exit firing on the
  same boundary for both rails — the operator's ask, fully realized — reusing the
  BUILT equal-power/sample-clock/store/bus primitives with no new DSP, no new FFI,
  and no new invariant exposure. A silent asset stays silent by design.
- **Negative / cost.** `multiview-audio` grows one pure module (`LoopDeck`) and
  `multiview-cli`'s `audio.rs` grows the `player_audio_loop` driver + a few lines
  of boot wiring; the vamp segment (`0.366 MiB/s`, ≤ `MAX_LOOP_SECONDS`) is held in
  RAM for the channel's life (capped per §5; player count is the sibling bound,
  [ADR-0057](ADR-0057.md) §8). Priming decodes the `[vamp_in, vamp_out + W)` window
  once at load (a bounded one-shot, off the hot path).
- **Invariants touched.** Inv #1 (the loop thread samples, never paces — proven by
  a wedge-the-loop chaos posture: the bus keeps ticking silence), inv #3 (exact
  integer sample counts), inv #10 (the shared mailbox is the only seam, bounded
  drop-oldest, never awaited). All upheld by construction and covered by the RED
  tests below.
- **Loudnorm interaction.** The program-bus R128 `LoudnormProcessor`
  ([ADR-R006](ADR-R006.md)) runs **downstream** of the loop deck. The
  correlation-adaptive crossfade keeps the seam *level* flat (no dip on a
  decorrelated seam, no +3 dB swell on a correlated one), so loudnorm sees a steady
  program across the wrap and does not pump against the seam; the −70 LUFS gate
  excludes any momentary dip from the integrated measure besides. No loudnorm-hold
  rule is needed for the loop seam (distinct from the intentional master-fade case
  [ADR-0059](ADR-0059.md) §3 addresses, where the level change *is* intended).
- **Deferred (named, not silent).** Per-bus aux routing of player audio (the
  multi-cursor `AudioReader` of [ADR-0059](ADR-0059.md) §5, still unbuilt) rides
  that later slice; play-on-take AFV for a player joining a switcher transition
  is [ADR-0059](ADR-0059.md) §2's concern, not this loop-seam slice. A live `load`
  that swaps a player's asset re-primes the deck (the MVP plays one bound default
  asset; probe-at-load asset swap is [ADR-0057](ADR-0057.md)'s post-MVP item, as
  for video).

## Adversarial self-review — residual risks (post cross-vendor review)

Two fresh-context reviewers returned **has-blocking-defects** on the first draft;
the three blocking findings are now resolved in the Decision above (the
frame-vs-tick unit error → `L` is a sample count, §3; equal-power-everywhere →
correlation-adaptive law, §1; the cursor-ownership contradiction → deck position
is a pure function of the bus's absolute read cursor, §2). The residual risks the
RED tests must gate:

1. **Cross-rail sample-lock under cadence mismatch.** The drift the review caught
   only shows up when the asset cadence differs from the output cadence, so the
   property test must assert the audio loop length equals the *audio duration of
   `vamp_len` asset frames* (not `total_at(vamp_out)−total_at(vamp_in)`) for an
   asset whose cadence ≠ output cadence — a period-stability test at one cadence
   would pass while the drift bug shipped.
2. **Seam level continuity for both correlation regimes.** The mandatory tests
   assert (a) flat *power* across a **decorrelated** seam (random equal-RMS
   content → equal-power chosen, no dip) **and** (b) flat *amplitude* across a
   **correlated** seam (a sustained sine across the loop point → linear chosen, no
   +3 dB swell, no click). A power-only test on random content would pass while a
   tone-loop swelled every lap.
3. **The forced-realign seam.** Because the deck is a pure function of the absolute
   frame, a realign (bus cursor jumps a catch-up) must still emit the correctly
   faded seam for the landed absolute position — the test reads the same absolute
   span via one big pull and via tiny pulls with an injected forward jump and
   asserts byte-identical seam output (no un-crossfaded click under load, rule 26).
4. **Short segment `L < 2W`.** `W = min(W_target, L/2)` prevents the tail/head
   windows from overlapping each other; for a degenerate `L < 2` (already rejected
   by `PlayoutGeometry`'s `vamp_in < vamp_out`) the crossfade degrades to a hard
   wrap honestly.
5. **Prime does not stall the clock.** The prime decode is a bounded one-shot off
   the output path; the store silence-fills until the first publish — the RED test
   asserts the bus reads silence (never a short block / wedge) before the deck is
   primed.
