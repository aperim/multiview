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
//! ## Bounded teardown off the hot path (safety rule §4 / inv #10)
//!
//! When the **last** reference to a source is released the entry is removed and its
//! decode actor is torn down **off every hot path** by a fixed-size **teardown pool**:
//! [`TEARDOWN_WORKERS`] worker threads draining a **bounded** queue of depth
//! [`TEARDOWN_QUEUE_DEPTH`]. The releasing `Drop` only *offers* the actor with a
//! non-blocking, bounded `try_send` — it never blocks and never spawns a thread. A
//! worker runs the blocking `shutdown()` (the decode-thread stop-and-join), so a join
//! that wedges forever ties up **at most one worker** and never stalls the releasing
//! thread nor its siblings.
//!
//! Because the worker count and the queue depth are **fixed** *and* every last-release
//! **reserves** its slot with a bounded CAS before offering, the teardown resource is
//! bounded no matter how many last-releases arrive concurrently or how many teardowns wedge:
//! the observable [`pending_teardowns`](SourceRegistry::pending_teardowns) — reserved (queued
//! **plus** in-flight) — can never exceed `TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS` (a shed at
//! the ceiling never increments it). This replaces an unbounded queue *and* unbounded threads
//! with a fixed pool (inv #10). When the queue is full (every worker busy or wedged) or the
//! pool is at its bound, the release **sheds**: it structurally signals the decode to
//! terminate ([`request_stop`](SourceActor::request_stop)) then drops the boxed actor instead
//! of enqueuing it — a wait-free, thread-free bound, never a blocking join inside a Tokio
//! async destructor. The explicit [`SourceRegistry::shutdown`] (synchronous teardown context)
//! disconnects the queue, **bounded-grace-joins** the workers for [`TEARDOWN_GRACE`], then
//! **detaches** any still wedged rather than blocking forever.
//!
//! ## Scope: the pool is bounded now; a real decode actor's promptness is forward
//!
//! The bounded pool above is the complete, tested isolation fix, and it holds for *any*
//! number of actors: the **pool** is bounded ≤ D+K structurally (the reservation ceiling),
//! and a shed **structurally** signals decode termination
//! ([`request_stop`](SourceActor::request_stop) — a *required* trait method the pool calls)
//! then detaches. That is present, tested behavior, exercised by the thread-owning
//! `ThreadedProbe` unit test (a shed terminates its real decode thread via `request_stop`).
//!
//! What is **not** present yet is a *real* decode actor: in MP-2 production
//! (`Pipeline::build`) uses [`acquire_store`](SourceRegistry::acquire_store), whose entry
//! carries **no** [`SourceActor`] (`actor: None`) — decode stop/join still lives in the run's
//! external `StopRegistry` — so only tests inject actors (via
//! [`acquire`](SourceRegistry::acquire)). When decode ownership is hoisted into the registry
//! (a later milestone), the real actor must make `request_stop` **promptly** wake/interrupt
//! its decode I/O so the detached thread exits without delay — that bounded-I/O promptness is
//! the implementor's forward contract (the pool cannot force-kill a wedged OS thread; Rust has
//! no such primitive). The honest bound: the teardown **pool** is bounded ≤ D+K
//! unconditionally; a shed decode-thread's exit is prompt to the extent the implementor makes
//! its I/O interruptible.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
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
/// Teardown has two paths, both keeping the engine's teardown resource **bounded**
/// (inv #10 / safety rule §4):
///
/// * **Graceful** — [`shutdown`](SourceActor::shutdown) stops the actor and blocks
///   until its decode thread has fully stopped (a join). It is **blocking** and runs
///   **only on a fixed-size teardown-pool worker thread**, never on a program's hot
///   path and never inside a Tokio async destructor. A last-release `Drop` only
///   *offers* the boxed actor to the pool via a non-blocking, bounded `try_send`.
/// * **Shed / fallback** — when the bounded teardown queue is full (every worker is busy or
///   wedged) or the pool is at its `TEARDOWN_CAPACITY` bound, the actor is **shed instead of
///   shut down**, keeping the teardown resource bounded (inv #10). The pool **structurally**
///   signals the decode to terminate first — it calls
///   [`request_stop`](SourceActor::request_stop) (non-blocking) then detaches by dropping the
///   actor. That is "signal-and-detach", not merely "detach": `request_stop` is a **required**
///   trait method, so every implementor is compile-forced to provide a wake-decode signal —
///   the guarantee is structural, not a `Drop` convention.
///
///   What remains the implementor's **forward** responsibility is *promptness*: a real decode
///   actor must make `request_stop` actually wake/interrupt its decode I/O so the detached
///   thread exits without delay (its bounded-I/O contract). The pool cannot force-kill a
///   wedged OS thread — Rust has no such primitive — so the honest bound is: the teardown
///   **pool** is bounded ≤ D+K unconditionally; a shed decode-thread's exit is as prompt as
///   the implementor's I/O is interruptible. In MP-2 no real actor is wired yet (production's
///   store-only [`acquire_store`](SourceRegistry::acquire_store) path carries no actor; the
///   only implementors are test doubles, incl. the thread-owning `ThreadedProbe` that proves
///   a shed terminates its thread via `request_stop`).
///
/// The teardown pool calls [`request_stop`](SourceActor::request_stop) FIRST on **both**
/// paths — before the graceful `shutdown` (so decode is already winding down even if the join
/// wedges) and before a shed's detach — so an idempotent `request_stop` is all an implementor
/// needs to make both paths signal decode termination. `request_stop`, the actor's `Drop`,
/// and (ideally) `shutdown` must **not panic**: the worker's `catch_unwind` keeps the fixed
/// pool alive through a *single* panic, but a panic in the actor's `Drop` while `shutdown` is
/// already unwinding is a *double* panic that aborts — a Rust fundamental no `catch_unwind`
/// can prevent, and one the trait contract forbids.
pub trait SourceActor: Send + 'static {
    /// Signal the actor to begin terminating its decode — **non-blocking**, safe on any
    /// thread (including a Tokio async destructor). Sets the stop flag / closes the command
    /// channel the decode loop observes and returns at once; it does **not** wait for the
    /// decode thread to stop (that is [`shutdown`](SourceActor::shutdown)'s job on the
    /// graceful path, or the thread winding down on its own after a shed). Idempotent —
    /// safe to call more than once and before [`shutdown`](SourceActor::shutdown).
    ///
    /// This is the **structural** wake-decode signal the teardown pool calls before both a
    /// graceful `shutdown` and a shed, so every implementor must provide one (inv #10).
    fn request_stop(&self);

    /// Stop the actor and block until its decode thread has fully stopped. Called at
    /// most once, on a fixed-size teardown-pool worker thread — never on a hot path
    /// and never inside a Tokio async destructor (safety rule §4).
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
///    source that owns a decode actor, a non-blocking, bounded `try_send` *offer* of
///    the actor to the teardown pool (or, when the queue is full, an immediate
///    signal-and-detach shed). The blocking decode-thread join runs on a pool worker,
///    **never here** (see the module "Bounded teardown off the hot path" docs).
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

