# multiview-licence — agent notes

The machine-side **entitlement plane** (Conspect): the signed entitlement/lease resource, the
Ed25519 lease-**verification** path, the enforcement ladder as **pure data**, machine-identity
**fingerprint scoring**, and the published `LicenceStatus` hand-off shape. Greenfield leaf crate
(ADR-0050 / the Conspect brief).

**The load-bearing invariant — never off air (inv #1).** This crate **computes data and verifies
signatures, and nothing else.** It has **no** `multiview-engine` dependency, no engine handle, no
process control, spawns no task, does no I/O. The hardest rung the ladder can compute merely *asks*
the engine (via two booleans the cli derives and the engine reads off the hot loop) to lock
reconfiguration or stamp a corner watermark. Every computed state answers `program_stays_on_air()`
== `true` by construction. **Do not** add an engine dependency, a clock read, a network call, or any
code that could stop/stall output — that breaks the product promise and the crate's reason to exist.

**Data minimisation (brief §8).** Fingerprint scoring is over **salted digests handed in** — never
gather raw serials/MACs here. Verification is **verification-only**: no key generation, no RNG in
non-test code (the RNG lives in dev-deps for test keypairs only). Keep it that way.

**Constants are EXACT (ADR-0050 §4 / brief §2).** The lease terms, activation window and fingerprint
thresholds live in [`constants.rs`](src/constants.rs) — read the values there; never restate them
here or in another doc where they can drift. A portal showing "35 days" and a machine enforcing 30 is
a support incident — do not round or re-derive. Property tests pin the day boundaries; never weaken
them.

**Dependencies:** `core`, `events`, `serde`, `thiserror`, `tracing`, `chrono` (exact arithmetic —
**never float**), `ed25519-dalek` (verify-only, deny-clean). No GPU, no FFmpeg, no engine.

**Conventions:** `#![forbid(unsafe_code)]` + `#![warn(missing_docs)]`; serde unions tagged (never
`untagged`); wire resources `#[non_exhaustive]` (use constructors); no `unwrap`/`expect`/`panic`/`as`
/indexing in non-test code (`?`/`match`/`TryFrom`).

**The heartbeat network client (CONSPECT-3, ADR-0096) lives here** behind the off-by-default
`heartbeat` feature: [`heartbeat.rs`](src/heartbeat.rs) — the Conspect key-trust verifier
(pinned ECDSA-P256 root → root-attested dual-pin Ed25519 intermediates + revocation, a hand-rolled
RFC 8949 §4.2.1 canonical-CBOR pre-image), the bare-Ed25519 signed-lease verifier, and the
`HeartbeatClient<S: LicenceServer>` loop that drives `store::install_binding` on a positively-verified
lease and **keeps last-good on every failure/withheld lease** (never off air). The default build stays
network-free + `cargo deny`-clean; the **live HTTP transport is the cli's** `ConspectHttpServer` (it
owns `reqwest`), so this leaf crate opens no socket. The consumers sit **outside** the charter and
stay there: the S1/S2/S3 engine seams + cli wiring in
[`multiview-cli`](../multiview-cli/src/licence.rs) (with the never-off-air chaos gates in its
`tests/`), the routes in `multiview-control`, the operator screen in `web/`.

Depth: [conspect-account-architecture](../../docs/research/conspect-account-architecture.md) (§2
constants, §6 ladder, §8 fingerprint, §12 state machines) ·
[ADR-0050](../../docs/decisions/ADR-0050.md) · [conventions](../../docs/architecture/conventions.md).

**Before touching the heartbeat client (CONSPECT-3):** read [ADR-0096](../../docs/decisions/ADR-0096.md)
(the gate + resolved wire) and [ADR-I006](../../docs/decisions/ADR-I006.md) (the implementation
decisions) — the device licensing wire was finalized by Conspect API v0.6.1 (key-trust via the public
`/.well-known/conspect-licensing-keys.json` ECDSA-P256-root → root-attested dual-pin Ed25519
intermediates; lease = bare Ed25519 hex over standard-base64 `leaseBytes` = RFC 8949 §4.2.1
deterministic CBOR; auth = account JWT Bearer today, device PoP deferred). The canonical-CBOR
key pre-image is proven byte-exact against the live well-known doc — keep it that way.
