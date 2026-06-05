//! The HTTP handlers and router assembly under `/api/v1`.
//!
//! One resource is wired end to end here — **layouts** — over the
//! [`Repository`](crate::repository::Repository) trait, with `ETag`/`If-Match`
//! optimistic concurrency on every mutation (ADR-W006). The operational
//! commands (`start`/`stop`/`swap`) submit to the engine command bus and return
//! `202 Accepted` + an operation id; their outcome arrives later on the realtime
//! stream (ADR-W008). Errors are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::command::{Command, OperationId};
use crate::concurrency::{IdempotencyKey, IfMatch, Reservation};
use crate::error::{ControlError, ControlResult};
#[cfg(feature = "openapi")]
use crate::problem::Problem;
use crate::repository::{Layout, LayoutInput, VersionedLayout, LAYOUT_KIND};
use crate::state::AppState;

pub mod alarms;
pub mod audit;
pub mod config;
pub mod outputs;
pub mod overlays;
pub mod salvos;
pub mod sources;
pub mod tally;

/// A `202 Accepted` body returned for an asynchronously-applied command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AcceptedBody {
    /// The operation id correlating this command's eventual outcome on the
    /// realtime stream.
    pub operation_id: String,
    /// The command kind (e.g. `start`).
    pub kind: String,
}

/// The body of a `POST /commands/swap` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SwapRequest {
    /// The tile/cell id whose source binding changes.
    pub tile: String,
    /// The new source/input id to bind.
    pub source: String,
}

/// Attach the resource's `ETag` to a successful response carrying a layout.
fn layout_response(status: StatusCode, versioned: &VersionedLayout) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.layout.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/layouts` — list all layouts (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/layouts",
        tag = "layouts",
        responses(
            (status = 200, description = "All layouts, id-sorted.", body = [Layout]),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = Problem),
        ),
    )
)]
async fn list_layouts(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Layout>>> {
    principal.role.require(Action::Read)?;
    let layouts = state
        .repository
        .list_layouts()?
        .into_iter()
        .map(|v| v.layout)
        .collect();
    Ok(Json(layouts))
}

/// `GET /api/v1/layouts/{id}` — fetch one layout (role: read; per-object authz).
async fn get_layout(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.repository.get_layout(&id)?;
    Ok(layout_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/layouts/{id}` — create a layout (role: write; per-object authz).
async fn create_layout(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<LayoutInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.repository.create_layout(&id, input)?;
    // Audit only after the mutation succeeded (who/what/when).
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        LAYOUT_KIND,
        &id,
        Some(versioned.layout.body.clone()),
    );
    Ok(layout_response(StatusCode::CREATED, &versioned))
}

/// `PUT /api/v1/layouts/{id}` — replace a layout (role: write; If-Match → 412).
async fn update_layout(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<LayoutInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    // Enforce the optimistic-concurrency precondition against the live version
    // before mutating.
    let current = state.repository.get_layout(&id)?;
    if_match.require(LAYOUT_KIND, &id, current.version)?;
    let versioned = state.repository.update_layout(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        LAYOUT_KIND,
        &id,
        Some(versioned.layout.body.clone()),
    );
    Ok(layout_response(StatusCode::OK, &versioned))
}

/// `DELETE /api/v1/layouts/{id}` — delete a layout (role: administer; If-Match).
async fn delete_layout(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.repository.get_layout(&id)?;
    if_match.require(LAYOUT_KIND, &id, current.version)?;
    state.repository.delete_layout(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        LAYOUT_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Submit a command, honoring the `Idempotency-Key` and returning `202`.
///
/// Shared by every operational-command handler (start/stop/swap and the salvo +
/// tally-override surfaces): it reserves the idempotency key, builds the command
/// with the minted [`OperationId`], and `try_submit`s it **non-blocking** so a
/// full bus sheds to `503` (invariant #10) rather than ever blocking the engine.
pub(crate) fn submit_accepted(
    state: &AppState,
    idem: &IdempotencyKey,
    build: impl FnOnce(OperationId) -> Command,
) -> ControlResult<Response> {
    match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Replay(op) => {
            // A retried request with the same key: return the original id
            // without re-enqueuing the command.
            let body = AcceptedBody {
                operation_id: op.to_string(),
                kind: "replay".to_owned(),
            };
            Ok((StatusCode::ACCEPTED, Json(body)).into_response())
        }
        Reservation::Fresh(op) => {
            let command = build(op.clone());
            let kind = command.kind().to_owned();
            // Non-blocking submit: a full bus sheds load (503) rather than
            // blocking the engine (invariant #10). If the submit is shed, the
            // command never reached the engine, so we MUST release the
            // idempotency reservation we just took — otherwise a client retry
            // with the same key would hit `Reservation::Replay` and receive a
            // false `202 Accepted` (kind:"replay") for a command that was never
            // enqueued. Releasing lets the retry re-reserve and actually submit.
            if let Err(_shed) = state.commands.try_submit(command) {
                state.idempotency.release(idem.0.as_deref(), &op);
                return Err(ControlError::EngineBusy);
            }
            let body = AcceptedBody {
                operation_id: op.to_string(),
                kind,
            };
            Ok((StatusCode::ACCEPTED, Json(body)).into_response())
        }
    }
}

/// `POST /api/v1/commands/start` — start program output (role: write; 202).
async fn cmd_start(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    submit_accepted(&state, &idem, |op| Command::Start { op })
}

/// `POST /api/v1/commands/stop` — stop program output (role: write; 202).
async fn cmd_stop(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    submit_accepted(&state, &idem, |op| Command::Stop { op })
}

/// `POST /api/v1/commands/swap` — swap a tile's source (role: write; 202).
async fn cmd_swap(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<SwapRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &req.tile)?;
    let tile = req.tile.clone();
    let source = req.source.clone();
    let response = submit_accepted(&state, &idem, |op| Command::SwapSource {
        op,
        tile: req.tile,
        source: req.source,
    })?;
    // Audit the accepted command (the engine reports its outcome separately on
    // the realtime stream; what we audit here is the operator's request).
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "tile",
        &tile,
        Some(serde_json::json!({ "command": "swap", "source": source })),
    );
    Ok(response)
}

