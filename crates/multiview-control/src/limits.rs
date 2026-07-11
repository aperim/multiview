//! Management-plane rate limiting — the keyed, lock-free rate limiter that backs
//! the SEC-14 control-plane `DoS` floor.
//!
//! This module is the **pure, clock-injected** core: a [`RateLimiter`] that
//! answers "may this key make one more request right now?" with a [`Decision`].
//! The axum middleware that keys it on the peer IP (pre-auth) and the API-key id
//! (post-auth), and the global concurrency cap, live alongside it and are wired in
//! [`crate::router`] — but the accounting here has no socket, no `AppState`, and no
//! wall clock, so it is exhaustively testable offline (the same seam pattern the
//! rest of this crate uses for clock-driven logic).
//!
//! ## Bounded by construction — the `DoS`-resistance property
//!
//! A rate limiter that grows a per-key map is itself a memory-exhaustion vector:
//! an attacker rotating source addresses inflates the map without bound. This
//! limiter instead hashes every key into a **fixed-size table** of cells
//! ([`RateLimiter::with_hasher`] allocates the cells once and never grows), so
//! memory is `O(cells)` regardless of how many distinct keys are ever seen. Two
//! keys that hash to the same cell **share** a limiter — which can only make
//! limiting *stricter* for that pair, never looser, so the floor is preserved.
//! The hasher is seeded per process (a random [`RandomState`] in production), so
//! an attacker cannot predict or force a collision to target a specific victim.
//!
//! ## Lock-free hot path
//!
//! Each cell is a single [`AtomicU64`] holding a **theoretical arrival time**
//! (`tat`) — the virtual-scheduling (GCRA) formulation of a token bucket, its exact
//! dual: a `tat` running ahead of `now` by up to the burst tolerance `tau` is a
//! bucket that many requests below full. Because the whole per-cell state is one
//! word, [`RateLimiter::check`] is a wait-free load plus a single compare-and-swap
//! — never a mutex. A single-key flood (which concentrates on one cell) serialises
//! on that one atomic word for a few nanoseconds and can **never park a Tokio
//! worker holding a lock**, which a blocking `std::sync::Mutex` on the async hot
//! path could.
//!
//! ## Timekeeping
//!
//! [`RateLimiter::check`] takes the current time as an explicit monotonic
//! nanosecond count, so tests drive it deterministically; the middleware supplies
//! it from a monotonic clock. Accounting is integer-only virtual time — never float
//! — and every operation saturates, so a frozen, jumped, wrapped, or **regressed**
//! clock can neither drift, panic, nor grant a bonus (a rewound clock is absorbed
//! by `max(tat, now)`, so it never double-refills).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use multiview_config::limits::ManagementLimits;
use tokio::sync::Semaphore;

use crate::auth::ApiKeyStore;
use crate::problem::Problem;

/// Nanoseconds per second — the fixed-point base for the virtual-scheduling clock.
/// The per-request increment (`increment_ns`) is `NANOS_PER_SEC / refill_per_sec`,
/// i.e. the emission interval `T` of one request at the configured rate.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Rate parameters: a burst `capacity` (max requests admitted at once) replenished
/// at `refill_per_sec` requests per second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rate {
    /// The maximum number of requests permitted in an instantaneous burst.
    pub capacity: u32,
    /// The steady-state replenishment rate, in requests per second.
    pub refill_per_sec: u32,
}

/// The verdict for one request against a key's cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The request is within budget; the cell has been advanced.
    Allowed,
    /// The request exceeds the budget. `retry_after` is the time until the cell has
    /// freed enough for one request — surfaced verbatim in the `Retry-After`
    /// response header.
    Limited {
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
}

