//! The Conspect **entitlement-plane wiring** for the cli (CONSPECT-2b/10,
//! ADR-0050 §3/§5/§7, the brief §13.6).
//!
//! The cli owns the single shared entitlement [`LeaseStore`] and threads its
//! computed state into the engine seams as **data**:
//!
//! * **S1 (startup gate):** the sampled [`EnforcementLevel`] the
//!   [`SoftwareEngine::build_gated`](crate::run::SoftwareEngine::build_gated)
//!   consults *before* building a NEW engine — see [`current_level`].
//! * **S3 (tile watermark):** the wait-free [`WatermarkSignal`] the overlay bake
//!   samples each frame to decide whether to stamp the corner watermark.
//! * The same store backs the control plane's `LicenceState`, so the API, the
//!   chrome, and the engine all read the **same** ladder state — there is no
//!   second opinion (ADR-0050 §2).
//!
//! # Never off air (invariant #1 / #10)
//!
//! Everything here is **data the engine samples**, never control flow on the tick
//! path. The [`WatermarkSignal`] is an `arc_swap`'d [`EnforcementLevel`] read with
//! a single wait-free load — no lock, no allocation, no `.await` — so the overlay
//! bake's per-frame watermark decision cannot pace or stall the output clock. The
//! signal is updated **off-thread** (a background poll of the lease store), never
//! from the hot loop. The startup gate runs only at build time; a running engine
//! is never re-gated (ADR-0050 §6.3).

use std::sync::Arc;

use arc_swap::ArcSwap;
#[cfg(feature = "heartbeat")]
use multiview_licence::heartbeat::{DeviceIdentity, HeartbeatClient, HeartbeatConfig};
use multiview_licence::verify::PinnedKey;
use multiview_licence::watcher::{LeaseDirectoryWatcher, DEFAULT_LEASE_DIR};
use multiview_licence::{EnforcementLevel, LeaseStore};
use multiview_mesh::MeshState;

/// The env var the pinned issuer **verifying key** is read from (hex-encoded
/// Ed25519 public key, 64 hex chars / 32 bytes). When unset (or malformed), no
/// key is pinned: the machine runs **unlicensed-honest** — no lease verifies, so
/// the entitlement plane reports no installed lease and every seam fails toward
/// leniency (no watermark, no config-lock, no start block). Never embedded in the
/// binary (secret-hygiene + the honest source-build caveat, ADR-0050 §7).
pub const PUBKEY_ENV: &str = "MULTIVIEW_LICENCE_PUBKEY";

/// The env var the lease **directory** is read from (overriding
/// [`DEFAULT_LEASE_DIR`]). A dropped, signed lease file there is verified against
/// the pinned key and installed at startup; absent/unreadable is fine (no lease).
pub const LEASE_DIR_ENV: &str = "MULTIVIEW_LICENCE_DIR";

/// The wait-free **tile-watermark signal** the overlay bake samples each frame
/// (Conspect S3, ADR-0050 §5).
///
/// It holds an `arc_swap`'d [`EnforcementLevel`] — the single published ladder
/// level. The bake reads [`WatermarkSignal::watermark`] with one lock-free load
/// (no lock, no allocation), so the per-frame watermark decision is O(1) and
/// cannot stall the output clock (invariant #1). The level is updated off-thread
/// via [`WatermarkSignal::set`] (a background poll of the lease store), never from
/// the hot loop.
#[derive(Clone)]
pub struct WatermarkSignal {
    /// The published ladder level. `arc_swap` gives a wait-free latest-value read
    /// + an off-thread atomic store (ADR-I001 isolation primitive).
    level: Arc<ArcSwap<EnforcementLevel>>,
}

impl WatermarkSignal {
    /// A clean signal (no watermark): the compliant default. Equivalent to a
    /// machine with a valid lease — the canvas is unmarked.
    #[must_use]
    pub fn clean() -> Self {
        Self::at(EnforcementLevel::Active)
    }

    /// A signal pinned at `level` (tests inject a watermark rung directly to drive
    /// a deterministic golden bake).
    #[must_use]
    pub fn at(level: EnforcementLevel) -> Self {
        Self {
            level: Arc::new(ArcSwap::from_pointee(level)),
        }
    }

    /// Publish a new ladder `level` (off-thread). The next bake reads it.
    pub fn set(&self, level: EnforcementLevel) {
        self.level.store(Arc::new(level));
    }

    /// The currently-published ladder level (a wait-free load).
    #[must_use]
    pub fn level(&self) -> EnforcementLevel {
        **self.level.load()
    }

    /// Whether the bake should stamp the corner watermark this frame — a single
    /// wait-free load + the pure `EnforcementLevel::watermark()` test. Called once
    /// per baked frame, off the hot loop (the bake runs on collected output
    /// frames, ADR-0050 §5).
    #[must_use]
    pub fn watermark(&self) -> bool {
        self.level().watermark()
    }
}

impl Default for WatermarkSignal {
    fn default() -> Self {
        Self::clean()
    }
}

