//! Display-node enrollment & pairing (DEV-B6, ADR-0045 / managed-devices §9).
//!
//! A `multiview node` appliance becomes a managed [`Device`](multiview_config::Device)
//! by **enrolling** against the controller and binding to an Ed25519 keypair it
//! generates on first start — there are no passwords, nothing to rotate
//! (ADR-0045). Two paths reach an enrolled device:
//!
//! * **Zero-touch (token):** the operator mints a TTL'd enrollment token in
//!   Settings → Display Nodes (one-time display, hashed at rest); the node
//!   presents it once at `POST /devices/enroll` and appears as a `displaynode`
//!   device, ONLINE, immediately. The token is one-time: consumed by the first
//!   keypair that redeems it, refused for any other.
//! * **Screen pairing (no token):** a node that boots without a token shows a
//!   pairing card (a six-character code in the unambiguous alphabet + a QR) on
//!   its attached display and long-polls `POST /devices/enroll`, which answers
//!   `202` with that code. The operator reads the code off the screen and
//!   completes it at `POST /devices/pair`; the node's next poll flips to
//!   `enrolled` with the operator-chosen id.
//!
//! Thereafter the node proves liveness with a **keypair-signed heartbeat**
//! (`POST /devices/{id}/heartbeat`): an Ed25519 signature over a
//! [`canonical_message`] of the request, with a strictly-increasing UNIX
//! timestamp (replay defence) inside a freshness window. The heartbeat answers
//! with the node's current display assignment, and re-reports the node's display
//! heads (the ADR-M009 facet (c) projection read by `GET
//! /devices/{id}/display-heads`).
//!
//! ## Isolation (invariant #10)
//!
//! Every store here is plain control-plane state behind a `Mutex`; the engine
//! never produces, forwards, or awaits anything in this module, and the
//! pending-pairing table is **bounded** (drop the request, never grow). Nothing
//! here can back-pressure the engine — the same proof shape as the device
//! registry and broadcaster.

use std::collections::HashMap;
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq as _;

/// The default enrollment-token TTL when the operator does not specify one
/// (one hour) — long enough to flash a node and let it boot, short enough that
/// a leaked token is not a standing liability.
pub const DEFAULT_TOKEN_TTL_SECS: u64 = 3_600;

/// The smallest accepted enrollment-token TTL (one minute): a shorter window is
/// a configuration mistake — the node would not finish booting before it lapses.
pub const MIN_TOKEN_TTL_SECS: u64 = 60;

/// The largest accepted enrollment-token TTL (seven days): a longer-lived token
/// is a standing credential, which the one-time-display model exists to avoid.
pub const MAX_TOKEN_TTL_SECS: u64 = 7 * 24 * 3_600;

/// The cadence the controller tells a node to heartbeat at (seconds). The node
/// signs and POSTs one heartbeat per interval; the controller marks it stale if
/// none arrives within a few intervals (the device status lane's staleness).
pub const HEARTBEAT_SECS: u64 = 10;

/// How long a node should wait before re-polling `enroll` while it is pairing
/// (seconds) — the long-poll retry cadence shown on the node's pairing card.
pub const PAIRING_RETRY_SECS: u64 = 5;

/// The freshness window for a signed heartbeat (seconds): a heartbeat whose
/// timestamp is more than this far from the controller's clock is refused, so a
/// captured-then-replayed heartbeat cannot be accepted long after the fact. The
/// per-device strictly-increasing-timestamp rule defends against replay inside
/// the window.
pub const HEARTBEAT_FRESHNESS_SECS: u64 = 300;

/// The maximum number of concurrent **pending** pairing requests held at once
/// (bounded memory — invariant #10): a 33rd distinct unpaired node is shed with
/// `429` rather than growing the table without bound.
pub const MAX_PENDING_PAIRINGS: usize = 32;

/// The pairing-code alphabet: upper-case letters and digits with the visually
/// ambiguous `0`/`O`/`1`/`I` removed, since the code is read off a screen and
/// typed by an operator (WCAG-honest legibility, managed-devices §9).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// The pairing-code length (six characters — managed-devices §9).
const CODE_LEN: usize = 6;

/// A wall-clock source in **milliseconds since the Unix epoch**, injectable so
/// tests can cross a TTL deterministically.
pub type MillisClock = std::sync::Arc<dyn Fn() -> u64 + Send + Sync>;