/// A bounded, fixed-size, sharded, **lock-free** rate limiter keyed by an arbitrary
/// hashable key.
///
/// Each cell is one [`AtomicU64`] holding a theoretical arrival time (`tat`) — the
/// virtual-scheduling (GCRA) dual of a token bucket. See the module docs for the
/// bounded-memory / collision / seeding / lock-free rationale.
pub(crate) struct RateLimiter<S = RandomState> {
    /// The fixed table of cells, each a `tat` timestamp in ns. Allocated once; never
    /// grows.
    cells: Box<[AtomicU64]>,
    /// The per-process-seeded hasher that maps a key to a cell.
    hasher: S,
    /// The emission interval `T`: nanoseconds of virtual time one request consumes
    /// (`NANOS_PER_SEC / refill_per_sec`, rounded UP so the effective rate never
    /// exceeds the configured one). At least 1.
    increment_ns: u64,
    /// The burst tolerance `tau = capacity * T`: how far `tat` may run ahead of
    /// `now` while still admitting — the instantaneous burst.
    tau_ns: u64,
    /// The largest `now` the limiter accepts before clamping, so `tat` (bounded by
    /// `now + tau + T`) always has headroom below [`u64::MAX`] and the accounting
    /// can never saturate. ~584 years of ns — unreachable by a real monotonic clock,
    /// this only makes synthetic/adversarial timestamps total.
    now_ceil: u64,
}

impl RateLimiter<RandomState> {
    /// Build a limiter with `cells` cells and the given [`Rate`], seeded with a
    /// per-process-random hasher so cell placement is not attacker-predictable.
    ///
    /// `cells` is clamped to at least 1.
    pub(crate) fn new(cells: usize, rate: Rate) -> Self {
        Self::with_hasher(cells, rate, RandomState::new())
    }
}

