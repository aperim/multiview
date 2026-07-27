# multiview-ffmpeg — agent notes (ALL RAW libav* FFI LIVES HERE)

Safe RAII wrappers over libav\* — demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe
transfer/map. Everything is behind the off-by-default `ffmpeg` feature. This crate (and the
`*-sys` leaves) override `unsafe_code` from workspace `forbid` to `deny`; the workspace lint set
is otherwise restated verbatim in this crate's `Cargo.toml` — **keep the two in sync by hand,
nothing checks it.**

The full contract is [data-plane-safety §4](../../docs/architecture/data-plane-safety.md). The
three things that actually bite here:

- **Every `unsafe` block carries a `// SAFETY:` comment** stating the invariant upheld.
  `clippy::undocumented_unsafe_blocks` is **not** enabled — this is convention + review only.
- **Context wrappers are `Send + !Sync`** (or Mutex-guarded). Distinct contexts on distinct
  threads are fine; per-context access must be synchronized. Frame-threading callbacks can fire
  from several threads — expose a frame only after the decoder yields ownership. Static assertions
  live in `tests/decode_first_frame.rs`.
- **`extern "C"` callbacks run on threads Rust does not own.** `get_format`, the log bridge and the
  custom IO callbacks are entered by libav on a foreign/decoder thread. Keep them
  **allocation-light**, and **never let a Rust panic unwind across the FFI boundary** — unwinding
  into C is undefined behaviour. `catch_unwind` at the boundary and return the sentinel
  (`AV_PIX_FMT_NONE` for `get_format`). A pooled buffer's releasing `Drop` may only `try_send`;
  it must never block, spawn, or join a decode thread, and must never run in a Tokio async
  destructor.

**Licensing is load-bearing here** (see [data-plane-safety §8](../../docs/architecture/data-plane-safety.md)):
the default build stays LGPL-clean — `scale_cuda`, never `scale_npp`; `gpl-codecs` makes the whole
build GPL and is opt-in only.

Read first: [core-engine §7, §8.1, §12](../../docs/research/core-engine.md) ·
[ffmpeg-strategy](../../docs/research/ffmpeg-strategy.md) · ADR-0002 / 0004 / 0031 / 0009.
Map: [codebase-map](../../docs/development/codebase-map.md).