/// The assembled cli entitlement plane: the shared lease store, the published
/// watermark signal (S3), and the optional pinned key (so the control plane can
/// also accept a presented lease over `POST /api/v1/licence/lease`).
///
/// Everything here is **data** the engine seams sample; nothing holds an engine
/// handle (ADR-0050 §5/§6.3, invariant #1/#10). The same `store` backs the
/// control plane's `LicenceState`, so the API, the chrome, and the engine read
/// the **same** ladder state.
pub struct EntitlementPlane {
    /// The shared verified-lease store (also the control plane's `LicenceState`).
    pub store: Arc<LeaseStore>,
    /// The wait-free watermark signal the overlay bake samples (S3).
    pub signal: WatermarkSignal,
    /// The pinned issuer key, if one was configured (the install path needs it).
    pub pinned: Option<PinnedKey>,
    /// The shared local-mesh discovery/relay state (Conspect, ADR-0051): the
    /// untrusted discovered-peer inventory + the relay opt-in the control plane's
    /// `/api/v1/mesh/*` routes render + toggle, and (under `mesh-mdns`) the
    /// always-on announce/browse loop maintains. Always wired so the API serves a
    /// real shared store; control-plane only, no engine handle (invariant #10).
    pub mesh: Arc<MeshState>,
}

