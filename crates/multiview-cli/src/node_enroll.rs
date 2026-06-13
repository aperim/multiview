//! The `multiview node` enrollment client (DEV-B6, ADR-0045 §9).
//!
//! A node whose config carries a `[controller]` block becomes a managed
//! `displaynode` device: it generates (and persists) an Ed25519 keypair on first
//! start, enrolls against the controller — presenting a one-time token for
//! zero-touch, else showing a pairing card and long-polling — and then proves
//! liveness with keypair-signed heartbeats.
//!
//! This module is split into a **pure core** that is always compiled and tested
//! ([`NodeIdentity`] keypair generation/persistence, the enroll-request body
//! builder, and the heartbeat signer, which reuses the controller's
//! [`canonical_message`](multiview_control::canonical_message) so the signed
//! wire form has one source of truth) and a **live runner** behind the
//! off-by-default `node-enroll` feature (the `reqwest` HTTP transport). The
//! baseline build pulls no socket; the run wiring is feature-gated and additive
//! — a node with no `[controller]` block runs exactly as DEV-B5.
//!
//! ## Isolation (invariant #10)
//!
//! Enrollment runs on its own background task. It only ever reads the node's
//! identity and POSTs to the controller; it never touches the output clock, the
//! compositor, or any engine channel — losing the controller leaves the node
//! decoding and presenting exactly as before.

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};

/// The node's persisted Ed25519 identity: the keypair every enrollment and
/// heartbeat authenticates with. Generated on first start and reused across
/// reboots (so a re-enroll maps to the SAME device — no duplicate records).
pub struct NodeIdentity {
    signing_key: SigningKey,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material — only the (public) fingerprint.
        f.debug_struct("NodeIdentity")
            .field("public_key", &self.public_key_b64())
            .finish()
    }
}

impl NodeIdentity {
    /// Generate a fresh random identity from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns an error string when the OS RNG is unavailable (never expected;
    /// surfaced rather than panicked so the node fails loudly, not silently).
    pub fn generate() -> Result<Self, String> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed)
            .map_err(|e| format!("generating a node keypair: OS RNG unavailable: {e}"))?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    /// Load the identity from `path` (32 raw seed bytes), or generate and
    /// persist a fresh one when the file is absent. The seed file is written
    /// `0600` so the private key is owner-only.
    ///
    /// # Errors
    ///
    /// A human-readable error for an unreadable/malformed key file or a failed
    /// generate/persist.
    pub fn load_or_generate(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let seed: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                    format!(
                        "node identity {} is not a 32-byte Ed25519 seed (delete it to \
                         regenerate)",
                        path.display()
                    )
                })?;
                Ok(Self {
                    signing_key: SigningKey::from_bytes(&seed),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Self::generate()?;
                identity.persist(path)?;
                Ok(identity)
            }
            Err(e) => Err(format!("reading node identity {}: {e}", path.display())),
        }
    }

    /// Persist the keypair seed to `path` with `0600` permissions (owner-only).
    ///
    /// # Errors
    ///
    /// A human-readable error when the write or the permission-set fails.
    pub fn persist(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.signing_key.to_bytes())
            .map_err(|e| format!("writing node identity {}: {e}", path.display()))?;
        set_owner_only(path)?;
        Ok(())
    }

    /// The node's public key as standard base64 (the enroll/heartbeat wire form).
    #[must_use]
    pub fn public_key_b64(&self) -> String {
        BASE64.encode(self.signing_key.verifying_key().to_bytes())
    }

    /// Sign `message` with the node's private key, returning the base64
    /// signature (the heartbeat `X-Multiview-Node-Signature` header value).
    #[must_use]
    pub fn sign_b64(&self, message: &[u8]) -> String {
        BASE64.encode(self.signing_key.sign(message).to_bytes())
    }

    /// Build the `(timestamp, signature)` pair for a heartbeat to
    /// `device_id` at `path`, signing the controller's canonical message over
    /// `body`. `ts` is the strictly-increasing UNIX-second timestamp the node
    /// must send in the `X-Multiview-Node-Ts` header.
    #[must_use]
    pub fn sign_heartbeat(&self, device_id: &str, path: &str, ts: u64, body: &[u8]) -> String {
        let message = multiview_control::canonical_message("POST", path, device_id, ts, body);
        self.sign_b64(message.as_bytes())
    }
}

