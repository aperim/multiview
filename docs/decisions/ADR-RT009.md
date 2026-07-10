# ADR-RT009: Connect-time broadcast watermark suppresses deltas already reflected in the snapshot

- **Status:** Accepted
- **Area:** Realtime API
- **Date:** 2026-07-10
- **Source:** cross-vendor auth-panel finding on PR #211 (reviewer A2; pre-existing defect, task #8)

## Context

The WS/SSE realtime stream follows snapshot-then-delta + resume-by-seq
([ADR-RT002](ADR-RT002.md)/[ADR-RT003](ADR-RT003.md)): on connect a client gets a
`$snapshot` of current state "with the seq/ts it is current as of", then a stream of
deltas, one per engine event. Both transports
(`crates/multiview-control/src/realtime.rs` `run_ws_session` / `sse_handler`)
subscribe to the engine's drop-oldest event broadcast **before** reading the engine
state snapshot — the correct order, because subscribing first guarantees no event is
missed between the snapshot read and the first live delta.

But **no watermark suppresses the events that land in that subscribe→snapshot window.**
An event can enter the subscription, be folded into engine state, be **included in the
connect-time snapshot**, and then **also be delivered again as a delta** — a duplicate.
Worse, several queued pre-snapshot transitions of the same object can replay *after* the
newer snapshot, so a client's state **rolls backward transiently** (e.g. snapshot shows a
tile `Live`, then a stale queued `Live→NoSignal` delta replays). ADR-RT003 mandates
`snapshot ⊕ ordered deltas = current truth`; delivering a delta already folded into the
snapshot violates it.

Two facts bound the fix:

1. **The engine keeps two independent sequence counters**
   (`crates/multiview-engine/src/isolation.rs`): the latest-state slot (`LatestState`)
   has its own `sequence()`, and the event broadcast (`EventStream`) has its own. The
   value threaded through the connect path today as `snapshot_seq`
   (`state.engine.state.sequence()`) is the **state-slot** counter — used only for the
   envelope `ts`. It is **not** comparable to a broadcast `SeqEvent.seq`, so it cannot be
   a delta watermark. The watermark must come from the **event** counter,
   `state.engine.events.sequence()`.