impl EntitlementPlane {
    /// Assemble the entitlement plane from the process environment: read the
    /// pinned key from [`PUBKEY_ENV`] (hex), build the shared store, and — when a
    /// key is pinned — load any dropped lease file from [`LEASE_DIR_ENV`] (or
    /// [`DEFAULT_LEASE_DIR`]) once at startup. The published [`WatermarkSignal`] is
    /// initialised from the resulting state.
    ///
    /// All-data, fail-toward-leniency: a missing/malformed key, a missing lease
    /// dir, or a garbage lease file leaves the machine unlicensed-honest (no
    /// watermark, no lock, no start block) — never a crash, never off air.
    #[must_use]
    pub fn from_env() -> Self {
        let store = Arc::new(LeaseStore::new());
        let pinned = pinned_key_from_env();
        if let Some(pinned) = pinned.as_ref() {
            let dir = std::env::var(LEASE_DIR_ENV)
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| DEFAULT_LEASE_DIR.to_owned());
            // One-shot load of any dropped, signed lease file (the directory
            // watcher's poll step). A missing dir / garbage file is logged + skipped
            // inside `poll_once` (never off air).
            let watcher = LeaseDirectoryWatcher::new(dir, pinned.clone(), Arc::clone(&store));
            let _ = watcher.poll_once(multiview_licence::store::system_now());
        } else {
            tracing::info!(
                "no {PUBKEY_ENV} pinned — running unlicensed-honest (no lease verifies; \
                 enforcement seams fail toward leniency)"
            );
        }
        let signal = WatermarkSignal::clean();
        refresh_signal(&store, &signal);
        Self {
            store,
            signal,
            pinned,
            // A fresh, empty mesh state: always-on discovery, no peers yet, relay
            // declined (opt-out default). The control plane renders + toggles it;
            // under `mesh-mdns` the spawned announce/browse loop folds discovered
            // neighbours into it. Control-plane only (invariant #10).
            mesh: Arc::new(MeshState::new()),
        }
    }

    /// Spawn the always-on local-mesh **browse + age** loop (Conspect, ADR-0051
    /// §2/§5) — the `mesh-mdns` feature only.
    ///
    /// Starts the live mDNS service (IPv6-first `ff02::fb`, IPv4 legacy interop)
    /// and folds discovered neighbours (untrusted) into the shared
    /// [`MeshState`](multiview_mesh::MeshState) the control plane serves, aging out
    /// peers it stops hearing from. **Best-effort, never blocks (invariant #10):**
    /// a daemon that fails to start is logged and skipped (discovery simply yields
    /// no peers — never off air, never a crash); the browse drain is non-blocking
    /// and the loop sleeps between rounds, holding no engine handle.
    ///
    /// The loop **browses only** — it does not yet *announce* a signed summary,
    /// because signing requires this machine's own Ed25519 key and salted
    /// fingerprint digests, which are provisioned by the licence-server handshake +
    /// claim-time salt (the operator-confirm items O1/O2/O6, the conspect brief
    /// §14) that are sequenced after the local plane. Once that material is wired,
    /// the announce side folds in here over the same transport (the
    /// [`announce_browse_step`](multiview_mesh::driver::announce_browse_step) round
    /// the offline tests already cover) without touching the discovery path.
    #[cfg(feature = "mesh-mdns")]
    pub fn spawn_mesh_discovery(&self) {
        use std::time::Instant;

        use multiview_mesh::peer::PeerObservation;
        use multiview_mesh::service::{decode_received, MdnsService, DEFAULT_PORT};
        use multiview_mesh::transport::MeshTransport;

        // The instance name is this machine's stable hex peer id once a fingerprint
        // is provisioned; until then a per-process random hex anchor keeps our own
        // announcement (if any) distinct. Browse-only here, so the host/instance are
        // informational. A `.local.` host derived from the process is fine for mDNS.
        let host = "multiview.local.".to_owned();
        let instance = format!("multiview-{:016x}", std::process::id());
        let service = match MdnsService::start(&instance, &host, DEFAULT_PORT) {
            Ok(service) => service,
            Err(err) => {
                tracing::info!(
                    %err,
                    "local-mesh mDNS discovery unavailable this run (no multicast \
                     interface?) — best-effort, never off air; /api/v1/mesh serves an \
                     empty inventory"
                );
                return;
            }
        };
        let mesh = Arc::clone(&self.mesh);
        let start = Instant::now();
        tokio::spawn(async move {
            loop {
                // Browse (non-blocking) + fold untrusted observations + age out.
                // Browse-only until the machine signing key + salt land (O1/O2/O6).
                let now = start.elapsed();
                match service.poll_received() {
                    Ok(received) => {
                        for announcement in &received {
                            match decode_received(announcement) {
                                Ok(payload) => {
                                    if let Some(key) = payload.peer_key() {
                                        mesh.observe(PeerObservation {
                                            key,
                                            claim_state: payload.claim_state,
                                            observed_at: now,
                                        });
                                    }
                                }
                                Err(err) => {
                                    tracing::debug!(%err, "ignoring a malformed mesh announcement");
                                }
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(%err, "mesh browse poll failed this round (best-effort)");
                    }
                }
                mesh.age_out(now);
                tokio::time::sleep(multiview_mesh::driver::ANNOUNCE_INTERVAL).await;
            }
        });
        tracing::info!(
            service = multiview_mesh::service::SERVICE_TYPE,
            "local-mesh discovery running (always-on mDNS browse, IPv6-first; \
             /api/v1/mesh/peers serves the untrusted inventory)"
        );
    }

    /// No-op when the `mesh-mdns` feature is off: the shared [`MeshState`] is still
    /// wired (so `/api/v1/mesh/*` serves it), but no live socket loop runs and the
    /// default build stays socket-free + `cargo deny`-clean (ADR-0051 §6).
    #[cfg(not(feature = "mesh-mdns"))]
    pub fn spawn_mesh_discovery(&self) {
        tracing::debug!(
            "local-mesh live discovery is OFF (build without `mesh-mdns`); \
             /api/v1/mesh serves the wired (empty) inventory"
        );
    }

    /// Spawn the Conspect **device-licensing heartbeat** loop (CONSPECT-3,
    /// ADR-0096) — the `heartbeat` feature only.
    ///
    /// Reads the heartbeat settings from the environment ([`HeartbeatSettings`]):
    /// the organisation id (config-driven, O4), the pinned ECDSA-P256 **root**
    /// key, the Conspect base URL, the account JWT bearer, and the salted device
    /// identity. When the essential settings are unset, the loop is **not** started
    /// (logged) — the machine runs unlicensed-honest, the same fail-toward-leniency
    /// posture as the rest of the plane.
    ///
    /// When configured, it `tokio::spawn`s a [`HeartbeatClient`] loop that renews
    /// the lease against conspect.studio and drives the shared
    /// [`LeaseStore::install_binding`](multiview_licence::LeaseStore::install_binding)
    /// convergence — the same path the offline file-drop and mesh relay already
    /// use, so S1/S2/S3 re-sample with no extra wiring (via the existing
    /// [`refresh_signal`] poll). **Best-effort, never off air (invariants #1/#10):**
    /// the task holds only [`Arc<LeaseStore>`](multiview_licence::LeaseStore) + the
    /// pinned root + the client — **no** engine handle/channel/lock — and only ever
    /// tightens on a positively-verified signed lease; every failure (or a withheld
    /// lease) keeps the last-good lease and lets it age.
    #[cfg(feature = "heartbeat")]
    pub fn spawn_heartbeat(&self) {
        let Some(settings) = HeartbeatSettings::from_env() else {
            tracing::info!(
                "Conspect heartbeat is OFF (unconfigured): set {} + {} + {} + {} to enable the \
                 device-licensing phone-home; running unlicensed-honest until then",
                HeartbeatSettings::ORG_ENV,
                HeartbeatSettings::ROOT_ENV,
                HeartbeatSettings::API_ENV,
                HeartbeatSettings::TOKEN_ENV
            );
            return;
        };
        let pinned_root = match multiview_licence::heartbeat::PinnedRoot::from_base64url(
            &settings.pinned_root_b64url,
        ) {
            Ok(root) => root,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "Conspect heartbeat is OFF: {} is not a valid base64url ECDSA-P256 root \
                     point — running unlicensed-honest",
                    HeartbeatSettings::ROOT_ENV
                );
                return;
            }
        };
        // Fail closed: if the HTTPS-only client cannot be built, do NOT fall back
        // to a plaintext-capable client that would leak the bearer JWT — the
        // heartbeat stays OFF (the entitlement plane keeps last-good).
        let server =
            match ConspectHttpServer::new(settings.api_base.clone(), settings.bearer_token.clone())
            {
                Ok(server) => std::sync::Arc::new(server),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Conspect heartbeat is OFF: could not build the HTTPS-only client — \
                         running unlicensed-honest (never a plaintext credential-carrying client)"
                    );
                    return;
                }
            };
        // Durable idempotency nonce beside the lease state (the same dir the
        // offline file-drop watcher reads), so a restart never reuses a prior
        // lifetime's Idempotency-Key (cross-restart duplicate-mutation defence).
        let lease_dir = std::env::var(LEASE_DIR_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_LEASE_DIR.to_owned());
        // Fail closed: if the nonce lock cannot be taken (another `multiview` owns
        // this lease dir, or the dir is unwritable) do NOT start a second minter
        // that could issue colliding Idempotency-Keys — the heartbeat stays OFF
        // (the entitlement plane keeps last-good).
        let nonce_store: multiview_licence::heartbeat::SharedNonceStore =
            match FileNonceStore::in_dir(&lease_dir) {
                Ok(store) => std::sync::Arc::new(store),
                Err(err) => {
                    tracing::warn!(
                        %err,
                        "Conspect heartbeat is OFF: could not take the durable idempotency-nonce \
                         lock (a second heartbeat owner on this lease dir?) — keeping last-good"
                    );
                    return;
                }
            };
        let client = HeartbeatClient::with_nonce(
            server,
            std::sync::Arc::clone(&self.store),
            pinned_root,
            settings.heartbeat_config(),
            settings.identity.clone(),
            nonce_store,
        );
        let org = settings.org_id.clone();
        tokio::spawn(async move {
            client.run_forever().await;
        });
        tracing::info!(
            org = %org,
            api = %settings.api_base,
            "Conspect device heartbeat running (control-plane phone-home; renews the lease via \
             the shared install convergence; never off air)"
        );
    }

    /// No-op when the `heartbeat` feature is off: the entitlement plane is fully
    /// wired (the offline file-drop install + the read-only heartbeat-status
    /// surface still work), but no live licence-server loop runs and the default
    /// build stays network-free + `cargo deny`-clean (ADR-0096).
    #[cfg(not(feature = "heartbeat"))]
    pub fn spawn_heartbeat(&self) {
        tracing::debug!(
            "Conspect device heartbeat is OFF (build without `heartbeat`); the entitlement \
             plane serves local lease state only"
        );
    }

    /// The current sampled enforcement level for the S1 startup gate.
    #[must_use]
    pub fn level(&self) -> Option<EnforcementLevel> {
        current_level(&self.store)
    }
}