/// The process-global source registry: canonical [`SourceKey`] → one shared
/// decode + [`TileStore`], ref-counted per consumer.
///
/// Owned by the `ProgramSet` (process-global, above any single program's
/// `Pipeline`) so decode-once holds *across* programs. Construct with
/// [`SourceRegistry::new`]; share via the returned [`Arc`]. See the module docs for
/// the isolation and teardown guarantees.
pub struct SourceRegistry<T> {
    entries: Mutex<HashMap<SourceKey, Entry<T>>>,
    /// The bounded teardown queue feeding the fixed worker pool. A non-blocking
    /// `try_send` *offers* a teardown; a full queue sheds via the actor's `Drop`
    /// (signal-and-detach). Wrapped in `Option` so [`SourceRegistry::shutdown`] and
    /// `Drop` can **take** it — dropping the sender disconnects the queue so the
    /// workers drain what is buffered then exit.
    teardown_tx: Mutex<Option<SyncSender<Teardown>>>,
    /// Join handles for the fixed pool of teardown worker threads, for the explicit
    /// [`SourceRegistry::shutdown`] bounded-grace-join.
    teardown_workers: Mutex<Vec<JoinHandle<()>>>,
    /// Reserved teardown slots — queued **plus** in-flight — **hard-bounded** at
    /// `TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS` by a bounded-CAS reservation
    /// ([`reserve_slot`]): a last-release at the ceiling sheds WITHOUT counting, so no number
    /// of concurrent last-releases can push this past the bound (inv #10). Reserved when a
    /// teardown is offered and decremented (exactly once, panic-safe) when it completes or is
    /// shed — see [`Teardown`]. Observable via [`SourceRegistry::pending_teardowns`]. An
    /// [`Arc`] so a detached straggler's guard can still decrement it after the registry
    /// itself is gone.
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
        let (tx, rx) = mpsc::sync_channel::<Teardown>(TEARDOWN_QUEUE_DEPTH);
        // One receiver shared by the fixed worker pool. A worker holds this lock only
        // to `recv` the next job — never while running a (possibly wedged) shutdown —
        // so a wedged teardown ties up its worker but never the receiver.
        let rx = Arc::new(Mutex::new(rx));
        let pending = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::with_capacity(TEARDOWN_WORKERS);
        for i in 0..TEARDOWN_WORKERS {
            let rx = Arc::clone(&rx);
            // If a worker cannot be spawned (never in practice) the pool is just
            // smaller; a full queue then sheds via the actor's non-blocking Drop, so
            // teardown stays bounded and `release` stays infallible either way.
            if let Ok(handle) = std::thread::Builder::new()
                .name(format!("mv-source-teardown-{i}"))
                .spawn(move || teardown_worker(&rx))
            {
                workers.push(handle);
            }
        }
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            teardown_tx: Mutex::new(Some(tx)),
            teardown_workers: Mutex::new(workers),
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

    /// The number of source teardowns currently **reserved** (queued plus in flight) — a
    /// telemetry/test observable of the isolation guarantee (inv #10). A last-release reserves
    /// a slot with a bounded CAS ([`reserve_slot`]) capped at `TEARDOWN_QUEUE_DEPTH +
    /// TEARDOWN_WORKERS`; at the cap it sheds without reserving. So this count can **never
    /// exceed `TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS`** — by construction, however many
    /// last-releases arrive concurrently or how many teardowns wedge. A wedged `shutdown()`
    /// occupies exactly one worker; it never grows the queue behind it without bound.
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

    /// Stop the teardown pool: disconnect the queue so the workers drain what is
    /// buffered then exit, **bounded-grace-join** them for [`TEARDOWN_GRACE`], then
    /// **detach** any still wedged rather than blocking forever.
    ///
    /// Call from a **synchronous** teardown context **after** all handles have been
    /// released — never from a Tokio async destructor. Bounded: a stuck decode-thread
    /// join ties up its worker but never hangs shutdown (it is detached past the
    /// grace). Idempotent — a second call finds the queue already disconnected and no
    /// workers to join.
    pub fn shutdown(&self) {
        // Take + drop the queue sender: once every sender (this one and any transient
        // `release` clone) is gone, `recv` returns `Err` and idle workers exit. Any teardowns
        // still buffered drop with the channel — each Teardown::drop structurally signals its
        // decode to terminate (request_stop) then detaches, non-blocking.
        drop(lock(&self.teardown_tx).take());
        let workers = std::mem::take(&mut *lock(&self.teardown_workers));
        grace_join(workers);
    }

