# ADR-T018 — Output clock holds wall-clock cadence under overload (drop/repeat-to-cadence)

Status: Accepted (implementation in progress on `fix/output-clock-drop-to-cadence`)
Area: T (streaming/timing) · Relates to: ADR-T001 (output clock), ADR-T015 (switcher exact-rational frame durations), ADR-R001/R002 (last-good), ADR-E007 (degradation)
Invariants: #1 (output-clock), #2 (last-good), #3 (tick-derived PTS), #9 (degradation), #10 (isolation)

## Context

Invariant #1 says the output clock emits **one valid, correctly-timestamped frame per tick,
forever, independent of any input**. The implementation (`runtime.rs::run_inner`) advances the
clock counter by exactly **one per loop iteration** (`tick(); compose()`), and `RealtimePacer`
returns immediately when the next deadline is already in the past
(`runtime.rs:86-87 if remaining <= 0 { return }`). Consequently, when `compose()` + encode
overrun the tick budget — i.e. on a **CPU/GPU-contended host** — every subsequent deadline is
already past, the pacer never parks, and the loop **free-runs at composition speed**, emitting
each index late. Media-time then falls progressively behind wall-clock.

This was observed and proven on the frigate test box: the HLS multiview produced segments at
**~0.4–0.5× real-time**, accumulating **~84 minutes of media-vs-wall-clock lag over 3 hours**
(`PROGRAM-DATE-TIME` 84 min behind; media-sequence advancing <1/s). A live HLS player starves on
the gaps (pause) and chases the live edge on resume (race) — the operator-reported
"rapid play → stall → loop". The root cause is **the output clock is render-bound, not
wall-clock-paced under overload** — it should hold cadence by *dropping/repeating* frames, never
by slipping. (The host contention itself is a separate, expected operating condition — see the
host-contention resilience program; this ADR makes the engine robust to it.)

## Decision

Make the **emitted tick track wall-clock** and **re-emit last-good** for the gap, entirely within
the existing single-threaded, lock-free, channel-free drive loop (chosen over decoupling compose
onto its own task, which would need an `Arc`-canvas anyway plus a new engine→task channel and a
fresh inv-#10 proof).

1. **`OutputClock::skip_to(&mut self, index)`** — sets `next_index = next_index.max(index)`
   (monotonic, saturating, **never rewinds**). `pts_at`/`deadline_nanos` are unchanged and exact
   (`MediaTime::from_tick`, no float).
2. **Drive loop (`run_inner`)**:
   - Park on the **due** tick's deadline exactly as today (`pacer.wait_until(deadline(due))`).
     This preserves the "never run ahead" half of inv #1 unchanged.
   - After waking, **repeat last-good while `now_nanos >= deadline_nanos(next_index+1, seed)`**:
     for each such skipped index call `tick()` and publish the **held last-good frame** stamped at
     that index's fresh, monotonic `pts` — bounded by a per-iteration skip cap (no spin on a huge
     time jump).
   - Then `tick()` and **compose exactly one fresh frame** for the current tick; cache it as
     last-good.
3. **Last-good cache:** wrap the composited canvas in **`Arc<Nv12Image>`** so holding and
   republishing last-good is a cheap refcount bump, never a multi-MB plane copy on the hot loop.
4. **Accounting:** a repeated tick **counts as an emitted tick** (it is published, one
   `publish_state` per emitted tick), so the published sequence stays **contiguous** and
   `ticks_emitted`/`sequence`/`outcome.ticks` remain mutually consistent. Add an
   `AtomicU64 frames_repeated` counter (mirrors `ticks_emitted`) as the overload signal for
   telemetry/degradation.

### Why the deadline predicate, not a computed wall-index (adversarial-review fix D1)