/// Read + parse the pinned issuer verifying key from [`PUBKEY_ENV`] (hex). Returns
/// `None` when unset, empty, or not a valid 32-byte Ed25519 key (logged) — the
/// unlicensed-honest default.
#[must_use]
fn pinned_key_from_env() -> Option<PinnedKey> {
    let hex = std::env::var(PUBKEY_ENV).ok().filter(|s| !s.is_empty())?;
    let bytes = decode_hex(hex.trim())?;
    if let Ok(key) = PinnedKey::from_slice(&bytes) {
        Some(key)
    } else {
        tracing::warn!(
            "{PUBKEY_ENV} is not a valid 32-byte Ed25519 public key — ignoring \
             (running unlicensed-honest)"
        );
        None
    }
}

/// Decode a hex string to bytes, or `None` on any non-hex / odd-length input
/// (no `as` casts; total + panic-free).
#[must_use]
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i + 1 < s.len() {
        let hi = nibble(*bytes.get(i)?)?;
        let lo = nibble(*bytes.get(i + 1)?)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// Sample the current entitlement [`EnforcementLevel`] from the lease `store`, or
/// `None` when no lease is installed.
///
/// This is what the startup gate (S1) consults *before* building a new engine
/// (`build_gated`). Reading off the store is wait-free and off the hot loop; the
/// gate fails toward leniency on `None` (ADR-0050 §6.3).
#[must_use]
pub fn current_level(store: &LeaseStore) -> Option<EnforcementLevel> {
    store.status().map(|s| s.enforcement)
}

/// Publish the lease `store`'s current level into `signal` (off-thread refresh).
///
/// Called by the cli's background entitlement poll so the wait-free watermark
/// signal tracks the lease as it ages across day boundaries / is renewed —
/// without ever reading the store on the hot loop. A store with no lease publishes
/// the compliant `Active` level (fail-toward-leniency: no positive lapse evidence
/// → no watermark).
pub fn refresh_signal(store: &LeaseStore, signal: &WatermarkSignal) {
    let level = current_level(store).unwrap_or(EnforcementLevel::Active);
    signal.set(level);
}

// ===========================================================================
// Conspect heartbeat (CONSPECT-3, ADR-0096) — the cli-boundary HTTP transport
// + environment-driven settings. Feature `heartbeat`, OFF by default.
// ===========================================================================

/// The environment-driven heartbeat settings the cli assembles for the
/// [`HeartbeatClient`]. Read from the process environment so the pinned root, the
/// account token, and the salted device identity never live in the binary
/// (secret hygiene; mirrors [`PUBKEY_ENV`]). The **organisation id is
/// config-driven** (ADR-0096 O4): the operator sets it explicitly (the free
/// auto-issue default org is an external-doc residual), so there is no hard-coded
/// guess.
#[cfg(feature = "heartbeat")]
#[derive(Debug, Clone)]
pub struct HeartbeatSettings {
    /// The organisation id the device heartbeats against (`{orgId}`).
    pub org_id: String,
    /// The pinned ECDSA-P256 **root** key, base64url uncompressed point.
    pub pinned_root_b64url: String,
    /// The Conspect API base URL (e.g. `https://api.conspect.studio/v0`).
    pub api_base: String,
    /// The account JWT bearer token (operator role for heartbeat). Never logged.
    pub bearer_token: String,
    /// The salted device identity the requests carry (no raw identifiers).
    pub identity: DeviceIdentity,
}

#[cfg(feature = "heartbeat")]
impl HeartbeatSettings {
    /// The env var the organisation id is read from (config-driven, O4).
    pub const ORG_ENV: &'static str = "MULTIVIEW_LICENCE_ORG";
    /// The env var the pinned ECDSA-P256 root key (base64url) is read from.
    pub const ROOT_ENV: &'static str = "MULTIVIEW_LICENCE_ROOT";
    /// The env var the Conspect API base URL is read from.
    pub const API_ENV: &'static str = "MULTIVIEW_LICENCE_API";
    /// The env var the account JWT bearer token is read from (never logged).
    pub const TOKEN_ENV: &'static str = "MULTIVIEW_LICENCE_TOKEN";
    /// The env var the salted hardware-fingerprint digest is read from (hex).
    pub const FP_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_FP_DIGEST";
    /// The env var the fingerprint score is read from (0–100).
    pub const FP_SCORE_ENV: &'static str = "MULTIVIEW_LICENCE_FP_SCORE";
    /// The env var the registered machine id is read from.
    pub const MACHINE_ENV: &'static str = "MULTIVIEW_LICENCE_MACHINE_ID";
    /// The env var the instance id is read from.
    pub const INSTANCE_ENV: &'static str = "MULTIVIEW_LICENCE_INSTANCE_ID";
    /// The env var the instance binding id is read from (once known).
    pub const BINDING_ENV: &'static str = "MULTIVIEW_LICENCE_BINDING_ID";
    /// The env var the salted hardware digest is read from.
    pub const HW_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_HW_DIGEST";
    /// The env var the instance discriminator hash is read from.
    pub const DISC_HASH_ENV: &'static str = "MULTIVIEW_LICENCE_DISC_HASH";
    /// The env var the instance discriminator digest is read from.
    pub const DISC_DIGEST_ENV: &'static str = "MULTIVIEW_LICENCE_DISC_DIGEST";
    /// The env var the device Ed25519 proof-of-possession public key (base64url)
    /// is read from.
    pub const DEVICE_KEY_ENV: &'static str = "MULTIVIEW_LICENCE_DEVICE_KEY";

    /// Assemble settings from the process environment, or `None` when the
    /// essential ones (org id, pinned root, API base, bearer token) are unset —
    /// the heartbeat then stays off and the machine runs unlicensed-honest.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let org_id = non_empty_env(Self::ORG_ENV)?;
        let pinned_root_b64url = non_empty_env(Self::ROOT_ENV)?;
        let api_base = non_empty_env(Self::API_ENV)?;
        let bearer_token = non_empty_env(Self::TOKEN_ENV)?;
        let identity = DeviceIdentity {
            machine_id: non_empty_env(Self::MACHINE_ENV).unwrap_or_default(),
            instance_id: non_empty_env(Self::INSTANCE_ENV).unwrap_or_default(),
            binding_id: non_empty_env(Self::BINDING_ENV),
            fingerprint_digest: non_empty_env(Self::FP_DIGEST_ENV).unwrap_or_default(),
            fingerprint_score: non_empty_env(Self::FP_SCORE_ENV)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            hardware_digest: non_empty_env(Self::HW_DIGEST_ENV).unwrap_or_default(),
            instance_discriminator_hash: non_empty_env(Self::DISC_HASH_ENV).unwrap_or_default(),
            instance_discriminator_digest: non_empty_env(Self::DISC_DIGEST_ENV).unwrap_or_default(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            device_public_key_b64url: non_empty_env(Self::DEVICE_KEY_ENV).unwrap_or_default(),
        };
        Some(Self {
            org_id,
            pinned_root_b64url,
            api_base,
            bearer_token,
            identity,
        })
    }

    /// Build the [`HeartbeatConfig`] from these settings.
    #[must_use]
    pub fn heartbeat_config(&self) -> HeartbeatConfig {
        HeartbeatConfig {
            org_id: self.org_id.clone(),
            ..HeartbeatConfig::default()
        }
    }
}

/// Read an environment variable, returning `None` when unset or empty.
#[cfg(feature = "heartbeat")]
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

/// A durable, file-backed [`NonceStore`](multiview_licence::heartbeat::NonceStore)
/// for the idempotency mint counter — a tiny decimal file beside the lease state,
/// so the per-operation nonce survives a restart and a post-restart operation
/// never reuses a prior lifetime's key. The leaf crate does no I/O; this is the
/// cli-boundary implementation it injects via `with_clock_and_nonce`.
///
/// `load` reads the persisted high-water counter and **fails closed**: an ABSENT
/// file is `Ok(0)` (a fresh device legitimately starts at 0), but a PRESENT file
/// that is corrupt/unparseable or unreadable for any other reason returns an
/// `Err` — never a silent 0 that would reset the high-water and re-mint a colliding
/// key after a restart. `commit` writes the new high-water via
/// write-temp-then-rename (so a torn write cannot leave a truncated value) and
/// **propagates** a write OR rename failure as an `Err` — an un-persisted key must
/// block the mutation, not continue best-effort. The heartbeat client gates the
/// mint on these, so a nonce-store failure keeps last-good (never off air, never a
/// colliding-key mutation).
#[cfg(feature = "heartbeat")]
struct FileNonceStore {
    path: std::path::PathBuf,
    /// The exclusive advisory lock holder for the whole process lifetime. Held so
    /// a SECOND `multiview` process pointed at the same lease-state dir cannot
    /// concurrently mint colliding Idempotency-Keys: that process's
    /// [`FileNonceStore::in_dir`] fails to take the lock and its heartbeat declines
    /// to start (fail closed). The OS releases the lock when this `File` is dropped
    /// (process exit or `EntitlementPlane` teardown), so a crashed owner never
    /// strands the lock. `_lock` is never read — its lifetime IS the guarantee.
    _lock: std::fs::File,
}

#[cfg(feature = "heartbeat")]
impl FileNonceStore {
    /// Open the durable nonce store in `dir` (the lease-state dir), taking a
    /// **non-blocking exclusive advisory lock** on `<dir>/idempotency-nonce.lock`
    /// so this process is the sole minter of the nonce file. The data lives in
    /// `<dir>/idempotency-nonce`.
    ///
    /// # Errors
    /// [`NonceError`](multiview_licence::heartbeat::NonceError) when the lock dir
    /// cannot be created/opened, or when ANOTHER process already holds the lock
    /// (a second heartbeat owner on the same lease dir) — the caller then keeps the
    /// heartbeat OFF (fail closed; the entitlement plane keeps last-good).
    fn in_dir(dir: &str) -> Result<Self, multiview_licence::heartbeat::NonceError> {
        use multiview_licence::heartbeat::NonceError;
        let dir_path = std::path::Path::new(dir);
        // Ensure the lease-state dir exists so the lock/data files can be created
        // (the offline file-drop watcher tolerates an absent dir; the nonce owner
        // must materialise it to hold the lock).
        std::fs::create_dir_all(dir_path).map_err(|e| {
            NonceError::new(format!("could not create the lease-state dir {dir}: {e}"))
        })?;
        let lock_path = dir_path.join("idempotency-nonce.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| {
                NonceError::new(format!(
                    "could not open the idempotency-nonce lock {}: {e}",
                    lock_path.display()
                ))
            })?;
        // Non-blocking exclusive lock: if a second owner holds it, fail closed
        // rather than block the spawn. Released automatically when `lock` drops.
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |e| {
                NonceError::new(format!(
                    "another process holds the idempotency-nonce lock {} \
                     (a second heartbeat owner on this lease dir?): {e}",
                    lock_path.display()
                ))
            },
        )?;
        Ok(Self {
            path: dir_path.join("idempotency-nonce"),
            _lock: lock,
        })
    }
}