    /// Release one reference to `key`. When the **last** reference drops, the entry
    /// is removed and its decode actor is **offered** to the bounded teardown pool —
    /// the blocking stop-and-join runs on a pool worker, off every hot path (inv #10 /
    /// safety rule §4). Called from [`SourceHandle`]'s `Drop`; non-blocking on any
    /// thread. If the queue is full or the pool is gone the offer sheds (see
    /// [`offer_teardown`](SourceRegistry::offer_teardown)).
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
            self.offer_teardown(actor);
        }
    }

    /// Offer a decode actor to the bounded teardown pool. First **reserve** a bounded
    /// pending slot ([`reserve_slot`]); at the `TEARDOWN_CAPACITY` (D+K) ceiling the offer
    /// **sheds immediately without counting**, so N concurrent last-releases can never spike
    /// the observable past the bound (inv #10). Within the bound, a non-blocking `try_send`
    /// hands the actor to a worker that runs the blocking `shutdown()` off every hot path; if
    /// the queue is full or the pool is gone the reserved slot is released and the actor is
    /// shed. Every shed **structurally signals decode termination**
    /// ([`request_stop`](SourceActor::request_stop)) then detaches — a wait-free, thread-free
    /// shed. Never blocks; safe on any thread. (Today only test doubles are ever enqueued —
    /// production uses `acquire_store`, `actor: None`.)
    fn offer_teardown(&self, actor: Box<dyn SourceActor>) {
        // Reserve BEFORE touching the queue: only a successful reservation is ever counted,
        // and the reservation is bounded at D+K, so a shed never increments the observable
        // (finding 1 — no increment-before-try_send overshoot under concurrent releasers).
        let teardown = match Teardown::reserve(actor, &self.pending_teardowns) {
            Ok(teardown) => teardown,
            Err(actor) => return signal_and_detach(actor),
        };
        let sender = lock(&self.teardown_tx).clone();
        match sender {
            Some(tx) => {
                // A full channel or gone pool releases the reserved slot (Teardown::drop)
                // and sheds the actor (signal-and-detach) — never blocks, never over-counts.
                if let Err(TrySendError::Full(t) | TrySendError::Disconnected(t)) =
                    tx.try_send(teardown)
                {
                    drop(t);
                }
            }
            None => drop(teardown),
        }
    }
}

impl<T> Drop for SourceRegistry<T> {
    fn drop(&mut self) {
        // Non-blocking: disconnect the queue (take + drop the sender) so the workers
        // drain what is buffered then exit, and DETACH the worker threads (their join
        // handles drop with the struct — no join). A registry `Drop` may run in an
        // async destructor, so we never join here; the explicit `shutdown()` is the
        // graceful, bounded-grace-joining path. Any teardowns still buffered drop with the
        // channel — each Teardown::drop structurally signals its decode to terminate
        // (request_stop) then detaches, non-blocking.
        drop(lock(&self.teardown_tx).take());
    }
}

/// Number of fixed teardown-pool worker threads. Small: teardowns (a source
/// last-release) are rare, and the goal is a **bounded** resource, not throughput.
/// More than one so a single wedged decode-thread join cannot stall the whole pool —
/// a sibling worker keeps draining while one is stuck.
const TEARDOWN_WORKERS: usize = 2;

/// Depth of the bounded queue feeding the teardown pool. Buffers a burst of
/// simultaneous last-releases (e.g. a reconfiguration dropping many sources) waiting
/// for a worker; anything beyond `TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS` in flight
/// sheds via the actor's non-blocking `Drop`. Fixed, so the teardown resource is
/// bounded.
const TEARDOWN_QUEUE_DEPTH: usize = 32;

/// The hard ceiling on the [`pending_teardowns`](SourceRegistry::pending_teardowns)
/// observable: the bounded queue depth plus the fixed worker pool (D+K). A last-release
/// reserves one of these slots ([`reserve_slot`]) before offering; at the ceiling it sheds
/// without counting, so the observable can NEVER exceed this bound however many last-releases
/// contend concurrently or how many teardowns wedge (inv #10).
const TEARDOWN_CAPACITY: usize = TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS;

/// Total time the explicit [`SourceRegistry::shutdown`] waits for the teardown
/// workers to finish before **detaching** any straggler (a wedged decode-thread join)
/// rather than blocking forever. Generous enough for a healthy join to complete on a
/// contended host; bounded so a stuck join never hangs shutdown.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// Poll cadence while grace-joining the teardown workers on shutdown.
const TEARDOWN_POLL: Duration = Duration::from_millis(1);

/// A single reserved source teardown: the owned decode actor plus an RAII guard on the
/// [`SourceRegistry::pending_teardowns`] observable.
///
/// Constructed via a **bounded reservation** ([`Teardown::reserve`]) that increments the
/// observable only within the `TEARDOWN_CAPACITY` bound; its `Drop` decrements it exactly
/// once — so the count is correct whether the teardown completes on a worker, is shed on a
/// full queue, or **unwinds because `shutdown()` panicked** (panic-safe). If the actor is
/// still present when the guard drops (the shed / buffer-drop path — [`run`](Teardown::run)
/// was never called) it is **signalled to terminate** structurally
/// ([`request_stop`](SourceActor::request_stop)) then detached, non-blockingly.
struct Teardown {
    actor: Option<Box<dyn SourceActor>>,
    pending: Arc<AtomicUsize>,
}

impl Teardown {
    /// Reserve a bounded pending-teardown slot and wrap `actor`. Returns the actor back
    /// (`Err`), leaving the observable untouched, when it is already at the
    /// `TEARDOWN_CAPACITY` (D+K) bound — so the caller sheds without ever counting and the
    /// observable can never exceed the bound (inv #10). On success the returned guard owns
    /// the matching decrement (its `Drop`).
    fn reserve(
        actor: Box<dyn SourceActor>,
        pending: &Arc<AtomicUsize>,
    ) -> Result<Self, Box<dyn SourceActor>> {
        if reserve_slot(pending) {
            Ok(Self {
                actor: Some(actor),
                pending: Arc::clone(pending),
            })
        } else {
            Err(actor)
        }
    }

