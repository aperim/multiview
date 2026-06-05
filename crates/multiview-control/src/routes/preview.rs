//! Live preview snapshots under `/api/v1/preview`.
//!
//! Low-rate JPEG stills of the composited **program** and of each **input**, for
//! the web UI's monitoring view. The pixels come from the engine via the
//! isolation-safe [`PreviewProvider`](crate::preview::PreviewProvider) (a
//! wait-free latest-frame read + on-request encode — never on the output-clock
//! loop). `image/jpeg` with `Cache-Control: no-store`; `503` when no frame is
//! available yet (freshly started engine / unknown input), so the UI shows a
//! placeholder rather than an error.
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::auth::{Action, Principal};
use crate::state::AppState;

/// The JPEG quality used for preview stills (1–100): low enough to be cheap,
/// high enough to be useful for monitoring.
const PREVIEW_QUALITY: u8 = 70;

/// Build a `200 image/jpeg` response (no-store) for an encoded still.
fn jpeg_response(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response()
}

/// `GET /api/v1/preview/program.jpg` — the latest composited program frame
/// (role: read). `503` when no frame has been produced yet.
pub(crate) async fn program_jpeg(State(state): State<AppState>, principal: Principal) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    match state.preview.program_jpeg(PREVIEW_QUALITY) {
        Some(bytes) => jpeg_response(bytes),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// `GET /api/v1/preview/inputs/{id}.jpg` — the latest frame of input `id`
/// (role: read). `503` when the input is unknown or has produced no frame.
pub(crate) async fn input_jpeg(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    // `{id}.jpg` — strip the extension the UI requests so the id matches the
    // engine's source id.
    let id = id.strip_suffix(".jpg").unwrap_or(&id);
    match state.preview.input_jpeg(id, PREVIEW_QUALITY) {
        Some(bytes) => jpeg_response(bytes),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// `GET /api/v1/preview/inputs` — the ids of inputs that can be previewed
/// (role: read), for the UI to enumerate thumbnails.
pub(crate) async fn list_input_ids(
    State(state): State<AppState>,
    principal: Principal,
) -> Response {
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }
    Json(state.preview.input_ids()).into_response()
}