#[cfg(feature = "heartbeat")]
impl multiview_licence::heartbeat::NonceStore for FileNonceStore {
    fn load(&self) -> Result<u64, multiview_licence::heartbeat::NonceError> {
        use multiview_licence::heartbeat::NonceError;
        match std::fs::read_to_string(&self.path) {
            // A PRESENT value that does not parse is NOT trustworthy — fail closed
            // (never a silent 0 that resets the high-water and re-mints a colliding
            // key after a restart).
            Ok(s) => s.trim().parse::<u64>().map_err(|_| {
                NonceError::new(format!(
                    "idempotency nonce file {} is present but unparseable",
                    self.path.display()
                ))
            }),
            // Absent on a fresh device (the common case) — a trusted start at 0.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            // Present-but-unreadable (permissions, I/O error) is untrustworthy too.
            Err(e) => Err(NonceError::new(format!(
                "could not read the idempotency nonce file {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn commit(&self, value: u64) -> Result<(), multiview_licence::heartbeat::NonceError> {
        use multiview_licence::heartbeat::NonceError;
        // Write-temp-then-rename for atomicity: a crash mid-write leaves either the
        // old value or the new one, never a truncated counter. BOTH steps fail
        // closed — an un-persisted high-water must block the mutation (the caller
        // refuses to send a possibly-colliding key), never continue best-effort.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, value.to_string()).map_err(|e| {
            NonceError::new(format!(
                "could not persist the idempotency nonce to {}: {e}",
                tmp.display()
            ))
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            NonceError::new(format!(
                "could not finalise the idempotency nonce file {}: {e}",
                self.path.display()
            ))
        })?;
        Ok(())
    }
}

/// The live Conspect HTTP transport (the cli-boundary implementation of
/// [`multiview_licence::heartbeat::LicenceServer`]). Owns the `reqwest` (rustls)
/// client so the leaf crate stays socket-free; carries the account JWT bearer +
/// a fresh `Idempotency-Key` on every mutation (ADR-0096 D2: account-JWT auth
/// today; device-PoP request-signing deferred). IPv6-first via `reqwest`'s
/// resolver; HTTPS only.
#[cfg(feature = "heartbeat")]
struct ConspectHttpServer {
    client: reqwest::Client,
    api_base: String,
    bearer_token: String,
}

#[cfg(feature = "heartbeat")]
impl ConspectHttpServer {
    /// Build the HTTPS-only transport. **Fails closed (round-6 panel):** the
    /// `https_only(true)` client is propagated with `?` — NOT `unwrap_or_default()`,
    /// which would silently fall back to a DEFAULT (non-HTTPS-only) client while
    /// every request still attaches the bearer JWT, leaking the account credential
    /// over plaintext `http://`. A failed build disables the heartbeat (the caller
    /// keeps last-good), never a credential-carrying non-HTTPS client.
    ///
    /// # Errors
    /// [`HeartbeatError::Transport`] if the HTTPS-only `reqwest` client cannot be
    /// built.
    fn new(
        mut api_base: String,
        bearer_token: String,
    ) -> Result<Self, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let client = reqwest::Client::builder()
            .user_agent(concat!("multiview/", env!("CARGO_PKG_VERSION")))
            .https_only(true)
            .build()
            .map_err(|e| {
                HeartbeatError::Transport(format!("could not build the HTTPS-only client: {e}"))
            })?;
        // Normalise the base URL in place (consume the owned String — no realloc).
        api_base.truncate(api_base.trim_end_matches('/').len());
        Ok(Self {
            client,
            api_base,
            bearer_token,
        })
    }

    /// `GET` + JSON-decode against `url`, mapping every failure to a
    /// [`HeartbeatError`] the loop treats as "keep last-good".
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: String,
    ) -> Result<T, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .send()
            .await
            .map_err(|e| HeartbeatError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(HeartbeatError::Transport(format!(
                "{} returned HTTP {}",
                url,
                resp.status()
            )));
        }
        resp.json::<T>()
            .await
            .map_err(|e| HeartbeatError::Malformed(e.to_string()))
    }

    /// `POST` `body` as JSON with the required `Idempotency-Key`, JSON-decode the
    /// response.
    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        url: String,
        body: &B,
        idempotency_key: &str,
    ) -> Result<T, multiview_licence::heartbeat::HeartbeatError> {
        use multiview_licence::heartbeat::HeartbeatError;
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.bearer_token)
            .header("Idempotency-Key", idempotency_key)
            .json(body)
            .send()
            .await
            .map_err(|e| HeartbeatError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(HeartbeatError::Transport(format!(
                "{} returned HTTP {}",
                url,
                resp.status()
            )));
        }
        resp.json::<T>()
            .await
            .map_err(|e| HeartbeatError::Malformed(e.to_string()))
    }
}

