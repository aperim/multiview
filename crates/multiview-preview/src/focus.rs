//! The **focus gate**: a hard, deterministic cap on concurrent WHEP preview
//! focus sessions.
//!
//! Click-to-focus promotes one entity from the cheap JPEG grid to a single
//! low-latency WebRTC preview encode (preview brief §4). Each such session costs
//! a real preview-encode session, so worst-case preview load must be bounded
//! *deterministically* — brief §3 "CAP CONCURRENCY", ADR-P002 (one focus per
//! operator, base Apple silicon = 1 encode engine → cap WHEP to 1).
//!
//! [`FocusGate`] is the admission primitive that keeps that promise. It is keyed
//! by an opaque scope `K` (the control plane keys it by the WHEP scope label),
//! holds a **global** cap (server-wide) and an independent **per-scope** cap, and
//! hands out a [`FocusLease`] whose `Drop` releases the slot — exactly the
//! lazy-start / auto-stop refcount pattern [`crate::tap::TapRegistry`] already
//! uses for taps.
//!
//! ## Isolation (invariant #10)
//!
//! The gate holds **only its own counters**, behind a short-lived
//! `std::sync::Mutex` the engine's publish path never touches (the same pattern
//! as [`crate::tap::TapRegistry`]). It owns no engine handle, no command bus, and
//! never blocks the data plane: a focus that cannot be admitted is *rejected*
//! (the caller sheds to the always-available JPEG transport), never queued and
//! never able to back-pressure or starve the protected output.
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// The hard caps a [`FocusGate`] enforces: a server-wide ceiling and an
/// independent per-scope ceiling on concurrent focus sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusCaps {
    /// The maximum number of concurrent focus sessions **server-wide**, across
    /// every scope.
    pub global: usize,
    /// The maximum number of concurrent focus sessions for any **single** scope.
    pub per_scope: usize,
}

impl FocusCaps {
    /// Build a cap pair.
    #[must_use]
    pub const fn new(global: usize, per_scope: usize) -> Self {
        Self { global, per_scope }
    }
}

impl Default for FocusCaps {
    /// Conservative defaults: a single concurrent focus, server-wide and
    /// per-scope (ADR-P002: one focus at a time; base Apple silicon = 1 encode
    /// engine). Deployments raise these via config once the HAL has probed the
    /// real per-system encode-session ceiling.
    fn default() -> Self {
        Self::new(1, 1)
    }
}

/// Why a focus admission was refused — always because a hard cap is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FocusDenied {
    /// The server-wide [`FocusCaps::global`] ceiling is full.
    GlobalFull,
    /// The [`FocusCaps::per_scope`] ceiling for this scope is full.
    ScopeFull,
}

/// Shared, mutex-guarded counters — the gate's *entire* state.
#[derive(Debug)]
struct Inner<K> {
    caps: FocusCaps,
    /// Live focus count per scope. A scope with no live focus carries no entry,
    /// so the map shrinks back to empty when everything is released (idle-cost
    /// invariant, ADR-P003).
    per_scope: HashMap<K, usize>,
    /// Live focus count server-wide (the sum of `per_scope`'s values, tracked
    /// separately so the global check is O(1)).
    global: usize,
}

/// A hard, deterministic cap on concurrent WHEP focus sessions.
///
/// Cheap to clone (an `Arc` around the shared counters); hand clones to every
/// negotiate path. `K` is the opaque scope key (the control plane uses the WHEP
/// scope label `String`).
pub struct FocusGate<K> {
    inner: Arc<Mutex<Inner<K>>>,
}

impl<K> Clone for FocusGate<K> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K> std::fmt::Debug for FocusGate<K>
where
    K: Eq + Hash + Clone,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let active = self.active();
        f.debug_struct("FocusGate")
            .field("active", &active)
            .finish()
    }
}

impl<K> FocusGate<K>
where
    K: Eq + Hash + Clone,
{
    /// Build a gate with the given caps and no live sessions.
    #[must_use]
    pub fn new(caps: FocusCaps) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                caps,
                per_scope: HashMap::new(),
                global: 0,
            })),
        }
    }

    /// Try to admit a focus session for `scope`.
    ///
    /// RED STUB: this initial implementation admits unconditionally, ignoring
    /// the caps. The cap-enforcement tests therefore FAIL until the green
    /// implementation lands.
    ///
    /// # Errors
    ///
    /// Returns [`FocusDenied`] when a cap is full (not yet — see above).
    pub fn try_acquire(&self, scope: K) -> Result<FocusLease<K>, FocusDenied> {
        if let Ok(mut guard) = self.inner.lock() {
            guard.global = guard.global.saturating_add(1);
            *guard.per_scope.entry(scope.clone()).or_insert(0) += 1;
        }
        Ok(FocusLease {
            gate: self.clone(),
            scope,
            released: false,
        })
    }

    /// The number of live focus sessions server-wide.
    #[must_use]
    pub fn active(&self) -> usize {
        self.inner.lock().map_or(0, |g| g.global)
    }

    /// The number of live focus sessions for `scope`.
    #[must_use]
    pub fn active_in(&self, scope: &K) -> usize {
        self.inner
            .lock()
            .map_or(0, |g| g.per_scope.get(scope).copied().unwrap_or(0))
    }

    /// Release one focus slot for `scope`. Internal: called from
    /// [`FocusLease`]'s `Drop`.
    fn release(&self, scope: &K) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        guard.global = guard.global.saturating_sub(1);
        if let Some(count) = guard.per_scope.get_mut(scope) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                guard.per_scope.remove(scope);
            }
        }
    }
}

