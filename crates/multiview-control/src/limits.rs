//! Management-plane rate limiting — the keyed token-bucket that backs the SEC-14
//! control-plane DoS floor.
//!
//! This module is the **pure, clock-injected** core: a [`RateLimiter`] that
//! answers "may this key make one more request right now?" with a token-bucket
//! [`Decision`]. The axum middleware that keys it on the peer IP (pre-auth) and
//! the API-key id (post-auth), and the global concurrency cap, live alongside it
//! and are wired in [`crate::router`] — but the accounting here has no socket, no
//! `AppState`, and no wall clock, so it is exhaustively testable offline (the same
//! seam pattern the rest of this crate uses for clock-driven logic).
//!
//! ## Bounded by construction — the DoS-resistance property
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
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

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
    /// construction — the bounded-memory guarantee).
    pub(crate) fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// The cell index a key maps to. Stable for the life of the limiter.
    pub(crate) fn cell_of<K: Hash + ?Sized>(&self, key: &K) -> usize {
        let mut h = self.hasher.build_hasher();
        key.hash(&mut h);
        let hash = h.finish();
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
        let available = bucket
            .tokens
            .saturating_add(refill)
            .min(self.capacity_nano);
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
