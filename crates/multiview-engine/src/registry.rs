//! The process-global **source registry** — decode-once, use-many
//! (ADR-0030 §3, ADR-0034 §2).
//!
//! In a multi-program engine, many cells across many programs may bind the *same*
//! physical source. Decoding it once per binding wastes the scarcest budget
//! (decode megapixels/sec, invariant #6). The [`SourceRegistry`] is the owner of
//! **source identity → (one ingest/decode actor + one shared [`TileStore`])**: the
//! first consumer to reference a canonical [`SourceKey`] spins the decode; every
//! later consumer of the same key shares an [`Arc`] clone of the same store. The
//! decode is sized at the **first** acquire's requested resolution; a later, larger
//! acquire grows the recorded per-axis **supremum** *metadata* (ADR-0030 §3) but
//! does **not** resize a live store. Callers that need the supremum acquire it up
//! front (MP-2's `Pipeline::build` does); each consumer scales at composite.
//!
//! ## Isolation (inv #1 / inv #10)
//!
//! The registry is a *lifecycle* structure, never a data-plane one. Consumers
//! **sample** their source through the [`Arc<TileStore>`](TileStore) a handle
//! hands them — a lock-free read that **never touches the registry lock**. The
//! registry's `Mutex` is taken only on `acquire`/`release` (bounded, O(1)), never
//! per tick and never across a blocking operation. Consequently a wedged or absent
//! source can never stall a sibling consumer's sample path.
//!
//! ## Teardown off the hot path (safety rule §4 / inv #10)
//!
//! When the **last** reference to a source is released the entry is removed and its
//! decode actor is handed to a dedicated **reaper thread** via a wait-free,
//! non-blocking channel send. The reaper runs each blocking stop-and-join on its
//! **own detachable helper thread**, so even a decode-thread join that wedges
//! forever never blocks the reaper's consume loop nor grows the teardown queue
//! unboundedly behind it (inv #10) — a sustained stream of healthy last-releases
//! keeps [`pending_teardowns`](SourceRegistry::pending_teardowns) bounded. The
//! releasing consumer's `Drop` only sends, so an `Arc`-drop that returns a pooled
//! buffer never runs a blocking join inside a Tokio async destructor. The explicit
//! [`SourceRegistry::shutdown`] (synchronous teardown context) **bounded-grace-joins**
//! the in-flight teardowns then **detaches** any stragglers rather than blocking
//! forever on a wedged join.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use multiview_framestore::TileStore;

/// A requested decode resolution, in pixels.
///
/// The registry records the per-axis **supremum** of all consumers' requests as
/// *metadata* (ADR-0030 §3). The live decode is sized at the **first** acquire (see
/// [`SourceRegistry::acquire`]); the supremum tracks the largest request but does
/// **not** resize a live store.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RequestedSize {
    /// Requested width in pixels.
    pub width: u32,
    /// Requested height in pixels.
    pub height: u32,
}

impl RequestedSize {
    /// The per-axis supremum (max width, max height) of two requests — the size
    /// the shared decode must produce to satisfy both consumers (ADR-0030 §3).
    #[must_use]
    pub fn supremum(self, other: Self) -> Self {
        Self {
            width: self.width.max(other.width),
            height: self.height.max(other.height),
        }
    }
}

/// The canonical identity of a *physical* source in the [`SourceRegistry`].
///
/// Two consumers that resolve to the **same physical elementary stream** MUST
/// produce **equal** keys, so the registry decodes once and shares the result
/// (ADR-0030 §3, ADR-0034 §2). The key is derived from the source's kind +
/// location (url / path / name / sdp) + auth + decode placement — deliberately
/// **not** the operator `id` string alone: two ids pointing at one url should
/// share one decode, and one id re-pointed to a new url must **not** alias the old
/// decode. The kind-scoped `StableStreamId` refinement (ADR-0034 §1/§2 — TS PID,
/// HLS `group_id`+`name`, general `kind`+ordinal+codec+lang+title) composes *into*
/// this canonical string; it is layered in a later milestone.
///
/// Backed by an [`Arc<str>`] so cloning (done on every `acquire`) is a cheap
/// refcount bump.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SourceKey(Arc<str>);

impl SourceKey {
    /// Build a key from an already-canonicalized identity string.
    ///
    /// The caller owns canonicalization: equal physical streams must map to equal
    /// strings (see [`SourceKey`] for what the string must fold in).
    #[must_use]
    pub fn from_canonical(canonical: impl Into<Arc<str>>) -> Self {
        Self(canonical.into())
    }