/// The default millisecond clock: the system wall clock since the Unix epoch
/// (saturating to `0` on a pre-epoch clock, never panicking).
fn system_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The lifecycle state of a minted enrollment token, as the admin list reports
/// it. `#[non_exhaustive]` so a future state does not break the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TokenState {
    /// Minted, not yet redeemed, revoked, or expired.
    Pending,
    /// Redeemed by a node — `used_by` names the device created.
    Used,
    /// Explicitly revoked by an operator (the `DELETE` route).
    Revoked,
    /// Past its TTL (reported lazily on read; never redeemable).
    Expired,
}

/// One minted enrollment token's at-rest record: its id, the **hash** of its
/// secret (never the secret itself), its timing, and its redemption state.
#[derive(Debug, Clone)]
struct TokenRecord {
    /// The public token id (`enr-…`), the stable handle the admin list/revoke
    /// address it by.
    id: String,
    /// SHA-256 of the token secret — the at-rest form. The plaintext secret is
    /// returned exactly once at mint and never stored.
    secret_hash: [u8; 32],
    /// When the token was minted (epoch seconds).
    created_epoch_s: u64,
    /// When the token expires (epoch seconds).
    expires_epoch_s: u64,
    /// Whether the operator revoked it.
    revoked: bool,
    /// The device id that redeemed it, once used (one-time).
    used_by: Option<String>,
}

impl TokenRecord {
    /// The token's effective state at wall-clock `now_s` (expiry is reported
    /// lazily: a token past its TTL reads `expired` without a sweep).
    fn state(&self, now_s: u64) -> TokenState {
        if self.used_by.is_some() {
            TokenState::Used
        } else if self.revoked {
            TokenState::Revoked
        } else if now_s >= self.expires_epoch_s {
            TokenState::Expired
        } else {
            TokenState::Pending
        }
    }

    /// Whether this token can still be redeemed at `now_s` (pending and unexpired).
    fn is_redeemable(&self, now_s: u64) -> bool {
        matches!(self.state(now_s), TokenState::Pending)
    }
}

/// The admin-facing metadata for one enrollment token — never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct TokenSummary {
    /// The token id (`enr-…`).
    pub token_id: String,
    /// The token's current lifecycle state.
    pub state: TokenState,
    /// When it was minted (epoch seconds).
    pub created_epoch_s: u64,
    /// When it expires (epoch seconds).
    pub expires_epoch_s: u64,
    /// The device id that redeemed it, when `state == used`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_by: Option<String>,
}

/// The one-time mint response: the **only** time the bearer secret is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct MintedToken {
    /// The token id (`enr-…`).
    pub token_id: String,
    /// The bearer token in `<token_id>.<secret>` form — shown once, never stored
    /// (only its hash is). The operator copies it into the node now or never.
    pub token: String,
    /// When it was minted (epoch seconds).
    pub created_epoch_s: u64,
    /// When it expires (epoch seconds).
    pub expires_epoch_s: u64,
}

/// One display head a node reported (the ADR-M009 facet (c) projection): a
/// physical scanout output, EDID-derived where present. Reported at enrollment
/// and re-reported on every heartbeat; read by `GET /devices/{id}/display-heads`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct DisplayHead {
    /// A stable head id within the node (node-assigned, e.g. `head-0`).
    pub id: String,
    /// The KMS connector name driving this head (`HDMI-A-1`, `DP-1`, …).
    pub connector: String,
    /// Head width in pixels.
    pub width: u32,
    /// Head height in pixels.
    pub height: u32,
    /// Head refresh rate in millihertz (exact: `60_000` is 60.000 Hz — never a
    /// float, invariant #3).
    pub refresh_millihertz: u32,
    /// Whether a sink is currently connected to this head.
    pub connected: bool,
}

/// The bound identity of an enrolled node: the keypair it authenticates with and
/// the last heartbeat timestamp seen (the strictly-increasing replay guard).
#[derive(Debug, Clone)]
struct NodeIdentity {
    /// The node's Ed25519 public key (32 raw bytes), the heartbeat verifier.
    public_key: VerifyingKey,
    /// The highest heartbeat timestamp accepted so far (epoch seconds): a
    /// heartbeat at or below this is a replay and is refused.
    last_ts: u64,
    /// The node's last-reported display heads (the projection backing store).
    heads: Vec<DisplayHead>,
}

