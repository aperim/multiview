//! The display-node enrollment & pairing surface under `/api/v1/devices`
//! (DEV-B6, ADR-0045 / managed-devices §9).
//!
//! These routes turn a `multiview node` appliance into a managed
//! [`Device`](multiview_config::Device) of driver `displaynode`, bound to an
//! Ed25519 keypair the node generates on first start (no passwords). Two surfaces:
//!
//! * **Operator** (Settings → Display Nodes): mint/list/revoke TTL'd one-time
//!   enrollment tokens (`/devices/enrollment-tokens`, admin), and complete screen
//!   pairing (`/devices/pair`, operator) reading the pending list
//!   (`/devices/pairing-requests`, admin).
//! * **Node** (unauthenticated by Bearer; node-authenticated by token/keypair):
//!   `POST /devices/enroll` (token → enrolled; no token → a six-character pairing
//!   code) and the keypair-signed `POST /devices/{id}/heartbeat`.
//!
//! Plus the ADR-M009 facet (c) projection `GET /devices/{id}/display-heads`
//! (read auth), the node's reported scanout heads, refreshed on every heartbeat.
//!
//! A node enrolled here is created in the **device registry** exactly like an
//! operator-adopted device (config-as-code durable state: `driver = displaynode`,
//! the bound public key under `enrollment.public_key`, plus any display
//! assignment), and seeded ONLINE in the status registry — the brief's "appears
//! in Devices already ONLINE". Deleting the device forgets the node identity
//! (the shared `delete_device` handler calls [`AppState::node_enroll`]'s
//! `forget`), so the keypair no longer authenticates and a re-enroll is back to
//! pairing. Every store here is control-plane-only and bounded (invariant #10).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::devices::enroll::{
    DisplayHead, EnrollError, EnrollOutcome, EnrollRequest, HeartbeatBody, PairCompletion,
    PairRequest,
};
use crate::error::{ControlError, ControlResult};
use crate::resource_store::{ResourceInput, DEVICE_KIND};
use crate::state::AppState;
use multiview_events::{DeviceState, DeviceStatus};

/// Map an [`EnrollError`] onto the control-plane error taxonomy (and thence the
/// RFC 9457 problem document + status the route returns).
fn map_enroll_error(err: EnrollError) -> ControlError {
    match err {
        EnrollError::TokenRejected | EnrollError::Unauthorized => ControlError::Unauthenticated,
        EnrollError::Invalid(msg) => ControlError::Validation(msg),
        EnrollError::PairingTableFull => ControlError::TooManyRequests(
            "too many display nodes are pairing at once; retry shortly".to_owned(),
        ),
    }
}