    /// The canonical identity string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A running ingest/decode actor owned by a [`SourceRegistry`] entry.
///
/// Its [`shutdown`](SourceActor::shutdown) — the stop-and-join of the decode
/// thread — is **blocking** and runs **exclusively on a detachable helper thread
/// the reaper spawns for it**, never on a program's hot path and never inside a
/// Tokio async destructor (safety rule §4 / inv #10). A last-release `Drop` only
/// hands the boxed actor to the reaper via a wait-free channel send; the reaper
/// spawns a helper thread that performs the join off every hot path, so a wedged
/// join never blocks the reaper's consume loop.
///
/// Implementors' own `Drop` (reached only if the helper thread cannot be spawned,
/// or the registry is torn down while this actor is still queued — i.e. the reaper
/// is already gone) MUST be non-blocking: signal-and-detach, never a join, for the
/// same reason.
pub trait SourceActor: Send + 'static {
    /// Stop the actor and block until its decode thread has fully stopped. Called
    /// exactly once, on a detachable helper thread the reaper spawns for it.
    fn shutdown(self: Box<Self>);
}

/// The product of a first-reference factory: the shared [`TileStore`] the decode
/// publishes into, plus the [`SourceActor`] the registry owns for teardown.
pub struct SourceInit<T> {
    store: Arc<TileStore<T>>,
    actor: Box<dyn SourceActor>,
}

impl<T> SourceInit<T> {
    /// Build the init from a shared store and an owned decode actor.
    #[must_use]
    pub fn new(store: Arc<TileStore<T>>, actor: impl SourceActor) -> Self {
        Self {
            store,
            actor: Box::new(actor),
        }
    }
}

/// A ref-counted handle to a shared source in the [`SourceRegistry`].
///
/// While held it keeps the entry (and its single decode) alive; dropping it
/// releases one reference, tearing the entry down when the **last** handle drops.
/// The handle exposes the shared [`TileStore`] for **lock-free sampling** — the
/// sample path never touches the registry lock, so a wedged/absent source can
/// never stall a sibling consumer (inv #10).
pub struct SourceHandle<T> {
    registry: Arc<SourceRegistry<T>>,
    key: SourceKey,
    store: Arc<TileStore<T>>,
}

impl<T> SourceHandle<T> {
    /// The shared per-source frame store. Sample it lock-free via
    /// [`TileStore::read_at`] — this never blocks and never touches the registry.
    #[must_use]
    pub fn store(&self) -> &Arc<TileStore<T>> {
        &self.store
    }

    /// The canonical key this handle references.
    #[must_use]
    pub fn key(&self) -> &SourceKey {
        &self.key
    }
}

/// Dropping a [`SourceHandle`] is **non-blocking on any thread** — safe inside a
/// Tokio async destructor (safety rule §4). It does two things, both non-blocking:
///
/// 1. `release` — a brief O(1) registry-lock update plus, on the last release of a
///    source that owns a decode actor, a wait-free channel hand-off to the reaper.
///    The blocking decode-thread join runs on a reaper helper thread, **never
///    here** (see the module "Teardown off the hot path" docs).
/// 2. The handle's own `Arc<TileStore<T>>` drops; when it is the last reference the
///    [`TileStore`] drops with any held frame `Arc<T>`. [`TileStore`] has **no**
///    explicit `Drop` — its frame slot is an `arc_swap::ArcSwapOption<T>` plus a
///    bounded `ArcSwap<Vec<..>>` ring, so teardown just drops the held `Arc<T>`s.
///
/// For the production `T = Nv12Image` that final frame drop is non-blocking. A
/// **source** frame — the only kind a *source* store ever holds, since the public
/// constructors leave the pool-return path `None` — drops as a no-op early return,
/// freeing its two plain `Vec<u8>` planes to the global allocator with **no device
/// call, no pool round-trip, and no thread join** (`Nv12Image::drop`, in
/// `multiview-compositor`). Only **pooled OUTPUT** frames carry a return path (via
/// the *private* `Nv12Image::new_pooled`, used exclusively by the CPU compositor and
/// therefore never present in a source store), and even that path is just two
/// uncontended one-slot `std::sync::Mutex` swaps. Either way the drop cannot block.
impl<T> Drop for SourceHandle<T> {
    fn drop(&mut self) {
        self.registry.release(&self.key);
    }
}

/// One registry entry: the shared store, the owned decode actor (taken on
/// teardown), the live-handle refcount, and the per-axis supremum requested size.
struct Entry<T> {
    store: Arc<TileStore<T>>,
    actor: Option<Box<dyn SourceActor>>,
    refcount: usize,
    supremum: RequestedSize,
}

