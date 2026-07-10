# ADR-W026: Discovery-scope authorization axis (unified event scope model)

- **Status:** Accepted
- **Area:** Web/API
- **Date:** 2026-07-10
- **Source:** PR #235 review HIGH finding + operator decision + discovery-scope-axis-design workflow

## Context

The realtime WS/SSE stream delivers two non-object-scoped events to every authenticated reader, including object-scoped principals: `device.discovered` (untrusted mDNS inventory carrying management endpoints — the topology leak, ADR-0041 posture) and `timing.status` (program epoch/clock posture, `stream_id = ProgramId::MAIN`). The root mechanism is a fail-open classifier: `object_authz_scope_id` (realtime.rs:1258) must carry a `_ => None → deliver` wildcard because `Event` is `#[non_exhaustive]` (event.rs:1393) and `multiview-control` is a foreign crate — every future non-object event silently rides the firehose (e.g. `rist.link.stats` already does). Additionally, no discovery row carries a domain/site id today, and **no production path can mint a scoped principal at all**: the only key provisioning is the unscoped bootstrap admin (`provision_admin_keys` auth.rs:424, fed by `MULTIVIEW_CONTROL_TOKEN` at cli/control.rs:141–144). `ApiKeyStore` digests are HMAC under a per-process, never-persisted pepper (auth.rs:230, :419) — key persistence in SQLite is therefore not a drop-in. Four designs were adversarially judged on correctness/#10, compat/blast-radius, and security/completeness; the convergent kill-shots were (a) the wildcard classifier footgun survives per-crate predicate designs, (b) raw `"main"` in `scoped_output_ids` puns two id namespaces, and (c) an unmintable axis is a lock with no keyhole (rule 6 violation).

## Decision

Adopt the **unified scope model** (proposal 1 spine): one total, compile-enforced classification `Event::authz_scope() -> AuthzScope<'_>` owned by `multiview-events` with an exhaustive, wildcard-free match; one shared fail-closed allowlist predicate `scope_permits` owned by `auth.rs`; consumed identically by the realtime delta filter, connect snapshot, and REST. Graft onto it:

1. **Namespaced program grants** — `timing.status` classifies as `AuthzScope::Program(stream_id)`, matched only against `program:<id>`-prefixed entries in `scoped_output_ids`; plain output authorization ignores `program:*` entries and config validation reserves the prefix (kills the `"main"` punning).
2. **Config-declared API keys** (`[[api.keys]]`, secret via `secret_env`) registered at startup through the existing `ApiKeyStore::register` and re-appliable via config-as-code + `set_principal` (RT010 generation bump) — the axis is mintable in the same push without reworking the pepper/digest scheme.
3. **Mint invariant** — key-affecting config applies require a fully-unscoped admin principal; all key create/re-scope/revoke operations emit audit events.
4. **Mesh redaction** — discovery-scoped principals get node-level-gated `/mesh/peers` and a redacted `/mesh/status` (no `via`, `peers_count`, peer-derived `role`).
5. **Fail-closed everywhere**: a scoped principal never sees an unlabelled (`domain: None`) row, on any surface.

Domain provenance: **operator-declared node config** (`[discovery] domain`), stamped by the observing node — never from responder payloads, TXT records, or mesh peer identity.

## The scope model (Principal + events + filter)

**`multiview-events` — the classification (new, replaces per-crate wildcard maps):**

```rust
/// Never serialized; deliberately NOT #[non_exhaustive] so adding an axis
/// breaks every consumer match until handled (the second ratchet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzScope<'a> {
    Public,                            // explicit, greppable, review-gated firehose declaration
    Object(&'a str),                   // vs scoped_object_ids
    Output(&'a str),                   // vs scoped_output_ids (plain entries only)
    Program(&'a str),                  // vs scoped_output_ids "program:<id>" entries
    DiscoveryDomain(Option<&'a str>),  // vs scoped_discovery_domains; None label = fail-closed
}
```

`Event::authz_scope()` is an EXHAUSTIVE, wildcard-free match — every variant classified, each `Public` arm carrying a one-line justification comment. Adding an `Event` variant now fails compilation at the point of definition until its author classifies it — the firehose generator is removed, not patched. An in-crate golden-table test constructs a sample of every variant and asserts its classification (the exhaustive match forces the sample list to grow too).

**`multiview-control/src/auth.rs` — Principal + the one predicate:**

