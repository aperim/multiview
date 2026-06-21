# ADR-W025: Object scope restricts visibility — collection reads and realtime delivery filter to the principal's allowlist

- **Status:** Accepted
- **Area:** Web/API Stack
- **Date:** 2026-06-21
- **Source:** PR #205 BOLA review panel (tracked task #140) — extends [ADR-W005](ADR-W005.md)

## Context

[ADR-W005](ADR-W005.md) mandates per-object (BOLA, OWASP API1) authorization on
**every resource** in addition to the coarse RBAC role: a principal may carry an
explicit object allowlist (`Principal::scoped_object_ids`) and is denied any id
outside it even when its role would otherwise permit the action. The per-object
handlers enforce this on the path id — `GET /api/v1/cast/sessions/{id}`,
`GET /api/v1/devices/{id}`, and the device/cast mutations all call
[`authorize_object`](../../crates/multiview-control/src/auth.rs) on the addressed
id and return `403` for an out-of-scope object.

ADR-W005 left one thing implicit: whether the scope restricts only *mutation* of
a single object, or also the *visibility* of objects across the read surfaces —
the collection `GET`s and the realtime WebSocket/SSE stream. The PR #205 review
panel found that the two collection reads (`list_cast_sessions`, `list_devices`)
and the realtime snapshot+delta surface ([`realtime.rs`](../../crates/multiview-control/src/realtime.rs))
returned the **full** device/cast surface to a scoped principal — every row and
every event, regardless of its allowlist. That is a pre-existing surface-wide gap
on `main` (not introduced by #205), tracked as task #140. The task-140 review
panel's exhaustive sweep additionally found the same class on the AMWA NMOS Node
API: the IS-04 LIST routes (`/x-nmos/.../devices|senders|receivers`) were
role-gated only, and the IS-05 single-receiver connection **read**
(`GET .../receivers/{id}/active`) was not per-object authorized even though its
sibling stage `PATCH` was — both folded into this decision.

The decision is bounded by invariant #10 (isolation): the realtime path is
best-effort, bounded, and **physically incapable of back-pressuring the engine**
([ADR-P001](ADR-P001.md), [ADR-RT004](ADR-RT004.md)). Any per-principal filtering
on that path must be a pure read-side projection — no blocking, no new awaits, no
channel the engine can stall on.

## Decision

**An object-scoped principal's allowlist restricts VISIBILITY, not just
mutation.** Because a single-object `GET` returns `403` for an id outside the
allowlist, a collection read or realtime stream that returns that same object is
an enumeration leak — it tells a scoped principal an object it may not read
exists, and discloses its contents. By parity, the read surfaces must filter to
the allowlist:

1. **`list_cast_sessions`** ([`routes/cast_sessions.rs`](../../crates/multiview-control/src/routes/cast_sessions.rs))
   — after the role gate, retain only rows for which
   `authorize_object(&principal, &record.id)` succeeds. An unscoped principal
   (`scoped_object_ids: None`, e.g. admin/unrestricted operator) sees every row,
   exactly as before.
2. **`list_devices`** ([`routes/devices.rs`](../../crates/multiview-control/src/routes/devices.rs))
   — the same per-row filter on the device resource id.
3. **Realtime WS/SSE** ([`realtime.rs`](../../crates/multiview-control/src/realtime.rs))
   — [`SessionStream`](../../crates/multiview-control/src/realtime.rs) carries an
   optional object-scope allowlist set from the connecting principal. A delta
   whose event scopes to a Devices-domain **object** id outside the allowlist is
   dropped (returns `None`, exactly like the resume/conflated skips); the
   connect-time `device.status` snapshot frames skip out-of-scope device ids. An
   unscoped principal's stream is unchanged. The filter is a per-client read
   decision on the already-delivered event — the engine's publish path
   (`broadcast::send`) is untouched (invariant #10).
4. **AMWA NMOS Node API** ([`nmos/mod.rs`](../../crates/multiview-control/src/nmos/mod.rs))
   — the IS-04 LIST routes filter to the allowlist by parity with the IS-05
   per-receiver handlers: `list_devices` by the device's own id; `list_senders` /
   `list_receivers` by the resource's own id **or** its `device_id` link
   (`nmos_resource_in_scope`). The per-receiver IS-05 connection read
   (`get_active`) and stage (`patch_staged`) share one gate (`authorize_receiver`),
   so reading, staging, and listing an NMOS receiver use the same object-scope
   model; a known out-of-scope receiver is `403`, an unknown id stays `404`.

The scope axis filtered is the **object** axis (`scoped_object_ids`,
[`authorize_object`](../../crates/multiview-control/src/auth.rs)), matching the
per-object handlers. The output axis (`scoped_output_ids`,
[`authorize_output`](../../crates/multiview-control/src/auth.rs)) is unchanged —
it gates the cast-target rendition on `start_cast_session`, a write, and is not a
read-visibility concern here.

On the realtime path, scope is matched against the event's object id as the
**dedicated, narrower** helper
[`device_object_scope_id`](../../crates/multiview-control/src/realtime.rs)
derives it — the device id for `device.*` and the session id for
`cast.session.*`. This is **intentionally narrower** than the existing
[`event_scope_id`](../../crates/multiview-control/src/realtime.rs) (used to set
the envelope `id` for the `ids` filter): `event_scope_id` also returns an id for
`tile.state` (an input/tile id), `timing.status` (a program/output stream id),
and `media.player_state` (a switcher player id), none of which are
`authorize_object`-gated device/cast objects. Filtering those by
`scoped_object_ids` would be a wrong-axis check that could over-restrict, so the
object-scope filter uses `device_object_scope_id` and leaves every other event —
the `$control`/tiles/alerts/audio firehose, and `device.discovered` rows which
have no registry id yet — gated only by the connect-time role (`Action::Read`),
never by object scope.

## Rationale

- **Read/write parity is the only coherent BOLA posture.** If `GET /{id}` is a
  `403` but the object still shows up in `GET` (collection) and on the stream,
  the per-object check is cosmetic — the attacker enumerates and reads the same
  data through the unfiltered surfaces. The strongest single-object guard in the
  codebase already treats scope as a visibility boundary; the collection and
  realtime reads were the outliers.
- **Filtering is cheaper and safer than the alternatives.** A per-row
  `authorize_object(...).is_ok()` is the identical predicate the handlers already
  use — no new policy, no second source of truth. On the realtime path the filter
  is a `match`/lookup on an event already pulled from the broadcast; it adds no
  await and cannot block, so invariant #10 holds by construction.
- **Default deployments are unaffected.** Object scoping is opt-in
  (`scoped_object_ids: Some(..)`); the common admin/operator/viewer principals
  are unscoped and see everything exactly as before. The change only narrows what
  an explicitly-scoped principal sees.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Leave collection/realtime unfiltered (scope = mutation only) | The enumeration + disclosure leak the panel found; contradicts the single-`GET` `403` and ADR-W005's "BOLA on every resource". |
| Filter the REST collections but not realtime | The realtime stream re-leaks the same objects (snapshot + every delta) the collections now hide — half a fix. |
| Add a server-side filtered subscription / per-scope broadcast channel | Would put policy on the engine publish path and risk a per-client channel the engine could stall on (invariant #10). A read-side projection on the existing shared subscription is strictly safer. |
| Return `403` on the whole collection/stream for any scoped principal | Over-restrictive: a scoped principal legitimately reads its own objects; it must see its in-scope rows/events, just not others'. |

## Consequences

- **Easier:** the BOLA posture is now uniform — single-object, collection, and
  realtime reads all honour the same allowlist; a future scoped role
  (config-declared API keys) gets correct visibility for free.
- **Committed to maintain:** any **new** collection read (REST or NMOS) or any
  new Devices-domain realtime event must apply the same per-row / per-event
  object filter, and any new device/cast **object** event must be added to
  [`device_object_scope_id`](../../crates/multiview-control/src/realtime.rs) (the
  narrow authz helper, not the broader `event_scope_id`) so the realtime filter
  can gate its id. A new collection that forgets the filter is a re-introduced
  leak — covered by the BOLA tests added with this ADR.
- **Invariant #10 (isolation):** preserved. The realtime filter is a pure
  per-client read-side projection on events already received from the bounded
  broadcast; it adds no blocking, no await, and never touches the engine publish
  path. A dropped (out-of-scope) event is simply not forwarded to that one
  client — the same shape as the existing lagged-skip / resume / conflated skips.
- **No API/schema change.** Response shapes are unchanged; a scoped principal
  simply receives a subset. Unscoped principals (the default) are byte-for-byte
  unaffected.
