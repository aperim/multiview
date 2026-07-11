//! Management-plane rate limiting — the keyed token-bucket that backs the SEC-14
//! control-plane `DoS` floor.
//!
//! This module is the **pure, clock-injected** core: a [`RateLimiter`] that
//! answers "may this key make one more request right now?" with a token-bucket
//! [`Decision`]. The axum middleware that keys it on the peer IP (pre-auth) and
//! the API-key id (post-auth), and the global concurrency cap, live alongside it
//! and are wired in [`crate::router`] — but the accounting here has no socket, no
//! `AppState`, and no wall clock, so it is exhaustively testable offline (the same
//! seam pattern the rest of this crate uses for clock-driven logic).
//!
//! ## Bounded by construction — the `DoS`-resistance property
//!
//! A rate limiter that grows a per-key map is itself a memory-exhaustion vector:
//! an attacker rotating source addresses inflates the map without bound. This
//! limiter instead hashes every key into a **fixed-size table** of buckets
//! ([`RateLimiter::with_hasher`] allocates the cells once and never grows), so
//! memory is `O(cells)` regardless of how many distinct keys are ever seen. Two
//! keys that hash to the same cell **share** a bucket — which can only make
//! limiting *stricter* for that pair, never looser, so the floor is preserved.
//! The hasher is seeded per process (a random [`RandomState`] in production), so
//! an attacker cannot predict or force a collision to target a specific victim.
//!
//! ## Timekeeping
//!
//! [`RateLimiter::check`] takes the current time as an explicit monotonic
//! nanosecond count, so tests drive it deterministically; the middleware supplies
//! it from a monotonic clock. Accounting is integer-only (nano-token fixed point)
//! — never float — so it neither drifts nor panics on overflow (all arithmetic is
//! saturating).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash};
use std::net::SocketAddr;
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

/// Nano-tokens per whole token. Chosen equal to nanoseconds-per-second so that
/// the refill rate in nano-tokens **per nanosecond** is exactly the configured
/// tokens-per-second — the identity that keeps the bucket math integer-only.
const TOKEN_SCALE: u64 = 1_000_000_000;

/// The cost of a single request, in nano-tokens (one whole token).
const REQUEST_COST: u64 = TOKEN_SCALE;

/// Token-bucket parameters: a burst `capacity` (max tokens held) refilled at
/// `refill_per_sec` tokens per second.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rate {
    /// The maximum number of requests permitted in an instantaneous burst.
    pub capacity: u32,
    /// The steady-state replenishment rate, in requests per second.
    pub refill_per_sec: u32,
}

/// The verdict for one request against a key's bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The request is within budget; a token has been spent.
    Allowed,
    /// The request exceeds the budget. `retry_after` is the time until the
    /// bucket has accrued enough for one request — surfaced verbatim in the
    /// `Retry-After` response header.
    Limited {
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
}

/// One cell of the fixed table: a token bucket's current fill and the instant it
/// was last updated.
#[derive(Debug)]
struct Bucket {
    /// Tokens currently available, in nano-tokens (see [`TOKEN_SCALE`]).
    tokens: u64,
    /// The monotonic nanosecond timestamp of the last [`RateLimiter::check`].
    last_ns: u64,
}

/// A bounded, fixed-size, sharded token-bucket rate limiter keyed by an arbitrary
/// hashable key.
///
/// See the module docs for the bounded-memory / collision / seeding rationale.
pub(crate) struct RateLimiter<S = RandomState> {
    /// The fixed table of buckets. Allocated once; never grows.
    cells: Box<[Mutex<Bucket>]>,
    /// The per-process-seeded hasher that maps a key to a cell.
    hasher: S,
    /// The burst ceiling, in nano-tokens (`capacity * TOKEN_SCALE`).
    capacity_nano: u64,
    /// The refill rate in tokens/second (also nano-tokens per nanosecond),
    /// clamped to at least 1 so accounting never divides by zero.
    refill_per_sec: u64,
}

