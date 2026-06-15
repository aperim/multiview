# multiview-rist-sys — agent notes

Direct **librist** (VSF TR-06, **BSD-2-Clause**) FFI leaf for RIST **link
statistics** (ADR-0095 Tier-1 / RIST-5). The sole `unsafe` boundary on the RIST
stats path, so `multiview-output` (the consumer) stays `forbid(unsafe_code)`.

- **Why it exists:** FFmpeg's `rist://` protocol (Tier-0) registers **no**
  `rist_stats` callback and exposes **no** stats AVOption (`ffmpeg -h
  protocol=rist`), and it owns its librist context privately — so a RIST link's
  health is invisible through it. Stats need a librist context **we** own.
- **Loading:** runtime `dlopen` (`librist.so.4` / `librist.so`), the ADR-0028
  own-the-binding pattern (same model as `multiview-ndi-sys`). librist is
  **NEVER linked at build time and never vendored**. The default build — and even
  a `session`-feature build — compiles/links with no librist present; only a run
  that opens a session needs the runtime `.so`.
- **`unsafe`:** crate-level `unsafe_code = "deny"` (not `forbid`); every `unsafe`
  block carries a `// SAFETY:` note. The C-ABI stats structs in `raw.rs` are
  hand-mirrored from librist 0.2.x `stats.h` (size-checked in tests) — keep them
  byte-exact if you touch them.
- **Stats callback (inv #10):** runs on librist's own thread; it only ever
  `try_send`s a decoded sample on a bounded **drop-oldest** channel — never
  blocks, never allocates beyond the sample, never unwinds across the FFI
  boundary. The engine is never back-pressured.
- **Honest boundary:** only the **sender** session is built (leaf-sized: an
  egress sink consuming already-encoded packets, replacing no shared transport).
  The direct-librist **receiver** with stats owns the receive+demux loop (a new
  `Source`) — a larger Tier-2-shaped change, NOT built here; `decode_stats`
  already handles the receiver-flow shape for that follow-up. **Never fabricate a
  stat** — surface only what a librist context we own produced.
- **License:** BSD-2-Clause (permissive/LGPL-clean, already on the `deny.toml`
  allow-list). The default build stays LGPL-clean.

Read first: [ADR-0095](../../docs/decisions/ADR-0095.md) ·
[rist-transport](../../docs/research/rist-transport.md) ·
[conventions §4/§7](../../docs/architecture/conventions.md) · safety rules §4
(CLAUDE.md). Template: `crates/multiview-ndi-sys/`.