/// A held focus slot. Dropping it returns the slot to the gate (decrementing the
/// global and per-scope counters), mirroring [`crate::tap::TapLease`].
pub struct FocusLease<K>
where
    K: Eq + Hash + Clone,
{
    gate: FocusGate<K>,
    scope: K,
    released: bool,
}

impl<K> FocusLease<K>
where
    K: Eq + Hash + Clone,
{
    /// The scope this lease holds a slot for.
    #[must_use]
    pub fn scope(&self) -> &K {
        &self.scope
    }
}

impl<K> std::fmt::Debug for FocusLease<K>
where
    K: Eq + Hash + Clone + std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FocusLease")
            .field("scope", &self.scope)
            .field("released", &self.released)
            .finish()
    }
}

impl<K> Drop for FocusLease<K>
where
    K: Eq + Hash + Clone,
{
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.gate.release(&self.scope);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn admits_up_to_the_global_cap_then_denies() {
        let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(2, 2));
        let a = gate
            .try_acquire("program".to_owned())
            .expect("1st admitted");
        let b = gate
            .try_acquire("input:cam-1".to_owned())
            .expect("2nd admitted");
        assert_eq!(gate.active(), 2);
        // The 3rd exceeds the global cap of 2.
        assert_eq!(
            gate.try_acquire("input:cam-2".to_owned()).err(),
            Some(FocusDenied::GlobalFull)
        );
        drop(a);
        drop(b);
    }

    #[test]
    fn releasing_a_lease_frees_a_global_slot() {
        let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(1, 1));
        let lease = gate.try_acquire("program".to_owned()).expect("admitted");
        assert_eq!(gate.active(), 1);
        // At capacity: the next is denied.
        assert!(gate.try_acquire("input:cam-1".to_owned()).is_err());
        drop(lease);
        // Releasing freed the only slot.
        assert_eq!(gate.active(), 0);
        let _next = gate
            .try_acquire("input:cam-1".to_owned())
            .expect("slot freed");
        assert_eq!(gate.active(), 1);
    }

    #[test]
    fn per_scope_caps_are_independent() {
        // Global is generous; the per-scope cap of 1 bounds each scope alone.
        let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(8, 1));
        let _p = gate.try_acquire("program".to_owned()).expect("program 1");
        // A *second* focus on the SAME scope hits the per-scope cap...
        assert_eq!(
            gate.try_acquire("program".to_owned()).err(),
            Some(FocusDenied::ScopeFull)
        );
        // ...but a different scope is unaffected (independence).
        let _i = gate
            .try_acquire("input:cam-1".to_owned())
            .expect("a different scope is independent");
        assert_eq!(gate.active(), 2);
        assert_eq!(gate.active_in(&"program".to_owned()), 1);
        assert_eq!(gate.active_in(&"input:cam-1".to_owned()), 1);
    }

    #[test]
    fn dropping_all_leases_returns_active_to_zero() {
        let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(4, 4));
        {
            let _a = gate.try_acquire("a".to_owned()).expect("a");
            let _b = gate.try_acquire("a".to_owned()).expect("a2");
            let _c = gate.try_acquire("b".to_owned()).expect("b");
            assert_eq!(gate.active(), 3);
        }
        // ADR-P003 idle-cost invariant: every counter returns to zero and the
        // per-scope map shrinks back to empty.
        assert_eq!(gate.active(), 0);
        assert_eq!(gate.active_in(&"a".to_owned()), 0);
        assert_eq!(gate.active_in(&"b".to_owned()), 0);
    }

    #[test]
    fn concurrent_acquire_release_never_exceeds_the_cap() {
        // A real concurrency stress (std threads, no proptest dep): many workers
        // hammer acquire/release; the live count must NEVER exceed the cap and
        // must settle to zero. This proves the gate is the single source of truth
        // under contention (invariant #10: bounded, deterministic).
        const CAP: usize = 4;
        const WORKERS: usize = 16;
        const ITERS: usize = 2_000;
        let gate: FocusGate<String> = FocusGate::new(FocusCaps::new(CAP, CAP));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for w in 0..WORKERS {
            let gate = gate.clone();
            let max_seen = Arc::clone(&max_seen);
            handles.push(thread::spawn(move || {
                let scope = format!("scope-{}", w % 3);
                for _ in 0..ITERS {
                    if let Ok(lease) = gate.try_acquire(scope.clone()) {
                        let now = gate.active();
                        // Record the high-water mark of the live count.
                        let mut prev = max_seen.load(Ordering::Relaxed);
                        while now > prev {
                            match max_seen.compare_exchange_weak(
                                prev,
                                now,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(actual) => prev = actual,
                            }
                        }
                        drop(lease);
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("worker joins");
        }
        assert!(
            max_seen.load(Ordering::Relaxed) <= CAP,
            "live focus count {} exceeded the cap {CAP}",
            max_seen.load(Ordering::Relaxed)
        );
        assert_eq!(gate.active(), 0, "all leases released; idle cost is zero");
    }
}
