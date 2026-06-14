//! The read-only structured log-tail REST surface under `/api/v1/logs`
//! (ADR-0060 §5.2).
//!
//! `GET /api/v1/logs` returns recent buffered [`LogRecord`]s from the bounded,
//! drop-oldest log ring ([`AppState::logs`]), filterable by:
//!
//! * `resource_id` — only records attributed to this source/output/layout id;
//! * `kind` — only records of this resource kind (`source`/`output`/…);
//! * `level` — minimum severity (`trace`/`debug`/`info`/`warn`/`error`);
//! * `since` — only records with a sequence strictly greater than this cursor
//!   (the live-tail paging cursor; pair with the `seq` of the last seen record);
//! * `limit` — at most this many of the most-recent matching records.
//!
//! Role: **read**. Errors are RFC 9457 problem documents (a typo'd `level` /
//! `kind` is a `422`). The ring is the producer fed by the telemetry
//! `LogCaptureLayer`; nothing here is on the engine's data plane, and the ring is
//! bounded drop-oldest — the log tail can never back-pressure the engine
//! (invariant #10). The live WebSocket tail on `Topic::Logs` is a separate
//! transport owned by the realtime/web surface; this is the buffered read.
use axum::extract::{Query, State};
use axum::Json;
use multiview_telemetry::{LogFilter, LogLevel, LogRecord, LogResourceKind};
use serde::Deserialize;

use crate::auth::{Action, Principal};
use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// The query parameters accepted by `GET /api/v1/logs`. All optional.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::IntoParams))]
pub struct LogQuery {
    /// Keep only records attributed to this stable config resource id.
    #[serde(default)]
    pub resource_id: Option<String>,
    /// Keep only records of this resource kind (`source`/`output`/`layout`/
    /// `program`/`device`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Keep only records at or above this severity (`trace`/`debug`/`info`/
    /// `warn`/`error`).
    #[serde(default)]
    pub level: Option<String>,
    /// Keep only records with a `seq` strictly greater than this cursor.
    #[serde(default)]
    pub since: Option<u64>,
    /// Return at most this many of the most-recent matching records.
    #[serde(default)]
    pub limit: Option<usize>,
}

impl LogQuery {
    /// Translate the query into a [`LogFilter`], rejecting an unrecognised
    /// `level` or `kind` with a `422` rather than silently ignoring the typo.
    ///
    /// # Errors
    ///
    /// [`ControlError::Validation`] if `level` or `kind` is present but not a
    /// recognised name.
    pub fn into_filter(self) -> ControlResult<LogFilter> {
        let min_level =
            match self.level {
                None => None,
                Some(name) => Some(LogLevel::parse(&name).ok_or_else(|| {
                    ControlError::Validation(format!("unknown log level {name:?}"))
                })?),
            };
        let resource_kind = match self.kind {
            None => None,
            Some(name) => Some(LogResourceKind::parse(&name).ok_or_else(|| {
                ControlError::Validation(format!("unknown resource kind {name:?}"))
            })?),
        };
        Ok(LogFilter {
            resource_id: self.resource_id,
            resource_kind,
            min_level,
            since_seq: self.since,
            limit: self.limit,
        })
    }
}

/// `GET /api/v1/logs` — recent buffered structured log records (role: read).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/logs",
        tag = "logs",
        params(LogQuery),
        responses(
            (status = 200, description = "Matching log records, oldest first.", body = [crate::openapi_schemas::LogRecordDoc]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Authenticated but not authorized to read.", body = crate::problem::Problem),
            (status = 422, description = "An unrecognised level or kind filter.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_logs(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<LogQuery>,
) -> ControlResult<Json<Vec<LogRecord>>> {
    principal.role.require(Action::Read)?;
    let filter = query.into_filter()?;
    Ok(Json(state.logs.query(&filter)))
}
