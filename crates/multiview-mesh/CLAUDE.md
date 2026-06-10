# multiview-mesh — agent notes

The Conspect **local-mesh discovery + relay** plane (ADR-0051 / the Conspect brief §9): always-on
mDNS announce/browse of **salted, signed** summaries, an **untrusted** discovered-peer inventory,
mesh **role** determination (direct/relay/leaf), and the end-to-end-signed relay **carrier**.
Greenfield leaf crate.

**The load-bearing invariant — isolation (inv #10).** This crate is a **control-plane actor**, the
proven managed-devices/NMOS isolation shape. It has **no** `multiview-engine` dependency, no engine
handle, spawns nothing on the data plane. The relay queue + peer table are **bounded drop-oldest**;
the announce/browse loop is best-effort and **never blocks on the network**. A wedged/failed mesh
loop can never stall the engine or even the local heartbeat. **Do not** add an engine dependency or
any path that could back-pressure or stall output.

**Data minimisation (brief §8) — PINNED BY TEST.** The announce payload carries **only**
`{protocol_version, digests, claim_state, entitlement, signature}`; the entitlement summary carries
**only** `{level, granted_at, expires_at}`. **Never** a serial / MAC / URL / hostname / media /
config / raw identifier — the types have **no field** that could hold one. `tests/announce_payload.rs`
enumerates the wire keys exhaustively; **never weaken that test**. Salted digests are **handed in**
already salted+hashed (this crate never gathers raw identifiers — same contract as
`multiview-licence::fingerprint`). Ed25519 is **verification-only** in non-test code (the announcer
signs its own summary; the keygen RNG lives in dev-deps only).

**Always-on discovery has NO off switch.** `DiscoveryMode` has exactly one value (`AlwaysOn`); there
is no field, method, or endpoint that disables discovery (the spec's locked row). Only **relay**
opt-in toggles (`MeshState::set_relay_enabled`, `PUT /api/v1/mesh/relay`). Do not add a discovery
off-switch.

**Untrusted inventory + confirm-adopt (ADR-0041 doctrine).** A discovered peer is **never**
auto-trusted or auto-relayed: `relaying_for_us` is set only by an explicit operator action
(`MeshState::adopt_relay`, which requires the peer to already be in the inventory). The relayer is a
**dumb carrier** — it lacks the originator/server keys, so a tampered/forged relayed `LeaseBinding`
fails verification at the destination (against the **pinned server key**, never the relayer's).

**IPv6-first (ADR-0042, hard rule).** mDNS multicast is `ff02::fb` (IPv6 primary), IPv4
`224.0.0.251` legacy interop only. The `mdns-sd` daemon joins both per interface. Never design an
IPv4-only/IPv4-first path.

**Features:** `mdns` (off by default) pulls the deny-clean `mdns-sd` crate (MIT OR Apache-2.0;
closure all `MIT`/`Apache-2.0`/`BSD` — proven `cargo deny --features multiview-mesh/mdns check`) +
`tokio` for the announce loop. The pure logic (announce payload, peer table, role, relay carrier,
the `announce_browse_step`) is **always compiled + tested without a socket** via the `MeshTransport`
trait; the live-network test (`tests/mdns_live.rs`) is `#[ignore]`d + hardware-gated.

**Dependencies:** `core`, `licence` (the signed binding/summary types it announces + relays),
`events`, `serde`, `serde_json`, `thiserror`, `tracing`, `chrono` (no `clock` — instants handed in),
`ed25519-dalek` (verify-only). No GPU, no FFmpeg, **no engine**.

**Conventions:** `#![forbid(unsafe_code)]` + `#![warn(missing_docs)]`; serde unions tagged (never
`untagged` — `MeshRole`/`Connectivity`/`ClaimState`); wire resources `#[non_exhaustive]` (use
constructors); no `unwrap`/`expect`/`panic`/`as`/indexing in non-test code (`?`/`match`/`TryFrom`).

Depth: [conspect-account-architecture](../../docs/research/conspect-account-architecture.md) (§8 data
minimisation, §9 mesh, §11 endpoints) · [ADR-0051](../../docs/decisions/ADR-0051.md) ·
[ADR-0042](../../docs/decisions/ADR-0042.md) (IPv6-first) · [ADR-0050](../../docs/decisions/ADR-0050.md)
(the entitlement plane mesh carries) · [conventions](../../docs/architecture/conventions.md).