/// A teardown message to the reaper thread.
enum Reap {
    /// Stop-and-join this actor (off every hot path).
    Actor(Box<dyn SourceActor>),
    /// Drain any remaining queued actors, then exit (explicit shutdown).
    Stop,
}

/// The process-global source registry: canonical [`SourceKey`] → one shared
/// decode + [`TileStore`], ref-counted per consumer.
///
/// Owned by the `ProgramSet` (process-global, above any single program's
/// `Pipeline`) so decode-once holds *across* programs. Construct with
/// [`SourceRegistry::new`]; share via the returned [`Arc`]. See the module docs for
/// the isolation and teardown guarantees.
pub struct SourceRegistry<T> {
    entries: Mutex<HashMap<SourceKey, Entry<T>>>,
    reaper_tx: Sender<Reap>,
    reaper_join: Mutex<Option<JoinHandle<()>>>,
    /// Count of source teardowns currently in flight (a blocking decode-thread
    /// join on a detachable helper thread). Shared with the reaper and each teardown
    /// helper thread so the isolation guarantee is observable via
    /// [`SourceRegistry::pending_teardowns`]. An [`Arc`] so a detached straggler
    /// thread can still decrement it after the registry itself is gone.
    pending_teardowns: Arc<AtomicUsize>,
}