/// A pending screen-pairing request: a node showing a code, awaiting the
/// operator. Keyed by the node's public key (a re-poll from the same key returns
/// the same code).
#[derive(Debug, Clone)]
struct PendingPairing {
    /// The pairing code shown on the node's screen (stored upper-case).
    code: String,
    /// The node's Ed25519 public key — bound to the device on completion.
    public_key: VerifyingKey,
    /// The node's reported model (operator-facing metadata in the pending list).
    model: String,
    /// The node's reported name (the device's display name when paired).
    node_name: String,
    /// The node's reported display heads (carried onto the device when paired).
    heads: Vec<DisplayHead>,
    /// When the request first pended (epoch seconds) — the eviction key.
    created_epoch_s: u64,
    /// The operator-chosen device id once paired (the node's next poll reads it).
    paired_device_id: Option<String>,
}

/// One operator-facing pending-pairing row — model/name metadata only, never the
/// code (the code stays on the node's screen) or the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct PairingRequestSummary {
    /// A stable handle for this pending request (the public-key fingerprint),
    /// so the operator can disambiguate two unnamed nodes.
    pub fingerprint: String,
    /// The node's reported model.
    pub model: String,
    /// The node's reported name.
    pub node_name: String,
    /// When the request first pended (epoch seconds).
    pub created_epoch_s: u64,
}

/// The outcome of `POST /devices/enroll` — either an enrolled device or a
/// pending pairing the node should keep polling. The route maps `Enrolled` to
/// `200` and `Pairing` to `202`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollOutcome {
    /// The node is enrolled: bound to `device_id` and should start heartbeating.
    Enrolled {
        /// The device id the node is bound to.
        device_id: String,
        /// The heartbeat cadence the node should keep (seconds).
        heartbeat_secs: u64,
    },
    /// The node must pair: show this code (and QR) and re-poll after the delay.
    Pairing {
        /// The six-character pairing code to display.
        pairing_code: String,
        /// How long to wait before the next poll (seconds).
        retry_secs: u64,
    },
}

/// Why an enrollment, pairing, or heartbeat was refused (mapped to HTTP at the
/// route boundary). Kept distinct so the route returns the right status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnrollError {
    /// A presented token was unknown, revoked, expired, or already consumed by a
    /// different key — `401`. (One-time-use is enforced here, not by the caller.)
    TokenRejected,
    /// A submitted field was malformed (e.g. a public key that is not 32 base64
    /// bytes) — `422`. The string names the offending field.
    Invalid(String),
    /// The pending-pairing table is full — `429` (bounded memory, invariant #10).
    PairingTableFull,
    /// A signed heartbeat failed verification (unknown device, wrong key, stale
    /// or replayed timestamp, malformed signature) — `401`.
    Unauthorized,
}

/// The outcome of completing a pairing at `POST /devices/pair`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairCompletion {
    /// The code matched a pending request, now marked paired to `device_id`.
    /// The route creates the device and `201`s.
    Completed {
        /// The (operator-chosen or generated) device id.
        device_id: String,
        /// The node's reported display name.
        node_name: String,
        /// The node's public key (base64), bound onto the created device.
        public_key_b64: String,
        /// The node's reported display heads, carried onto the device.
        heads: Vec<DisplayHead>,
    },
    /// No pending request carries that code — the route `404`s.
    NotFound,
}

/// A verified heartbeat: the bound device id, and the heads the node re-reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedHeartbeat {
    /// The device id the heartbeat authenticated as.
    pub device_id: String,
    /// The heads the node re-reported (the refreshed projection).
    pub heads: Vec<DisplayHead>,
    /// The heartbeat cadence to echo back to the node (seconds).
    pub heartbeat_secs: u64,
}

/// The body a node submits to `POST /devices/enroll`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct EnrollRequest {
    /// The one-time enrollment token (`<id>.<secret>`), or absent for the
    /// screen-pairing path.
    #[serde(default)]
    pub token: Option<String>,
    /// The node's Ed25519 public key (standard base64 of the 32 raw bytes).
    pub public_key: String,
    /// The node's hardware model (operator-facing metadata).
    #[serde(default)]
    pub model: String,
    /// The node's human-friendly name (becomes the device display name).
    #[serde(default)]
    pub node_name: String,
    /// The node's EDID-derived display heads.
    #[serde(default)]
    pub heads: Vec<DisplayHead>,
}