    /// Run the graceful stop-and-join on a pool worker, consuming the actor (so its own
    /// `Drop` does not additionally fire — `shutdown` already joined). Signals decode
    /// termination structurally FIRST ([`request_stop`](SourceActor::request_stop)), so the
    /// decode is already winding down even if the blocking join then wedges. The observable
    /// is decremented when `self` drops at the end of this call, **including if
    /// `request_stop`/`shutdown` panics and unwinds through here**.
    fn run(mut self) {
        if let Some(actor) = self.actor.take() {
            actor.request_stop();
            actor.shutdown();
        }
    }
}

impl Drop for Teardown {
    fn drop(&mut self) {
        // Shed / buffer-drop path: if `run` never took the actor it is still here. Signal
        // its decode to terminate structurally then detach — NON-BLOCKING (never a join).
        // Then decrement the observable exactly once — panic-safe: this runs even if `run`'s
        // `shutdown()` unwound.
        if let Some(actor) = self.actor.take() {
            signal_and_detach(actor);
        }
        self.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A teardown-pool worker: pull the next [`Teardown`] from the shared bounded queue
/// and run its blocking `shutdown()` off every hot path.
///
/// The receiver lock is held **only** to `recv` the next job, never while running a
/// (possibly wedged) `shutdown()`, so a wedged teardown ties up this worker alone and the
/// sibling workers keep draining. The job runs under
/// [`catch_unwind`](std::panic::catch_unwind), and it is the **only** operation in the loop
/// that can panic — `recv` returns a `Result` and [`lock`] recovers a poisoned guard — so a
/// panicking `request_stop`/`shutdown`/actor-`Drop` cannot kill the worker or shrink the
/// fixed pool: the worker survives and keeps draining, and the [`Teardown`] guard still
/// decrements the observable on the unwind. (The one thing no `catch_unwind` can survive is a
/// *double* panic — an actor that panics in `Drop` while `shutdown` is already unwinding —
/// which aborts the process; the trait contract forbids a panicking `Drop`.) `recv` returning
/// `Err` (every sender dropped) means the pool is stopping — the worker exits.
fn teardown_worker(rx: &Arc<Mutex<Receiver<Teardown>>>) {
    loop {
        let job = lock(rx).recv();
        match job {
            // AssertUnwindSafe: on a panic the actor is consumed/dropped either way, so
            // no broken invariant is observed across the catch; the point is only to
            // keep this worker alive so the fixed pool does not shrink.
            Ok(teardown) => {
                let _ = std::panic::catch_unwind(AssertUnwindSafe(move || teardown.run()));
            }
            Err(_) => return,
        }
    }
}

/// Wait up to [`TEARDOWN_GRACE`] for the teardown workers to finish, then **detach**
/// any straggler (drop its handle) rather than blocking forever on a wedged
/// decode-thread join. Called only from the explicit, synchronous
/// [`SourceRegistry::shutdown`] after the queue has been disconnected.
fn grace_join(mut workers: Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + TEARDOWN_GRACE;
    loop {
        workers.retain(|h| !h.is_finished());
        if workers.is_empty() {
            return;
        }
        if Instant::now() >= deadline {
            // Detach stragglers: dropping the handles leaves any wedged worker running
            // (it owns its actor) — shutdown never blocks.
            return;
        }
        std::thread::sleep(TEARDOWN_POLL);
    }
}

/// Claim one of the [`TEARDOWN_CAPACITY`] (D+K) bounded pending-teardown slots, bumping the
/// observable. Returns `false` — leaving the observable untouched — when the bound is already
/// reached, so the caller sheds WITHOUT ever counting; the observable can therefore never
/// exceed the bound however many last-releases contend concurrently (inv #10). A bounded CAS
/// loop, not increment-then-undo, so a shed at the ceiling never even *transiently*
/// overshoots the bound.
fn reserve_slot(pending: &AtomicUsize) -> bool {
    let mut cur = pending.load(Ordering::Relaxed);
    loop {
        if cur >= TEARDOWN_CAPACITY {
            return false;
        }
        match pending.compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(observed) => cur = observed,
        }
    }
}

/// Shed an actor that will never reach a pool worker (the queue is full or gone, or the pool
/// is at its bound): structurally signal its decode to terminate
/// ([`request_stop`](SourceActor::request_stop)) then detach by dropping it. Non-blocking on
/// any thread — never a join. Promptness of the decode thread's actual exit is the
/// implementor's bounded-I/O contract (a real actor must make its I/O interruptible).
fn signal_and_detach(actor: Box<dyn SourceActor>) {
    actor.request_stop();
    drop(actor);
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
    use super::{
        RequestedSize, SourceActor, SourceInit, SourceKey, SourceRegistry, TEARDOWN_QUEUE_DEPTH,
        TEARDOWN_WORKERS,
    };
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
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
        // A counting double owns no decode thread, so the structural stop signal is a
        // no-op here; the thread-owning `ThreadedProbe` exercises request_stop's effect.
        fn request_stop(&self) {}
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

    /// Shared observability for a group of [`Probe`] teardown actors.
    #[derive(Clone)]
    struct Counters {
        /// Concurrent `shutdown()` executions right now (peak = teardown parallelism).
        active: Arc<AtomicUsize>,
        /// Total `shutdown()` entries (a fixed pool caps this under a wedge; a
        /// thread-per-teardown design lets it scale with the stream).
        shutdown_calls: Arc<AtomicUsize>,
        /// `shutdown()` calls that ran to completion.
        completed: Arc<AtomicUsize>,
        /// Actors whose `Drop` ran on the **shed** path (no `shutdown()`), proving the
        /// bounded-queue overflow still terminates decode non-blockingly.
        shed_terminated: Arc<AtomicUsize>,
    }

    impl Counters {
        fn new() -> Self {
            Self {
                active: Arc::new(AtomicUsize::new(0)),
                shutdown_calls: Arc::new(AtomicUsize::new(0)),
                completed: Arc::new(AtomicUsize::new(0)),
                shed_terminated: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// A fresh probe actor sharing these counters. `gate: Some(..)` wedges its
        /// `shutdown()` until the gate is cleared (a decode-thread join stuck forever).
        fn probe(&self, gate: Option<Arc<AtomicBool>>) -> Probe {
            Probe {
                gate,
                counters: self.clone(),
                shutdown_ran: AtomicBool::new(false),
            }
        }
    }

    /// A test ingest/decode actor with two observable teardown paths:
    ///
    /// * `shutdown()` (the graceful path a pool worker runs) — bumps `active` for its
    ///   duration, optionally wedges on `gate`, then records `completed`.
    /// * `Drop` (the shed / signal-and-detach path) — when it runs **without** a prior
    ///   `shutdown()` it records `shed_terminated`, standing in for "the decode was
    ///   signalled to terminate non-blockingly".
    struct Probe {
        gate: Option<Arc<AtomicBool>>,
        counters: Counters,
        shutdown_ran: AtomicBool,
    }

    impl SourceActor for Probe {
        // Counting double: no decode thread to signal (see `ThreadedProbe` for the
        // structural request_stop → thread-termination proof).
        fn request_stop(&self) {}
        fn shutdown(self: Box<Self>) {
            self.counters.shutdown_calls.fetch_add(1, Ordering::Release);
            self.counters.active.fetch_add(1, Ordering::Release);
            if let Some(gate) = &self.gate {
                while gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            self.counters.active.fetch_sub(1, Ordering::Release);
            self.shutdown_ran.store(true, Ordering::Release);
            self.counters.completed.fetch_add(1, Ordering::Release);
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            // Shed path only: if `shutdown()` never ran, this Drop is the sole teardown
            // — it must non-blockingly signal decode termination (recorded here).
            if !self.shutdown_ran.load(Ordering::Acquire) {
                self.counters
                    .shed_terminated
                    .fetch_add(1, Ordering::Release);
            }
        }
    }

    /// A test decode actor that owns a **real** "decode" thread — so the SHED path can be
    /// PROVEN to *terminate* that thread non-blockingly (inv #10), not merely counted.
    ///
    /// The decode thread bumps `live` on spawn and loops until its `stop` flag is set, then
    /// decrements `live` and exits. It exits **only** on the structural
    /// [`request_stop`](SourceActor::request_stop) signal — the actor's own `Drop` does
    /// **not** set `stop` and does **not** join. So a shed that fails to call `request_stop`
    /// LEAKS the thread and `live` never returns to zero. `shutdown` (the graceful path)
    /// sets `stop`, optionally wedges the worker on `gate`, then JOINs.
    struct ThreadedProbe {
        stop: Arc<AtomicBool>,
        /// When `Some`, `shutdown` wedges the pool worker until the gate clears (a stuck
        /// decode-thread join) — used to force the bounded queue to overflow and shed.
        gate: Option<Arc<AtomicBool>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl ThreadedProbe {
        fn new(
            stop: Arc<AtomicBool>,
            gate: Option<Arc<AtomicBool>>,
            live: Arc<AtomicUsize>,
        ) -> Self {
            live.fetch_add(1, Ordering::Release);
            let handle = {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    live.fetch_sub(1, Ordering::Release);
                })
            };
            Self {
                stop,
                gate,
                handle: Some(handle),
            }
        }
    }

    impl SourceActor for ThreadedProbe {
        fn request_stop(&self) {
            // The one path that stops the decode thread: a non-blocking flag store.
            self.stop.store(true, Ordering::Release);
        }
        fn shutdown(mut self: Box<Self>) {
            // Graceful: signal, optionally wedge the worker on the gate, then JOIN.
            self.stop.store(true, Ordering::Release);
            if let Some(gate) = &self.gate {
                while gate.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    impl Drop for ThreadedProbe {
        fn drop(&mut self) {
            // Shed / detach path: DELIBERATELY do NOT set `stop` and do NOT join. The
            // decode thread must be terminated by the structural request_stop() the teardown
            // calls before dropping us; it then exits on its own (non-blocking detach —
            // dropping `handle` detaches). If request_stop was never called the thread leaks
            // and `live` never hits zero, which is exactly what the RED run catches.
        }
    }

    /// A test decode actor whose graceful `shutdown()` PANICS — proving a panicking teardown
    /// cannot shrink the fixed worker pool (the worker's `catch_unwind` keeps it alive). Its
    /// `request_stop`/`Drop` do **not** panic (a double panic would abort — a Rust
    /// fundamental the trait contract forbids).
    struct PanicProbe;
    impl SourceActor for PanicProbe {
        fn request_stop(&self) {}
        fn shutdown(self: Box<Self>) {
            panic!("teardown shutdown() panics — the pool must survive and keep draining");
        }
    }

    /// Sets every `stop` flag on drop, so a [`ThreadedProbe`] thread never leaks past the
    /// test — even if an assertion panics on a RED run before the explicit cleanup.
    struct StopAllOnDrop(Vec<Arc<AtomicBool>>);
    impl Drop for StopAllOnDrop {
        fn drop(&mut self) {
            for s in &self.0 {
                s.store(true, Ordering::Release);
            }
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
    fn a_wedged_teardown_never_stalls_the_pool_drain() {
        // F1 (inv #10) LIVENESS: one teardown wedged FOREVER in `shutdown()` occupies a
        // single pool worker; the sibling worker must keep draining a SUSTAINED stream
        // of healthy last-releases. Under the bounded design every healthy teardown is
        // ACCOUNTED — run to completion by a free worker, or shed via `Drop`
        // (signal-and-detach) when the bounded queue is full — and none is lost.
        // `pending_teardowns()` stays bounded and the wedged teardown never completes.
        //
        // (This is the LIVENESS half of the old chaos test; the tight resource bound is
        // proven by teardown_pool_bounds_threads_and_queue_under_sustained_pressure.
        // The old assertion "every healthy teardown *runs* shutdown" encoded the earlier
        // *unbounded* design — the panel-mandated bounded design instead SHEDS the
        // overflow, so the contract is now "completed-or-shed", not "all run".)
        const HEALTHY: usize = 500;
        // The HARD ceiling: queued + in-flight can never exceed D+K, no matter how long the
        // healthy stream nor how a straggler wedges (finding 4 — tightened from a loose 64).
        const PENDING_BOUND: usize = TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS;
        let reg = SourceRegistry::<u64>::new();

        // A wedged teardown, offered FIRST (FIFO) so a worker picks it up before the
        // stream; its `shutdown` spins on the gate until the test clears it.
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let wedged = Counters::new();
        let hw = reg
            .acquire(
                SourceKey::from_canonical("rtsp://wedged"),
                size(1920, 1080),
                {
                    let w = wedged.clone();
                    let gate = gate.clone();
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(store("wedged"), w.probe(Some(gate))))
                    }
                },
            )
            .unwrap();
        drop(hw);
        // Ensure the wedged teardown actually occupies a worker before the stream.
        wait_until(
            || wedged.active.load(Ordering::Acquire) >= 1,
            Duration::from_secs(5),
            "the wedged teardown occupies a pool worker",
        );

        // A SUSTAINED stream of healthy last-releases (distinct keys). Sample
        // `pending_teardowns()` as we go.
        let healthy = Counters::new();
        let mut max_pending = 0;
        for i in 0..HEALTHY {
            let hc = healthy.clone();
            let h = reg
                .acquire(
                    SourceKey::from_canonical(format!("rtsp://healthy/{i}")),
                    size(320, 180),
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(store("healthy"), hc.probe(None)))
                    },
                )
                .unwrap();
            drop(h);
            max_pending = max_pending.max(reg.pending_teardowns());
        }

        // Every healthy teardown is accounted for despite the wedged worker: completed
        // by the free worker or shed via Drop. (A design where the wedge stalls the
        // whole drain never reaches HEALTHY → times out.)
        wait_until(
            || {
                healthy.completed.load(Ordering::Acquire)
                    + healthy.shed_terminated.load(Ordering::Acquire)
                    >= HEALTHY
            },
            Duration::from_secs(10),
            "every healthy teardown is completed-or-shed while one teardown is wedged forever",
        );
        // The sibling worker actually drained some gracefully — the wedge did not stall
        // the pool.
        assert!(
            healthy.completed.load(Ordering::Acquire) >= 1,
            "the sibling worker must keep draining while one teardown is wedged"
        );
        // In-flight + queued teardowns stay bounded — never scaling with stream length.
        assert!(
            max_pending <= PENDING_BOUND,
            "pending_teardowns must stay bounded under a wedged straggler \
             (saw {max_pending}, bound {PENDING_BOUND}, stream {HEALTHY})"
        );
        assert_eq!(
            wedged.completed.load(Ordering::Acquire),
            0,
            "the wedged teardown must still be stuck, off the drain path"
        );

        // Release the wedge and tidy up.
        gate.store(false, Ordering::Release);
        reg.shutdown();
    }

    #[test]
    fn teardown_pool_bounds_threads_and_queue_under_sustained_pressure() {
        // F1 REWORK (inv #10): the teardown mechanism must be BOUNDED under SUSTAINED
        // last-release pressure — not merely "eventually completes one finite burst".
        //
        // Every actor's `shutdown()` blocks on one shared gate, so any teardown that
        // reaches a worker piles up in-flight and stays there. A FIXED worker pool
        // draining a BOUNDED queue therefore caps (a) concurrent `shutdown()`
        // executions at the pool size and (b) queued+in-flight teardowns at
        // queue+workers; every release beyond that SHEDS via the actor's `Drop`
        // (signal-and-detach — no thread spawned, no join), which must still terminate
        // decode. A thread-per-teardown design FAILS all three: it spawns one thread
        // per release and calls `shutdown()` on every one, so concurrent shutdowns and
        // pending both climb to the full stream length and nothing ever sheds.
        // The ACTUAL bounds (finding 3 — tightened from loose 16/64): a fixed pool caps
        // concurrent shutdowns at the worker count, queued+in-flight at D+K; every release
        // beyond D+K sheds. A regression that lets either climb now FAILS.
        const N: usize = 96; // >> queue+workers, so the overflow must shed
        const ACTIVE_BOUND: usize = TEARDOWN_WORKERS;
        const PENDING_BOUND: usize = TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS;
        const MIN_SHED: usize = N - (TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS);

        let reg = SourceRegistry::<u64>::new();
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let counters = Counters::new();

        let mut max_pending = 0;
        for i in 0..N {
            let c = counters.clone();
            let gate = gate.clone();
            let h = reg
                .acquire(
                    SourceKey::from_canonical(format!("rtsp://flood/{i}")),
                    size(320, 180),
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(store("flood"), c.probe(Some(gate))))
                    },
                )
                .unwrap();
            drop(h); // last release → offered to the bounded teardown pool
            max_pending = max_pending.max(reg.pending_teardowns());
        }

        // Sample the peak concurrent shutdowns + pending over a bounded window. With
        // every teardown gated, a fixed pool holds steady at its bound; a
        // thread-per-teardown design climbs toward N as it spawns a thread each.
        let mut max_active = 0;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            max_active = max_active.max(counters.active.load(Ordering::Acquire));
            max_pending = max_pending.max(reg.pending_teardowns());
            std::thread::sleep(Duration::from_millis(2));
        }
        let shed = counters.shed_terminated.load(Ordering::Acquire);

        assert!(
            max_active <= ACTIVE_BOUND,
            "concurrent teardowns must be bounded by a fixed worker pool, not one \
             thread per release (saw {max_active} concurrent shutdowns, bound \
             {ACTIVE_BOUND}, stream {N})"
        );
        assert!(
            max_pending <= PENDING_BOUND,
            "queued+in-flight teardowns must stay bounded by queue+workers under \
             sustained pressure (saw {max_pending}, bound {PENDING_BOUND}, stream {N})"
        );
        assert!(
            shed >= MIN_SHED,
            "the bounded-queue overflow must SHED via Drop (signal-and-detach) and \
             still terminate decode (saw {shed} shed-terminations, need >= {MIN_SHED})"
        );

        gate.store(false, Ordering::Release); // let the wedged workers finish
        drop(reg); // non-blocking detach
    }

    #[test]
    fn shutdown_grace_joins_then_detaches_a_wedged_teardown() {
        // F1: the explicit `shutdown()` must BOUNDED-GRACE-JOIN in-flight teardowns —
        // proven by BOTH a lower bound (it waited ~TEARDOWN_GRACE before giving up, so
        // it grace-JOINED rather than detaching instantly) AND an upper bound (it
        // returned, so a wedged decode-thread join never blocks it forever). One
        // wedged teardown occupies a pool worker.
        use super::TEARDOWN_GRACE;
        let reg = SourceRegistry::<u64>::new();
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let counters = Counters::new();
        let h = reg
            .acquire(
                SourceKey::from_canonical("rtsp://wedged-shutdown"),
                size(1, 1),
                {
                    let c = counters.clone();
                    let gate = gate.clone();
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(
                            store("wedged-shutdown"),
                            c.probe(Some(gate)),
                        ))
                    }
                },
            )
            .unwrap();
        drop(h); // last-release → wedged teardown handed to a pool worker