impl<T> SourceRegistry<T> {
    /// Create an empty registry and spawn its reaper thread. Returns an [`Arc`] so
    /// handles can hold a reference for their release-on-drop.
    ///
    /// No `T` bound is needed here: the reaper channel carries erased
    /// [`SourceActor`]s (`Box<dyn SourceActor>`), never `T`, so the registry is
    /// generic over any payload. `SourceRegistry<T>` is `Send + Sync` (shareable
    /// across program threads) automatically when `T: Send + Sync`.
    #[must_use]
    pub fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Reap>();
        let pending = Arc::new(AtomicUsize::new(0));
        let pending_for_reaper = Arc::clone(&pending);
        // The reaper performs every blocking decode-thread join off the hot path.
        // If the thread cannot be spawned (never in practice), `rx` is dropped and
        // later teardowns fall back to dropping the actor (its non-blocking Drop) —
        // `release` stays infallible either way.
        let join = std::thread::Builder::new()
            .name("mv-source-reaper".to_owned())
            .spawn(move || reaper_loop(&rx, &pending_for_reaper))
            .ok();
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            reaper_tx: tx,
            reaper_join: Mutex::new(join),
            pending_teardowns: pending,
        })
    }

    /// Acquire a ref-counted handle to the source identified by `key`, decoding
    /// once and sharing thereafter.
    ///
    /// On the **first** reference to `key` the `factory` runs (given the `requested`
    /// size) to create the shared store + decode actor — this **fixes the decode
    /// size**. Every later reference to the same key skips the factory, bumps the
    /// refcount, grows the recorded supremum *metadata* to the per-axis max, and
    /// returns an [`Arc`] clone of the same store. Growing the supremum does **not**
    /// resize the live store/decode: the metadata tracks the max requested (for a
    /// future decode-ownership hoist), and callers that need the supremum acquire it
    /// up front. `factory` runs under the registry lock and MUST be non-blocking
    /// (spawn-and-return) and MUST NOT re-enter the registry.
    ///
    /// # Errors
    ///
    /// Propagates the `factory`'s error `E` when a first reference fails to create
    /// the source (e.g. the decode thread cannot be spawned); no entry is inserted.
    pub fn acquire<F, E>(
        self: &Arc<Self>,
        key: SourceKey,
        requested: RequestedSize,
        factory: F,
    ) -> Result<SourceHandle<T>, E>
    where
        F: FnOnce(RequestedSize) -> Result<SourceInit<T>, E>,
    {
        self.acquire_inner(key, requested, |req| {
            factory(req).map(|init| (init.store, Some(init.actor)))
        })
    }

    /// Acquire a ref-counted handle to a source whose decode teardown is owned
    /// **externally** — the store-only sibling of [`acquire`].
    ///
    /// This is the adoption seam for callers (e.g. the CLI `Pipeline`) that own the
    /// shared store + its sizing here but whose decode thread's stop/join still
    /// lives elsewhere (the run's `StopRegistry`) until the decode lifecycle is
    /// hoisted into the registry. At those callers' construction time the decode
    /// threads do not exist yet, so there is genuinely no actor to own: the entry
    /// registers with **no** [`SourceActor`], and last-release removes it without a
    /// reaper hand-off (nothing to join). Decode-once/use-many and per-axis
    /// supremum growth are identical to [`acquire`]; on the **first** reference the
    /// `factory` builds only the shared store, and every later reference shares an
    /// [`Arc`] clone of it. `factory` runs under the registry lock and MUST be
    /// non-blocking and MUST NOT re-enter the registry. When the decode lifecycle is
    /// later hoisted, callers move to [`acquire`] and pass the owning actor.
    ///
    /// # Errors
    ///
    /// Propagates the `factory`'s error `E` when a first reference fails to create
    /// the store; no entry is inserted.
    pub fn acquire_store<F, E>(
        self: &Arc<Self>,
        key: SourceKey,
        requested: RequestedSize,
        factory: F,
    ) -> Result<SourceHandle<T>, E>
    where
        F: FnOnce(RequestedSize) -> Result<Arc<TileStore<T>>, E>,
    {
        self.acquire_inner(key, requested, |req| {
            factory(req).map(|store| (store, None))
        })
    }

    /// Shared insert-or-bump for [`acquire`] and [`acquire_store`]. On the **first**
    /// reference to `key` the `factory` builds the shared store and (optionally) the
    /// decode actor **under the lock**, so two racing first-references cannot both
    /// spawn (decode-once, no TOCTOU). Every later reference bumps the refcount,
    /// grows the recorded supremum to the per-axis max, and returns an [`Arc`] clone
    /// of the one store.
    fn acquire_inner<F, E>(
        self: &Arc<Self>,
        key: SourceKey,
        requested: RequestedSize,
        factory: F,
    ) -> Result<SourceHandle<T>, E>
    where
        F: FnOnce(RequestedSize) -> Result<(Arc<TileStore<T>>, Option<Box<dyn SourceActor>>), E>,
    {
        let store = {
            let mut entries = lock(&self.entries);
            if let Some(entry) = entries.get_mut(&key) {
                entry.refcount = entry.refcount.saturating_add(1);
                entry.supremum = entry.supremum.supremum(requested);
                Arc::clone(&entry.store)
            } else {
                let (store, actor) = factory(requested)?;
                let shared = Arc::clone(&store);
                entries.insert(
                    key.clone(),
                    Entry {
                        store,
                        actor,
                        refcount: 1,
                        supremum: requested,
                    },
                );
                shared
            }
        };
        Ok(SourceHandle {
            registry: Arc::clone(self),
            key,
            store,
        })
    }

    /// Number of live (referenced) source entries. Test/telemetry accessor.
    #[must_use]
    pub fn active_len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// The number of source teardowns (blocking decode-thread joins) currently in
    /// flight. A telemetry/test observable of the isolation guarantee (inv #10): a
    /// single wedged `shutdown()` occupies exactly one teardown slot and never
    /// blocks the reaper's consume loop, so a sustained stream of healthy
    /// last-releases keeps this count **bounded** instead of growing an unbounded
    /// teardown backlog behind a stuck join.
    #[must_use]
    pub fn pending_teardowns(&self) -> usize {
        self.pending_teardowns.load(Ordering::Relaxed)
    }

    /// Whether a source with `key` is currently registered (has ≥ 1 reference).
    #[must_use]
    pub fn contains(&self, key: &SourceKey) -> bool {
        lock(&self.entries).contains_key(key)
    }

    /// The per-axis supremum requested size recorded for `key`, or [`None`] if no
    /// such source is registered. This is *metadata*: the per-axis max of all
    /// requests, **not** necessarily the live decode size — the decode is sized at
    /// the first [`acquire`](SourceRegistry::acquire), and a later, larger acquire
    /// grows this supremum without resizing the store (ADR-0030 §3).
    #[must_use]
    pub fn requested_supremum(&self, key: &SourceKey) -> Option<RequestedSize> {
        lock(&self.entries).get(key).map(|entry| entry.supremum)
    }

    /// The shared [`TileStore`] registered for `key`, or [`None`] if no such source
    /// is registered.
    ///
    /// A **lifecycle / telemetry accessor**: it clones the entry's [`Arc`] under the
    /// registry lock. It is **not** the sample path — consumers sample lock-free
    /// through the [`Arc`] a [`SourceHandle`] hands them ([`SourceHandle::store`]),
    /// never through the registry lock, so a wedged/absent source can never stall a
    /// sibling's sampling (inv #10).
    #[must_use]
    pub fn store(&self, key: &SourceKey) -> Option<Arc<TileStore<T>>> {
        lock(&self.entries)
            .get(key)
            .map(|entry| Arc::clone(&entry.store))
    }

    /// Drain and stop the reaper, bounded-grace-joining every in-flight decode
    /// teardown then **detaching** any straggler rather than blocking forever.
    ///
    /// Call from a **synchronous** teardown context **after** all handles have been
    /// released — never from a Tokio async destructor. Bounded: the reaper waits up
    /// to [`TEARDOWN_GRACE`] for the in-flight teardown helper threads to finish,
    /// then detaches whichever are still wedged (a stuck decode-thread join never
    /// hangs shutdown). Idempotent.
    pub fn shutdown(&self) {
        // Reaper may already have exited if the registry was torn down; a failed
        // send just means there is nothing left to drain (rule 37: intentional
        // discard of the send result).
        let _ = self.reaper_tx.send(Reap::Stop);
        let handle = lock(&self.reaper_join).take();
        if let Some(handle) = handle {
            // The one blocking join, in a synchronous context: the reaper drains
            // queued teardowns then exits. A panicked reaper yields `Err`; the
            // actors it held are released by unwinding — nothing actionable here
            // (rule 37: intentional discard of the join result).
            let _ = handle.join();
        }
    }

    /// Release one reference to `key`. When the **last** reference drops, the entry
    /// is removed and its decode actor is handed to the reaper via a wait-free,
    /// non-blocking channel send — the blocking stop-and-join runs off every hot
    /// path (inv #10 / safety rule §4). Called from [`SourceHandle`]'s `Drop`.
    fn release(&self, key: &SourceKey) {
        let actor = {
            let mut entries = lock(&self.entries);
            let remove = match entries.get_mut(key) {
                None => return,
                Some(entry) => {
                    entry.refcount = entry.refcount.saturating_sub(1);
                    entry.refcount == 0
                }
            };
            if remove {
                entries.remove(key).and_then(|entry| entry.actor)
            } else {
                None
            }
        };
        if let Some(actor) = actor {
            // Wait-free hand-off: the reaper performs the blocking join. If the
            // reaper is already gone (registry torn down), the returned actor is
            // dropped here — its `Drop` is contractually non-blocking (rule 37:
            // intentional discard on send failure).
            let _ = self.reaper_tx.send(Reap::Actor(actor));
        }
    }
}