/// The body an operator submits to `POST /devices/pair`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct PairRequest {
    /// The six-character code read off the node's screen (case-insensitive).
    pub code: String,
    /// The device id to assign the paired node (generated when omitted).
    #[serde(default)]
    pub device_id: Option<String>,
    /// An optional display name override for the paired device.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// The body a node signs and submits to `POST /devices/{id}/heartbeat`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub struct HeartbeatBody {
    /// The node's current display heads (the refreshed projection).
    #[serde(default)]
    pub heads: Vec<DisplayHead>,
    /// The node's reported temperature (°C), if it measures one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f32>,
}

/// The canonical, signed message for a node request: the method, the request
/// path, the node/device id, the UNIX-second timestamp, and the SHA-256 of the
/// body — joined by `\n`. Both the node (signing) and the controller (verifying)
/// derive **exactly** this string, so a single source of truth pins the wire
/// form. Hashing the body keeps the signed message bounded regardless of body
/// size and binds the signature to the exact bytes sent.
#[must_use]
pub fn canonical_message(
    method: &str,
    path: &str,
    device_id: &str,
    ts: u64,
    body: &[u8],
) -> String {
    let body_hash = Sha256::digest(body);
    let body_hex = hex_lower(&body_hash);
    format!("{method}\n{path}\n{device_id}\n{ts}\n{body_hex}")
}

/// Lower-case hex of a byte slice (no `as` casts; small fixed alphabet).
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        let hi = usize::from(b >> 4);
        let lo = usize::from(b & 0x0f);
        out.push(char::from(HEX[hi]));
        out.push(char::from(HEX[lo]));
    }
    out
}

/// The lock-guarded enrollment state: minted tokens, pending pairings, and bound
/// node identities.
#[derive(Default)]
struct EnrollInner {
    /// Minted tokens, keyed by token id.
    tokens: HashMap<String, TokenRecord>,
    /// Pending screen pairings, keyed by the node public-key fingerprint.
    pending: HashMap<String, PendingPairing>,
    /// Bound node identities, keyed by device id.
    identities: HashMap<String, NodeIdentity>,
    /// A monotonically increasing counter feeding token/device id suffixes, so
    /// ids are unique without leaking a sequence the way a raw count would.
    seq: u64,
}

/// The display-node enrollment & pairing state (DEV-B6).
///
/// One instance lives on the [`AppState`](crate::state::AppState); the
/// enrollment routes drive it. All state is control-plane only, behind one
/// `Mutex`, and bounded — it can never back-pressure the engine (invariant #10).
pub struct NodeEnrollState {
    inner: Mutex<EnrollInner>,
    /// The injected wall-clock (milliseconds since the Unix epoch). Tests cross
    /// a TTL deterministically by advancing it.
    clock: MillisClock,
}

impl std::fmt::Debug for NodeEnrollState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let guard = self.lock();
        f.debug_struct("NodeEnrollState")
            .field("tokens", &guard.tokens.len())
            .field("pending", &guard.pending.len())
            .field("identities", &guard.identities.len())
            .finish()
    }
}

impl Default for NodeEnrollState {
    fn default() -> Self {
        Self::new()
    }
}

