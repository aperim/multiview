//! `OpenAPI` 3.1 document + Scalar try-it-out (feature `openapi`, default on).
//!
//! Per ADR-W002 the spec is code-first via utoipa (verified `OpenAPI` 3.1 only).
//! [`ApiDoc`] aggregates the path + schema definitions; [`openapi_router`]
//! serves the document at `/api/v1/openapi.json` and the Scalar UI at `/docs`.
use axum::extract::State;
use axum::Json;
use axum::Router;
use utoipa::OpenApi;
use utoipa_scalar::{Scalar, Servable};

use crate::state::AppState;

/// The aggregate `OpenAPI` 3.1 document for the control API.
///
/// Paths and component schemas are registered here so the served
/// `openapi.json` is the single source of truth the SPA client is generated
/// from.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Multiview Control API",
        version = "1.0.0",
        description = "REST + WebSocket + SSE management API for the Multiview engine.",
    ),
    paths(
        crate::routes::list_layouts,
        crate::routes::sources::list_sources,
        crate::routes::outputs::list_outputs,
        crate::routes::overlays::list_overlays,
        crate::routes::alarms::list_alarms,
        crate::routes::alarms::ack_alarm,
        crate::routes::salvos::list_salvos,
        crate::routes::salvos::put_salvo,
        crate::routes::salvos::arm_salvo,
        crate::routes::salvos::take_salvo,
        crate::routes::salvos::cancel_salvo,
        crate::routes::tally::list_tally,
        crate::routes::tally::list_profiles,
        crate::routes::tally::put_profile,
        crate::routes::tally::set_override,
        crate::routes::tally::clear_override,
        crate::routes::audit::list_audit,
        crate::nmos::get_self,
        crate::nmos::list_devices,
        crate::nmos::list_senders,
        crate::nmos::list_receivers,
        crate::nmos::patch_staged,
    ),
    components(schemas(
        crate::problem::Problem,
        crate::repository::Layout,
        crate::repository::LayoutInput,
        crate::resource_store::Resource,
        crate::resource_store::ResourceInput,
        crate::routes::AcceptedBody,
        crate::routes::SwapRequest,
        crate::auth::Role,
        crate::command::OperationId,
        crate::audit::AuditEntry,
        crate::audit::AuditAction,
        crate::versioning::ConfigRevision,
        crate::versioning::RevisionId,
        crate::versioning::DocumentDiff,
        crate::routes::config::CommitRequest,
        crate::routes::config::RollbackRequest,
        crate::openapi_schemas::AlarmRecordDoc,
        crate::openapi_schemas::AlarmKindDoc,
        crate::openapi_schemas::AlarmScopeDoc,
        crate::openapi_schemas::AckStateDoc,
        crate::openapi_schemas::PerceivedSeverityDoc,
        crate::openapi_schemas::TallyColorDoc,
        crate::openapi_schemas::BusSourceDoc,
        crate::openapi_schemas::TallyStateDoc,
        crate::openapi_schemas::TallyTargetDoc,
        crate::openapi_schemas::TallyEntryDoc,
        crate::openapi_schemas::OverrideRequestDoc,
        crate::openapi_schemas::ClearOverrideRequestDoc,
        crate::openapi_schemas::SourceRecallDoc,
        crate::openapi_schemas::TallyRecallDoc,
        crate::openapi_schemas::UmdRecallDoc,
        crate::openapi_schemas::SalvoDoc,
        crate::openapi_schemas::BitColorDoc,
        crate::openapi_schemas::IndexCellDoc,
        crate::openapi_schemas::TallyProfileDoc,
        crate::nmos::is04::Node,
        crate::nmos::is04::Device,
        crate::nmos::is04::Sender,
        crate::nmos::is04::Receiver,
        crate::nmos::is04::ResourceCore,
        crate::nmos::is04::MediaFormat,
        crate::nmos::is04::Registration,
        crate::nmos::is04::ResourceType,
        crate::nmos::is05::ConnectionRequest,
        crate::nmos::is05::ConnectionState,
        crate::nmos::is05::Activation,
        crate::nmos::is05::ActivationMode,
        crate::nmos::is05::TransportParams,
        crate::nmos::is08::ChannelMap,
        crate::nmos::is08::ChannelSource,
        crate::nmos::is10::Is10Claims,
        crate::nmos::is10::NmosApiClaim,
        crate::nmos::is10::NmosAccess,
    )),
    tags(
        (name = "layouts", description = "Layout resource CRUD"),
        (name = "sources", description = "Source (managed input) resource CRUD"),
        (name = "outputs", description = "Output (managed sink/server) resource CRUD"),
        (name = "overlays", description = "Overlay (managed overlay layer) resource CRUD"),
        (name = "commands", description = "Operational commands (start/stop/swap)"),
        (name = "alarms", description = "Monitoring alarms: list + acknowledge"),
        (name = "salvos", description = "Salvo definitions + arm/take/cancel"),
        (name = "tally", description = "Tally state, profiles, and manual override"),
        (name = "audit", description = "Read-only change audit log (who/what/when)"),
        (name = "config", description = "Config/layout versioning: revisions, diff, rollback"),
        (name = "nmos", description = "AMWA NMOS Node API: IS-04 resources + IS-05 connection"),
        (name = "realtime", description = "WebSocket + SSE event stream"),
    )
)]
pub struct ApiDoc;