impl<S: BuildHasher> RateLimiter<S> {
    /// Build a limiter with an explicit [`BuildHasher`] — the seam tests use to
    /// make cell placement deterministic.
    ///
    /// `cells` is clamped to at least 1 and `refill_per_sec` to at least 1 (a zero
    /// rate would never replenish; config validation rejects it upstream, this clamp
    /// keeps the accounting total).
    pub(crate) fn with_hasher(cells: usize, rate: Rate, hasher: S) -> Self {
        let n = cells.max(1);
        // Emission interval T = ns per request at the configured rate, rounded UP so
        // the effective steady-state rate is never faster than configured (a DoS
        // floor errs strict). refill is clamped to >= 1.
        let increment_ns = NANOS_PER_SEC.div_ceil(u64::from(rate.refill_per_sec.max(1)));
        // Burst tolerance tau = capacity * T (saturating: an absurd capacity clamps
        // rather than overflows; config bounds it upstream).
        let tau_ns = increment_ns.saturating_mul(u64::from(rate.capacity));
        // Keep tat (<= now + tau + one increment) below u64::MAX for any accepted now.
        let now_ceil = u64::MAX.saturating_sub(tau_ns.saturating_add(increment_ns));
        let cells = (0..n)
            // A fresh cell is `tat = 0` (far in the virtual past ⇒ a full burst).
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            hasher,
            increment_ns,
            tau_ns,
            now_ceil,
        }
    }

    /// The number of cells in the fixed table (never changes after construction —
    /// the bounded-memory guarantee). Test-only: the accessor exists to pin that the
    /// table does not grow.
    #[cfg(test)]
    pub(crate) fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// The cell index a key maps to. Stable for the life of the limiter.
    pub(crate) fn cell_of<K: Hash + ?Sized>(&self, key: &K) -> usize {
        let hash = self.hasher.hash_one(key);
        // Work in u64 then narrow: `idx < n <= usize::MAX`, so the narrowing is
        // lossless. No `as` (workspace-banned) and no panic.
        let n = u64::try_from(self.cells.len()).unwrap_or(u64::MAX).max(1);
        usize::try_from(hash % n).unwrap_or(0)
    }

    /// Account for one request from `key` at monotonic time `now_ns`, and report
    /// whether it is [`Decision::Allowed`] or [`Decision::Limited`].
    ///
    /// Virtual-scheduling (GCRA) accounting: the cell's `tat` is the earliest
    /// virtual time at which the next request may arrive; a request is admitted when
    /// advancing `tat` by one increment keeps it within `tau` of `now`. Lock-free —
    /// a wait-free load plus a single CAS; a rejected request never advances `tat`.
    /// Every operation saturates and `now` is clamped, so a frozen, jumped, wrapped,
    /// or regressed clock can neither panic nor grant a bonus request.
    pub(crate) fn check<K: Hash + ?Sized>(&self, key: &K, now_ns: u64) -> Decision {
        let idx = self.cell_of(key);
        // `idx < cells.len()` by construction; fail **open** (never limit) on the
        // impossible out-of-range rather than panic — a DoS floor must not become a
        // self-inflicted outage on an internal accounting slip.
        let Some(cell) = self.cells.get(idx) else {
            return Decision::Allowed;
        };
        // Clamp `now` so `tat` keeps headroom below u64::MAX; only synthetic or
        // adversarial timestamps ever reach the ceiling (a real monotonic ns clock
        // starts near 0 and never approaches ~584 years).
        let now = now_ns.min(self.now_ceil);
        let mut tat = cell.load(Ordering::Relaxed);
        loop {
            // The effective arrival time is max(stored tat, now): a cell idle past
            // its tat resets to `now` (a full burst), and — crucially — a REGRESSED
            // clock can never rewind below the recorded tat, so a wobble grants no
            // bonus request (F3).
            let base = tat.max(now);
            // How far the post-admission tat would sit ahead of `now`, computed as
            // (base - now) + T to sidestep any `now + tau` overflow near u64::MAX.
            let excess = base.saturating_sub(now).saturating_add(self.increment_ns);
            if excess <= self.tau_ns {
                // Within burst: advance tat by one increment, committed with a CAS so
                // a concurrent admit on the same cell can never be lost (the exact
                // accounting a lock would give, without the lock).
                let new_tat = base.saturating_add(self.increment_ns);
                match cell.compare_exchange_weak(
                    tat,
                    new_tat,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Decision::Allowed,
                    // Another thread advanced this cell; re-read and retry.
                    Err(observed) => tat = observed,
                }
            } else {
                // Over burst: refuse WITHOUT advancing tat (a rejected request never
                // consumes virtual time). The wait until one increment frees is
                // `excess - tau` (> 0 here, so saturating_sub cannot underflow).
                let retry_after_ns = excess.saturating_sub(self.tau_ns);
                return Decision::Limited {
                    retry_after: Duration::from_nanos(retry_after_ns),
                };
            }
        }
    }
}

/// The fixed number of rate-limiter cells per keyed limiter. `O(1)` memory that
/// never grows regardless of how many distinct IPs / keys are ever seen — the
/// bounded-memory property that keeps the `DoS` floor from becoming its own
/// memory-exhaustion vector. Sized generously so a hash collision between the
/// handful of real operators / keys is astronomically unlikely.
const RATE_LIMIT_CELLS: usize = 4096;

/// The `Retry-After` hint on a `503` concurrency shed. Unlike a rate limit there
/// is no exact refill time (a permit frees when some in-flight request
/// completes), so a short fixed hint is offered.
const CONCURRENCY_RETRY_AFTER: Duration = Duration::from_secs(1);

// F4: the config-side ceiling MUST equal the runtime `Semaphore::MAX_PERMITS`, so
// `ManagementLimits::validate` rejects exactly the concurrency caps `Semaphore::new`
// would reject (fail-closed at config load) rather than let the runtime silently
// clamp to a different effective cap. This static assertion fails the build if a
// tokio upgrade ever moves the ceiling out from under the config crate.
const _: () = assert!(
    multiview_config::limits::MAX_CONCURRENT_REQUESTS_CEILING == Semaphore::MAX_PERMITS,
    "config MAX_CONCURRENT_REQUESTS_CEILING must track tokio Semaphore::MAX_PERMITS"
);

