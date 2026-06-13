//! The presentation-sync-groups resource surface under `/api/v1/sync-groups`
//! (ADR-M008 / ADR-M010).
//!
//! A **sync group** is a set of member devices presenting one synchronized
//! canvas to a target skew, stored as config-as-code
//! (`multiview_config::SyncGroup`) over the generic versioned
//! [`ResourceRepository`](crate::resource_store::ResourceRepository), with
//! `ETag`/`If-Match` optimistic concurrency on every mutation (ADR-W006), RBAC
//! via [`Principal`], typed-body validation against `multiview_config::SyncGroup`
//! (ADR-W015), and an audit record after each successful write. This module
//! exposes:
//!
//! * `GET /api/v1/sync-groups` — list (role: read).
//! * `GET /api/v1/sync-groups/{id}` — fetch one (role: read; `ETag`).
//! * `POST /api/v1/sync-groups/{id}` — create (role: write; `422` on an invalid
//!   `SyncGroup` document, e.g. an empty member list).
//! * `PUT /api/v1/sync-groups/{id}` — replace (role: write; `If-Match` → `412`).
//! * `DELETE /api/v1/sync-groups/{id}` — delete (role: administer; `If-Match`).
//! * `POST /api/v1/sync-groups/{id}/measure` — kick off a skew measurement
//!   (role: write; `202` + operation id; the result arrives on the realtime
//!   stream once the driver actors land — DEV-A4/A5).
//!
//! Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::{IdempotencyKey, IfMatch, Reservation};
use crate::error::{ControlError, ControlResult};
use crate::resource_store::{Resource, ResourceInput, VersionedResource, SYNC_GROUP_KIND};
use crate::state::AppState;
use crate::typed_resources::{validated_body, with_apply_restart, TypedCollection};

/// Attach the sync-group resource's `ETag` to a successful response.
fn group_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/sync-groups` — list all sync groups, id-sorted (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sync-groups",
        tag = "sync-groups",
        responses(
            (status = 200, description = "All sync groups, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_sync_groups(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    let groups = state
        .sync_groups
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .collect();
    Ok(Json(groups))
}

/// `GET /api/v1/sync-groups/{id}` — fetch one (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sync-groups/{id}",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        responses(
            (status = 200, description = "The sync group (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this group.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.sync_groups.get(&id)?;
    Ok(group_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/sync-groups/{id}` — create a sync group (role: write).
///
/// Validates the body against `multiview_config::SyncGroup` (`422` on an invalid
/// document, e.g. an empty member list).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sync-groups/{id}",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        request_body = crate::openapi_schemas::SyncGroupResourceInputDoc,
        responses(
            (status = 201, description = "The created sync group (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid sync-group document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::SyncGroups, &id, &input.body)?,
    };
    let versioned = state.sync_groups.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        SYNC_GROUP_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(group_response(
        StatusCode::CREATED,
        &versioned,
    )))
}

