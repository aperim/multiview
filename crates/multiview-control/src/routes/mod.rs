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
pub mod preview;
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

/// The body of a `POST /commands/apply-layout` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApplyLayoutRequest {
    /// The layout id to make active on the running multiview.
    pub layout: String,
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
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        responses(
            (status = 200, description = "The layout (ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to read this layout.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
        ),
    )
)]
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
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        request_body = LayoutInput,
        responses(
            (status = 201, description = "The created layout (ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to write.", body = Problem),
        ),
    )
)]
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
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        request_body = LayoutInput,
        responses(
            (status = 200, description = "The replaced layout (new ETag in the response header).", body = Layout),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to write.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
            (status = 412, description = "If-Match precondition failed.", body = Problem),
        ),
    )
)]
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
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/layouts/{id}",
        tag = "layouts",
        params(("id" = String, Path, description = "Layout id.")),
        responses(
            (status = 204, description = "The layout was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to administer.", body = Problem),
            (status = 404, description = "No layout with that id.", body = Problem),
            (status = 412, description = "If-Match precondition failed.", body = Problem),
        ),
    )
)]
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
            // The command-outcome correlation key (if any) — computed from the
            // command before it is submitted (and moved). Commands with a single,
            // unambiguous outcome event (start/stop, named salvo arm/take/cancel)
            // are keyed; others yield no key and ride the stream uncorrelated.
            let corr_key = crate::realtime::CorrKey::for_command(&command);
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
            // The command is enqueued: record its outcome correlation so the
            // realtime projection echoes this op id as `corr` on the matching
            // outcome event (ADR-W008). Recorded only on the success path, so a
            // shed command leaves no orphan correlation. The engine drains on its
            // own tick cadence (never synchronously here), and the realtime
            // projection runs on separate per-client tasks, so the correlation is
            // recorded before any outcome can be delivered. The registry is
            // bounded drop-oldest and never on the engine hot loop (invariant #10).
            if let Some(key) = corr_key {
                state.corr.record(key, op.clone());
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

/// `POST /api/v1/commands/apply-layout` — apply a layout to the running
/// multiview (role: write; per-object authz; 202).
///
/// Mirrors [`cmd_swap`]: it only `try_submit`s the command via
/// [`submit_accepted`] (idempotency + shed-on-full free) and never blocks the
/// engine (invariant #10). The eventual outcome arrives on the realtime stream
/// correlated by the returned operation id (ADR-W008).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/commands/apply-layout",
        tag = "commands",
        request_body = ApplyLayoutRequest,
        responses(
            (status = 202, description = "Apply-layout accepted; outcome on the realtime stream.", body = AcceptedBody),
            (status = 401, description = "Missing or invalid credentials.", body = Problem),
            (status = 403, description = "Not authorized to apply a layout.", body = Problem),
            (status = 503, description = "Engine command bus at capacity; shed.", body = Problem),
        ),
    )
)]
pub(crate) async fn cmd_apply_layout(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
    Json(req): Json<ApplyLayoutRequest>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &req.layout)?;
    let layout = req.layout.clone();
    let response = submit_accepted(&state, &idem, |op| Command::ApplyLayout {
        op,
        layout: req.layout,
    })?;
    // Audit the accepted command (the engine reports its outcome separately on
    // the realtime stream; what we audit here is the operator's request).
    state.audit(
        &principal.key_id,
        AuditAction::Command,
        "layout",
        &layout,
        Some(serde_json::json!({ "command": "apply_layout" })),
    );
    Ok(response)
}

impl axum::extract::FromRequestParts<AppState> for Principal {
    type Rejection = ControlError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Auth disabled (explicit trusted-network mode): every request is a local
        // admin, no credential required.
        if state.auth_disabled {
            return Ok(Principal::local_admin());
        }
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
        .route("/commands/apply-layout", post(cmd_apply_layout))
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
        // Live preview snapshots (program + per-input JPEG stills).
        .route("/preview/program.jpg", get(preview::program_jpeg))
        .route("/preview/inputs", get(preview::list_input_ids))
        .route("/preview/inputs/{id}", get(preview::input_jpeg))
        // WHEP focus sessions (sub-second WebRTC preview) per scope: SDP offer
        // in -> 201 + answer SDP + Location; DELETE the resource to release.
        .route("/preview/program/whep", post(preview::program_whep_open))
        .route(
            "/preview/program/whep/{session_id}",
            axum::routing::delete(preview::program_whep_close),
        )
        .route("/preview/inputs/{id}/whep", post(preview::input_whep_open))
        .route(
            "/preview/inputs/{id}/whep/{session_id}",
            axum::routing::delete(preview::input_whep_close),
        )
        .route("/preview/outputs/{id}/whep", post(preview::output_whep_open))
        .route(
            "/preview/outputs/{id}/whep/{session_id}",
            axum::routing::delete(preview::output_whep_close),
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