`Principal` gains `scoped_discovery_domains: Option<Vec<String>>` (None = all domains INCLUDING unlabelled rows — the compat default every pre-existing key gets; `Some([])` = sees no discovery inventory; `Some(list)` = only rows labelled with a listed domain, unlabelled rows DENIED — fail-closed). Plus `is_discovery_scoped()`, and `AuthzScopes { objects, outputs, discovery_domains }` cloned out via `Principal::scopes()`. The single fail-closed rule, encoded once:

```rust
fn allowlist_permits(scope: &Option<Vec<String>>, label: Option<&str>) -> bool {
    match (scope, label) {
        (None, _) => true,
        (Some(list), Some(l)) => list.iter().any(|x| x == l),
        (Some(_), None) => false,   // scoped principal + unlabelled row = deny
    }
}
```

`scope_permits(&AuthzScopes, AuthzScope)` matches exhaustively: `Public → true`; `Object(id)` vs objects; `Output(id)` vs outputs filtering out `program:*`; `Program(id)` vs outputs matching only `program:<id>`; `DiscoveryDomain(d)` vs discovery_domains via `allowlist_permits`. `authorize_scope` is the 403 twin. `authorize_object`/`authorize_output` (auth.rs:160/188) become thin wrappers over `authorize_scope` so REST and realtime cannot fork semantics. No `Default` on `Principal` — the compiler enumerates every construction site (~26 literals, mechanical `None`).