/// The runtime management-plane limiters (SEC-14), built once from
/// [`ManagementLimits`] and shared behind an `Arc` by the middleware.
///
/// Isolation-safe (invariant #10): purely control-plane, holds no engine handle;
/// the concurrency guard **sheds** (returns `503`) rather than queueing, and the
/// rate guards return `429` — neither ever blocks.
pub(crate) struct Limiters {
    /// The concurrent in-flight request cap ([`None`] ⇒ limits disabled).
    concurrency: Option<Arc<Semaphore>>,
    /// The per-source-IP rate limiter, applied pre-auth.
    per_ip: Option<RateLimiter>,
    /// The per-API-key rate limiter, applied post-auth.
    per_api_key: Option<RateLimiter>,
    /// The monotonic origin the middleware measures `now_ns` against.
    origin: Instant,
    /// Whether any limit is enforced.
    enabled: bool,
}

impl Limiters {
    /// Build the limiters from validated config. A disabled config yields the
    /// inert [`Limiters::disabled`] set.
    pub(crate) fn from_config(cfg: &ManagementLimits) -> Self {
        if !cfg.enabled {
            return Self::disabled();
        }
        // `validate()` rejects `max_concurrent_requests > MAX_PERMITS` at config
        // load (F4), so this `.min` is unreachable after validation; it stays purely
        // as a belt-and-suspenders guard so an unvalidated (e.g. test-constructed)
        // config can never panic `Semaphore::new` — it does NOT silently reshape a
        // real operator's configured cap, which validation has already bounded.
        let permits = cfg.max_concurrent_requests.min(Semaphore::MAX_PERMITS);
        Self {
            concurrency: Some(Arc::new(Semaphore::new(permits))),
            per_ip: Some(RateLimiter::new(
                RATE_LIMIT_CELLS,
                Rate {
                    capacity: cfg.per_ip.burst,
                    refill_per_sec: cfg.per_ip.refill_per_sec,
                },
            )),
            per_api_key: Some(RateLimiter::new(
                RATE_LIMIT_CELLS,
                Rate {
                    capacity: cfg.per_api_key.burst,
                    refill_per_sec: cfg.per_api_key.refill_per_sec,
                },
            )),
            origin: Instant::now(),
            enabled: true,
        }
    }

    /// The inert limiters: the default for the bare [`crate::state::AppState`]
    /// constructor (tests / embedders) and the result of a disabled config. No
    /// middleware is installed when disabled, so this is never on the hot path.
    pub(crate) fn disabled() -> Self {
        Self {
            concurrency: None,
            per_ip: None,
            per_api_key: None,
            origin: Instant::now(),
            enabled: false,
        }
    }