The first design derived the target index via `MediaTime::to_tick(elapsed)`, but `rescale` rounds
**half-away-from-zero, not floor**. On any ordinary late wake past the half-period (normal
scheduler jitter on a *healthy* box), that rounds up to `due+1`, so the loop would compose a tick
**before its wall-clock deadline** — running ahead (inv #1 violation) and dropping an on-schedule
frame. The fix is to **never compute a rounded index**: gate each repeat strictly on
`now >= deadline(next+1)` (the already-proven exact deadline primitive). Off the overload path
(`now < deadline(next+1)`) the skip branch is dormant and behavior is byte-identical to today.

**The same flooring discipline binds the cap-path resync.** When the per-iteration repeat count
hits `MAX_REPEATS_PER_TICK`, the loop resyncs the counter to wall-clock in one `skip_to`. That
target **must be the floor** — the greatest `idx` with `deadline_nanos(idx) <= now` — not
`to_tick(elapsed)`. A real jump (VM pause / `CLOCK_MONOTONIC` leap) is arbitrary, never
period-aligned, so the fractional case `elapsed = (N + r)·period, r >= 0.5` is the common one;
the nearest rounding gives `N+1`, and composing tick `N+1` (deadline in the future) runs ahead
exactly as D1 described. The cap path computes the floor in **O(1) on the hot path** — integer
division `elapsed / period_nanos` (`period_nanos = pts_at(1)`), **not a decrement loop** whose
length would depend on the (only `> 0`-validated, unbounded) cadence. Because `deadline_nanos`
recomputes `pts_at` with rounding rather than a strict `idx·period`, the estimate can sit ±1 off
the exact floor for a non-integer cadence, so a **single** exact down-correction (one comparison)
drops it to a truly-passed deadline, and a **single** exact backstop (one comparison) falls back to
`next` should the estimate ever be >1 high (unreachable at any real `idx` — u64 ticks span ~9.7
billion years at 60 fps — but never depended on). The skip stays a genuine forward jump because the
cap only fires once `deadline_nanos(next+1) <= now`, so the floor is `>= next+1`, and the final
`skip_to` target always has `deadline_nanos(target) <= now` (never a run-ahead). (Regression risk:
an *integer*-period jump test cannot catch the over-round — floor == nearest on an integer — so the
guard is a *fractional*-jump test asserting no fresh tick is published ahead of its compose-instant
deadline, with a hard bounded-completion assert so a no-run-ahead-but-parks regression also fails.)

## Consequences

- **Inv #1 strengthened to its true meaning:** output PTS tracks wall-clock at exactly 1.0×
  forever; under load the loop emits *k-1* last-good repeats + 1 fresh composite spanning the *k*
  overrun ticks, never *k* sequential late composites. No 84-minute drift can accumulate.
- **Inv #2:** the repeat path *is* last-good lifted to the output clock.
- **Inv #3:** every emitted tick (fresh or repeat) carries `pts_at(unique strictly-increasing
  index)`; repeats reuse last-good *pixels* under a *new* pts, never a duplicate/rewound pts.
- **Inv #9/#10:** complementary and intact. The skip decision is a pure local `now_nanos`
  comparison — no new channel, single `.await` (the pacer), wait-free publish unchanged. The
  degradation loop still sheds tiles/res/fps so compose fits the budget (repeats become rare);
  skip-to-cadence is the in-loop last-resort guaranteeing 1.0× in the gap before/under exhausted
  degradation.
- **Shutdown must survive the pacer park (correctness fix).** The loop's single `.await` is
  `Pacer::wait_until` on the *next* tick's deadline; between ticks the loop is parked there. The
  pacer therefore **must observe the `StopSignal`**, or a stop raised while parked on a deadline the
  clock has not yet reached — a frozen test clock, or a slow / CPU-contended host where wall-clock
  has not advanced a whole period — would never be seen and the loop would spin/sleep forever
  (`CooperativePacer` busy-spinning; `RealtimePacer` sleeping the full remaining duration). This was
  the CI ~37-minute "hang" (worst under `--features cluster`, whose extra HA tests oversubscribe the
  cores). `Pacer::wait_until` takes the `StopSignal`: `CooperativePacer` checks it each cooperative
  yield; `RealtimePacer` caps each sleep at `PACER_STOP_POLL` (10 ms) and re-checks. `run_inner`
  re-checks stop immediately after pacing and returns without composing an extra tick. With the
  catch-up loop already checking stop per iteration and bounded by `MAX_REPEATS_PER_TICK`, **no wait
  or loop on the drive path can spin**: a stop is honoured within ~one poll interval regardless of
  clock state. The stop flag is a wait-free atomic the engine *reads* — it never awaits a client, so
  inv #10 is preserved.
- **`skip_to` drops ticks, so deterministic bounded harnesses must pace per tick, not jump.** The cap
  resync deliberately advances the tick *counter* past `emitted` (it drops the skipped ticks; PTS
  jumps once). A deterministic sleep-free harness that drives a fixed `max_ticks` budget under a
  `ManualTimeSource` must therefore **advance the clock one period per fresh compose**, never jump it
  once to `pts_at(max_ticks)`: a single up-front jump is read as overload, fires the cap+`skip_to` for
  any `max_ticks > MAX_REPEATS_PER_TICK`, advances the counter past `emitted`, and the bounded run can
  then never reach its budget (it parks on a post-skip deadline the frozen clock never covers — a
  hang). The engine's own tick tests and `SoftwareEngine::run_for*` pace one period per fresh compose
  for exactly this reason; the `huge_time_jump` resync test instead advances the clock per compose via
  its control hook, so its post-`skip_to` deadlines are met and the bounded run still completes.
- **Scope honesty:** this relieves **compositor** load (fewer composites under load). Re-emitted
  ticks still flow to the bake/encode consumer, so **encoder** load per tick is unchanged; under
  encode-bound overload the cadence-hold manifests as repeats feeding the existing
  `DropOnOverload` shedding — cadence still held.

## Testing (TDD; chaos gate per the host-contention program)

1. `overloaded_compositor_holds_wall_clock_cadence_by_repeating_not_slipping` (failing-first): a
   compose seam that advances the `ManualTimeSource` by N tick-periods per call (overrun); assert
   the published index tracks `floor(elapsed/period)` (1.0×, not lagging), fresh-composite count
   (via an explicit counter) < emitted ticks, PTS strictly increasing == oracle.
2. `mid_tick_wake_does_not_run_ahead_or_repeat_on_a_healthy_system`: wake at `deadline(due)+0.6·period`;
   assert the loop emits `due` (not `due+1`) and does not spuriously repeat — the exact case the
   floor-vs-nearest defect (D1) broke.
3. `clock_skip_to_is_monotonic_forward_only_and_keeps_exact_pts`: unit test on `skip_to`.
4. `published_indices_stay_contiguous_under_overload`: repeats keep sequence contiguous (no PTS
   gap to the muxer); only fresh-pixel ticks differ.
5. `skip_cap_bounds_work_on_a_huge_time_jump`: a far-ahead `set()` publishes ≤cap repeats, no spin.
6. `first_tick_with_no_last_good_composes_fresh` and the existing soak test
   (`runtime.rs:247-323`) pass unchanged (dormant off the overload path).
7. `runtime_stops_promptly_while_parked_on_a_future_tick_deadline` (failing-first): freeze the
   `ManualTimeSource` at the seed so tick 0 composes and the loop parks in the pacer for tick 1's
   unreachable deadline; confirm it is genuinely parked (no further ticks across a settle window),
   raise stop, and require a prompt return — the whole test under a bounded wall-clock
   `tokio::time::timeout` so a stop-blind pacer regression fails in seconds instead of hanging CI.
8. `fractional_huge_jump_resync_never_composes_a_tick_before_its_deadline` (failing-first): prime a
   0.6-period offset so the cap-path resync fires at a fractional elapsed `(K + 0.6)·period`, and
   assert at each fresh tick's compose-instant that its pts never exceeds elapsed wall-clock (no
   run-ahead). Catches the cap-path `to_tick` over-round that the integer-jump test (#5) cannot —
   floor == nearest on an integer. Fails at +0.4-period run-ahead before the floor fix, ok after.

Plus a CI chaos/soak gate running the engine under synthetic CPU starvation asserting cadence held
+ bounded lag + frames-repeated-not-slipped, and a hardware soak on a deliberately-loaded box.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