/// Set `0600` (owner read/write only) on a just-written key file. On non-Unix
/// targets this is a no-op (the node's blessed deployment is Linux/systemd).
fn set_owner_only(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("setting 0600 on node identity {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Build the JSON body a node POSTs to `/api/v1/devices/enroll`: the (optional)
/// one-time token, the node's public key, model/name metadata, and the
/// EDID-derived display heads.
#[must_use]
pub fn enroll_request_body(
    identity: &NodeIdentity,
    token: Option<&str>,
    model: &str,
    node_name: &str,
    heads: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "token": token,
        "public_key": identity.public_key_b64(),
        "model": model,
        "node_name": node_name,
        "heads": heads,
    })
}

/// Resolve the identity-file path for a controller block: the configured
/// `identity_path`, else `node-identity.key` beside the node `config_path`.
#[must_use]
pub fn identity_path(
    controller: &multiview_config::node::NodeController,
    config_path: &Path,
) -> PathBuf {
    if let Some(path) = &controller.identity_path {
        return path.clone();
    }
    config_path
        .parent()
        .map_or_else(|| PathBuf::from("node-identity.key"), |dir| {
            dir.join("node-identity.key")
        })
}

#[cfg(feature = "node-enroll")]
pub use live::run_enrollment;

/// The live enrollment runner (feature `node-enroll`): the `reqwest` transport
/// that enrolls and heartbeats against the controller.
#[cfg(feature = "node-enroll")]
mod live {
    use std::path::Path;
    use std::time::Duration;

    use super::{enroll_request_body, identity_path, NodeIdentity};
    use multiview_config::node::{NodeConfig, NodeController};