impl axum::extract::FromRequestParts<AppState> for Principal {
    type Rejection = ControlError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = header_value(&parts.headers, header::AUTHORIZATION);
        // Primary: native API key. If that fails and a JWT validator is
        // configured, fall back to OAuth2/JWT (the alternative authn path).
        match state.api_keys.verify_authorization(header.as_deref()) {
            Ok(principal) => Ok(principal),
            Err(api_key_err) => state
                .authenticate_jwt(header.as_deref())
                .ok_or(api_key_err)?,
        }
    }
}

/// Extract a header value as an owned string, if present and valid UTF-8.
fn header_value(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Build the `/api/v1` resource + command routes (without the realtime or docs
/// routes, which are wired by [`crate::router()`]).
pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/layouts", get(list_layouts))
        .route(
            "/layouts/{id}",
            get(get_layout)
                .post(create_layout)
                .put(update_layout)
                .delete(delete_layout),
        )
        // Sources resource CRUD (managed inputs), mirroring layouts.
        .route("/sources", get(sources::list_sources))
        .route(
            "/sources/{id}",
            get(sources::get_source)
                .post(sources::create_source)
                .put(sources::update_source)
                .delete(sources::delete_source),
        )
        // Outputs resource CRUD (managed sinks/servers), mirroring layouts.
        .route("/outputs", get(outputs::list_outputs))
        .route(
            "/outputs/{id}",
            get(outputs::get_output)
                .post(outputs::create_output)
                .put(outputs::update_output)
                .delete(outputs::delete_output),
        )
        // Overlays resource CRUD (managed overlay layers), mirroring layouts.
        .route("/overlays", get(overlays::list_overlays))
        .route(
            "/overlays/{id}",
            get(overlays::get_overlay)
                .post(overlays::create_overlay)
                .put(overlays::update_overlay)
                .delete(overlays::delete_overlay),
        )
        .route("/commands/start", post(cmd_start))
        .route("/commands/stop", post(cmd_stop))
        .route("/commands/swap", post(cmd_swap))
        .route("/alarms", get(alarms::list_alarms))
        .route("/alarms/{id}/ack", post(alarms::ack_alarm))
        // Salvo operator surface: CRUD + arm/take/cancel.
        .route("/salvos", get(salvos::list_salvos))
        .route(
            "/salvos/{id}",
            get(salvos::get_salvo)
                .put(salvos::put_salvo)
                .delete(salvos::delete_salvo),
        )
        .route("/salvos/{id}/arm", post(salvos::arm_salvo))
        .route("/salvos/{id}/take", post(salvos::take_salvo))
        .route("/salvos/{id}/cancel", post(salvos::cancel_salvo))
        // Tally operator surface: read resolved state, profiles, manual override.
        .route("/tally", get(tally::list_tally))
        .route(
            "/tally/override",
            axum::routing::put(tally::set_override).delete(tally::clear_override),
        )
        .route("/tally/profiles", get(tally::list_profiles))
        .route(
            "/tally/profiles/{id}",
            get(tally::get_profile)
                .put(tally::put_profile)
                .delete(tally::delete_profile),
        )
        // Read-only change audit log.
        .route("/audit", get(audit::list_audit))
        // Config versioning: history + commit, single revision, diff, rollback.
        .route(
            "/config/{target}",
            get(config::list_history).put(config::commit_revision),
        )
        .route("/config/{target}/rev/{revision}", get(config::get_revision))
        .route("/config/{target}/diff", get(config::diff_revisions))
        .route(
            "/config/{target}/rollback",
            post(config::rollback_revision),
        )
}