impl NodeEnrollState {
    /// A fresh enrollment state with the system wall clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(std::sync::Arc::new(system_millis))
    }

    /// A fresh enrollment state with an injected millisecond clock (tests).
    #[must_use]
    pub fn with_clock(clock: MillisClock) -> Self {
        Self {
            inner: Mutex::new(EnrollInner::default()),
            clock,
        }
    }

    /// Lock the inner state, recovering from a poisoned lock (a panic in one
    /// request must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, EnrollInner> {
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The current wall clock in whole epoch seconds.
    fn now_s(&self) -> u64 {
        (self.clock)() / 1_000
    }

    // --- Enrollment tokens ---------------------------------------------------

    /// Mint a one-time enrollment token with `ttl_secs` (defaulting to
    /// [`DEFAULT_TOKEN_TTL_SECS`]). Returns the bearer token **once** — only its
    /// hash is retained.
    ///
    /// # Errors
    ///
    /// [`EnrollError::Invalid`] when `ttl_secs` is outside
    /// `[MIN_TOKEN_TTL_SECS, MAX_TOKEN_TTL_SECS]`.
    pub fn mint_token(&self, ttl_secs: Option<u64>) -> Result<MintedToken, EnrollError> {
        let ttl = ttl_secs.unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        if !(MIN_TOKEN_TTL_SECS..=MAX_TOKEN_TTL_SECS).contains(&ttl) {
            return Err(EnrollError::Invalid(format!(
                "ttl_secs {ttl} is out of range ({MIN_TOKEN_TTL_SECS}..={MAX_TOKEN_TTL_SECS})"
            )));
        }
        let now = self.now_s();
        let mut guard = self.lock();
        guard.seq = guard.seq.wrapping_add(1);
        let token_id = format!("enr-{}", random_hex(8));
        let secret = random_hex(32);
        let bearer = format!("{token_id}.{secret}");
        let secret_hash = sha256(secret.as_bytes());
        let created_epoch_s = now;
        let expires_epoch_s = now.saturating_add(ttl);
        guard.tokens.insert(
            token_id.clone(),
            TokenRecord {
                id: token_id.clone(),
                secret_hash,
                created_epoch_s,
                expires_epoch_s,
                revoked: false,
                used_by: None,
            },
        );
        Ok(MintedToken {
            token_id,
            token: bearer,
            created_epoch_s,
            expires_epoch_s,
        })
    }

    /// Every minted token's metadata (id-sorted), never the secret.
    #[must_use]
    pub fn list_tokens(&self) -> Vec<TokenSummary> {
        let now = self.now_s();
        let guard = self.lock();
        let mut out: Vec<TokenSummary> = guard
            .tokens
            .values()
            .map(|t| TokenSummary {
                token_id: t.id.clone(),
                state: t.state(now),
                created_epoch_s: t.created_epoch_s,
                expires_epoch_s: t.expires_epoch_s,
                used_by: t.used_by.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        out
    }

    /// Revoke a token by id. Returns `true` when a token was revoked, `false`
    /// when no token carries that id (the route `404`s on `false`).
    pub fn revoke_token(&self, token_id: &str) -> bool {
        let mut guard = self.lock();
        match guard.tokens.get_mut(token_id) {
            Some(record) => {
                record.revoked = true;
                true
            }
            None => false,
        }
    }

    // --- Enrollment ----------------------------------------------------------

    /// Drive `POST /devices/enroll` for a node presenting `request`.
    ///
    /// * A valid one-time token (and a well-formed key) → [`EnrollOutcome::Enrolled`],
    ///   binding the key to a fresh `displaynode` device id.
    /// * A key already bound (re-poll after reboot, the one-time token gone) →
    ///   the same device id, no token needed (idempotent).
    /// * A key with a completed pairing → the operator-chosen device id.
    /// * No usable token and no binding → [`EnrollOutcome::Pairing`] with a
    ///   stable six-character code (the same code on every re-poll).
    ///
    /// # Errors
    ///
    /// [`EnrollError::Invalid`] (malformed key), [`EnrollError::TokenRejected`]
    /// (bad/used/expired token), or [`EnrollError::PairingTableFull`].
    pub fn enroll(&self, request: &EnrollRequest) -> Result<EnrollOutcome, EnrollError> {
        let public_key = parse_public_key(&request.public_key)?;
        let fingerprint = key_fingerprint(&public_key);
        let now = self.now_s();
        let mut guard = self.lock();

        // 1) Already bound (a reboot re-poll, or a token already consumed): the
        //    same keypair maps to the SAME device — never a duplicate record.
        if let Some(device_id) = guard
            .identities
            .iter()
            .find(|(_, id)| id.public_key == public_key)
            .map(|(device_id, _)| device_id.clone())
        {
            return Ok(EnrollOutcome::Enrolled {
                device_id,
                heartbeat_secs: HEARTBEAT_SECS,
            });
        }

        // 2) A completed screen pairing for this key: flip the node to enrolled
        //    with the operator-chosen id and bind it now.
        if let Some(device_id) = guard
            .pending
            .get(&fingerprint)
            .and_then(|p| p.paired_device_id.clone())
        {
            let pending = guard.pending.remove(&fingerprint);
            if let Some(pending) = pending {
                guard.identities.insert(
                    device_id.clone(),
                    NodeIdentity {
                        public_key,
                        last_ts: 0,
                        heads: pending.heads,
                    },
                );
            }
            return Ok(EnrollOutcome::Enrolled {
                device_id,
                heartbeat_secs: HEARTBEAT_SECS,
            });
        }

        // 3) A presented token: redeem it one-time and bind a fresh device.
        if let Some(bearer) = request.token.as_deref() {
            let device_id = self.redeem_token(&mut guard, bearer, now)?;
            guard.identities.insert(
                device_id.clone(),
                NodeIdentity {
                    public_key,
                    last_ts: 0,
                    heads: request.heads.clone(),
                },
            );
            return Ok(EnrollOutcome::Enrolled {
                device_id,
                heartbeat_secs: HEARTBEAT_SECS,
            });
        }

        // 4) No token, no binding: the screen-pairing path. Re-polling returns
        //    the SAME code; a brand-new node takes a pending slot (bounded).
        if let Some(pending) = guard.pending.get(&fingerprint) {
            let code = pending.code.clone();
            return Ok(EnrollOutcome::Pairing {
                pairing_code: code,
                retry_secs: PAIRING_RETRY_SECS,
            });
        }
        if guard.pending.len() >= MAX_PENDING_PAIRINGS {
            return Err(EnrollError::PairingTableFull);
        }
        let code = self.fresh_code(&guard);
        guard.pending.insert(
            fingerprint,
            PendingPairing {
                code: code.clone(),
                public_key,
                model: request.model.clone(),
                node_name: request.node_name.clone(),
                heads: request.heads.clone(),
                created_epoch_s: now,
                paired_device_id: None,
            },
        );
        Ok(EnrollOutcome::Pairing {
            pairing_code: code,
            retry_secs: PAIRING_RETRY_SECS,
        })
    }

    /// Redeem a bearer token under the lock, minting a fresh `displaynode`
    /// device id and marking the token used. One-time: a token already used (or
    /// revoked/expired/unknown) is refused.
    fn redeem_token(
        &self,
        guard: &mut EnrollInner,
        bearer: &str,
        now_s: u64,
    ) -> Result<String, EnrollError> {
        let (token_id, secret) = bearer.split_once('.').ok_or(EnrollError::TokenRejected)?;
        let presented_hash = sha256(secret.as_bytes());
        let record = guard
            .tokens
            .get(token_id)
            .ok_or(EnrollError::TokenRejected)?;
        // Constant-time hash compare so a redeem attempt cannot probe the secret
        // by timing.
        if record.secret_hash.ct_eq(&presented_hash).unwrap_u8() != 1 {
            return Err(EnrollError::TokenRejected);
        }
        if !record.is_redeemable(now_s) {
            return Err(EnrollError::TokenRejected);
        }
        guard.seq = guard.seq.wrapping_add(1);
        let device_id = format!("node-{}", random_hex(8));
        if let Some(record) = guard.tokens.get_mut(token_id) {
            record.used_by = Some(device_id.clone());
        }
        Ok(device_id)
    }

    /// Mint a fresh pairing code not currently shown by another pending node
    /// (so two simultaneous pairings never collide).
    fn fresh_code(&self, guard: &EnrollInner) -> String {
        // A handful of tries is plenty: the alphabet is 31^6 ≈ 8.9e8, and at
        // most 32 codes are live, so a collision is vanishingly rare. After the
        // bounded tries (never an unbounded loop) we accept the last candidate.
        for _ in 0..8 {
            let code = random_code();
            if !guard.pending.values().any(|p| p.code == code) {
                return code;
            }
        }
        random_code()
    }

    // --- Pairing completion --------------------------------------------------

    /// Complete a screen pairing: find the pending request carrying `code`
    /// (case-insensitive), assign it `device_id` (or a generated one), and mark
    /// it paired so the node's next `enroll` poll flips to enrolled.
    ///
    /// Returns [`PairCompletion::NotFound`] when no pending request carries the
    /// code (the route `404`s). The caller is responsible for the device-id
    /// uniqueness check (`409`) and for creating the device record.
    pub fn complete_pairing(
        &self,
        code: &str,
        device_id: Option<&str>,
        display_name: Option<&str>,
    ) -> PairCompletion {
        let wanted = code.trim().to_ascii_uppercase();
        let mut guard = self.lock();
        let Some(fingerprint) = guard
            .pending
            .iter()
            .find(|(_, p)| p.code == wanted && p.paired_device_id.is_none())
            .map(|(fingerprint, _)| fingerprint.clone())
        else {
            return PairCompletion::NotFound;
        };
        let assigned = match device_id {
            Some(id) => id.to_owned(),
            None => {
                guard.seq = guard.seq.wrapping_add(1);
                format!("node-{}", random_hex(8))
            }
        };
        let Some(pending) = guard.pending.get_mut(&fingerprint) else {
            return PairCompletion::NotFound;
        };
        pending.paired_device_id = Some(assigned.clone());
        let node_name = display_name
            .map(str::to_owned)
            .unwrap_or_else(|| pending.node_name.clone());
        let public_key_b64 = BASE64.encode(pending.public_key.to_bytes());
        let heads = pending.heads.clone();
        PairCompletion::Completed {
            device_id: assigned,
            node_name,
            public_key_b64,
            heads,
        }
    }

    /// Undo a pairing assignment (the caller's device create failed, e.g. a
    /// `409` id collision): clear `paired_device_id` so the code stays valid and
    /// the operator can retry with a different id.
    pub fn unassign_pairing(&self, device_id: &str) {
        let mut guard = self.lock();
        for pending in guard.pending.values_mut() {
            if pending.paired_device_id.as_deref() == Some(device_id) {
                pending.paired_device_id = None;
            }
        }
    }

    /// Every operator-facing pending-pairing row (id-sorted by fingerprint),
    /// model/name metadata only — never the code or the key.
    #[must_use]
    pub fn list_pairing_requests(&self) -> Vec<PairingRequestSummary> {
        let guard = self.lock();
        let mut out: Vec<PairingRequestSummary> = guard
            .pending
            .iter()
            .filter(|(_, p)| p.paired_device_id.is_none())
            .map(|(fingerprint, p)| PairingRequestSummary {
                fingerprint: fingerprint.clone(),
                model: p.model.clone(),
                node_name: p.node_name.clone(),
                created_epoch_s: p.created_epoch_s,
            })
            .collect();
        out.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        out
    }

    // --- Heartbeats ----------------------------------------------------------

    /// Verify a keypair-signed heartbeat and, on success, advance the device's
    /// replay guard and refresh its display-head projection.
    ///
    /// `signature_b64` is the base64 Ed25519 signature over
    /// [`canonical_message`]`("POST", path, device_id, ts, body)`. The heartbeat
    /// is refused (`401`) when the device is unknown, the signature does not
    /// verify against the bound key, the timestamp is outside the freshness
    /// window, or the timestamp is not strictly greater than the last accepted
    /// one (replay).
    ///
    /// # Errors
    ///
    /// [`EnrollError::Unauthorized`] on any verification failure.
    pub fn verify_heartbeat(
        &self,
        device_id: &str,
        ts: u64,
        signature_b64: &str,
        path: &str,
        body: &[u8],
        heads: Vec<DisplayHead>,
    ) -> Result<VerifiedHeartbeat, EnrollError> {
        let now = self.now_s();
        // Freshness: a timestamp too far from our clock (either direction) is
        // refused — a captured heartbeat cannot be replayed long after the fact.
        let delta = now.abs_diff(ts);
        if delta > HEARTBEAT_FRESHNESS_SECS {
            return Err(EnrollError::Unauthorized);
        }
        let signature = parse_signature(signature_b64).ok_or(EnrollError::Unauthorized)?;
        let message = canonical_message("POST", path, device_id, ts, body);
        let mut guard = self.lock();
        let identity = guard
            .identities
            .get_mut(device_id)
            .ok_or(EnrollError::Unauthorized)?;
        // Strictly-increasing timestamp per device: an exact replay or an older
        // timestamp is refused.
        if ts <= identity.last_ts {
            return Err(EnrollError::Unauthorized);
        }
        identity
            .public_key
            .verify_strict(message.as_bytes(), &signature)
            .map_err(|_| EnrollError::Unauthorized)?;
        identity.last_ts = ts;
        identity.heads = heads.clone();
        Ok(VerifiedHeartbeat {
            device_id: device_id.to_owned(),
            heads,
            heartbeat_secs: HEARTBEAT_SECS,
        })
    }

    // --- Projection + lifecycle ---------------------------------------------

    /// The display heads last reported by `device_id`, or [`None`] when no node
    /// identity is bound to that id (the route `404`s on `None`).
    #[must_use]
    pub fn display_heads(&self, device_id: &str) -> Option<Vec<DisplayHead>> {
        self.lock()
            .identities
            .get(device_id)
            .map(|id| id.heads.clone())
    }

    /// Whether a node identity is bound to `device_id`.
    #[must_use]
    pub fn is_enrolled(&self, device_id: &str) -> bool {
        self.lock().identities.contains_key(device_id)
    }

    /// Forget a node's bound identity (the device was deleted): the keypair no
    /// longer authenticates a heartbeat, and a fresh enroll from that key is
    /// back to the pairing flow. Any stale pending pairing for that device is
    /// dropped too, so the binding is genuinely gone, not cached.
    pub fn forget(&self, device_id: &str) {
        let mut guard = self.lock();
        guard.identities.remove(device_id);
        guard
            .pending
            .retain(|_, p| p.paired_device_id.as_deref() != Some(device_id));
    }
}

/// Parse a node public key from standard base64 of 32 raw Ed25519 bytes.
fn parse_public_key(b64: &str) -> Result<VerifyingKey, EnrollError> {
    let bytes = BASE64
        .decode(b64.trim())
        .map_err(|_| EnrollError::Invalid("public_key is not valid base64".to_owned()))?;
    let array: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| EnrollError::Invalid("public_key must be 32 bytes".to_owned()))?;
    VerifyingKey::from_bytes(&array)
        .map_err(|_| EnrollError::Invalid("public_key is not a valid Ed25519 key".to_owned()))
}