        // Ensure the wedged teardown is actually running in a worker before shutdown(),
        // so the grace-join has an in-flight teardown to wait on.
        wait_until(
            || counters.active.load(Ordering::Acquire) >= 1,
            Duration::from_secs(5),
            "the wedged teardown reaches a pool worker",
        );

        // Run shutdown() on a helper thread and record how long it takes to return.
        let waited: Arc<Mutex<Option<Duration>>> = Arc::new(Mutex::new(None));
        let reg_bg = Arc::clone(&reg);
        let waited_bg = waited.clone();
        let joiner = std::thread::spawn(move || {
            let started = Instant::now();
            reg_bg.shutdown();
            *super::lock(&waited_bg) = Some(started.elapsed());
        });
        wait_until(
            || super::lock(&waited).is_some(),
            TEARDOWN_GRACE * 4,
            "shutdown() must bounded-grace-join then detach, not block forever",
        );
        let waited = super::lock(&waited).expect("shutdown() returned above");

        // Lower bound: it grace-JOINED (waited ~the grace budget) before detaching —
        // an instant-detach shutdown() would fail this.
        assert!(
            waited >= TEARDOWN_GRACE.mul_f64(0.8),
            "shutdown() must grace-join up to TEARDOWN_GRACE before detaching a wedged \
             teardown, not detach instantly (waited {waited:?}, grace {TEARDOWN_GRACE:?})"
        );
        // Upper bound: a wedged decode-thread join never blocks shutdown() forever.
        assert!(
            waited < TEARDOWN_GRACE * 3,
            "shutdown() must not block forever on a wedged join (waited {waited:?})"
        );
        // The wedged teardown was detached, never joined to completion.
        assert_eq!(
            counters.completed.load(Ordering::Acquire),
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

    #[test]
    fn pending_stays_bounded_under_concurrent_last_releases() {
        // Finding 1 (inv #10): the observable must stay ≤ D+K even when MANY threads hit a
        // last-release SIMULTANEOUSLY. A design that bumps the counter BEFORE the bounded
        // `try_send` lets each concurrent shed spike `pending_teardowns()` while the
        // releasers contend, so it climbs toward the thread count (>> D+K). The bounded
        // reservation caps the counter at D+K no matter how many releasers race: a shed at
        // the ceiling never increments.
        const THREADS: usize = 64; // >> D+K, all releasing at once
        const BOUND: usize = TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS;
        let reg = SourceRegistry::<u64>::new();
        // Gate every teardown so the pool fills and stays full — maximising the contention
        // window a broken (increment-before-send) counter would overshoot in.
        let gate = Arc::new(AtomicBool::new(true));
        let _clear = ClearGateOnDrop(gate.clone());
        let counters = Counters::new();

        // Pre-acquire THREADS distinct handles; each drop is a last-release.
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let c = counters.clone();
                let gate = gate.clone();
                reg.acquire(
                    SourceKey::from_canonical(format!("rtsp://concurrent/{i}")),
                    size(1, 1),
                    move |_r| Ok::<_, Infallible>(SourceInit::new(store("c"), c.probe(Some(gate)))),
                )
                .unwrap()
            })
            .collect();