/// The `POST /api/v1/devices/enrollment-tokens` request body (an optional TTL).
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MintTokenRequest {
    /// The token's time-to-live in seconds (defaults to one hour; bounded to
    /// `[60, 604800]`). A value outside the range is `422`.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// `POST /api/v1/devices/enrollment-tokens` — mint a one-time enrollment token
/// (role: administer). Returns the bearer token **once** (one-time display); only
/// its hash is retained.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/enrollment-tokens",
        tag = "devices",
        request_body = MintTokenRequest,
        responses(
            (status = 201, description = "The minted token (shown once — the secret is never stored or re-displayed).", body = crate::devices::enroll::MintedToken),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to mint enrollment tokens.", body = crate::problem::Problem),
            (status = 422, description = "ttl_secs is out of range.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn mint_enrollment_token(
    State(state): State<AppState>,
    principal: Principal,
    Json(request): Json<MintTokenRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    let minted = state
        .node_enroll
        .mint_token(request.ttl_secs)
        .map_err(map_enroll_error)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        DEVICE_KIND,
        &minted.token_id,
        Some(serde_json::json!({ "action": "mint-enrollment-token" })),
    );
    Ok((StatusCode::CREATED, Json(minted)).into_response())
}

/// `GET /api/v1/devices/enrollment-tokens` — list enrollment tokens
/// (role: administer). Metadata only — the secret never appears.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/enrollment-tokens",
        tag = "devices",
        responses(
            (status = 200, description = "All enrollment tokens, id-sorted (metadata only).", body = [crate::devices::enroll::TokenSummary]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to list enrollment tokens.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_enrollment_tokens(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    Ok(Json(state.node_enroll.list_tokens()).into_response())
}

/// `DELETE /api/v1/devices/enrollment-tokens/{id}` — revoke an enrollment token
/// (role: administer; `204`, `404` when unknown).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/devices/enrollment-tokens/{id}",
        tag = "devices",
        params(("id" = String, Path, description = "Enrollment token id.")),
        responses(
            (status = 204, description = "The token was revoked."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to revoke enrollment tokens.", body = crate::problem::Problem),
            (status = 404, description = "No enrollment token with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn revoke_enrollment_token(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    if !state.node_enroll.revoke_token(&id) {
        return Err(ControlError::NotFound {
            kind: "enrollment-token",
            id,
        });
    }
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        DEVICE_KIND,
        &id,
        Some(serde_json::json!({ "action": "revoke-enrollment-token" })),
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The `200`/`202` body of `POST /devices/enroll`.
///
/// A flat document discriminated by [`status`](EnrollResponse::status)
/// (`"enrolled"` or `"pairing"`) — never a serde `untagged` enum (the project
/// bans `untagged`). The `enrolled` arm carries `device_id`/`heartbeat_secs`;
/// the `pairing` arm carries `pairing_code`/`retry_secs`; the irrelevant fields
/// are elided.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EnrollResponse {
    /// `"enrolled"` (the node is bound) or `"pairing"` (the node must pair).
    pub status: &'static str,
    /// The device id the node is bound to (`enrolled` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// The heartbeat cadence the node should keep, seconds (`enrolled` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_secs: Option<u64>,
    /// The six-character pairing code to display (`pairing` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_code: Option<String>,
    /// How long to wait before the next poll, seconds (`pairing` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_secs: Option<u64>,
}

/// `POST /api/v1/devices/enroll` — a node enrolls (unauthenticated by Bearer:
/// node-authenticated by token/keypair).
///
/// With a valid one-time token (and a well-formed key) the node is enrolled and
/// a `displaynode` device is created ONLINE (`200`). Without a usable token the
/// node is told to pair: `202` with a stable six-character code (the same code
/// on every re-poll) until the operator completes `POST /devices/pair`.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/enroll",
        tag = "devices",
        request_body = crate::devices::enroll::EnrollRequest,
        responses(
            (status = 200, description = "Enrolled: the node is bound to a device and should heartbeat.", body = EnrollResponse),
            (status = 202, description = "Pairing: show the code (and QR) and re-poll.", body = EnrollResponse),
            (status = 401, description = "The enrollment token was rejected (unknown/expired/revoked/used).", body = crate::problem::Problem),
            (status = 422, description = "The request body is malformed (e.g. a bad public key).", body = crate::problem::Problem),
            (status = 429, description = "Too many display nodes are pairing at once; retry shortly.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn enroll_node(
    State(state): State<AppState>,
    Json(request): Json<EnrollRequest>,
) -> ControlResult<Response> {
    let outcome = state
        .node_enroll
        .enroll(&request)
        .map_err(map_enroll_error)?;
    match outcome {
        EnrollOutcome::Enrolled {
            device_id,
            heartbeat_secs,
        } => {
            // Ensure the device record + ONLINE status exist (idempotent: a
            // re-poll for an already-created device is a no-op). The token path
            // creates a fresh device here; the paired path's record was created
            // at `pair` time, so this only seeds status if missing.
            ensure_enrolled_device(&state, &device_id, &request.node_name, &request.public_key);
            let body = EnrollResponse {
                status: "enrolled",
                device_id: Some(device_id),
                heartbeat_secs: Some(heartbeat_secs),
                pairing_code: None,
                retry_secs: None,
            };
            Ok((StatusCode::OK, Json(body)).into_response())
        }
        EnrollOutcome::Pairing {
            pairing_code,
            retry_secs,
        } => {
            let body = EnrollResponse {
                status: "pairing",
                device_id: None,
                heartbeat_secs: None,
                pairing_code: Some(pairing_code),
                retry_secs: Some(retry_secs),
            };
            Ok((StatusCode::ACCEPTED, Json(body)).into_response())
        }
    }
}

/// Create (idempotently) the `displaynode` device record for an enrolled node
/// and seed its ONLINE runtime status.
///
/// The record is config-as-code durable state: `driver = displaynode`, the bound
/// public key under `enrollment.public_key`, and any display assignment carried
/// from the node's report. A device id that already exists (the paired path
/// created it, or a re-poll) is left intact — the create is skipped, the status
/// re-ensured ONLINE. Control-plane-only, off the engine hot loop (invariant #10).
fn ensure_enrolled_device(
    state: &AppState,
    device_id: &str,
    node_name: &str,
    public_key_b64: &str,
) {
    if state.devices.get(device_id).is_err() {
        let body = displaynode_body(public_key_b64, None);
        let name = if node_name.is_empty() {
            device_id.to_owned()
        } else {
            node_name.to_owned()
        };
        if let Err(e) = state
            .devices
            .create(device_id, ResourceInput { name, body })
        {
            tracing::warn!(
                device = %device_id,
                error = %e,
                "enrolled device record create failed; the node is bound but unlisted"
            );
        }
    }
    // Seed/refresh ONLINE: an enrolled node is reachable by construction (it
    // just spoke to us). Latest-wins; never persisted/exported.
    state.node_enroll_status_online(device_id);
}

/// The `multiview_config::Device` JSON body for a `displaynode`, carrying the
/// bound public key and any display assignment. Built as JSON (round-trips to
/// `Device`) so the registry stores it opaquely like every other device.
fn displaynode_body(
    public_key_b64: &str,
    display_assign: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "id": serde_json::Value::Null,
        "driver": "displaynode",
        "enrollment": { "public_key": public_key_b64 },
    });
    // The `id` is injected by the store from the path; carry it absent so the
    // typed `Device` body does not fight the path id. (The registry stores the
    // body opaquely; the `enrollment` block is additive metadata the device
    // detail UI reads.)
    if let Some(obj) = body.as_object_mut() {
        obj.remove("id");
        if let Some(display) = display_assign {
            obj.insert("display".to_owned(), display);
        }
    }
    body
}

/// `POST /api/v1/devices/pair` — the operator completes a screen pairing
/// (role: write). The code is read off the node's attached display.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/pair",
        tag = "devices",
        request_body = crate::devices::enroll::PairRequest,
        responses(
            (status = 201, description = "Pairing completed: the node is bound to the (operator-chosen) device id; the node's next poll flips to enrolled.", body = PairResponse),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to complete pairing.", body = crate::problem::Problem),
            (status = 404, description = "No pending pairing carries that code.", body = crate::problem::Problem),
            (status = 409, description = "The requested device id already exists.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn pair_node(
    State(state): State<AppState>,
    principal: Principal,
    Json(request): Json<PairRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    // Refuse an explicit device id that already exists BEFORE marking the
    // pairing complete, so a collision never leaves a half-paired request.
    if let Some(id) = request.device_id.as_deref() {
        if state.devices.get(id).is_ok() {
            return Err(ControlError::Conflict(format!(
                "device {id:?} already exists; choose a different id for the paired node"
            )));
        }
    }
    let completion = state.node_enroll.complete_pairing(
        &request.code,
        request.device_id.as_deref(),
        request.display_name.as_deref(),
    );
    match completion {
        PairCompletion::NotFound => Err(ControlError::NotFound {
            kind: "pairing-code",
            id: request.code.clone(),
        }),
        PairCompletion::Completed {
            device_id,
            node_name,
            public_key_b64,
            heads: _heads,
        } => {
            // Create the device record now (the node's next enroll poll then
            // flips to enrolled, binding the identity). A create failure (e.g. a
            // racing id collision) unwinds the pairing so the code stays valid.
            if state.devices.get(&device_id).is_err() {
                let body = displaynode_body(&public_key_b64, None);
                if let Err(e) = state.devices.create(
                    &device_id,
                    ResourceInput {
                        name: if node_name.is_empty() {
                            device_id.clone()
                        } else {
                            node_name
                        },
                        body,
                    },
                ) {
                    state.node_enroll.unassign_pairing(&device_id);
                    return Err(e);
                }
            } else {
                state.node_enroll.unassign_pairing(&device_id);
                return Err(ControlError::Conflict(format!(
                    "device {device_id:?} already exists; choose a different id"
                )));
            }
            state.node_enroll_status_online(&device_id);
            state.audit(
                &principal.key_id,
                AuditAction::Create,
                DEVICE_KIND,
                &device_id,
                Some(serde_json::json!({ "action": "pair-display-node" })),
            );
            let body = PairResponse {
                device_id: device_id.clone(),
            };
            Ok((StatusCode::CREATED, Json(body)).into_response())
        }
    }
}

/// The `201` body of `POST /devices/pair`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PairResponse {
    /// The device id the node was bound to.
    pub device_id: String,
}

/// `GET /api/v1/devices/pairing-requests` — the pending screen pairings
/// (role: administer). Model/name metadata only — never the code or the key.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/pairing-requests",
        tag = "devices",
        responses(
            (status = 200, description = "The pending pairing requests (metadata only — the code stays on the node's screen).", body = [crate::devices::enroll::PairingRequestSummary]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to list pairing requests.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_pairing_requests(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    Ok(Json(state.node_enroll.list_pairing_requests()).into_response())
}

/// The `200` body of a signed heartbeat: the node's current display assignment
/// and the cadence to keep.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HeartbeatResponse {
    /// The node's current display assignment (the `display.assign` value:
    /// `{ "program": true }` / `{ "output": "out-…" }` / `{ "wall_head": "head-…" }`),
    /// or `null` when the node is unassigned.
    pub assignment: serde_json::Value,
    /// The heartbeat cadence the node should keep (seconds).
    pub heartbeat_secs: u64,
}

/// The node-auth header names for a signed heartbeat (DEV-B6): the device id,
/// the strictly-increasing UNIX-second timestamp, and the base64 Ed25519
/// signature over the canonical message.
const NODE_ID_HEADER: &str = "x-multiview-node-id";
const NODE_TS_HEADER: &str = "x-multiview-node-ts";
const NODE_SIG_HEADER: &str = "x-multiview-node-signature";

/// `POST /api/v1/devices/{id}/heartbeat` — a node proves liveness with a
/// keypair-signed heartbeat (unauthenticated by Bearer: node-authenticated by
/// the Ed25519 signature over the canonical message). On success it answers with
/// the node's current display assignment and refreshes the display-head
/// projection. Any verification failure is `401` (never a `500`).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/devices/{id}/heartbeat",
        tag = "devices",
        params(("id" = String, Path, description = "The enrolled device id.")),
        request_body = crate::devices::enroll::HeartbeatBody,
        responses(
            (status = 200, description = "Heartbeat accepted; the current assignment is returned.", body = HeartbeatResponse),
            (status = 401, description = "Heartbeat verification failed (unknown device, wrong key, stale or replayed timestamp).", body = crate::problem::Problem),
            (status = 422, description = "Malformed heartbeat body.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn heartbeat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body_bytes: axum::body::Bytes,
) -> ControlResult<Response> {
    // The signed bytes are EXACTLY the request body as received (the node signed
    // these); parse a typed view for the heads, but verify against the raw bytes.
    let parsed: HeartbeatBody = serde_json::from_slice(&body_bytes)
        .map_err(|e| ControlError::Validation(format!("malformed heartbeat body: {e}")))?;
    let ts = parse_node_ts(&headers).ok_or(ControlError::Unauthenticated)?;
    let node_id = header_str(&headers, NODE_ID_HEADER).ok_or(ControlError::Unauthenticated)?;
    // The path id is the authority; a mismatched node-id header is a refusal.
    if node_id != id {
        return Err(ControlError::Unauthenticated);
    }
    let signature_b64 =
        header_str(&headers, NODE_SIG_HEADER).ok_or(ControlError::Unauthenticated)?;
    let path = format!("/api/v1/devices/{id}/heartbeat");
    let verified = state
        .node_enroll
        .verify_heartbeat(&id, ts, &signature_b64, &path, &body_bytes, parsed.heads)
        .map_err(map_enroll_error)?;
    // Refresh the device runtime status ONLINE (the node just answered).
    state.node_enroll_status_online(&verified.device_id);
    // The current assignment is read from the device record's `display.assign`
    // (operator-set via the Display tab); absent ⇒ `null`.
    let assignment = device_assignment(&state, &id);
    let body = HeartbeatResponse {
        assignment,
        heartbeat_secs: verified.heartbeat_secs,
    };
    Ok((StatusCode::OK, Json(body)).into_response())
}

/// `GET /api/v1/devices/{id}/display-heads` — the node's reported scanout heads
/// (ADR-M009 facet (c)); role: read. `404` when the device is not an enrolled
/// node (or unknown).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/devices/{id}/display-heads",
        tag = "devices",
        params(("id" = String, Path, description = "The enrolled device id.")),
        responses(
            (status = 200, description = "The node's reported display heads.", body = [crate::devices::enroll::DisplayHead]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this device.", body = crate::problem::Problem),
            (status = 404, description = "No enrolled node with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn display_heads(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let heads: Vec<DisplayHead> =
        state
            .node_enroll
            .display_heads(&id)
            .ok_or_else(|| ControlError::NotFound {
                kind: DEVICE_KIND,
                id: id.clone(),
            })?;
    Ok(Json(heads).into_response())
}

/// Read the device's `display.assign` value from its stored record (the
/// heartbeat assignment answer), or [`serde_json::Value::Null`] when unassigned
/// / the record is absent.
fn device_assignment(state: &AppState, device_id: &str) -> serde_json::Value {
    state
        .devices
        .get(device_id)
        .ok()
        .and_then(|v| {
            v.resource
                .body
                .get("display")
                .and_then(|d| d.get("assign"))
                .cloned()
        })
        .unwrap_or(serde_json::Value::Null)
}

/// Parse the strictly-increasing UNIX-second timestamp from the node-ts header.
fn parse_node_ts(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, NODE_TS_HEADER)?.parse::<u64>().ok()
}

/// Read a header value as a borrowed-then-owned UTF-8 string.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

impl AppState {
    /// Seed/refresh a device's runtime status as ONLINE (an enrolled node is
    /// reachable by construction). Latest-wins; never persisted/exported.
    fn node_enroll_status_online(&self, device_id: &str) {
        self.device_status
            .set_status(DeviceStatus::new(device_id, DeviceState::Online));
    }
}