impl<T> Drop for SourceRegistry<T> {
    fn drop(&mut self) {
        // Non-blocking: signal the reaper to stop and DETACH it. A registry `Drop`
        // may run in an async destructor, so we never join here — the explicit
        // `shutdown()` is the graceful, joining path. Dropping `reaper_tx` (with the
        // struct) also disconnects the channel, so the reaper exits even if this
        // Stop races (rule 37: intentional discard of the send result).
        let _ = self.reaper_tx.send(Reap::Stop);
        // The JoinHandle is dropped without a join, detaching the reaper thread.
    }
}

/// Total time the explicit [`SourceRegistry::shutdown`] waits for in-flight
/// teardowns to finish before **detaching** any straggler (a wedged decode-thread
/// join) rather than blocking forever. Generous enough for a healthy join to
/// complete on a contended host; bounded so a stuck join never hangs shutdown.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// Poll cadence while grace-joining in-flight teardown helper threads.
const TEARDOWN_POLL: Duration = Duration::from_millis(1);

/// The reaper thread body. Each [`Reap::Actor`] teardown (the blocking
/// stop-and-join of a decode thread) runs on its **own detachable helper thread**,
/// so a teardown wedged forever never blocks the reaper's consume loop nor grows
/// the teardown queue unboundedly behind it (inv #10). On [`Reap::Stop`] the reaper
/// drains any already-queued teardowns onto their own helper threads, then
/// [`grace_join`]s the in-flight set and detaches stragglers — so an explicit
/// [`SourceRegistry::shutdown`] joins healthy teardowns within [`TEARDOWN_GRACE`]
/// without ever blocking on a wedged one.
fn reaper_loop(rx: &Receiver<Reap>, pending: &Arc<AtomicUsize>) {
    let mut in_flight: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(msg) = rx.recv() {
        // Sweep finished helpers so the set tracks only genuinely-in-flight
        // teardowns. A finished handle is dropped (detached) — its thread has
        // already exited, so there is nothing to join and nothing leaks.
        in_flight.retain(|h| !h.is_finished());
        match msg {
            Reap::Actor(actor) => spawn_teardown(actor, pending, &mut in_flight),
            Reap::Stop => {
                // Drain queued teardowns onto helper threads (never inline — a
                // wedged one must not block the drain), then bounded-grace-join.
                while let Ok(Reap::Actor(actor)) = rx.try_recv() {
                    spawn_teardown(actor, pending, &mut in_flight);
                }
                grace_join(&mut in_flight);
                return;
            }
        }
    }
    // `recv` returned `Err`: every sender was dropped (the registry is gone). Detach
    // any in-flight teardowns (their helper threads own their actors and run to
    // completion); never block here — nothing is waiting on the reaper now.
}