/// `PUT /api/v1/sync-groups/{id}` — replace (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/sync-groups/{id}",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        request_body = crate::openapi_schemas::SyncGroupResourceInputDoc,
        responses(
            (status = 200, description = "The replaced sync group (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
            (status = 422, description = "The body is not a valid sync-group document (detail names the field path).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Preconditions before content (RFC 9110 §13.2.2).
    let current = state.sync_groups.get(&id)?;
    if_match.require(SYNC_GROUP_KIND, &id, current.version)?;
    let input = ResourceInput {
        name: input.name,
        body: validated_body(TypedCollection::SyncGroups, &id, &input.body)?,
    };
    let versioned = state.sync_groups.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        SYNC_GROUP_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(with_apply_restart(group_response(
        StatusCode::OK,
        &versioned,
    )))
}

/// `DELETE /api/v1/sync-groups/{id}` — delete (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/sync-groups/{id}",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        responses(
            (status = 204, description = "The sync group was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.sync_groups.get(&id)?;
    if_match.require(SYNC_GROUP_KIND, &id, current.version)?;
    state.sync_groups.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        SYNC_GROUP_KIND,
        &id,
        None,
    );
    Ok(with_apply_restart(StatusCode::NO_CONTENT.into_response()))
}

/// `POST /api/v1/sync-groups/{id}/measure` — kick off a skew measurement
/// (role: write; `202` + operation id).
///
/// In this slice (no driver actor) the operation id is minted and `202`'d; the
/// measurement result (`device.sync`) arrives on the realtime stream once the
/// driver actors land (DEV-A4/A5). A retried `Idempotency-Key` returns the
/// original id.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sync-groups/{id}/measure",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id to measure.")),
        responses(
            (status = 202, description = "Measurement accepted; result on the realtime stream.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to measure.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn measure_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Fail fast on an unknown group before reserving an idempotency key.
    state
        .sync_groups
        .get(&id)
        .map_err(|_| ControlError::NotFound {
            kind: SYNC_GROUP_KIND,
            id: id.clone(),
        })?;
    let op = match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Fresh(op) | Reservation::Replay(op) => op,
    };
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        SYNC_GROUP_KIND,
        &id,
        Some(serde_json::json!({ "action": "measure" })),
    );
    let body = crate::routes::AcceptedBody {
        operation_id: op.to_string(),
        kind: "measure".to_owned(),
        applied_live: None,
        carried_only: None,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// The default test-pattern duration (10 s) when the request omits it.
const DEFAULT_TEST_PATTERN_DURATION_S: u32 = 10;
/// The inclusive ceiling on the test-pattern duration (5 min) — beyond it the
/// value is a typo, not an operator intent.
const MAX_TEST_PATTERN_DURATION_S: u32 = 300;
/// The default binary-flash period (1 Hz) when the request omits it.
const DEFAULT_FLASH_PERIOD_MS: u32 = 1_000;

/// `GET /api/v1/sync-groups/{id}/status` — the read-only runtime status of a
/// sync group (role: read): the **weakest-member** achieved tier (never
/// over-claimed), per-member measured skew, and drift-alarm state.
///
/// Reads the latest-wins [`SyncGroupRuntime`](crate::devices::sync_runtime::SyncGroupRuntime)
/// (never persisted/exported). A freshly-seeded group with no measurements yet
/// honestly reports the `none` tier. `404` when the id is not a configured group.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sync-groups/{id}/status",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        responses(
            (status = 200, description = "The group's runtime status (weakest-member tier, per-member skew, drift state).", body = crate::openapi_schemas::SyncGroupStatusDoc),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this group.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_sync_group_status(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Json<crate::devices::sync_runtime::SyncGroupStatus>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let status = state
        .sync_runtime
        .status(&id)
        .ok_or_else(|| ControlError::NotFound {
            kind: SYNC_GROUP_KIND,
            id: id.clone(),
        })?;
    Ok(Json(status))
}

/// `POST /api/v1/sync-groups/{id}/test-pattern` — emit a burnt-in frame counter
/// + binary flash on the group's members for visual sync verification (role:
/// write; `202` + operation id).
///
/// Publishes a `sync.test-pattern` lifecycle event the member nodes consume to
/// drive their overlay machinery (the existing `Identify` flash + a frame
/// counter); the engine program output is untouched (invariant #1/#10). The
/// request body is optional — sensible defaults apply.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sync-groups/{id}/test-pattern",
        tag = "sync-groups",
        params(("id" = String, Path, description = "Sync-group id.")),
        request_body = crate::openapi_schemas::SyncGroupTestPatternInputDoc,
        responses(
            (status = 202, description = "Test pattern accepted; the members render it.", body = crate::routes::AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No sync group with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn test_pattern_sync_group(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Path(id): Path<String>,
    body: Option<Json<crate::openapi_schemas::SyncGroupTestPatternInputDoc>>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Fail fast on an unknown group before reserving an idempotency key — the
    // runtime knows every configured group.
    if state.sync_runtime.status(&id).is_none() {
        return Err(ControlError::NotFound {
            kind: SYNC_GROUP_KIND,
            id: id.clone(),
        });
    }
    let input = body.map(|Json(input)| input);
    let seconds = input
        .as_ref()
        .and_then(|i| i.duration_s)
        .unwrap_or(DEFAULT_TEST_PATTERN_DURATION_S)
        .clamp(1, MAX_TEST_PATTERN_DURATION_S);
    let frame_counter = input.as_ref().and_then(|i| i.frame_counter).unwrap_or(true);
    let flash_period_ms = input
        .as_ref()
        .and_then(|i| i.flash_period_ms)
        .unwrap_or(DEFAULT_FLASH_PERIOD_MS);
    let duration_ms = seconds.saturating_mul(1_000);

    let op = match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Fresh(op) | Reservation::Replay(op) => op,
    };
    // Publish through the canonical device broadcaster (control-plane only,
    // non-blocking drop-oldest; the engine never awaits this — invariant #10).
    let _seq = state.poller_wiring().broadcaster.sync_test_pattern(
        &id,
        duration_ms,
        frame_counter,
        flash_period_ms,
    );
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        SYNC_GROUP_KIND,
        &id,
        Some(serde_json::json!({
            "action": "test-pattern",
            "duration_ms": duration_ms,
            "frame_counter": frame_counter,
            "flash_period_ms": flash_period_ms,
        })),
    );
    let body = crate::routes::AcceptedBody {
        operation_id: op.to_string(),
        kind: "test-pattern".to_owned(),
        applied_live: None,
        carried_only: None,
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}