2. **The publish path must not gain a client-shared lock** (invariant #10): the engine
   never awaits a client and never sends on a channel a slow consumer can fill
   ([ADR-RT004](ADR-RT004.md)). Any fix is a **read-side, per-connection** decision on
   events already received from the bounded broadcast — never a change to the publish
   path, never a new lock/await/channel into the engine.

## Decision

Capture a per-connection **broadcast watermark** = `state.engine.events.sequence()` at
connect, paired with the snapshot read, and **drop every subscribed event whose
`seq <= watermark`** before issuing the per-connection sequence. Concretely, in
`crates/multiview-control/src/realtime.rs`:

- `SessionStream` gains an `Option<u64>` `snapshot_watermark` field and a
  `with_snapshot_watermark(u64)` builder (mirroring `with_object_scope` /
  `with_corr_registry`). `None` = unchanged behaviour (existing transport-only and resume
  tests).
- `SessionStream::frame_for` drops any event with `seq <= watermark`, returning `None`,
  **before `issue_seq`** — so the drop leaves no gap in the per-connection seq, exactly
  like the existing resume/conflated/object-scope skips. It sits after the resume-skip
  block and before the object-scope filter.
- Both `run_ws_session` and `sse_handler` capture the watermark **immediately after
  `subscribe()` and before the snapshot read**:
  `let sub = state.engine.subscribe(); let watermark = state.engine.events.sequence();
  let (snapshot, snapshot_seq) = current_engine_snapshot(&state);` and pass it via
  `.with_snapshot_watermark(watermark)`.

**Atomicity (why watermark-before-snapshot).** The engine publishes **state-then-event**
within a tick (`crates/multiview-engine/src/runtime.rs`: `publish_state(state_of(frame))`
precedes `publish_event(event)`, no `.await` between). So the state fold for any event
with `seq <= watermark` *happens-before* that event's seq is assigned, which
*happens-before* our read of the watermark, which *happens-before* our snapshot read.
Therefore the snapshot reflects **every** event with `seq <= watermark`: dropping those
deltas loses nothing (no lost events, no persistent desync). This is the same fence idiom
already used in this crate for the discovery-scan correlation window
(`routes/discovery.rs`: `from_seq = state.engine.events.sequence()`; `CorrWindow` stamps
only events with `seq > from_seq`).

## Rationale

- **Correct sequence space.** The watermark is read from the same counter that stamps
  `SeqEvent.seq` and the resume cursor (`resume_after`), so `seq <= watermark` is a
  meaningful comparison; the old `snapshot_seq` (state-slot counter) is not.
- **No lost events over no duplicates.** Read-side capture cannot be perfectly atomic
  with a lagging state blob without an engine-shared lock (forbidden by #10). Given the
  choice of race residual, capturing the watermark **before** the snapshot biases to *at
  most a transient, self-healing duplicate* (an event whose state folds into the snapshot
  but whose seq lands just above the watermark), never a **dropped** delta. RT003 is an
  at-least-once, idempotent `snapshot ⊕ delta` rebuild — a sub-microsecond duplicate is
  within contract and converges on the next delta; a lost delta would desync the client
  until the next unrelated transition.
- **Invariant #10 by construction.** `events.sequence()` and the state reads are single
  wait-free atomic loads. No lock the publisher needs, no `.await`, no new channel, no
  back-pressure. A slow/filtered client still only lags its own bounded broadcast
  receiver.
- **Composes with the rest of the contract.** Dropping before `issue_seq` keeps the
  per-connection seq gapless (resume-by-seq intact); the watermark drop runs before the
  [ADR-W005](ADR-W005.md)/[ADR-W025](ADR-W025.md) object-scope filter, which is unchanged.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Reuse the existing `snapshot_seq` (`state.engine.state.sequence()`) as the watermark | Wrong sequence space — the state-slot and event-broadcast counters are independent; comparing a state-slot seq to a `SeqEvent.seq` is meaningless and would drop/keep arbitrary deltas. |
| Capture the watermark **after** the snapshot (snapshot-first) | Eliminates the transient duplicate but, given state-then-event ordering, risks **dropping** an event whose seq was assigned before the watermark read but whose state fold had not yet reached the snapshot — a lost delta and permanent desync, strictly worse than a self-healing duplicate. |
| Engine-side atomic (snapshot, event-seq) capture under one lock | Introduces a lock (or critical section) shared between the publish path and the connecting consumer — reintroduces the back-pressure surface [ADR-RT004](ADR-RT004.md)/#10 forbids. |
| Content/identity dedupe of deltas against the snapshot | Stateful, unbounded, and event-type-specific; the monotonic broadcast seq already gives an O(1), bounded, type-agnostic fence. |
| Set `resume_after = Some(watermark)` on connect instead of a new field | `resume_after` switches `next_delta` to the non-blocking `try_recv` replay-drain mode (for reconnect); a fresh connect must stay on the blocking live-tail `recv().await`. Conflating them would break the connect path's liveness. |

## Consequences

- The systematic connect-time defect is fixed: the entire pre-snapshot backlog buffered
  in the subscribe→snapshot window is suppressed, so a client joining a busy stream no
  longer sees duplicate deltas or a transient backward roll on top of its fresh snapshot.
- **Invariant #10 preserved and re-stated:** the change adds no lock, no `.await`, no
  channel into the engine, and no back-pressure; the watermark is one wait-free atomic
  load and a per-connection integer compare. The engine publish path is untouched.
- Resume-by-seq ([ADR-RT003](ADR-RT003.md)), the per-connection gapless seq, and the
  #211 object-scope projection ([ADR-W025](ADR-W025.md)) are all unchanged — the watermark
  drop is one more pre-`issue_seq` read-side skip alongside them.
- We commit to the engine's **state-then-event** publish ordering for the *no-lost-events*
  guarantee. If that ordering ever inverts, the residual degrades to "no worse than
  today" (a possible transient duplicate), not to lost events — but the invariant should
  be re-verified with this ADR. A residual sub-microsecond duplicate remains possible and
  is deliberately accepted as self-healing under the at-least-once contract.