    /// Whether any limit is enforced — drives whether [`crate::router`] installs
    /// the middleware at all.
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The current monotonic time in nanoseconds since construction (saturating).
    fn now_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// The state the per-API-key middleware needs: the shared limiters plus the API
/// key store it resolves the presented `Bearer` credential against, so only a
/// **validated** key is rate-limited (a wrong-secret request cannot drain a
/// victim key's bucket).
#[derive(Clone)]
pub(crate) struct PerKeyLimitState {
    /// The shared runtime limiters.
    pub(crate) limiters: Arc<Limiters>,
    /// The API key store used to resolve the presented credential's `key_id`.
    pub(crate) api_keys: Arc<ApiKeyStore>,
}

/// Round a retry delay up to whole seconds for the `Retry-After` header (RFC 9110
/// §10.2.3 delta-seconds), never below one second.
fn retry_after_secs(retry_after: Duration) -> u64 {
    let round_up = u64::from(retry_after.subsec_nanos() > 0);
    retry_after.as_secs().saturating_add(round_up).max(1)
}

/// Build an RFC-9457 problem response for a limit rejection, with the
/// `Retry-After` header set from `retry_after`.
fn limit_response(
    status: StatusCode,
    slug: &str,
    title: &str,
    detail: &str,
    retry_after: Duration,
) -> Response {
    let mut response = Problem::new(status.as_u16(), slug, title)
        .with_detail(detail.to_owned())
        .into_response();
    // A decimal integer of seconds is always a valid header value; guard anyway.
    if let Ok(value) = HeaderValue::try_from(retry_after_secs(retry_after).to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// The `429 Too Many Requests` response for a token-bucket rejection.
fn too_many(retry_after: Duration) -> Response {
    limit_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate-limited",
        "Too Many Requests",
        "Request rate limit exceeded; see the Retry-After header.",
        retry_after,
    )
}

/// The `503 Service Unavailable` response for a concurrency shed.
fn overloaded(retry_after: Duration) -> Response {
    limit_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded",
        "Service Unavailable",
        "The management plane is at its concurrent-request capacity; see the Retry-After header.",
        retry_after,
    )
}

/// Pre-auth per-source-IP rate limit. Keyed on the peer IP (from `ConnectInfo`,
/// wired by [`crate::serve_router`]) so it protects the auth path from
/// brute-force. A request whose peer IP is unavailable is not limited here (the
/// concurrency cap + per-key limit still apply — fail open, never a self-inflicted
/// outage).
pub(crate) async fn per_ip_rate_limit(
    State(limiters): State<Arc<Limiters>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = limiters.per_ip.as_ref() else {
        return next.run(request).await;
    };
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.ip());
    let Some(ip) = peer_ip else {
        return next.run(request).await;
    };
    match limiter.check(&ip, limiters.now_ns()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => too_many(retry_after),
    }
}

/// Post-auth per-API-key rate limit. Resolves the presented `Bearer` credential
/// against the key store and limits only a **validated** key (keyed on its
/// `key_id`), so one credential cannot monopolise the management plane.
/// Unauthenticated requests pass through — the per-IP + concurrency limits cover
/// them.
pub(crate) async fn per_api_key_rate_limit(
    State(state): State<PerKeyLimitState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(limiter) = state.limiters.per_api_key.as_ref() else {
        return next.run(request).await;
    };
    let header_value = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let Ok(principal) = state.api_keys.verify_authorization(header_value) else {
        return next.run(request).await;
    };
    match limiter.check(principal.key_id.as_str(), state.limiters.now_ns()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => too_many(retry_after),
    }
}

