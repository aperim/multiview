# ADR-I006: CONSPECT-3 device heartbeat client — key-trust verification & install convergence

- **Status:** Accepted
- **Area:** Implementation build-out
- **Date:** 2026-06-16
- **Source:** agent session implementing CONSPECT-3 against the finalized Conspect `/v0` API (v0.6.1) + the live well-known key document; refines the plan in ADR-0096.

## Context

The runtime licensing subsystem (`multiview-licence`, ADR-0050/0051/0052) was ~90%
shipped; the only gap was the device→server heartbeat client, gated by ADR-0096 on
the Conspect API publishing D1 (key-trust), D2 (PoP request-signing), and D3
(canonical-CBOR lease pre-image). The blocker is now resolved: the Conspect `/v0`
API is finalized at v0.6.1 and the live well-known document
(`GET https://api.conspect.studio/.well-known/conspect-licensing-keys.json`) is
published. ADR-0096 sketched the implementation but left three load-bearing
details to the implementing wave; the live data settles them. Binding constraints:
the `multiview-licence` crate is **physically incapable of touching output** (no
engine handle, no I/O in the default build — its CLAUDE.md invariant); the default
`cargo check`/`cargo deny` must stay a network-free, LGPL-clean shell; and the
never-off-air invariants (#1 output-clock, #10 isolation) bind any licensing code.

ADR-0096 specified an "object-safe `trait LicenceServer`" and assumed a CBOR codec
could produce the canonical pre-image; both needed refinement against the real
crypto and the house concurrency patterns.

## Decision

Land CONSPECT-3 as the off-by-default `heartbeat` feature on `multiview-licence`
(+ the cli `heartbeat` feature pulling the live `reqwest`/rustls transport),
appended to the `nvidia`/`apple`/`linux-vaapi` presets only (`full` inherits;
mirrors `mesh-mdns`). The implementation makes these concrete decisions:

1. **Key-trust against the live well-known doc** (`crates/multiview-licence/src/heartbeat.rs`,
   `keytrust`): `PinnedRoot` (ECDSA P-256, SEC1 uncompressed point via `p256`
   0.13) + `TrustedKeys::verify` — verify the advertised root byte-matches the
   pinned anchor (`RootMismatch` up front), verify the **root-attested revocation
   list** (fail closed: a forged/absent `root_revocation_sig` rejects the keyset),
   verify each intermediate's `root_sig` (ECDSA-P256/SHA-256 raw r‖s, base64url)
   over its canonical key pre-image, then trust the attested intermediates that are
   in-validity and not revoked, accepting both `current` and `next` (dual-pin).
2. **A hand-rolled RFC 8949 §4.2.1 canonical CBOR encoder** for the key &
   revocation pre-images (`canonical_key_preimage` / `canonical_revocation_preimage`),
   **not** a serde-CBOR codec. The pre-image is a fixed small map whose keys, in
   the well-known `key_pre_image` order `[key_id, key_type, statement,
   public_key(bstr raw-32), valid_from(uint), valid_until(uint)]`, already equal
   the canonical sort order — verified **byte-exact** against the live
   `intermediate-v1`/`intermediate-v2` `root_sig` and `root_revocation_sig` (a
   golden-vector test pins the exact bytes).
3. **Bare-Ed25519 signed-lease verification** (`verify_signed_lease_chain`):
   resolve `signerKeyId` to a trusted intermediate, verify the lease's bare Ed25519
   signature (lower-case hex, 64 bytes) over the **STANDARD-base64**-decoded
   (RFC 4648 §4, *not* base64url) `leaseBytes` — the device verifies the signature
   over the exact bytes received and never re-serializes the lease body — then
   `ciborium`-parse that body for the authoritative offline-enforcement inputs
   (`gpu_limit`, `hardware_class`, `not_after`).
4. **A generic `HeartbeatClient<S: LicenceServer>`** with a native
   `async fn`-in-trait + `Send` future seam (the house `ZowietekTransport`
   pattern), **not** an `async-trait`/`dyn`-object trait. The live HTTP transport
   (`ConspectHttpServer`) lives at the cli/app boundary (which owns `reqwest`);
   the leaf crate opens no socket. The loop drives the existing
   `LeaseStore::install_binding` convergence on a positively-verified lease and
   keeps last-good on every failure **and** on a withheld lease (`lease: null` —
   revocation by non-reissue).
5. **Environment-driven config** in the cli (`HeartbeatSettings::from_env`,
   `crates/multiview-cli/src/licence.rs`): the org id (config-driven, ADR-0096 O4),
   the pinned root, the API base, the account JWT, an optional claim code, and the
   salted device identity, all read from `MULTIVIEW_LICENCE_*` env vars — secret
   material never in the binary. `spawn_heartbeat()` (real under `cfg(heartbeat)`,
   no-op otherwise) is called beside `spawn_mesh_discovery()` at both `main.rs`
   run sites.
6. **The verified-body → entitlement mapping is fail-closed and purpose-bound**
   (hardened after the rule-21 Codex panel — the crypto chain is correct, but the
   guarantees must not be dropped at the mapping). Binding rules:
   - **Key-purpose binding:** a `root_sig`-attested intermediate is trusted as a
     lease signer **only** when its signed pre-image declared `key_type == "lease"`.
     A root-attested key minted for another purpose (e.g. an `update` key) is never
     a lease signer (a skip, not a keyset-poisoning reject). **The unsigned `status`
     field is NOT a trust gate** (see decision point #7): trust rests only on signed
     fields — `key_type`, the validity window, and the revocation list.
   - **Signed expiry is authoritative:** the installed lease's `expires_at` is the
     cryptographically-signed `not_after` (`Lease::new_online_expiring_at`
     back-derives `granted_at = not_after − 35d`), **never** `system_now() + 35d`.
     A signed lease already past its `not_after` is rejected
     (`HeartbeatError::LeaseExpired`, keep last-good) — so a short-lived or
     replayed-older still-Ed25519-valid lease can never mint a fresh term.
   - **Decode exactly:** `leaseBytes` is `STANDARD.decode(trim())` (no `=`
     stripping) — a canonically-padded body (CBOR length % 3 ≠ 0) decodes.
   - **Required fields fail closed:** `instance_binding_id` / `serial` /
     `licence_id` must be present **and non-empty** (`ok_or(MalformedBody)`), never
     `unwrap_or_default()`.
   - **Address the binding by id:** renewals use the server-issued
     `instanceBindingId` learned from the verified body
     (`HeartbeatClient::learned_binding_id`), **never** the lease serial.
   - **Reject nonsensical timestamps:** a negative `valid_from`/`valid_until` is
     rejected up front rather than coerced to unsigned 0 by the CBOR encoder.
   The never-off-air isolation is re-proven with a **genuine in-flight stall**
   (a black-holed transport call), asserting a concurrent store reader and the
   output clock are never blocked while a heartbeat call is parked (inv #10).
7. **Identity anchoring + signed-only trust** (hardened after the **round-2**
   rule-21 panel, which confirmed #1–#6 correct and found deeper holes). Binding
   rules:
   - **Cross-instance replay defence:** `install()` is anchored to this device's
     established binding (configured, or learned from a prior successful install).
     Once a binding is established, a returned body whose `instance_binding_id`
     differs is rejected (`HeartbeatError::BindingMismatch`) — a valid
     Conspect-signed lease minted for **another device** cannot be replayed here.
   - **Real fingerprint gate:** `seal_for_install` stamps the device's **actual**
     `identity.fingerprint_score`, never an unconditional `FINGERPRINT_MATCH_STRONG`.
     The store's fingerprint-continuity gate then genuinely rejects a non-matching
     machine (`HeartbeatError::FingerprintMismatch`, keep last-good).
   - **No reject-path identity poisoning:** the server-issued `instanceBindingId`
     is learned (`remember_binding_id`) **only after** a successful `install()`.
     A rejected (expired / foreign / stale) lease never mutates the learned
     binding, so the next renewal still addresses the legitimate binding.
   - **`status` is not a security gate (cryptographic binding):** the well-known
     `status` field is **not** in the root-signed key pre-image (`map(6)`:
     `key_id,key_type,statement,public_key,valid_from,valid_until`), so a MITM /
     compromised document can flip a retired key's `status` to `current` without
     breaking `root_sig`. Trust therefore rests on **signed** fields only:
     `key_type == "lease"` ∧ `now ∈ [valid_from, valid_until]` ∧ not in the signed
     revocation list. Retirement is expressed via the signed validity window or the
     signed revocation list — never the unsigned `status` (operational hint only).
   - **`gpu_limit` fails closed:** a **present-but-invalid** `gpu_limit` (negative,
     non-integer, or `> u32::MAX`) is `MalformedBody`; only an **absent** `gpu_limit`
     means `Unlimited`. A malformed-but-signed body can no longer install unlimited
     GPUs (the least-restrictive value).
   - **Deterministic expiry test:** the round-1 replay test passed via
     `InstallError::Stale` (a fixed-future fake epoch masked the path); a lease with
     an absolute-past `not_after` now deterministically exercises
     `HeartbeatError::LeaseExpired`.
8. **Install-genuineness + acceptance-time trust** (hardened after the **round-3**
   rule-21 panel, which confirmed #1–#7 correct and found two deep fail-open paths).
   Binding rules:
   - **"Ok from `install()`" ≠ "installed":** `install()` returns an
     `InstallOutcome::{Installed, StaleNoop}` rather than folding `Err(Stale) → Ok`.
     The binding id is learned (`remember_binding_id`) **only on `Installed`** —
     never on the stale no-op. Otherwise a crypto-valid-but-**stale FOREIGN** lease,
     which the store correctly keeps-last-good (`Stale`), would still poison the
     learned identity with the foreign binding without anything being installed.
   - **The store IS the device identity:** a genuine install records the binding in
     the store (in round-4 this moved into `LeaseStore::install_binding` as the
     single chokepoint — see decision point #9); the heartbeat anchor is
     `established_binding = configured ?? learned ?? store.current_binding_id()`. So
     a device that already holds a lease is **never "fresh"** — the cross-instance
     guard rejects a foreign binding even on the activation path (the round-2 guard
     was skipped when `established_binding == None`).
   - **Trust is re-evaluated at lease-ACCEPTANCE with a fresh clock (no TOCTOU):**
     the key-trust chain is verified again with a fresh `now()` immediately before
     `verify_signed_lease_chain` accepts the returned lease (`HeartbeatClient` gains
     a `NowMs` clock seam). A signer whose signed validity window elapses — or that
     is revoked — **during** an arbitrarily-stalled network call no longer validates
     the lease, because `verify_signed_lease_chain`/`lease_key` only check the
     Ed25519 signature, not time-validity/revocation. The pre-network verify is a
     fast-fail only; both reads go through the seam.
9. **Fresh-fetch acceptance trust + the single binding-anchor chokepoint +
   retry-stable idempotency** (hardened after the **round-4** rule-21 panel, which
   confirmed #1–#8 correct and found the round-3 acceptance re-check did not fully
   close two fail-open paths). Binding rules:
   - **Acceptance trust is re-evaluated against a FRESHLY RE-FETCHED key document,
     not the pre-network one (no revocation TOCTOU):** revocation is set-membership
     over the signed key document, so the round-3 fix — a fresh `now()` against the
     **same** fetched document — caught only an elapsed *validity window*, never a
     signer **added to the (signed) revocation list** during a stalled call.
     `run_once` now re-fetches `fetch_keys()` at acceptance and re-runs
     `TrustedKeys::verify` on that fresh document (which re-checks the root match,
     the root-attested revocation signature, every intermediate `root_sig`, the
     signed validity window at the fresh `now()`, **and** the revocation set). A
     newly-revoked or newly-expired signer is dropped from the re-fetched trusted
     set, so `verify_signed_lease_chain` cannot resolve `signerKeyId` and rejects
     the lease. The re-fetch fails closed on a transport error (keep last-good).
   - **`LeaseStore::install_binding` IS the binding-anchor chokepoint:** the
     round-3 anchor (`store.record_binding_id`) fired **only** from the heartbeat
     genuine-install path, so a lease installed via the control-route/offline
     upload, the file-drop watcher, or the mesh relay (all of which call
     `install_binding` directly) left `current_binding_id() == None` — the device
     looked "fresh" and a foreign-binding activate **skipped** the cross-instance
     guard (identity poisoning). The binding id now rides on the `LeaseBinding`
     (`instance_binding_id: Option<String>`, carried by every producer) and
     `install_binding` records it **atomically with the install**, so **every**
     surface anchors the device identity uniformly; the now-redundant
     heartbeat-path `record_binding_id` call is removed (no double-record).
   - **The anchor comes from SIGNED material (never an unsigned sidecar):** the
     panel's follow-up flagged that `LeaseBinding.instance_binding_id` was outside
     the crate's `SignedLease` envelope (which covered serial+source+dates only),
     so the offline/file-drop/relay surface could anchor an attacker-chosen binding
     id. `SignedLease::signing_bytes` now also covers `instance_binding_id` (a
     1-byte presence tag + length-prefixed bytes, so `None`, `Some("")`, and
     `Some("x")` are distinct signed values), `verify_signed_lease` recomputes
     over the binding id the caller will anchor, and `install_binding` passes
     `binding.instance_binding_id` into that check — so a grafted/tampered or
     absent-vs-present binding id fails `SignatureInvalid` and never anchors. Every
     producer signs over the id it carries (`seal_for_install`, the offline/relay
     minters). A binding-less (`None`) producer still installs and anchors nothing
     — forward-compatible with a server later signing a binding id.
   - **The Idempotency-Key is retry-stable per logical operation:** the round-3
     key was `format!("mv-{}", unix_millis_now())` — minted fresh per call inline,
     so a retry of the same logical operation issued a **new** key, defeating "a
     retry replays, never re-issues" (lost-response duplicate-mutation risk). The
     client now holds an `IdempotencyState { counter, current }`: a key is minted
     once per logical operation from a **monotonic per-client counter + the device
     machine id** (never the wall clock), **replayed** on every retry, and rotated
     **only after a fully-successful contact** (install / stale-no-op / withheld
     lease). An error *after* the mutation landed does not rotate, so its retry
     also replays — the server dedupes, never duplicating a binding/lease.

New deps behind `heartbeat`: `p256`, `base64`, `hex` (all `MIT`/`Apache-2.0`, with
the ECDSA closure resolving to `MIT`/`Apache-2.0`/`BSD-3-Clause`/`BSD-1-Clause`),
`tokio`; the cli adds `reqwest` (rustls). `cargo deny` runs `all-features = false`,
so none enter the scanned default graph.

## Rationale

- **Hand-rolled canonical CBOR** is the safe choice because `ciborium::into_writer`
  preserves map *insertion* order rather than sorting keys to RFC 8949 §4.2.1, so
  relying on it for a signature pre-image would be a latent security bug if a field
  were ever reordered. A fixed, total, ~30-line encoder is auditable and is proven
  byte-exact against the production `root_sig` — the strongest possible golden
  vector.
- **Generic over `dyn`** matches the established `ZowietekTransport` pattern (no
  `async-trait` dep, no per-call heap alloc, no `Send`-bound surprises), and the
  spawn point holds exactly one concrete server type, so generics lose nothing.
- **Verify-over-received-bytes, never re-serialize** sidesteps any dependence on
  reproducing Conspect's lease-body canonicalization: the server hands the exact
  signed bytes, and `install_binding`'s body fields are read order-independently.
- **Fail closed on trust, lenient on enforcement** keeps the never-off-air promise:
  a rejected chain or bad signature merely withholds the next lease (the previous
  ages naturally); only a positively-verified signed lease ever tightens anything.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Use `ciborium` (a serde-CBOR codec) to build the key pre-image | It preserves insertion order, not RFC 8949 §4.2.1 canonical key order; trusting that for a signature pre-image is a latent security defect. The pre-image is a tiny fixed map — a hand-rolled canonical encoder is auditable and provably correct against the live `root_sig`. |
| Object-safe `dyn LicenceServer` (boxed-future trait or `async-trait`) per ADR-0096's wording | The repo's house pattern is native `async fn` in trait + generic dispatch (`ZowietekTransport`/`ZowietekClient<T>`); it is alloc-free, `async-trait`-free, and the spawn point holds one concrete server, so a generic `HeartbeatClient<S>` is strictly better. `async fn` in trait is not `dyn`-compatible without boxing anyway. |
| Re-derive Conspect's canonical lease-body CBOR to re-verify the lease | Unnecessary and fragile: the server returns the exact signed `leaseBytes`; verifying the bare-Ed25519 signature over the received bytes (then parsing fields order-independently) is simpler and cannot drift from the server's encoding. |
| Put the `reqwest` HTTP transport inside `multiview-licence` | Breaks the leaf crate's "no I/O, no socket" invariant (its CLAUDE.md) and would pull a TLS stack into a pure-data crate. The crypto/verify lives in the testable crate; the socket lives at the cli boundary behind the `LicenceServer` seam. |
| Hard-code the free auto-issue default org id | The default `{orgId}` for a free self-host is an external-doc residual (ADR-0096 O4, not yet provided). A guessed value would be wrong; the org id is a clearly-named env-driven config field, off unless set. |
| Mint the installed lease term from `system_now() + 35d` (ignoring the signed `not_after`) | Defeats the licensing crypto: a short-lived or **replayed older** still-Ed25519-valid lease would become a fresh 35-day entitlement (signed-expiry bypass / lease replay). The installed `expires_at` MUST be the signed `not_after`, and an already-expired signed lease is rejected. |
| Trust any `root_sig`-attested intermediate as a lease signer (ignore `key_type`) | A root may attest keys for several purposes; accepting a non-lease (e.g. `update`) key as a lease signer is a key-purpose bypass that the signed `key_type` field exists to prevent. Enforce `key_type == "lease"` (signed) — and **only** signed fields for the rest (validity window, revocation), never the unsigned `status`. |
| Default omitted/empty `instance_binding_id`/`serial` to `""` (`unwrap_or_default`) | A signed-but-malformed body would install with an empty id and mis-bind enforcement/renewals. Required identity fields fail closed. |
| Trust a lease for any binding (no install-time identity check) + stamp `FINGERPRINT_MATCH_STRONG` | Cross-instance replay: a valid Conspect-signed lease minted for another device installs onto this one, and the hardcoded strong stamp bypasses the store's fingerprint gate. Anchor install to the established binding + stamp the real fingerprint score. |
| Use the well-known `status` field to gate lease-signer trust | `status` is NOT in the root-signed pre-image, so a MITM can flip a retired key to `current` without breaking `root_sig`. Only signed fields (`key_type`, validity window, revocation list) may gate trust; `status` is an operational hint. |
| Treat any `Ok` from `install()` (incl. the `Stale → Ok` fold) as "installed" and learn the binding | Reject/stale identity poisoning: a crypto-valid-but-**stale FOREIGN** lease that the store keeps-last-good (`Stale`) would still poison the learned binding with the foreign id, with nothing installed. `install()` returns `Installed` vs `StaleNoop`; learn only on `Installed`. |
| Anchor the cross-instance guard to the heartbeat-learned binding only (skip it when `established_binding == None`) | A device with a pre-existing local store lease but no learned/configured heartbeat binding takes the activate path and the guard is skipped — a foreign lease installs. Fold the store's current lease binding into the anchor (`store.current_binding_id()`), recorded on every genuine install. |
| Evaluate signer time-validity / revocation only once, with the pre-network timestamp | Key-trust TOCTOU: a signer whose signed `valid_until` elapses (or that is revoked) **during** an arbitrarily-stalled network call still validates the returned lease, because the frozen `trusted` set is reused and `verify_signed_lease_chain` checks only the Ed25519 signature. Re-evaluate trust with a fresh `now()` at lease-acceptance. |
| Re-check acceptance trust with a fresh clock against the **same** fetched key document (round-3) | Catches only an elapsed validity *window*; revocation is set-membership over the signed document, so a signer added to the revocation list **during** a stalled call is never observed against the stale doc. Re-FETCH the key document at acceptance and re-verify the fresh one (root + revocation-sig + intermediates + window + revocation set). |
| Record the binding anchor only from the heartbeat genuine-install path (`store.record_binding_id`) | A lease installed via the control-route/offline upload, the file-drop watcher, or the mesh relay leaves `current_binding_id() == None`, so the device looks "fresh" and a foreign-binding activate skips the cross-instance guard (identity poisoning). Carry the id on `LeaseBinding` and record it atomically inside the single `install_binding` chokepoint every surface converges on. |
| Anchor identity from the `LeaseBinding.instance_binding_id` field as an unsigned sidecar | The field sat outside the crate's `SignedLease` envelope (serial+source+dates only), so the offline/file-drop/relay surface could anchor an attacker-chosen binding id (identity poisoning / renewal DoS). Bind `instance_binding_id` into `signing_bytes` (presence tag + bytes) and verify it in `install_binding`, so the anchor always rests on signed material; a grafted id fails `SignatureInvalid`. |
| Mint the Idempotency-Key from `unix_millis_now()` per call | A retry of the same logical operation mints a NEW key, so a lost response can create a DUPLICATE binding/lease ("replay, never re-issue" violated). Use a per-operation key from a monotonic counter + the device id, replayed on retry and rotated only after a successful contact. |

## Consequences

- **Easier:** a configured device renews its lease against conspect.studio over the
  same `install_binding` convergence the offline file-drop and mesh relay use, so
  S1/S2/S3 + the control routes + the web screens re-sample with zero extra wiring;
  the read-only heartbeat-status route now has a real device→server producer.
- **Committed to maintain:** the canonical-CBOR encoder must stay byte-exact with
  the Conspect attestation contract (the golden-vector test guards this against the
  live well-known doc); the `MULTIVIEW_LICENCE_*` env surface is the device-config
  contract; the `p256`/`reqwest` closures must stay deny-clean.
- **Invariants:** the heartbeat task holds no engine handle/channel/lock (#10) and
  only ever tightens on a positively-verified signed lease, keeping last-good on
  every failure/withheld lease (#1) — re-proven by the extended never-off-air chaos
  gate (`crates/multiview-cli/tests/heartbeat_never_off_air.rs`) that SIGKILLs /
  stalls / partitions the heartbeat task while asserting one-frame-per-tick.
- **Residual (non-blocking):** the free-tier default org id (O4) and the device-PoP
  request-signing wire format (D2, deferred server-side, slice 5d) — the
  `devicePublicKey` is captured + stored but does not yet authenticate requests
  (account-JWT bearer today). Both are honestly tracked, not stubbed.
- **CI/licensing:** the default build is unchanged (network-free, LGPL-clean); the
  `heartbeat` feature is on only in the shipped deploy presets. `cargo deny`
  (`all-features = false`) is unaffected; the `webpki-roots` `CDLA-Permissive-2.0`
  edge under `--features heartbeat` is pre-existing for every `reqwest` feature
  (e.g. `devices-net`) and is never in CI's scanned default graph.