impl ApiDoc {
    /// The static list of REST routes this API exposes, in the order they are
    /// registered. Used by tests to assert the surface is complete without
    /// depending on utoipa's `#[utoipa::path]` macro wiring for every handler.
    #[must_use]
    pub fn rest_routes() -> &'static [(&'static str, &'static str)] {
        &[
            ("GET", "/api/v1/layouts"),
            ("GET", "/api/v1/layouts/{id}"),
            ("POST", "/api/v1/layouts/{id}"),
            ("PUT", "/api/v1/layouts/{id}"),
            ("DELETE", "/api/v1/layouts/{id}"),
            ("GET", "/api/v1/sources"),
            ("GET", "/api/v1/sources/{id}"),
            ("POST", "/api/v1/sources/{id}"),
            ("PUT", "/api/v1/sources/{id}"),
            ("DELETE", "/api/v1/sources/{id}"),
            ("GET", "/api/v1/outputs"),
            ("GET", "/api/v1/outputs/{id}"),
            ("POST", "/api/v1/outputs/{id}"),
            ("PUT", "/api/v1/outputs/{id}"),
            ("DELETE", "/api/v1/outputs/{id}"),
            ("GET", "/api/v1/overlays"),
            ("GET", "/api/v1/overlays/{id}"),
            ("POST", "/api/v1/overlays/{id}"),
            ("PUT", "/api/v1/overlays/{id}"),
            ("DELETE", "/api/v1/overlays/{id}"),
            ("POST", "/api/v1/commands/start"),
            ("POST", "/api/v1/commands/stop"),
            ("POST", "/api/v1/commands/swap"),
            ("GET", "/api/v1/alarms"),
            ("POST", "/api/v1/alarms/{id}/ack"),
            ("GET", "/api/v1/salvos"),
            ("GET", "/api/v1/salvos/{id}"),
            ("PUT", "/api/v1/salvos/{id}"),
            ("DELETE", "/api/v1/salvos/{id}"),
            ("POST", "/api/v1/salvos/{id}/arm"),
            ("POST", "/api/v1/salvos/{id}/take"),
            ("POST", "/api/v1/salvos/{id}/cancel"),
            ("GET", "/api/v1/tally"),
            ("PUT", "/api/v1/tally/override"),
            ("DELETE", "/api/v1/tally/override"),
            ("GET", "/api/v1/tally/profiles"),
            ("GET", "/api/v1/tally/profiles/{id}"),
            ("PUT", "/api/v1/tally/profiles/{id}"),
            ("DELETE", "/api/v1/tally/profiles/{id}"),
            // Read-only change audit log.
            ("GET", "/api/v1/audit"),
            // Config versioning: history + commit, single revision, diff, rollback.
            ("GET", "/api/v1/config/{target}"),
            ("PUT", "/api/v1/config/{target}"),
            ("GET", "/api/v1/config/{target}/rev/{revision}"),
            ("GET", "/api/v1/config/{target}/diff"),
            ("POST", "/api/v1/config/{target}/rollback"),
            // AMWA NMOS Node API (under the standardised /x-nmos base).
            ("GET", "/x-nmos/node/v1.3/self"),
            ("GET", "/x-nmos/node/v1.3/devices"),
            ("GET", "/x-nmos/node/v1.3/senders"),
            ("GET", "/x-nmos/node/v1.3/receivers"),
            (
                "GET",
                "/x-nmos/connection/v1.1/single/receivers/{id}/active",
            ),
            (
                "PATCH",
                "/x-nmos/connection/v1.1/single/receivers/{id}/staged",
            ),
            ("GET", "/api/v1/ws"),
            ("GET", "/api/v1/events"),
        ]
    }
}

/// Serve the raw `OpenAPI` document as JSON.
async fn openapi_json(State(_state): State<AppState>) -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Build the router serving the `OpenAPI` JSON (`/api/v1/openapi.json`) and the
/// Scalar UI (`/docs`).
pub fn openapi_router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/openapi.json", axum::routing::get(openapi_json))
        .merge(Scalar::with_url("/docs", ApiDoc::openapi()))
}
