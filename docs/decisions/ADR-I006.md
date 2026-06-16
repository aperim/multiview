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
     lease signer **only** when its signed pre-image declared `key_type == "lease"`
     **and** its `status ∈ {current, next}` (dual-pin). A root-attested key minted
     for another purpose (e.g. an `update` key) or in a `retired`/unknown status is
     never a lease signer (a skip, not a keyset-poisoning reject).
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
| Trust any `root_sig`-attested intermediate as a lease signer (ignore `key_type`/`status`) | A root may attest keys for several purposes; accepting a non-lease (e.g. `update`) key — or a `retired` key — as a lease signer is a key-purpose bypass that the signed `key_type` field exists to prevent. Enforce `key_type == "lease"` ∧ `status ∈ {current,next}`. |
| Default omitted/empty `instance_binding_id`/`serial` to `""` (`unwrap_or_default`) | A signed-but-malformed body would install with an empty id and mis-bind enforcement/renewals. Required identity fields fail closed. |

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