/// Parse a base64 Ed25519 signature (64 raw bytes), returning [`None`] on any
/// malformation (the heartbeat is then `401`, never a `500`).
fn parse_signature(b64: &str) -> Option<Signature> {
    let bytes = BASE64.decode(b64.trim()).ok()?;
    let array: [u8; 64] = bytes.as_slice().try_into().ok()?;
    Some(Signature::from_bytes(&array))
}

/// A stable fingerprint of a public key (the first 16 hex chars of its SHA-256),
/// the pending-pairing map key and the operator-facing handle.
fn key_fingerprint(key: &VerifyingKey) -> String {
    let digest = sha256(&key.to_bytes());
    hex_lower(&digest).chars().take(16).collect()
}

/// SHA-256 of a byte slice as a fixed array.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// `n` bytes of CSPRNG randomness rendered as lower-case hex (`2n` chars). Falls
/// back to a clock-derived value if the OS RNG is briefly unavailable — never
/// panics on the control plane.
fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Defensive fallback (the OS RNG failing is not expected): derive from
        // the monotonic clock so a token is still unique within a run.
        let now = system_millis();
        for (i, b) in bytes.iter_mut().enumerate() {
            let shift = u32::try_from((i % 8) * 8).unwrap_or(0);
            *b = u8::try_from((now >> shift) & 0xff).unwrap_or(0);
        }
    }
    hex_lower(&bytes)
}