    /// Enroll this node against its configured controller and then heartbeat in
    /// a loop until cancelled. Runs as a detached background task off the
    /// engine/output path (invariant #10): every step only reads the node
    /// identity and talks to the controller, so a controller outage never
    /// affects decode-and-present.
    ///
    /// The flow (ADR-0045 §9): POST `/devices/enroll` with the token (zero-touch)
    /// or without (screen pairing — the node logs the code to show on its head,
    /// then re-polls on the controller's `retry_secs` cadence); once enrolled,
    /// heartbeat every `heartbeat_secs` with a strictly-increasing timestamp.
    ///
    /// # Errors
    ///
    /// A human-readable error for a malformed controller URL or an
    /// unrecoverable identity load; transient HTTP failures are logged and
    /// retried, never fatal (bulletproof-continuous-output discipline).
    pub async fn run_enrollment(
        node: &NodeConfig,
        controller: &NodeController,
        config_path: &Path,
    ) -> Result<(), String> {
        controller
            .validate()
            .map_err(|e| format!("invalid [controller] block: {e}"))?;
        let path = identity_path(controller, config_path);
        let identity = NodeIdentity::load_or_generate(&path)?;
        tracing::info!(
            controller = %controller.url,
            public_key = %identity.public_key_b64(),
            "node enrollment: identity ready"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("building the enrollment HTTP client: {e}"))?;
        let base = controller.url.trim_end_matches('/').to_owned();
        let model = "multiview-node";
        let node_name = controller
            .node_name
            .clone()
            .unwrap_or_else(hostname_or_default);
        let heads = head_descriptors(node);

        // Enroll (zero-touch or screen-pairing), retrying transient failures.
        let device_id = loop {
            let body = enroll_request_body(
                &identity,
                controller.enrollment_token.as_deref(),
                model,
                &node_name,
                &heads,
            );
            match post_enroll(&client, &base, &body).await {
                Ok(EnrollPoll::Enrolled { device_id, heartbeat_secs }) => {
                    tracing::info!(device_id, heartbeat_secs, "node enrolled");
                    break (device_id, heartbeat_secs);
                }
                Ok(EnrollPoll::Pairing { pairing_code, retry_secs }) => {
                    // The pairing card the node shows on its attached display:
                    // the operator reads this code (and a QR encoding the same)
                    // and completes it in Settings → Display Nodes.
                    tracing::info!(
                        pairing_code,
                        "node pairing: show this code on the display and complete it in the \
                         controller; re-polling shortly"
                    );
                    tokio::time::sleep(Duration::from_secs(retry_secs.max(1))).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "enroll poll failed; retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };
        let (device_id, heartbeat_secs) = device_id;

        // Heartbeat forever with a strictly-increasing timestamp.
        let mut last_ts = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(heartbeat_secs.max(1))).await;
            let ts = now_secs().max(last_ts + 1);
            last_ts = ts;
            if let Err(e) = post_heartbeat(&client, &base, &identity, &device_id, ts, &heads).await {
                tracing::warn!(error = %e, "heartbeat failed; the controller will mark us stale \
                    until the next one lands");
            }
        }
    }

    /// The parsed outcome of an enroll poll.
    enum EnrollPoll {
        Enrolled { device_id: String, heartbeat_secs: u64 },
        Pairing { pairing_code: String, retry_secs: u64 },
    }

    /// POST `/api/v1/devices/enroll`, parsing the `200`/`202` body.
    async fn post_enroll(
        client: &reqwest::Client,
        base: &str,
        body: &serde_json::Value,
    ) -> Result<EnrollPoll, String> {
        let url = format!("{base}/api/v1/devices/enroll");
        let resp = client
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        let status = resp.status();
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decoding enroll response: {e}"))?;
        if status.as_u16() == 200 {
            let device_id = value
                .get("device_id")
                .and_then(serde_json::Value::as_str)
                .ok_or("enroll 200 missing device_id")?
                .to_owned();
            let heartbeat_secs = value
                .get("heartbeat_secs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(10);
            Ok(EnrollPoll::Enrolled { device_id, heartbeat_secs })
        } else if status.as_u16() == 202 {
            let pairing_code = value
                .get("pairing_code")
                .and_then(serde_json::Value::as_str)
                .ok_or("enroll 202 missing pairing_code")?
                .to_owned();
            let retry_secs = value
                .get("retry_secs")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(5);
            Ok(EnrollPoll::Pairing { pairing_code, retry_secs })
        } else {
            Err(format!("enroll rejected: HTTP {status}"))
        }
    }

    /// POST a signed `/api/v1/devices/{id}/heartbeat`.
    async fn post_heartbeat(
        client: &reqwest::Client,
        base: &str,
        identity: &NodeIdentity,
        device_id: &str,
        ts: u64,
        heads: &[serde_json::Value],
    ) -> Result<(), String> {
        let path = format!("/api/v1/devices/{device_id}/heartbeat");
        let url = format!("{base}{path}");
        let body = serde_json::json!({ "heads": heads });
        let bytes = serde_json::to_vec(&body).map_err(|e| format!("encoding heartbeat: {e}"))?;
        let signature = identity.sign_heartbeat(device_id, &path, ts, &bytes);
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .header("x-multiview-node-id", device_id)
            .header("x-multiview-node-ts", ts.to_string())
            .header("x-multiview-node-signature", signature)
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("POST {url}: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("heartbeat rejected: HTTP {}", resp.status()))
        }
    }

    /// Describe the node's configured display heads as the enroll/heartbeat
    /// `heads` array (the EDID-derived projection). Geometry is taken from each
    /// head's configured mode where present, else the canvas geometry.
    fn head_descriptors(node: &NodeConfig) -> Vec<serde_json::Value> {
        let (cw, ch, cfps) = node.canvas_geometry();
        let canvas_mhz = refresh_millihertz(cfps.rational());
        node.displays
            .iter()
            .enumerate()
            .map(|(i, display)| {
                let spec = display.mode.as_ref().or(display.forced_mode.as_ref());
                let (w, h, mhz) = spec.map_or((cw, ch, canvas_mhz), |s| {
                    (s.width, s.height, refresh_millihertz(s.refresh.rational()))
                });
                serde_json::json!({
                    "id": format!("head-{i}"),
                    "connector": display.connector,
                    "width": w,
                    "height": h,
                    "refresh_millihertz": mhz,
                    "connected": true,
                })
            })
            .collect()
    }

    /// Convert an exact rational refresh rate to millihertz (`60/1` → `60_000`),
    /// integer math only (never a float — invariant #3). Saturating into `u32`.
    fn refresh_millihertz(rate: multiview_core::time::Rational) -> u32 {
        if rate.den == 0 {
            return 0;
        }
        let mhz = rate.num.saturating_mul(1_000) / rate.den;
        u32::try_from(mhz).unwrap_or(u32::MAX)
    }

    /// The current UNIX time in whole seconds (heartbeat timestamp base).
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// The host name, or `"multiview-node"` when it cannot be read.
    fn hostname_or_default() -> String {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "multiview-node".to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_generated_identity_signs_what_the_controller_verifies() {
        // The node side signs the controller's canonical message; the verifier
        // (control plane) must accept it. We re-derive the message here exactly
        // as the controller does and verify with the public key.
        let identity = NodeIdentity::generate().unwrap();
        let body = br#"{"heads":[]}"#;
        let ts = 1_700_000_000u64;
        let path = "/api/v1/devices/node-a/heartbeat";
        let sig_b64 = identity.sign_heartbeat("node-a", path, ts, body);

        // Verify with the public key, exactly as the control plane would.
        let pub_bytes: [u8; 32] = BASE64
            .decode(identity.public_key_b64())
            .unwrap()
            .try_into()
            .unwrap();
        let verifying = ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes).unwrap();
        let sig_bytes: [u8; 64] = BASE64.decode(sig_b64).unwrap().try_into().unwrap();
        let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        let message =
            multiview_control::canonical_message("POST", path, "node-a", ts, body);
        verifying
            .verify_strict(message.as_bytes(), &signature)
            .expect("the controller verifies the node's signature");
    }

    #[test]
    fn load_or_generate_persists_then_reuses_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node-identity.key");
        let first = NodeIdentity::load_or_generate(&path).unwrap();
        let first_pub = first.public_key_b64();
        // The file now exists (0600 on unix) and a reload yields the SAME key,
        // so a reboot maps to the same device (no duplicate records).
        assert!(path.exists());
        let second = NodeIdentity::load_or_generate(&path).unwrap();
        assert_eq!(second.public_key_b64(), first_pub);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the private key file is owner-only");
        }
    }

