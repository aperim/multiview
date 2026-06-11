//! The runtime store of **ephemeral** cast sessions (DEV-D2, ADR-M011).
//!
//! An ad-hoc cast session is runtime-only state: it lives here (and as a
//! running session actor in the
//! [`DevicePollerRegistry`](crate::devices::DevicePollerRegistry)) and **never**
//! enters the devices resource store, so a config export can never emit it.
//! "Save as device" promotes a session into a normal `Device{driver: cast}`
//! registry entry — at which point the *device* is durable desired state and
//! this record is dropped.
//!
//! Control-plane only, `Mutex`-guarded, bounded by the number of live
//! sessions (invariant #10 — the engine never touches this).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// One ephemeral cast session's descriptive record (the live lifecycle state
/// rides the latest-wins status registry under the same id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastSessionRecord {
    /// The runtime session id (`cast-session-…`, UUID-fresh per start).
    pub id: String,
    /// The operator-facing name, if given.
    pub name: Option<String>,
    /// The device authority dialled (`host[:port]`, IPv6 bracketed).
    pub address: String,
    /// The output id whose rendition the session casts.
    pub output: String,
    /// The resolved device-reachable media URL the session LOADs.
    pub media_url: String,
}

/// The `Mutex`-guarded map of live ephemeral sessions, id-sorted.
#[derive(Debug, Default)]
pub struct CastSessionStore {
    inner: Mutex<BTreeMap<String, CastSessionRecord>>,
}

impl CastSessionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the map, recovering from a poisoned lock (a panic in one request
    /// must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CastSessionRecord>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Insert (or replace) a session record.
    pub fn insert(&self, record: CastSessionRecord) {
        self.lock().insert(record.id.clone(), record);
    }

    /// The record for `id`, if the session is live.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<CastSessionRecord> {
        self.lock().get(id).cloned()
    }

    /// Remove (and return) the record for `id`.
    pub fn remove(&self, id: &str) -> Option<CastSessionRecord> {
        self.lock().remove(id)
    }

    /// All live session records, id-sorted.
    #[must_use]
    pub fn list(&self) -> Vec<CastSessionRecord> {
        self.lock().values().cloned().collect()
    }
}