/// A fresh six-character pairing code from the unambiguous alphabet, drawn from
/// the OS CSPRNG (rejection-free modulo: 256 mod 31 bias is negligible for a
/// human-typed pairing code, and the alphabet length is fixed at compile time).
fn random_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    if getrandom::getrandom(&mut bytes).is_err() {
        let now = system_millis();
        for (i, b) in bytes.iter_mut().enumerate() {
            let shift = u32::try_from((i % 8) * 8).unwrap_or(0);
            *b = u8::try_from((now >> shift) & 0xff).unwrap_or(0);
        }
    }
    let len = CODE_ALPHABET.len();
    bytes
        .iter()
        .map(|&b| {
            let idx = usize::from(b) % len;
            char::from(CODE_ALPHABET.get(idx).copied().unwrap_or(b'A'))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn canonical_message_is_deterministic_and_body_bound() {
        let a = canonical_message("POST", "/p", "dev", 10, b"body");
        let b = canonical_message("POST", "/p", "dev", 10, b"body");
        assert_eq!(a, b, "the canonical message is a pure function of its parts");
        let c = canonical_message("POST", "/p", "dev", 10, b"BODY");
        assert_ne!(a, c, "a different body hashes differently");
        assert!(a.contains("dev"));
        assert!(a.contains("\n10\n"));
    }

    #[test]
    fn token_ttl_is_range_checked() {
        let state = NodeEnrollState::new();
        assert!(matches!(
            state.mint_token(Some(1)),
            Err(EnrollError::Invalid(_))
        ));
        assert!(matches!(
            state.mint_token(Some(MAX_TOKEN_TTL_SECS + 1)),
            Err(EnrollError::Invalid(_))
        ));
        assert!(state.mint_token(Some(MIN_TOKEN_TTL_SECS)).is_ok());
        assert!(state.mint_token(None).is_ok());
    }

    #[test]
    fn pairing_code_uses_the_unambiguous_alphabet() {
        for _ in 0..256 {
            let code = random_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code
                .chars()
                .all(|c| "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(c)));
        }
    }

    #[test]
    fn random_hex_is_the_right_length_and_lowercase() {
        let h = random_hex(8);
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }
}