    #[test]
    fn a_malformed_identity_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.key");
        std::fs::write(&path, b"not-32-bytes").unwrap();
        let err = NodeIdentity::load_or_generate(&path).expect_err("a short key is rejected");
        assert!(err.contains("32-byte"), "{err}");
    }

    #[test]
    fn enroll_body_carries_the_token_key_and_heads() {
        let identity = NodeIdentity::generate().unwrap();
        let heads = vec![serde_json::json!({ "id": "head-0", "connector": "HDMI-A-1" })];
        let body = enroll_request_body(&identity, Some("enr-a.b"), "hp-t630", "Lobby", &heads);
        assert_eq!(body["token"], "enr-a.b");
        assert_eq!(body["public_key"], identity.public_key_b64());
        assert_eq!(body["model"], "hp-t630");
        assert_eq!(body["node_name"], "Lobby");
        assert_eq!(body["heads"].as_array().unwrap().len(), 1);

        // No-token form (screen pairing): the token is JSON null.
        let body = enroll_request_body(&identity, None, "hp-t630", "Lobby", &heads);
        assert!(body["token"].is_null());
    }

    #[test]
    fn identity_path_defaults_beside_the_config() {
        let controller: multiview_config::node::NodeController = serde_json::from_value(
            serde_json::json!({ "url": "https://[fd00:db8::1]:8080" }),
        )
        .unwrap();
        let resolved = identity_path(&controller, Path::new("/etc/multiview/node.toml"));
        assert_eq!(resolved, Path::new("/etc/multiview/node-identity.key"));

        let controller: multiview_config::node::NodeController = serde_json::from_value(
            serde_json::json!({
                "url": "https://[fd00:db8::1]:8080",
                "identity_path": "/var/lib/multiview/id.key"
            }),
        )
        .unwrap();
        let resolved = identity_path(&controller, Path::new("/etc/multiview/node.toml"));
        assert_eq!(resolved, Path::new("/var/lib/multiview/id.key"));
    }
}
