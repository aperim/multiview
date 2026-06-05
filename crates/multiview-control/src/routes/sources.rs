//! The sources resource surface under `/api/v1/sources`.
//!
//! A **source** is a managed input (`multiview_config::Source`): RTSP/HLS/SRT/
//! RTMP/NDI/file/test. This module mirrors the layouts handlers
//! ([`crate::routes`]) over the [`ResourceRepository`](crate::resource_store::ResourceRepository)
//! trait, with `ETag`/`If-Match` optimistic concurrency on every mutation
//! (ADR-W006), RBAC via [`Principal`], and an audit record after each successful
//! write. The stored `body` is the opaque config-as-code document; engine-side
//! validation against `multiview-config` happens before it is applied. Errors
//! are RFC 9457 problem documents.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::concurrency::IfMatch;
use crate::error::ControlResult;
use crate::resource_store::{Resource, ResourceInput, VersionedResource, SOURCE_KIND};
use crate::state::AppState;

/// Attach the resource's `ETag` to a successful response carrying a source.
fn source_response(status: StatusCode, versioned: &VersionedResource) -> Response {
    let etag = versioned.version.to_etag();
    let mut response = (status, Json(versioned.resource.clone())).into_response();
    if let Ok(value) = header::HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

/// `GET /api/v1/sources` — list all sources (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources",
        tag = "sources",
        responses(
            (status = 200, description = "All sources, id-sorted.", body = [crate::resource_store::Resource]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_sources(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<Resource>>> {
    principal.role.require(Action::Read)?;
    let sources = state
        .sources
        .list()?
        .into_iter()
        .map(|v| v.resource)
        .collect();
    Ok(Json(sources))
}

/// `GET /api/v1/sources/{id}` — fetch one source (role: read; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 200, description = "The source (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read this source.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn get_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> ControlResult<Response> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.sources.get(&id)?;
    Ok(source_response(StatusCode::OK, &versioned))
}

/// `POST /api/v1/sources/{id}` — create a source (role: write; per-object authz).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::resource_store::ResourceInput,
        responses(
            (status = 201, description = "The created source (ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn create_source(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let versioned = state.sources.create(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Create,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(source_response(StatusCode::CREATED, &versioned))
}

/// `PUT /api/v1/sources/{id}` — replace a source (role: write; If-Match → 412).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        put,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        request_body = crate::resource_store::ResourceInput,
        responses(
            (status = 200, description = "The replaced source (new ETag in the response header).", body = crate::resource_store::Resource),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to write.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn update_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
    Json(input): Json<ResourceInput>,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.sources.get(&id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    let versioned = state.sources.update(&id, input)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        SOURCE_KIND,
        &id,
        Some(versioned.resource.body.clone()),
    );
    Ok(source_response(StatusCode::OK, &versioned))
}

/// `DELETE /api/v1/sources/{id}` — delete a source (role: administer; If-Match).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        delete,
        path = "/api/v1/sources/{id}",
        tag = "sources",
        params(("id" = String, Path, description = "Source id.")),
        responses(
            (status = 204, description = "The source was deleted."),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to administer.", body = crate::problem::Problem),
            (status = 404, description = "No source with that id.", body = crate::problem::Problem),
            (status = 412, description = "If-Match precondition failed.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn delete_source(
    State(state): State<AppState>,
    principal: Principal,
    if_match: IfMatch,
    Path(id): Path<String>,
) -> ControlResult<StatusCode> {
    principal.role.require(Action::Administer)?;
    crate::auth::authorize_object(&principal, &id)?;
    let current = state.sources.get(&id)?;
    if_match.require(SOURCE_KIND, &id, current.version)?;
    state.sources.delete(&id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Delete,
        SOURCE_KIND,
        &id,
        None,
    );
    Ok(StatusCode::NO_CONTENT)
}
