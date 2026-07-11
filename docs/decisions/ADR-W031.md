# ADR-W031: Control-plane header-read timeout via a hand-rolled hyper serve loop (SEC-14 slowloris floor)

- **Status:** Accepted
- **Area:** Web/API stack · control-plane security (invariant #10 isolation)
- **Date:** 2026-07-11
- **Source:** SEC-14 follow-up (task #126); the F-A finding on
  [ADR-W028](ADR-W028.md) (request-concurrency + rate caps); builds on the TLS-0
  serve path ([ADR-W029](ADR-W029.md)).

## Context

[ADR-W028](ADR-W028.md) added the management-plane DoS floor: a concurrent-request
cap (`503`) and per-IP / per-API-key rate caps (`429`). Those guards are axum
middleware — they engage **only after hyper has parsed a request's full header
block**. A **slowloris** client that opens a TCP connection and then dribbles (or
never finishes) its request headers therefore holds a hyper task + socket + read
buffer open **indefinitely without ever taking a concurrency permit or a rate
token**. W028 documented this (finding F-A) as out of scope for that change and
tracked the fix as #126.

The fix is an **HTTP header-read timeout**: bound the time the server will wait to
read a request's header block, dropping the connection when it elapses. The
obstacle is that neither serve entry point exposes one:

- **Plain HTTP** used `axum::serve` (axum 0.8.9), which constructs the
  `hyper_util::server::conn::auto::Builder` internally and exposes only
  `with_graceful_shutdown` / `local_addr` — no `header_read_timeout` hook.
- **TLS** ([ADR-W029](ADR-W029.md)) uses `axum-server` 0.8.

`hyper_util`'s `auto::Builder` *does* have `http1().header_read_timeout(..)`, but it
**panics when armed without a `Timer`**.

## Decision

Own the accept loop so the header-read timeout can be configured on the hyper
connection builder, on **both** serve paths.

1. **Plain HTTP — hand-rolled accept loop.** `serve_router_with` replaces
   `axum::serve` with a loop over `listener.accept()` driving
   `hyper_util::server::conn::auto::Builder`:
   `http1().timer(TokioTimer::new()).header_read_timeout(configured)` (the timer is
   installed unconditionally, since arming the timeout without it panics),
   `serve_connection_with_upgrades` (so `/api/v1/ws` still upgrades over h1 and h2
   extended-CONNECT), and a `GracefulShutdown` watcher that drains in-flight
   connections on the shutdown future, bounded by a 10 s ceiling so a wedged client
   cannot hang a restart forever (safety §3). Each connection's peer `SocketAddr` is
   re-injected as a `ConnectInfo` request extension — axum's
   `into_make_service_with_connect_info` cannot be reused because its `IncomingStream`
   constructor is private — so the SEC-14 per-IP guard keeps its peer-IP key.
2. **TLS — configure axum-server's builder.** `axum-server` 0.8 exposes
   `Server::http_builder() -> &mut hyper_util auto::Builder`, i.e. the *same builder
   type* the plaintext loop drives. `serve_router_tls_with` configures
   `http_builder().http1().timer(..).header_read_timeout(..)` before `.serve(..)`.
   **No rewrite of the TLS-0 code** ([ADR-W029](ADR-W029.md)) and **no
   `axum-server → tokio-rustls` swap** — `RustlsMaterial` / `load_tls_material` are
   untouched.
3. **Config.** `[control.limits] header_read_timeout_secs` (`ManagementLimits`),
   default **20 s**, rejects `0`. Applied to every served connection **independent of
   the rate/concurrency `enabled` flag**: a generous header-read timeout has no
   downside for a legitimate client (headers arrive in milliseconds), so it stays on
   even for a trusted-network deployment where the shed layers are disabled. The CLI
   threads it into both serve paths.

The bare `serve_router` / `serve` / `serve_router_tls` / `serve_tls` entry points
delegate to the `*_with` variants with `ServeOptions::default()`, so every existing
caller gets the guard and the whole control suite now runs through the new loop.

## Alternatives considered

- **Reverse proxy in front (W028's original note).** Still valid for
  defence-in-depth, but the in-process floor must not *depend* on external infra on a
  self-hosted appliance — the daemon terminates TLS directly (TLS-0). Kept as an
  optional layer, not the mechanism.
- **Keep `axum::serve`, wrap the IO with a first-bytes deadline.** "End of the header
  block" is not observable at the raw-IO layer without re-implementing HTTP parsing;
  strictly worse than hyper's built-in `header_read_timeout`. Rejected.
- **Hand-roll TLS on `tokio-rustls` + drop `axum-server`.** The initial plan before
  `http_builder()` was found. Rejected once the accessor was confirmed: it would
  rewrite just-merged TLS-0 code and swap a dependency for no benefit.

## Consequences

- **Isolation (#10) preserved.** The loop only serves HTTP and never awaits the
  engine; a slow/abusive client cannot back-pressure the data plane. A transient
  `accept` error backs off and continues rather than tearing down the plane. The full
  `multiview-control` suite (serve, management_limits, realtime, ws_ticket, tls_serve)
  passes through the new loop.
- **Behaviourally verified.** A plain-HTTP and a TLS slowloris test (raw rustls
  client) each prove a stalled header block is dropped at the deadline
  (RED held the connection ~5 s; GREEN drops it in <1 s); positive controls prove a
  complete request is still served.
- **Dependencies.** `hyper` + `hyper-util` become direct deps of `multiview-control`
  (both already in the lock via axum); `tokio-rustls` is a new dev-dep for the TLS
  slowloris client (already in the lock via the cast backend). All pure-Rust,
  LGPL-clean, `cargo deny`-clean.
- **Scope.** `header_read_timeout` bounds HTTP/1 header reads; an HTTP/2 peer is
  bounded by the frame model + SETTINGS. This closes the W028 F-A residual; W028 is
  updated to point here.