**Realtime filter (invariant #10):** `SessionStream.object_scope` becomes `scopes: AuthzScopes` + `with_scopes(...)`; the single gate at the existing drop point in `frame_for` becomes `if !scope_permits(&self.scopes, seq_event.event.authz_scope()) { return None; }`, still **before `issue_seq`** (no per-connection seq gaps). `object_authz_scope_id` is deleted. Pure borrowed match + small-Vec linear scans on a session-owned struct over an event already pulled from the bounded drop-oldest broadcast — no lock, no await, no allocation; engine publish path untouched. Both WS and SSE funnel through the one `SessionStream` core.

**RT010 live re-scope (mandatory):** `install_live_reauth` must carry all three axes into the session; `reauthorize` compares the full `AuthzScopes` (derive `PartialEq`); any-axis change → `ScopeChanged` → `$resync`. The `$resync` resubscribe set is `[Tiles, Devices]` — both affected events ride `Topic::Devices`, already covered; `timing.status` has no snapshot frame and heals at the next ~1 Hz publish.

## Data provenance (where the domain id comes from)

A discovered row is **untrusted responder data** — the device can never assert its own domain. The only trustworthy fact at discovery time is which observer saw it. Therefore:

- **Source of truth:** new `DiscoveryConfig.domain: Option<String>`, TOML `[discovery] domain = "site-a"`, validated non-empty, ≤64 chars, `[a-z0-9-]` (DNS-label-like). One node = one domain; per-NIC/per-VLAN split is explicitly out of scope.
- **Stamping chain (single origin):** `AppState.discovery_config.domain` → the scan handler passes `state.discovery_config.domain.clone()` into `run_scan` → stamps BOTH `DeviceBroadcaster::discovered` (gains a `domain: Option<String>` param) AND the `DiscoveredService` inventory row before `inventory.upsert` — one value, one origin, event and REST can never disagree. `DiscoveredService::from_raw` gains the `domain` param and explicitly never reads it from `raw`/TXT.
- **Rejected sources:** mesh peer identity (deliberately uncorrelatable salted digest — wrong trust direction for an authz label); network-segment derivation (multi-homed hosts, spoofable LAN input, not operator-legible).
- **Forward rule (DEV-B6 / ADR-0045 remote-node enrollment):** the controller stamps the domain from **its own registry record of the enrolled node** — never from the reporting node's wire payload. Pinned by a doc comment on the field and a test asserting `broadcaster.discovered`'s domain argument originates from local config, never `RawDiscoveredService`.

## timing.status handling

`TimingStatus` classifies as `AuthzScope::Program(&t.stream_id)` (`stream_id == ProgramId::MAIN == "main"`; pinned by a test referencing the `ProgramId::MAIN` constant, not a string literal). Delivery rule: output-unscoped principals see it; output-scoped principals see it iff `scoped_output_ids` contains **`program:main`**. This resolves the namespace punning every judge flagged:

- `program:*` entries are inert for plain output authorization (`authorize_output`/`Output` arm filters them out), so a timing grant can never silently confer REST authority over an output that happens to be named `main`.
- `multiview-config` output-id validation **rejects ids containing `:`**, reserving the prefix; the release-note remediation is unambiguous: *add `"program:main"`*.
- Fail-closed: an output-scoped key that has not been granted `program:main` stops receiving `timing.status` — least-privilege, and behaviorally inert today because no production scoped keys exist yet.

Payload redaction of `TimingStatus.groups` for scoped grantees is deliberately deferred (see Residual risks): the grant is an explicit operator opt-in to timing telemetry, not an automatic intersection.

## REST + connect-snapshot parity

- **Connect/`$resync` snapshot: no filtering work.** The snapshot set is exactly `$hello` + tiles + per-device `device.status`. Neither `device.discovered` nor `timing.status` has a snapshot frame. Add a pinning test only. Snapshot helpers reroute through `allowlist_permits`/`scope_permits` so the rule cannot fork.
- **`GET /api/v1/discovery/devices`**: after the role check, filter `snapshot()` rows by `allowlist_permits(&principal.scoped_discovery_domains, row.domain.as_deref())` — unlabelled rows hidden from scoped principals; list-filtering, not 403 (matches the #211 convention, no existence oracle per ADR-W005).
- **`POST /api/v1/discovery/devices/scan`**: `authorize_scope(principal, DiscoveryDomain(node_domain))` — a principal that cannot see the results may not spend the single-flight scan budget or correlate scan activity via the 202 window.
- **`GET /api/v1/mesh/peers`**: every peer is this node's same-segment observation — node-level gate: scoped principal sees the list iff the node's own domain is set and in its allowlist, else `200 []`. `Peer` (`deny_unknown_fields`) is not touched.
- **`GET /api/v1/mesh/status`**: for discovery-scoped principals outside the node's domain, redact peer-derived fields — `via: None`, `peers_count: 0`, `role` pinned neutral — keeping only node-local `discovery`/`relay_enabled`.
- **Timing REST:** verified none exists — WS-only enforcement is complete, no oracle divergence.

## Backward-compat & wire/serde/schema

- **Principals:** never serialized, in-memory only — no data migration. New field defaults `None` = see-all at every construction site; every existing key behaves identically until an operator opts in. Two deliberate, release-noted tightenings, both currently inert in production (no scoped keys can exist yet): output-scoped keys lose `timing.status` until granted `program:main`; discovery-scoped keys see only labelled rows.
- **`DeviceDiscovered`**: gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub domain: Option<String>` — exactly the existing `name` pattern; old readers ignore it and old payloads parse as `None` (wire byte-identical while unset). While touching it, add `#[non_exhaustive]` + `DeviceDiscovered::new(driver, address, family)` + `with_name`/`with_domain` builders.
- **`DiscoveredService`**: same `domain` field/attrs — OpenAPI flows from the derive.
- **`DiscoveryConfig.domain`** + `[[api.keys]]`: `#[serde(default, ...)]` — old TOML/JSON configs parse clean.
- **Spec regen in the same PR** (CI spec-freshness gate): `device_discovered_schema()` gains optional `domain` (not in `required`); `cargo xtask gen-asyncapi && cargo xtask gen-openapi`, commit `docs/api/{openapi,asyncapi}.json`; regenerate `web/src/api/schema.ts`.
- **Tests that ratify firehose delivery** are rewritten as tightened-behavior assertions in their own justified commit (rule 19).

## API + Web UI surface

Verified gap: **no key CRUD exists for any axis** — shipping enforcement without settability is the banned partial-ship. Resolution (avoids SQLite key persistence, incompatible with the per-process HMAC pepper):

1. **Config-declared API keys** — new `[[api.keys]]` in `multiview-config` (`key_id`, `secret_env`, `role`, optional `scoped_object_ids`/`scoped_output_ids`/`scoped_discovery_domains`). Registered at startup beside `provision_admin_keys` (bootstrap admin unchanged, always unscoped); config-as-code import/apply re-registers via `set_principal`/`revoke` — the RT010 generation bump propagates re-scopes to live sessions within one delta/≤5 s. Validation: unique `key_id`s, domain charset, `program:`-prefix reservation, `secret_env` presence checked at apply with a clear error.
2. **Mint invariant:** any config apply that touches `[[api.keys]]` requires a **fully unscoped** admin principal; every key create/re-scope/revoke emits an audit event carrying the acting `key_id`. A scoped admin can never mint or widen its way out of the scope model.
3. **Read surfaces:** `GET /api/v1/auth/keys` (admin-only, metadata + all three scope axes, never secrets/digests); whoami on `GET /api/v1/account` gains `role` + the three effective scope axes so a confined key can see why its view is empty.
4. **Web UI:** Settings → API Keys panel (table: key_id, role, three scope chips; secrets shown as env-var names only); Settings → Discovery gains a named **"Discovery domain"** field with an empty-state warning; discovery inventory view gains a Domain column. Client regenerated from OpenAPI.

`[discovery].domain` is reachable via the existing versioned config resource + the named UI control — management-completeness satisfied for every new knob.

## Consequences

- The firehose bug **class** is dead: an unclassified `Event` variant is a compile error in `multiview-events`; an unhandled `AuthzScope` axis is a compile error in `auth.rs`. `Public` is an explicit reviewed token, never a fallthrough.
- Invariant #10 holds structurally: the filter remains a per-session, wait-free read predicate at the existing pre-`issue_seq` drop point; zero engine/data-plane crates in the diff.
- Fail-closed call, resolved: **scoped principal + unlabelled row = deny**, encoded once in `allowlist_permits`. Rationale: (a) fail-open makes the axis a security no-op — `domain` is unset on every existing deployment, recreating exactly the leak being fixed; (b) omission is attacker-adjacent; (c) uniform with the pinned `Some([]) = deny-all` semantics; (d) cost is availability-only, self-inflicted per key, one-line config remedy, and unscoped principals are unaffected. Deliberate, type-encoded asymmetry: `TileState{input: None}` is `Public` (structurally id-less placeholder), `DeviceDiscovered{domain: None}` is `DiscoveryDomain(None)` (policy-unlabelled, denied to scoped keys).
- Blast radius: `multiview-events` (S), `multiview-config` (M), `multiview-control` (L), `multiview-cli` (S), `web/` (M), generated specs + docs. ~18–22 files. High-risk class (new authz dimension) → rule-21 **3-reviewer panel**.
- Release notes: `program:main` grant recipe; domain-labelling recipe; scoped keys now settable via `[[api.keys]]`.

## Alternatives rejected

- **Per-crate wildcard predicates:** triples the `_ =>` fail-open pattern instead of killing it; `#[non_exhaustive] Event` makes control-crate matches permanently un-exhaustive.
- **Raw `"main"` in `scoped_output_ids`:** puns program and output id namespaces in one flat allowlist — silent privilege coupling if an output is ever named `main`, silent grant breakage under multi-program. Replaced by the `program:` namespace.
- **Program→outputs derivation map** for timing: per-event or cached config lookup that is stale under hot-reconfig — fail-closed availability bug and fail-open multi-program leak.
- **A third standalone timing axis:** fragments grants for one resource family; the program-namespaced output axis expresses it with zero new Principal surface.
- **SQLite-persisted key CRUD:** persisted digests are unverifiable under the per-process HMAC pepper — requires a credential-hashing rework that must not ride an event-filter PR.
- **Deferring key settability:** an unmintable axis is dead code and a rule-6 partial-ship.
- **`Peer.domain` field / per-row mesh filtering:** `deny_unknown_fields` version-skew rejection + data-minimisation doctrine; node-level gating suffices.
- **Fail-open on unlabelled rows:** re-opens the leak for the default state of every existing deployment.
- **mDNS/TXT self-asserted or mesh-identity-derived domains; network-segment inference:** untrusted, uncorrelatable-by-design, or spoofable — wrong trust direction for an authz label.

## Top residual risks

1. **Config-attested domain has no network binding (weakest-node trust).** A mislabelled node leaks its segment's endpoints into the wrong domain; an unlabelled node blanks scoped inventory (fail-closed surprise); a multi-VLAN node cannot split domains per interface; under future mesh-forwarded discovery, correctness is only as good as the controller stamping from its registry (never the wire) — pinned by test + doc comment, but the DEV-B6 implementer must honor it.
2. **`TimingStatus.groups` is delivered whole to `program:main` grantees** — cross-group sync topology (ids, membership, skew) beyond the grantee's own outputs. Accepted for v1 because the grant is an explicit operator opt-in; the identified follow-up is per-session `groups` redaction when multi-tenant timing consumers materialize.
3. **Classification authority is now cross-crate.** A future REST handler can gate a resource while the events-crate table says `Public` (or vice versa) with no compiler help across the boundary. Mitigated by the control-crate parity test pairing each `Object/Output/Program` classification with its REST authz surface + mandatory justification comments on `Public` arms — but it remains review-dependent.
4. **Tightening masquerades as breakage.** Post-upgrade, newly-scoped keys on unlabelled nodes see empty discovery ("scan broken") and output-scoped keys lose the epoch their outputs are stamped from ("sync broken"). Mitigated by whoami scope observability, the UI empty-state warning, and the two one-line remedies in the release notes — residual: external automation keying on the firehose breaks on opt-in.