impl RateLimiter<RandomState> {
    /// Build a limiter with `cells` buckets and the given [`Rate`], seeded with a
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
    /// `cells` is clamped to at least 1 and `refill_per_sec` to at least 1 (a
    /// zero rate would never replenish; config validation rejects it upstream,
    /// this clamp keeps the accounting total).
    pub(crate) fn with_hasher(cells: usize, rate: Rate, hasher: S) -> Self {
        let n = cells.max(1);
        let capacity_nano = u64::from(rate.capacity).saturating_mul(TOKEN_SCALE);
        let cells = (0..n)
            .map(|_| {
                Mutex::new(Bucket {
                    // Start full: a fresh key gets its whole burst immediately.
                    tokens: capacity_nano,
                    last_ns: 0,
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            cells,
            hasher,
            capacity_nano,
            refill_per_sec: u64::from(rate.refill_per_sec.max(1)),
        }
    }

    /// The number of buckets in the fixed table (never changes after
    /// construction — the bounded-memory guarantee). Test-only: the accessor
    /// exists to pin that the table does not grow.
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
    /// Integer nano-token accounting: the bucket accrues `refill_per_sec`
    /// nano-tokens per nanosecond (capped at the burst ceiling), and one request
    /// costs one whole token ([`REQUEST_COST`]). Every operation saturates, so a
    /// frozen, jumped, or wrapped clock can never panic the limiter.
    pub(crate) fn check<K: Hash + ?Sized>(&self, key: &K, now_ns: u64) -> Decision {
        let idx = self.cell_of(key);
        // `idx < cells.len()` by construction; fail **open** (never limit) on the
        // impossible out-of-range rather than panic — a DoS floor must not become
        // a self-inflicted outage on an internal accounting slip.
        let Some(cell) = self.cells.get(idx) else {
            return Decision::Allowed;
        };
        // A poisoned cell holds only two always-valid `u64`s; recover its inner
        // value rather than propagate a panic through the limiter.
        let mut bucket = match cell.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        // Fold the time since the last update into the bucket (clamped to the
        // burst ceiling), then spend or refuse one token.
        let elapsed = now_ns.saturating_sub(bucket.last_ns);
        let refill = elapsed.saturating_mul(self.refill_per_sec);
        let available = bucket.tokens.saturating_add(refill).min(self.capacity_nano);
        bucket.last_ns = now_ns;

        if available >= REQUEST_COST {
            bucket.tokens = available - REQUEST_COST;
            Decision::Allowed
        } else {
            bucket.tokens = available;
            // Time to accrue the shortfall: `deficit` nano-tokens at
            // `refill_per_sec` nano-tokens per nanosecond, rounded up. The rate is
            // clamped to at least 1 at construction, so this never divides by zero.
            let deficit = REQUEST_COST - available;
            let retry_after_ns = deficit.div_ceil(self.refill_per_sec);
            Decision::Limited {
                retry_after: Duration::from_nanos(retry_after_ns),
            }
        }
    }
}

/// The fixed number of token-bucket cells per keyed limiter. `O(1)` memory that
/// never grows regardless of how many distinct IPs / keys are ever seen — the
/// bounded-memory property that keeps the `DoS` floor from becoming its own
/// memory-exhaustion vector. Sized generously so a hash collision between the
/// handful of real operators / keys is astronomically unlikely.
const RATE_LIMIT_CELLS: usize = 4096;

/// The `Retry-After` hint on a `503` concurrency shed. Unlike a rate limit there
/// is no exact refill time (a permit frees when some in-flight request
/// completes), so a short fixed hint is offered.
const CONCURRENCY_RETRY_AFTER: Duration = Duration::from_secs(1);

/// The runtime management-plane limiters (SEC-14), built once from
/// [`ManagementLimits`] and shared behind an `Arc` by the middleware.
///
/// Isolation-safe (invariant #10): purely control-plane, holds no engine handle;
/// the concurrency guard **sheds** (returns `503`) rather than queueing, and the
/// rate guards return `429` — neither ever blocks.
pub(crate) struct Limiters {
    /// The concurrent in-flight request cap ([`None`] ⇒ limits disabled).
    concurrency: Option<Arc<Semaphore>>,
    /// The per-source-IP token bucket, applied pre-auth.
    per_ip: Option<RateLimiter>,
    /// The per-API-key token bucket, applied post-auth.
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
        // Clamp the permit count to tokio's ceiling so an absurd config value can
        // never panic `Semaphore::new`.
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

    #[test]
    fn disabled_config_builds_inert_limiters() {
        let mut disabled = ManagementLimits::default();
        disabled.enabled = false;
        let limiters = Limiters::from_config(&disabled);
        assert!(!limiters.is_enabled());
    }
}