/// Spawn a single source teardown (`actor.shutdown()` — the blocking decode-thread
/// join) onto its **own detachable helper thread**, so a wedged shutdown never
/// blocks the reaper's consume loop (inv #10). Bumps `pending` (the observable)
/// before spawning; the helper thread clears it on completion. The handle is
/// tracked in `in_flight` for the shutdown-path grace-join.
///
/// If the helper thread cannot be spawned (thread/resource exhaustion — never in
/// practice), [`std::thread::Builder::spawn`] drops the closure and with it the
/// boxed actor, so the actor's contractually **non-blocking** `Drop`
/// (signal-and-detach) runs instead of a blocking inline join — the reaper still
/// never blocks. The optimistic `pending` bump is then recovered.
fn spawn_teardown(
    actor: Box<dyn SourceActor>,
    pending: &Arc<AtomicUsize>,
    in_flight: &mut Vec<JoinHandle<()>>,
) {
    pending.fetch_add(1, Ordering::Relaxed);
    let pending_for_thread = Arc::clone(pending);
    let spawned = std::thread::Builder::new()
        .name("mv-source-teardown".to_owned())
        .spawn(move || {
            actor.shutdown();
            pending_for_thread.fetch_sub(1, Ordering::Relaxed);
        });
    match spawned {
        Ok(handle) => in_flight.push(handle),
        Err(_) => {
            // Recover the optimistic bump; `spawn` already dropped `actor` (running
            // its non-blocking Drop) when it dropped the closure on failure.
            pending.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Wait up to [`TEARDOWN_GRACE`] for the in-flight teardown helper threads to
/// finish, then **detach** any straggler (drop its handle) rather than blocking
/// forever on a wedged decode-thread join. Called only from the reaper's `Stop`
/// path (the explicit, synchronous [`SourceRegistry::shutdown`]).
fn grace_join(in_flight: &mut Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + TEARDOWN_GRACE;
    loop {
        in_flight.retain(|h| !h.is_finished());
        if in_flight.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            // Detach stragglers: dropping the handles leaves the wedged teardown
            // threads running (they own their actors) — shutdown never blocks.
            in_flight.clear();
            return;
        }
        std::thread::sleep(TEARDOWN_POLL);
    }
}

/// Lock a mutex, recovering the guard if a previous holder panicked. The registry
/// never leaves its map torn across a panic (every mutation is a simple map/counter
/// update), so recovering the poisoned guard is safe and keeps the lifecycle path
/// free of `unwrap`/`expect`/`panic` (rule 17).
fn lock<U>(m: &Mutex<U>) -> MutexGuard<'_, U> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::{RequestedSize, SourceActor, SourceInit, SourceKey, SourceRegistry};
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use multiview_core::time::MediaTime;
    use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    /// A test ingest/decode actor. Its `shutdown` (the reaper's blocking join)
    /// optionally spins on `gate` first — simulating a decode-thread join wedged
    /// forever — then records completion by bumping `completed`.
    struct TestActor {
        completed: Arc<AtomicUsize>,
        /// When `Some`, `shutdown` blocks until the gate is cleared (a stuck join).
        gate: Option<Arc<AtomicBool>>,
    }

    impl SourceActor for TestActor {
        fn shutdown(self: Box<Self>) {
            if let Some(gate) = &self.gate {
                while gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            self.completed.fetch_add(1, Ordering::Release);
        }
    }

    /// Clears a wedge gate on drop, so a panicking assertion (a RED run) still lets
    /// the wedged teardown thread exit instead of leaking it for the rest of the
    /// test binary.
    struct ClearGateOnDrop(Arc<AtomicBool>);
    impl Drop for ClearGateOnDrop {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    /// A store payload whose `Drop` bumps a counter — proves the store's held frame
    /// is dropped synchronously on the releasing thread, non-blocking (F3).
    struct CountedDrop(Arc<AtomicUsize>);
    impl Drop for CountedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    fn store(id: &str) -> Arc<TileStore<u64>> {
        Arc::new(TileStore::new(
            id,
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ))
    }

    fn size(width: u32, height: u32) -> RequestedSize {
        RequestedSize { width, height }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, within: Duration, what: &str) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < within, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn reaper_keeps_draining_under_a_wedged_teardown() {
        // F1 CHAOS (inv #10): one source teardown wedged FOREVER in `shutdown()`
        // must not stop the reaper draining a SUSTAINED stream of healthy
        // last-releases, and `pending_teardowns()` must stay BOUNDED — a stuck join
        // must never grow an unbounded teardown backlog behind it. A reaper that
        // joins inline FAILS this: the wedged join blocks the consume loop, so no
        // healthy teardown behind it ever runs.
        const HEALTHY: usize = 1000;
        const PENDING_BOUND: usize = 256;
        let reg = SourceRegistry::<u64>::new();

        // A wedged teardown: its `shutdown` spins on the gate until the test clears it.
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let wedged_done = Arc::new(AtomicUsize::new(0));
        let hw = reg
            .acquire(
                SourceKey::from_canonical("rtsp://wedged"),
                size(1920, 1080),
                {
                    let completed = wedged_done.clone();
                    let gate = gate.clone();
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(
                            store("wedged"),
                            TestActor {
                                completed,
                                gate: Some(gate),
                            },
                        ))
                    }
                },
            )
            .unwrap();
        // Reap the wedged actor FIRST (the mpsc is FIFO): the reaper picks it up
        // before any healthy teardown in the stream.
        drop(hw);

        // A SUSTAINED stream of healthy last-releases (distinct keys → each a
        // last-release handed to the reaper). Sample `pending_teardowns()` as we go.
        let healthy_done = Arc::new(AtomicUsize::new(0));
        let mut max_pending = 0;
        for i in 0..HEALTHY {
            let completed = healthy_done.clone();
            let h = reg
                .acquire(
                    SourceKey::from_canonical(format!("rtsp://healthy/{i}")),
                    size(320, 180),
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(
                            store("healthy"),
                            TestActor {
                                completed,
                                gate: None,
                            },
                        ))
                    },
                )
                .unwrap();
            drop(h);
            max_pending = max_pending.max(reg.pending_teardowns());
        }

        // The reaper must keep draining despite the wedged teardown: every healthy
        // teardown eventually runs. (An inline-join reaper never gets here → RED.)
        wait_until(
            || healthy_done.load(Ordering::Acquire) >= HEALTHY,
            Duration::from_secs(10),
            "all healthy teardowns complete while one teardown is wedged forever",
        );
        // In-flight teardowns stay bounded — never scaling with the stream length.
        assert!(
            max_pending <= PENDING_BOUND,
            "pending_teardowns must stay bounded under a wedged straggler \
             (saw {max_pending}, bound {PENDING_BOUND}, stream {HEALTHY})"
        );
        assert_eq!(
            wedged_done.load(Ordering::Acquire),
            0,
            "the wedged teardown must still be stuck (running off the consume loop)"
        );

        // Release the wedge and tidy up.
        gate.store(false, Ordering::Release);
        reg.shutdown();
    }

    #[test]
    fn shutdown_grace_joins_then_detaches_a_wedged_teardown() {
        // F1: the explicit `shutdown()` (Stop path) must bounded-grace-join
        // in-flight teardowns then DETACH stragglers — it must NOT block forever on
        // a wedged decode-thread join. An inline-join reaper makes `shutdown()`
        // block on the reaper stuck in the wedged join → RED (never returns).
        let reg = SourceRegistry::<u64>::new();
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let done = Arc::new(AtomicUsize::new(0));
        let hw = reg
            .acquire(
                SourceKey::from_canonical("rtsp://wedged-shutdown"),
                size(1, 1),
                {
                    let completed = done.clone();
                    let gate = gate.clone();
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(
                            store("wedged-shutdown"),
                            TestActor {
                                completed,
                                gate: Some(gate),
                            },
                        ))
                    }
                },
            )
            .unwrap();
        drop(hw); // last-release → wedged teardown handed to the reaper

        // Run shutdown() on a helper thread so a (buggy) forever-block cannot hang
        // the test; assert it RETURNS within a bounded budget.
        let returned = Arc::new(AtomicBool::new(false));
        let reg_bg = Arc::clone(&reg);
        let returned_bg = returned.clone();
        let joiner = std::thread::spawn(move || {
            reg_bg.shutdown();
            returned_bg.store(true, Ordering::Release);
        });
        wait_until(
            || returned.load(Ordering::Acquire),
            Duration::from_secs(10),
            "shutdown() must bounded-grace-join then detach a wedged teardown, not block forever",
        );
        // The wedged teardown was detached, never joined — it never completed.
        assert_eq!(
            done.load(Ordering::Acquire),
            0,
            "the wedged teardown is detached, not joined to completion"
        );

        gate.store(false, Ordering::Release); // let the detached straggler exit
        let _ = joiner.join();
    }

    #[test]
    fn later_larger_acquire_grows_supremum_metadata_but_not_decode_size() {
        // F2 characterization (rule 27): the store/decode is sized at the FIRST
        // acquire; a later, larger acquire grows the recorded supremum METADATA but
        // does NOT re-run the factory or resize the live store. (MP-2's
        // `Pipeline::build` acquires the full per-axis supremum up front, so this
        // fixed-size behaviour is correct in real usage — this test pins the
        // contract the module/`acquire`/ADR-0030 §3 docs now state.)
        let reg = SourceRegistry::<u64>::new();
        let key = SourceKey::from_canonical("rtsp://cam-fixed-size");
        let factory_sizes: Arc<Mutex<Vec<RequestedSize>>> = Arc::new(Mutex::new(Vec::new()));

        let first = {
            let sizes = factory_sizes.clone();
            move |req: RequestedSize| {
                sizes.lock().expect("poisoned").push(req);
                Ok::<_, Infallible>(SourceInit::new(
                    store("cam-fixed"),
                    TestActor {
                        completed: Arc::new(AtomicUsize::new(0)),
                        gate: None,
                    },
                ))
            }
        };
        let h1 = reg.acquire(key.clone(), size(640, 360), first).unwrap();

        // A later, BIGGER acquire must NOT re-run the factory (decode-once) and must
        // not resize the store — it only grows the supremum metadata.
        let h2 = reg
            .acquire(
                key.clone(),
                size(1920, 1080),
                |_r| -> Result<SourceInit<u64>, Infallible> {
                    panic!(
                        "a later acquire must NOT re-run the factory \
                         (decode size is fixed at first acquire)"
                    )
                },
            )
            .unwrap();

        // The recorded supremum METADATA grew to the larger per-axis request...
        assert_eq!(
            reg.requested_supremum(&key),
            Some(size(1920, 1080)),
            "the recorded supremum metadata grows to the per-axis max"
        );
        // ...but the factory ran exactly ONCE, with the FIRST (smaller) size: the
        // live decode is sized at first acquire, not resized by the larger acquire.
        assert_eq!(
            *factory_sizes.lock().expect("poisoned"),
            vec![size(640, 360)],
            "the decode is sized at the first acquire only; the larger acquire does not resize it"
        );
        // Both references share the ONE store — never rebuilt at the larger size.
        assert!(
            Arc::ptr_eq(h1.store(), h2.store()),
            "the store is shared, never rebuilt at the larger supremum"
        );

        drop((h1, h2));
        reg.shutdown();
    }

    #[test]
    fn dropping_a_handle_tears_down_the_store_without_blocking() {
        // F3 (safety §4): SourceHandle::drop → release() (a brief O(1) registry lock
        // + a wait-free reaper hand-off; the blocking decode join runs off-thread) →
        // then the handle's own Arc<TileStore<T>> drops. On a store-only last-release
        // (no actor) that final Arc drop tears down the TileStore — and the frame T
        // it holds — ON THE CALLING THREAD. That store/frame Drop must be
        // non-blocking (no device call, no pool round-trip, no thread join) so a
        // handle drop is safe on any thread, including a Tokio async destructor. This
        // pins that the teardown runs synchronously here and neither blocks nor panics.
        let reg = SourceRegistry::<CountedDrop>::new();
        let key = SourceKey::from_canonical("rtsp://drop-me");
        let drops = Arc::new(AtomicUsize::new(0));

        let h = reg
            .acquire_store(key.clone(), size(2, 2), |_r| {
                Ok::<_, Infallible>(Arc::new(TileStore::new(
                    "drop-me",
                    TileThresholds::default(),
                    NoSignalPolicy::HoldForever,
                )))
            })
            .unwrap();
        // Publish a Drop-counting frame into the store; tearing the store down drops it.
        h.store()
            .publish(CountedDrop(drops.clone()), MediaTime::from_nanos(0));
        assert_eq!(reg.active_len(), 1);

        let start = Instant::now();
        drop(h); // last release (no actor): entry removed + store Arc dropped here.
        let elapsed = start.elapsed();

        assert!(
            !reg.contains(&key),
            "the store-only entry is removed on last release"
        );
        assert!(
            elapsed < Duration::from_millis(100),
            "handle drop (store + frame teardown) must be non-blocking (took {elapsed:?})"
        );
        assert!(
            drops.load(Ordering::Acquire) >= 1,
            "the store's held frame was dropped synchronously on the calling thread"
        );
        reg.shutdown();
    }
}