#[cfg(feature = "heartbeat")]
impl multiview_licence::heartbeat::LicenceServer for ConspectHttpServer {
    async fn fetch_keys(
        &self,
    ) -> Result<
        multiview_licence::heartbeat::LicensingKeys,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        // The well-known document is served from the API host root, not under the
        // versioned `/v0` base path.
        let host = self
            .api_base
            .rsplit_once("/v0")
            .map_or(self.api_base.as_str(), |(h, _)| h);
        self.get_json(format!("{host}/.well-known/conspect-licensing-keys.json"))
            .await
    }

    async fn heartbeat(
        &self,
        org: &str,
        req: multiview_licence::heartbeat::HeartbeatRequest,
        idempotency_key: &str,
    ) -> Result<
        multiview_licence::heartbeat::HeartbeatResponse,
        multiview_licence::heartbeat::HeartbeatError,
    > {
        self.post_json(
            format!("{}/organisations/{org}/heartbeat", self.api_base),
            &req,
            idempotency_key,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_signal_does_not_watermark() {
        assert!(!WatermarkSignal::clean().watermark());
        assert!(!WatermarkSignal::default().watermark());
    }

    #[test]
    fn a_watermark_rung_signals_a_watermark() {
        assert!(WatermarkSignal::at(EnforcementLevel::Watermark).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::BlockNewInstance).watermark());
        assert!(WatermarkSignal::at(EnforcementLevel::UnlicensedBuild).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::ConfigLocked).watermark());
        assert!(!WatermarkSignal::at(EnforcementLevel::Warning).watermark());
    }

    #[test]
    fn set_publishes_a_new_level_read_wait_free() {
        let signal = WatermarkSignal::clean();
        assert!(!signal.watermark());
        signal.set(EnforcementLevel::Watermark);
        assert!(
            signal.watermark(),
            "an off-thread set is visible to the next read"
        );
        assert_eq!(signal.level(), EnforcementLevel::Watermark);
    }

    #[test]
    fn refresh_from_an_empty_store_is_clean() {
        let store = LeaseStore::new();
        let signal = WatermarkSignal::at(EnforcementLevel::Watermark);
        refresh_signal(&store, &signal);
        assert!(
            !signal.watermark(),
            "an empty store fails toward leniency: no lapse evidence → clean"
        );
        assert_eq!(current_level(&store), None);
    }

    #[test]
    fn decode_hex_round_trips_and_rejects_garbage() {
        assert_eq!(decode_hex("00ff10"), Some(vec![0x00, 0xff, 0x10]));
        assert_eq!(decode_hex("ABcd"), Some(vec![0xab, 0xcd]));
        assert_eq!(decode_hex(""), Some(vec![]));
        // Odd length and non-hex are rejected (never a panic).
        assert_eq!(decode_hex("abc"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("0x10"), None);
    }

    // --- Round-6 BLOCKER 1: the file-backed durable nonce FAILS CLOSED. ----------

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_load_is_ok_zero_when_absent() {
        // A fresh device legitimately has no nonce file → 0 is a correct, trusted
        // value (NOT an error): the in-process monotone guard covers same-lifetime
        // reuse and there is no prior lifetime to collide with.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        assert_eq!(
            store
                .load()
                .expect("an absent nonce file loads as a trusted 0"),
            0
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_load_errors_on_a_corrupt_file() {
        // A PRESENT but unparseable nonce file must NOT be silently trusted as 0 —
        // that would reset the high-water and re-mint a colliding key after restart.
        // load() returns an Error so the mint fails closed.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idempotency-nonce");
        std::fs::write(&path, "not-a-number").expect("seed corrupt nonce");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        assert!(
            store.load().is_err(),
            "a present-but-corrupt nonce file must load as an Error, never a silent 0"
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_commit_errors_when_unwritable() {
        // commit() must PROPAGATE a write/rename failure (not log-and-continue): an
        // un-persisted high-water is exactly the cross-restart collision risk. After
        // a clean open, make the data path unwritable by replacing the nonce-file
        // location with a DIRECTORY (so write to it fails), and assert commit errors.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        // Replace the would-be nonce data file with a directory of the same name:
        // write-temp succeeds but the rename onto a non-empty directory fails.
        std::fs::create_dir(dir.path().join("idempotency-nonce")).expect("dir in the way");
        std::fs::create_dir(dir.path().join("idempotency-nonce").join("busy"))
            .expect("make the dir non-empty so rename-onto-it fails");
        assert!(
            store.commit(1).is_err(),
            "an unwritable nonce path must make commit() return an Error"
        );
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn file_nonce_store_round_trips_a_committed_value() {
        // The happy path still works: a committed value reloads exactly.
        use multiview_licence::heartbeat::NonceStore as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileNonceStore::in_dir(dir.path().to_str().expect("utf8 path"))
            .expect("first owner takes the lock");
        store.commit(7).expect("commit persists");
        assert_eq!(store.load().expect("reload"), 7);
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn a_second_nonce_owner_on_the_same_dir_is_refused_fail_closed() {
        // INTERPROCESS GUARD (round-6 residual): two minters sharing one lease-state
        // dir could load the same high-water and mint COLLIDING keys. The first owner
        // holds a non-blocking exclusive advisory lock; a second `in_dir` on the SAME
        // dir is refused, so its heartbeat declines to start (fail closed). Releasing
        // the first (drop) then lets a new owner acquire it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf8 path");
        let first = FileNonceStore::in_dir(path).expect("first owner takes the lock");
        assert!(
            FileNonceStore::in_dir(path).is_err(),
            "a second owner on the same lease dir must be refused (no colliding minters)"
        );
        drop(first);
        // Once the first owner releases the lock, a fresh owner can take it.
        assert!(
            FileNonceStore::in_dir(path).is_ok(),
            "after the first owner drops, the lock is available again"
        );
    }

    // --- Round-6 MAJOR: the HTTP transport is HTTPS-only and FAILS CLOSED. -------

    #[cfg(feature = "heartbeat")]
    #[tokio::test]
    async fn the_transport_refuses_a_plaintext_http_base_https_only() {
        // The client must NEVER attach the bearer JWT to a plaintext http:// request
        // (credential leak). The reqwest client is built https_only(true) and the
        // constructor FAILS CLOSED (no unwrap_or_default fallback that would drop
        // https_only), so a heartbeat against an http:// base is rejected at the
        // transport with NO plaintext request leaving the host.
        use multiview_licence::heartbeat::{DeviceIdentity, HeartbeatClient, LicenceServer as _};
        let server = ConspectHttpServer::new(
            "http://insecure.example.invalid/v0".to_owned(),
            "super-secret-bearer".to_owned(),
        )
        .expect("an https-only client builds");
        // `HeartbeatRequest` is #[non_exhaustive]; build it via the public builder.
        let identity = DeviceIdentity {
            machine_id: "m".to_owned(),
            fingerprint_digest: "0".repeat(64),
            fingerprint_score: 0,
            binding_id: Some("ib_x".to_owned()),
            app_version: "test".to_owned(),
            instance_id: String::new(),
            hardware_digest: String::new(),
            instance_discriminator_hash: String::new(),
            instance_discriminator_digest: String::new(),
            device_public_key_b64url: String::new(),
        };
        let req = HeartbeatClient::<ConspectHttpServer>::build_heartbeat_request(&identity, None);
        let res = server.heartbeat("org_x", req, "mv-m-1").await;
        assert!(
            res.is_err(),
            "an http:// base must be refused by the https-only client (no plaintext \
             credential-carrying request)"
        );
    }
}