        // A concurrent sampler records the PEAK observable across the release storm.
        let sampling = Arc::new(AtomicBool::new(true));
        let peak = Arc::new(AtomicUsize::new(0));
        let sampler = {
            let reg = Arc::clone(&reg);
            let sampling = sampling.clone();
            let peak = peak.clone();
            std::thread::spawn(move || {
                while sampling.load(Ordering::Acquire) {
                    peak.fetch_max(reg.pending_teardowns(), Ordering::AcqRel);
                }
            })
        };

        // Release ALL handles at the SAME instant (barrier), each on its own thread.
        let barrier = Arc::new(Barrier::new(THREADS));
        let droppers: Vec<_> = handles
            .into_iter()
            .map(|h| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    drop(h); // last release → offered/shed under contention
                })
            })
            .collect();
        for d in droppers {
            let _ = d.join();
        }
        // One final sample after the storm, then stop the sampler.
        peak.fetch_max(reg.pending_teardowns(), Ordering::AcqRel);
        sampling.store(false, Ordering::Release);
        let _ = sampler.join();

        let observed_peak = peak.load(Ordering::Acquire);
        assert!(
            observed_peak <= BOUND,
            "pending_teardowns must stay bounded by D+K under concurrent last-releases \
             (peak {observed_peak}, bound {BOUND}, {THREADS} concurrent releasers)"
        );

        gate.store(false, Ordering::Release);
        reg.shutdown();
    }

    #[test]
    fn a_shed_teardown_terminates_the_decode_thread_via_request_stop() {
        // Finding 5 (inv #10): the SHED path must PROVE it TERMINATES the decode thread, not
        // merely count it. Each ThreadedProbe owns a REAL "decode" thread that exits ONLY on
        // the structural request_stop() signal (its Drop neither stops nor joins it). Flood
        // the bounded pool with GATED probes so the overflow SHEDS; a shed that fails to call
        // request_stop leaks its thread. After clearing the gate + shutdown, EVERY decode
        // thread — graceful, queued, and shed — must have terminated (`live` back to zero).
        const N: usize = 50; // >> D+K=34, so the overflow must shed
        let reg = SourceRegistry::<u64>::new();
        let gate = Arc::new(AtomicBool::new(true)); // wedge graceful shutdowns → force sheds
        let _clear = ClearGateOnDrop(gate.clone());
        let live = Arc::new(AtomicUsize::new(0));
        let stops: Vec<Arc<AtomicBool>> =
            (0..N).map(|_| Arc::new(AtomicBool::new(false))).collect();
        // Safety net: on ANY exit (including a RED assertion panic) stop every decode thread.
        let _stopper = StopAllOnDrop(stops.clone());

        for (i, stop) in stops.iter().enumerate() {
            let stop = stop.clone();
            let gate = gate.clone();
            let live = live.clone();
            let h = reg
                .acquire(
                    SourceKey::from_canonical(format!("rtsp://threaded/{i}")),
                    size(1, 1),
                    move |_r| {
                        Ok::<_, Infallible>(SourceInit::new(
                            store("t"),
                            ThreadedProbe::new(stop, Some(gate), live),
                        ))
                    },
                )
                .unwrap();
            drop(h); // last release → offered; the overflow sheds
        }

        // Clear the wedge so the graceful (in-flight + queued) shutdowns can join, then drain.
        gate.store(false, Ordering::Release);
        reg.shutdown();

        // Every decode thread must terminate — graceful joins AND shed request_stops. A shed
        // that only detaches (no request_stop) leaves its thread running → live never zeroes.
        wait_until(
            || live.load(Ordering::Acquire) == 0,
            Duration::from_secs(10),
            "every decode thread terminates after teardown — graceful joins and sheds signalled",
        );
    }

    #[test]
    fn repeated_shutdown_panics_keep_the_pool_draining() {
        // Finding 2 (inv #10): a panicking teardown must NOT shrink the fixed pool. The
        // worker runs shutdown() under catch_unwind, so a panic is contained and the worker
        // survives; the pool stays at TEARDOWN_WORKERS and keeps draining. Interleave a
        // stream of PANICKING teardowns with HEALTHY ones: every healthy teardown must still
        // complete (the pool never stalls) and the observable stays bounded. (A design where
        // a panic kills the worker stalls once both workers die → the stream never finishes.)
        //
        // Expected stderr: each panicking shutdown prints one caught-panic line — that is the
        // mechanism working, not a failure.
        const TOTAL: usize = 48;
        const HEALTHY: usize = 36; // every non-4th offer is healthy
        const BOUND: usize = TEARDOWN_QUEUE_DEPTH + TEARDOWN_WORKERS;
        let reg = SourceRegistry::<u64>::new();
        let healthy = Counters::new();
        let mut max_pending = 0;

        for i in 0..TOTAL {
            if i % 4 == 0 {
                // A panicking teardown (12 of them, ~6 per worker → "repeatedly").
                let h = reg
                    .acquire(
                        SourceKey::from_canonical(format!("rtsp://panic/{i}")),
                        size(1, 1),
                        |_r| Ok::<_, Infallible>(SourceInit::new(store("p"), PanicProbe)),
                    )
                    .unwrap();
                drop(h);
            } else {
                let hc = healthy.clone();
                let h = reg
                    .acquire(
                        SourceKey::from_canonical(format!("rtsp://ok/{i}")),
                        size(1, 1),
                        move |_r| Ok::<_, Infallible>(SourceInit::new(store("ok"), hc.probe(None))),
                    )
                    .unwrap();
                drop(h);
            }
            max_pending = max_pending.max(reg.pending_teardowns());
        }

        // The pool survived every panic and drained every healthy teardown.
        wait_until(
            || {
                healthy.completed.load(Ordering::Acquire)
                    + healthy.shed_terminated.load(Ordering::Acquire)
                    >= HEALTHY
            },
            Duration::from_secs(10),
            "every healthy teardown completes-or-sheds despite repeated shutdown() panics",
        );
        assert!(
            healthy.completed.load(Ordering::Acquire) >= 1,
            "the pool must keep draining gracefully through the panics (not merely shed)"
        );
        assert!(
            max_pending <= BOUND,
            "the observable stays bounded through panics (saw {max_pending}, bound {BOUND})"
        );

        reg.shutdown();
    }
}
