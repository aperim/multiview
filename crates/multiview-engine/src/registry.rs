//! The process-global **source registry** — decode-once, use-many
//! (ADR-0030 §3, ADR-0034 §2).
//!
//! In a multi-program engine, many cells across many programs may bind the *same*
//! physical source. Decoding it once per binding wastes the scarcest budget
//! (decode megapixels/sec, invariant #6). The [`SourceRegistry`] is the owner of
//! **source identity → (one ingest/decode actor + one shared [`TileStore`])**: the
//! first consumer to reference a canonical [`SourceKey`] spins the decode; every
//! later consumer of the same key shares an [`Arc`] clone of the same store. The
//! decode targets the **supremum** requested resolution across all consumers
//! (ADR-0030 §3) — each consumer scales at composite.
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
//! ## Teardown off the hot path (safety rule §4)
//!
//! When the **last** reference to a source is released the entry is removed and its
//! decode actor is handed to a dedicated **reaper thread** via a wait-free,
//! non-blocking channel send. The blocking stop-and-join of the decode thread runs
//! **on the reaper**, never in the releasing consumer's `Drop` — so an `Arc`-drop
//! that returns a pooled buffer never runs a blocking join inside a Tokio async
//! destructor. The one blocking join lives in the explicit [`SourceRegistry::shutdown`],
//! called from a synchronous teardown context.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use multiview_framestore::TileStore;

/// A requested decode resolution, in pixels.
///
/// The registry records the per-axis **supremum** of all consumers' requests so
/// the shared decode is sized to satisfy the largest (ADR-0030 §3).
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
/// thread — is **blocking** and runs **exclusively on the registry's reaper
/// thread**, never on a program's hot path and never inside a Tokio async
/// destructor (safety rule §4 / inv #10). A last-release `Drop` only hands the
/// boxed actor to the reaper via a wait-free channel send; the reaper performs the
/// join off every hot path.
///
/// Implementors' own `Drop` (reached only if the registry is torn down while this
/// actor is still queued — i.e. the reaper is already gone) MUST be non-blocking:
/// signal-and-detach, never a join, for the same reason.
pub trait SourceActor: Send + 'static {
    /// Stop the actor and block until its decode thread has fully stopped. Called
    /// exactly once, only on the reaper thread.
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
        // The reaper performs every blocking decode-thread join off the hot path.
        // If the thread cannot be spawned (never in practice), `rx` is dropped and
        // later teardowns fall back to dropping the actor (its non-blocking Drop) —
        // `release` stays infallible either way.
        let join = std::thread::Builder::new()
            .name("mv-source-reaper".to_owned())
            .spawn(move || reaper_loop(&rx))
            .ok();
        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            reaper_tx: tx,
            reaper_join: Mutex::new(join),
        })
    }

    /// Acquire a ref-counted handle to the source identified by `key`, decoding
    /// once and sharing thereafter.
    ///
    /// On the **first** reference to `key` the `factory` runs (given the initial
    /// supremum `requested` size) to create the shared store + decode actor. Every
    /// later reference to the same key skips the factory, bumps the refcount, grows
    /// the recorded supremum to the per-axis max, and returns an [`Arc`] clone of
    /// the same store. `factory` runs under the registry lock and MUST be
    /// non-blocking (spawn-and-return) and MUST NOT re-enter the registry.
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

    /// Whether a source with `key` is currently registered (has ≥ 1 reference).
    #[must_use]
    pub fn contains(&self, key: &SourceKey) -> bool {
        lock(&self.entries).contains_key(key)
    }

    /// The per-axis supremum requested size recorded for `key`, or [`None`] if no
    /// such source is registered. The shared decode targets this size (ADR-0030 §3).
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

    /// Drain and stop the reaper, joining every pending decode teardown.
    ///
    /// Call from a **synchronous** teardown context **after** all handles have been
    /// released — never from a Tokio async destructor (it blocks on the slowest
    /// decode-thread join). Idempotent.
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

/// The reaper thread body: stop-and-join each actor off every hot path, and on
/// [`Reap::Stop`] drain any already-queued teardowns before exiting so an explicit
/// [`SourceRegistry::shutdown`] joins **all** pending decodes.
fn reaper_loop(rx: &Receiver<Reap>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Reap::Actor(actor) => actor.shutdown(),
            Reap::Stop => {
                while let Ok(Reap::Actor(actor)) = rx.try_recv() {
                    actor.shutdown();
                }
                return;
            }
        }
    }
    // `recv` returned `Err`: every sender was dropped (the registry is gone). Any
    // actor still owned by a live handle keeps the decode alive until that handle
    // drops; nothing to reap here.
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
