//! Config-versioning routes under `/api/v1/config/{target}`.
//!
//! A config/layout document is tracked as an append-only sequence of immutable
//! revisions per `target` key (see [`crate::versioning`]). The routes expose:
//!
//! * `GET  /config/{target}` — the revision history (newest-first, role: read).
//! * `GET  /config/{target}/rev/{revision}` — one immutable revision (role:
//!   read). A distinct `rev/` prefix avoids any routing ambiguity with the
//!   sibling literal `diff`/`rollback` segments.
//! * `PUT  /config/{target}` — commit a new revision (role: write).
//! * `GET  /config/{target}/diff?from=&to=` — structural diff (role: read).
//! * `POST /config/{target}/rollback` — append a revision restoring a prior one
//!   (role: write).
//!
//! Every successful commit/rollback is recorded in the change audit log.
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use crate::audit::AuditAction;
use crate::auth::{Action, Principal};
use crate::error::ControlResult;
use crate::state::AppState;
use crate::versioning::{ConfigRevision, DocumentDiff, RevisionId, CONFIG_REVISION_KIND};

/// The body of a `PUT /config/{target}` commit.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(serde::Serialize, utoipa::ToSchema))]
pub struct CommitRequest {
    /// The document to commit as the next revision.
    pub document: serde_json::Value,
    /// A short commit message.
    #[serde(default)]
    pub message: String,
}

/// The body of a `POST /config/{target}/rollback`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(feature = "openapi", derive(serde::Serialize, utoipa::ToSchema))]
pub struct RollbackRequest {
    /// The revision id to restore (a new revision is appended).
    pub to: u64,
}

/// Query parameters for the revision diff.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct DiffQuery {
    /// The base revision.
    pub from: u64,
    /// The target revision.
    pub to: u64,
}

/// `GET /api/v1/config/{target}` — revision history, newest-first (role: read).
pub(crate) async fn list_history(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
) -> ControlResult<Json<Vec<ConfigRevision>>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    Ok(Json(state.config_versions.history(&target)?))
}

/// `GET /api/v1/config/{target}/diff?from=&to=` — structural diff (role: read).
pub(crate) async fn diff_revisions(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Query(query): Query<DiffQuery>,
) -> ControlResult<Json<DocumentDiff>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    let diff = state.config_versions.diff(
        &target,
        RevisionId::new(query.from),
        RevisionId::new(query.to),
    )?;
    Ok(Json(diff))
}

/// `GET /api/v1/config/{target}/{revision}` — one revision (role: read).
pub(crate) async fn get_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path((target, revision)): Path<(String, u64)>,
) -> ControlResult<Json<ConfigRevision>> {
    principal.role.require(Action::Read)?;
    crate::auth::authorize_object(&principal, &target)?;
    Ok(Json(
        state
            .config_versions
            .get(&target, RevisionId::new(revision))?,
    ))
}

/// `PUT /api/v1/config/{target}` — commit a new revision (role: write).
pub(crate) async fn commit_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Json(req): Json<CommitRequest>,
) -> ControlResult<(StatusCode, Json<ConfigRevision>)> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &target)?;
    let revision =
        state
            .config_versions
            .commit(&target, req.document, &principal.key_id, &req.message)?;
    state.audit(
        &principal.key_id,
        AuditAction::Update,
        CONFIG_REVISION_KIND,
        &target,
        Some(serde_json::json!({ "revision": revision.revision.get() })),
    );
    Ok((StatusCode::CREATED, Json(revision)))
}

/// `POST /api/v1/config/{target}/rollback` — restore a prior revision as a new
/// revision (role: write).
pub(crate) async fn rollback_revision(
    State(state): State<AppState>,
    principal: Principal,
    Path(target): Path<String>,
    Json(req): Json<RollbackRequest>,
) -> ControlResult<(StatusCode, Json<ConfigRevision>)> {
    principal.role.require(Action::Write)?;
    crate::auth::authorize_object(&principal, &target)?;
    let revision =
        state
            .config_versions
            .rollback(&target, RevisionId::new(req.to), &principal.key_id)?;
    state.audit(
        &principal.key_id,
        AuditAction::Rollback,
        CONFIG_REVISION_KIND,
        &target,
        Some(serde_json::json!({ "restored_to": req.to, "new_revision": revision.revision.get() })),
    );
    Ok((StatusCode::CREATED, Json(revision)))
}

/// `GET /api/v1/config/export` — render the live resource stores as a complete
/// `multiview.toml` document (ADR-W015).
///
/// Composes the working layout (the id-sorted first layout whose body carries a
/// `canvas`) with every stored source/output/overlay into a
/// [`multiview_config::MultiviewConfig`], validates the whole document, and
/// returns it as TOML. This closes the management loop honestly today: edit in
/// the UI → export → persist as the config file → the next start applies it.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/config/export",
        tag = "config",
        responses(
            (status = 200, description = "The composed configuration as TOML (`application/toml`).", body = String),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read.", body = crate::problem::Problem),
            (status = 422, description = "The stores do not compose into a valid configuration (detail names the violation).", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn export_config(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<axum::response::Response> {
    use axum::response::IntoResponse;

    principal.role.require(Action::Read)?;

    // The working layout carries { canvas, layout, cells }: pick the id-sorted
    // first layout document that declares a canvas (the seeded shape).
    let layouts = state.repository.list_layouts()?;
    let working = layouts
        .iter()
        .map(|v| &v.layout)
        .find(|layout| layout.body.get("canvas").is_some())
        .ok_or_else(|| {
            crate::error::ControlError::Validation(
                "no working layout (a layout body carrying `canvas`) to export".to_owned(),
            )
        })?;

    let collect = |repo: &std::sync::Arc<dyn crate::resource_store::ResourceRepository>|
     -> ControlResult<Vec<serde_json::Value>> {
        Ok(repo
            .list()?
            .into_iter()
            .map(|v| v.resource.body)
            .collect())
    };

    let document = serde_json::json!({
        "schema_version": 1,
        "canvas": working.body.get("canvas").cloned().unwrap_or(serde_json::Value::Null),
        "layout": working.body.get("layout").cloned().unwrap_or(serde_json::Value::Null),
        "cells": working.body.get("cells").cloned().unwrap_or_else(|| serde_json::json!([])),
        "sources": serde_json::Value::Array(collect(&state.sources)?),
        "outputs": serde_json::Value::Array(collect(&state.outputs)?),
        "overlays": serde_json::Value::Array(collect(&state.overlays)?),
    });

    let config: multiview_config::MultiviewConfig = serde_path_to_error::deserialize(document)
        .map_err(|err| {
            let path = err.path().to_string();
            crate::error::ControlError::Validation(format!(
                "stored resources do not compose into a valid configuration at `{path}`: {}",
                err.into_inner()
            ))
        })?;
    config.validate().map_err(|err| {
        crate::error::ControlError::Validation(format!(
            "composed configuration failed validation: {err}"
        ))
    })?;
    let toml = config.to_toml().map_err(|err| {
        crate::error::ControlError::Repository(format!("TOML render failed: {err}"))
    })?;

    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/toml")],
        toml,
    )
        .into_response())
}