/// Global concurrent-request cap. Acquires one permit for the duration of the
/// request; when the cap is reached the request is **shed** with `503` +
/// Retry-After rather than queued — bounded, never back-pressures (invariant #10).
pub(crate) async fn concurrency_cap(
    State(limiters): State<Arc<Limiters>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(semaphore) = limiters.concurrency.as_ref() else {
        return next.run(request).await;
    };
    match Arc::clone(semaphore).try_acquire_owned() {
        Ok(permit) => {
            // Hold the permit across the handler; release it once the response is
            // produced (drop after the await, never during it).
            let response = next.run(request).await;
            drop(permit);
            response
        }
        Err(_) => overloaded(CONCURRENCY_RETRY_AFTER),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::BuildHasherDefault;
    use std::time::Duration;

    use proptest::prelude::*;

    use super::{Decision, Rate, RateLimiter};

    /// A deterministic hasher so a test controls exactly which key lands in which
    /// cell (production uses a random-seeded `RandomState`).
    type FixedHasher = BuildHasherDefault<DefaultHasher>;

    const SEC_NS: u64 = 1_000_000_000;

    fn limiter(cells: usize, capacity: u32, refill_per_sec: u32) -> RateLimiter<FixedHasher> {
        RateLimiter::with_hasher(
            cells,
            Rate {
                capacity,
                refill_per_sec,
            },
            FixedHasher::default(),
        )
    }

    #[test]
    fn allows_a_full_burst_then_limits_within_the_same_instant() {
        let l = limiter(64, 3, 1);
        let key = "203.0.113.7";
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        // Fourth request in the same instant: the bucket is empty. Retry-after is
        // one second (one token must accrue at 1 token/second).
        assert_eq!(
            l.check(&key, 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn refills_at_the_configured_rate() {
        // capacity 1, 2 tokens/sec → one token every 500 ms.
        let l = limiter(64, 1, 2);
        let key = "203.0.113.8";
        assert_eq!(l.check(&key, 0), Decision::Allowed);
        assert_eq!(
            l.check(&key, 0),
            Decision::Limited {
                retry_after: Duration::from_millis(500),
            }
        );
        // 500 ms later exactly one token has accrued → one more request allowed.
        assert_eq!(l.check(&key, SEC_NS / 2), Decision::Allowed);
    }

    #[test]
    fn distinct_non_colliding_keys_keep_independent_allowances() {
        let l = limiter(256, 1, 1);
        let a = "198.51.100.1";
        let b = "198.51.100.2";
        // The test is only meaningful if the two keys occupy different cells.
        assert_ne!(
            l.cell_of(&a),
            l.cell_of(&b),
            "test fixture invalid: keys collide into one cell"
        );
        assert_eq!(l.check(&a, 0), Decision::Allowed);
        // `b` still has its own full bucket even though `a` is now drained.
        assert_eq!(l.check(&b, 0), Decision::Allowed);
        assert_eq!(
            l.check(&a, 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn a_single_cell_table_forces_all_keys_to_share_one_bucket() {
        // cells == 1 → every key hashes to the one bucket: bounded memory taken to
        // the extreme. With capacity 1, the SECOND distinct key in the same
        // instant is limited, proving keys share the fixed table (a per-key map
        // would instead hand `key-b` its own fresh bucket → Allowed).
        let l = limiter(1, 1, 1);
        assert_eq!(l.check(&"key-a", 0), Decision::Allowed);
        assert_eq!(
            l.check(&"key-b", 0),
            Decision::Limited {
                retry_after: Duration::from_secs(1),
            }
        );
        assert_eq!(l.cell_count(), 1);
    }

    #[test]
    fn table_never_grows_no_matter_how_many_distinct_keys_are_seen() {
        let l = limiter(128, 5, 5);
        for i in 0..100_000_u32 {
            let _ = l.check(&i, 0);
        }
        // The fixed table is the whole memory footprint — it must not have grown.
        assert_eq!(l.cell_count(), 128);
    }

    proptest! {
        /// At a frozen instant a fresh key admits **exactly** `capacity` requests,
        /// then every further request is limited with a strictly-positive
        /// `retry_after` no larger than the time to refill the whole bucket.
        /// Exercised across arbitrary capacities, rates, keys, and start instants
        /// — this kills off-by-one / mis-accounting mutants in the bucket math.
        #[test]
        fn frozen_burst_admits_exactly_capacity_then_limits(
            capacity in 1_u32..64,
            refill_per_sec in 1_u32..10_000,
            key in any::<u64>(),
            now_ns in any::<u64>(),
            extra in 0_u32..8,
        ) {
            let l = limiter(97, capacity, refill_per_sec);
            // Exactly `capacity` admitted at a single frozen instant.
            for _ in 0..capacity {
                prop_assert_eq!(l.check(&key, now_ns), Decision::Allowed);
            }
            // The next `1 + extra` requests at the same instant are all limited.
            let max_wait_ns = u64::from(capacity)
                .saturating_mul(1_000_000_000)
                .div_ceil(u64::from(refill_per_sec));
            for _ in 0..=extra {
                match l.check(&key, now_ns) {
                    Decision::Allowed => {
                        prop_assert!(false, "an over-capacity request was allowed");
                    }
                    Decision::Limited { retry_after } => {
                        prop_assert!(retry_after > Duration::ZERO);
                        prop_assert!(retry_after <= Duration::from_nanos(max_wait_ns));
                    }
                }
            }
        }
    }

    #[test]
    fn a_clock_wobble_does_not_double_refill() {
        // A non-monotonic injected clock (100 s → 50 s → 100 s) must NOT credit the
        // 50→100 span twice: the limiter accounts against the *latest* arrival time,
        // so only genuine forward progress past the high-water mark replenishes.
        // Capacity 1, refill 1 (one token per second).
        let l = limiter(64, 1, 1);
        let key = "203.0.113.9";
        let t100 = 100 * SEC_NS;
        let t50 = 50 * SEC_NS;

        // Spend the one token at t = 100 s; the next at the same instant is limited.
        assert_eq!(l.check(&key, t100), Decision::Allowed);
        assert!(matches!(l.check(&key, t100), Decision::Limited { .. }));

        // Clock jumps BACK to 50 s: still empty (no negative elapsed refill).
        assert!(matches!(l.check(&key, t50), Decision::Limited { .. }));

        // Clock returns to 100 s: this is NOT new time — the 50→100 span was already
        // accounted the first time we reached 100 s. Re-crediting it here is the bug.
        assert!(
            matches!(l.check(&key, t100), Decision::Limited { .. }),
            "a 100→50→100 clock wobble must not replenish a token twice"
        );

        // Only real forward progress past the high-water mark (101 s) accrues one.
        assert_eq!(l.check(&key, t100 + SEC_NS), Decision::Allowed);
    }
}

#[cfg(test)]
mod middleware_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{header, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use multiview_config::limits::ManagementLimits;
    use tower::ServiceExt;

    use super::{
        concurrency_cap, per_api_key_rate_limit, per_ip_rate_limit, Limiters, PerKeyLimitState,
    };
    use crate::auth::{ApiKeyStore, Principal};
    use crate::problem::PROBLEM_JSON;

    /// A limits config with a tight per-IP / per-API-key burst and a slow refill,
    /// so within a fast test the burst is the whole budget (no refill in flight).
    fn cfg(max_concurrent: usize, ip_burst: u32, key_burst: u32) -> ManagementLimits {
        let mut c = ManagementLimits::default();
        c.max_concurrent_requests = max_concurrent;
        c.per_ip.burst = ip_burst;
        c.per_ip.refill_per_sec = 1;
        c.per_api_key.burst = key_burst;
        c.per_api_key.refill_per_sec = 1;
        c
    }

    fn get_ip(uri: &str, ip: &str) -> Request<Body> {
        let mut req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request builds");
        let addr: SocketAddr = format!("{ip}:40000").parse().expect("addr parses");
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    #[tokio::test]
    async fn per_ip_limit_allows_the_burst_then_429s_the_same_ip() {
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );

        let first = app
            .clone()
            .oneshot(get_ip("/x", "203.0.113.7"))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .clone()
            .oneshot(get_ip("/x", "203.0.113.7"))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            second.headers().contains_key(header::RETRY_AFTER),
            "a 429 must carry Retry-After"
        );
        assert_eq!(
            second
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON),
            "a 429 is an RFC-9457 problem+json"
        );
        let body = second.into_body().collect().await.unwrap().to_bytes();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["status"], 429);
    }

    #[tokio::test]
    async fn per_ip_limit_is_independent_across_ips() {
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );

        // Drain IP A.
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.1"))
                .await
                .unwrap()
                .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // IP B is unaffected.
        assert_eq!(
            app.clone()
                .oneshot(get_ip("/x", "198.51.100.2"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn per_ip_limit_fails_open_without_connect_info() {
        // No ConnectInfo extension ⇒ no key ⇒ the per-IP guard cannot limit and
        // passes through (the concurrency cap + per-key limit still apply). Three
        // requests past a burst of 1 all succeed.
        let limiters = Arc::new(Limiters::from_config(&cfg(256, 1, 1)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, per_ip_rate_limit),
        );
        for _ in 0..3 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    fn per_key_state(cfg: &ManagementLimits) -> (PerKeyLimitState, String) {
        let mut store = ApiKeyStore::new(b"test-pepper".to_vec());
        let principal = Principal::local_admin();
        store.register("op", "s3cret", principal);
        (
            PerKeyLimitState {
                limiters: Arc::new(Limiters::from_config(cfg)),
                api_keys: Arc::new(store),
            },
            "Bearer op.s3cret".to_owned(),
        )
    }

    #[tokio::test]
    async fn per_api_key_limit_429s_a_valid_key_after_its_burst() {
        let (state, bearer) = per_key_state(&cfg(256, 1, 1));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(state, per_api_key_rate_limit),
        );
        let mk = || {
            Request::builder()
                .uri("/x")
                .header(header::AUTHORIZATION, &bearer)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.clone().oneshot(mk()).await.unwrap().status(),
            StatusCode::OK
        );
        let limited = app.clone().oneshot(mk()).await.unwrap();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));
    }

    #[tokio::test]
    async fn per_api_key_limit_ignores_unauthenticated_requests() {
        let (state, _bearer) = per_key_state(&cfg(256, 1, 1));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(state, per_api_key_rate_limit),
        );
        // No credential ⇒ not per-key limited; three past a burst of 1 all pass.
        for _ in 0..3 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn concurrency_cap_503s_when_no_permit_is_available() {
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        // Exhaust the single permit up front (in-crate test touches the field).
        let held = limiters
            .concurrency
            .as_ref()
            .expect("enabled ⇒ a semaphore")
            .clone()
            .try_acquire_owned()
            .expect("one permit is free");

        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters.clone(), concurrency_cap),
        );
        let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(resp.headers().contains_key(header::RETRY_AFTER));
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        drop(held);
    }

    #[tokio::test]
    async fn concurrency_cap_releases_the_permit_after_each_response() {
        // max_concurrent = 1, but sequential requests each release the permit, so
        // both succeed (proves the guard is not a one-shot).
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        let app = Router::new().route("/x", get(|| async { "ok" })).layer(
            axum::middleware::from_fn_with_state(limiters, concurrency_cap),
        );
        for _ in 0..2 {
            let req = Request::builder().uri("/x").body(Body::empty()).unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::OK
            );
        }
    }

    /// A response body that never yields a frame and never ends — a stand-in for an
    /// SSE / long-poll stream that stays open. It lets a test prove the concurrency
    /// permit is tied to the BODY's lifetime (F1), not just the handler's return.
    struct PendingBody;

    impl http_body::Body for PendingBody {
        type Data = axum::body::Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            std::task::Poll::Pending
        }

        fn is_end_stream(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn a_held_open_response_body_holds_its_permit_until_the_body_ends() {
        // cap = 1. The handler returns IMMEDIATELY with a body that never completes
        // (an SSE-like long-lived stream). The permit must ride the BODY, not just
        // the handler: while the first response is held open (body undrained) a
        // second request is shed `503`; dropping the first frees the permit.
        let limiters = Arc::new(Limiters::from_config(&cfg(1, 256, 256)));
        let app = Router::new()
            .route(
                "/stream",
                get(|| async { axum::response::Response::new(Body::new(PendingBody)) }),
            )
            .layer(axum::middleware::from_fn_with_state(
                limiters,
                concurrency_cap,
            ));

        // First request: hold the response object (never drain its still-open body).
        let first = app
            .clone()
            .oneshot(Request::builder().uri("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        // A second request while the first body is still open: no permit → `503`.
        let second = app
            .clone()
            .oneshot(Request::builder().uri("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a held-open streaming body must keep its concurrency permit"
        );

        // End the first stream (drop the response ⇒ drop its body ⇒ release permit).
        drop(first);
        drop(second);

        // A fresh request now finds the freed permit.
        let third = app
            .clone()
            .oneshot(Request::builder().uri("/stream").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(third.status(), StatusCode::OK);
    }

    #[test]
    fn disabled_config_builds_inert_limiters() {
        let mut disabled = ManagementLimits::default();
        disabled.enabled = false;
        let limiters = Limiters::from_config(&disabled);
        assert!(!limiters.is_enabled());
    }
}
