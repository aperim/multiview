//! The mDNS device-**discovery** surface under `/api/v1/discovery/devices`
//! (DEV-A5, ADR-M008 §6, ADR-0041 untrusted-inventory doctrine).
//!
//! Discovery produces an **untrusted inventory** of services found on the LAN —
//! Cast (`_googlecast._tcp`), NDI sources incl. `ZowieBox` (`_ndi._tcp`), and an
//! operator-configured (unverified) zowietek-control type. The inventory is a
//! list of **hints requiring explicit confirm-adopt**: discovery NEVER creates a
//! device. An operator confirms adoption by `POST`ing to the existing
//! [`create_device`](super::devices::create_device) (`POST /devices/{id}`)
//! referencing a discovered address — this module only *informs* that choice.
//!
//! * `POST /api/v1/discovery/devices/scan` — kick a time-bounded browse
//!   (role: write; `202` + operation id). The browse runs on a bounded
//!   control-plane task that populates the untrusted inventory and publishes
//!   `device.discovered` events on [`Topic::Devices`](multiview_events::Topic::Devices)
//!   via the conflating broadcaster (drop-oldest — invariant #10).
//! * `GET /api/v1/discovery/devices` — the current untrusted inventory snapshot
//!   (role: read; AAAA-first, IPv4 labelled legacy, stale rows purged on read).
//!
//! ## Isolation (invariant #10)
//!
//! The scan task touches only control-plane state: it browses (the browser owns
//! its own socket/thread), writes the **bounded drop-oldest** inventory, and
//! publishes events through the engine's **non-blocking** broadcast. It never
//! awaits a client and never sends on a channel a slow consumer can fill, so it
//! cannot back-pressure the engine — the same proof shape as the other
//! control-plane producers.
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth::{Action, Principal};
use crate::concurrency::{IdempotencyKey, Reservation};
use crate::devices::broadcaster::DeviceBroadcaster;
use crate::devices::discovery::{
    default_service_types, DiscoveredService, DiscoveryBrowser, DiscoveryInventory,
    DEFAULT_ENTRY_TTL, DEFAULT_SCAN_BUDGET,
};
use crate::error::ControlResult;
use crate::state::AppState;

/// The `202 Accepted` body for a scan: the operation id correlating the scan and
/// the human-readable scope of what is browsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ScanAccepted {
    /// The operation id for this scan (the `device.discovered` rows it produces
    /// stream on the realtime `devices` topic while it runs).
    pub operation_id: String,
    /// The service types being browsed (Cast + NDI, plus any configured
    /// zowietek-control type).
    pub service_types: Vec<String>,
    /// The scan time budget in milliseconds (the browse is always time-bounded).
    pub budget_ms: u64,
    /// A reminder that discovery is **untrusted**: rows require explicit
    /// confirm-adopt and discovery never creates a device (ADR-0041).
    pub note: String,
}

/// `POST /api/v1/discovery/devices/scan` — kick a time-bounded mDNS browse
/// (role: write; `202` + operation id).
///
/// The browse runs on a bounded control-plane task: it asks the injected browser
/// for services within a time budget, classifies each into the **untrusted
/// inventory** (AAAA-first, TTL-stamped), and publishes a `device.discovered`
/// event per service. It never creates a device (ADR-0041). A retried
/// `Idempotency-Key` returns the original operation id.
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        post,
        path = "/api/v1/discovery/devices/scan",
        tag = "discovery",
        responses(
            (status = 202, description = "Scan accepted; discovered rows stream as device.discovered events and land in the untrusted inventory.", body = ScanAccepted),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to scan.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn scan_devices(
    State(state): State<AppState>,
    principal: Principal,
    idem: IdempotencyKey,
) -> ControlResult<Response> {
    principal.role.require(Action::Write)?;

    // Mint (or replay) an operation id for this scan.
    let op = match state.idempotency.reserve(idem.0.as_deref()) {
        Reservation::Fresh(op) | Reservation::Replay(op) => op,
    };

    let service_types = default_service_types(None);
    let budget = DEFAULT_SCAN_BUDGET;

    // The browse runs off the request path on a bounded control-plane task. It
    // publishes via the engine's non-blocking drop-oldest broadcast and writes
    // the bounded inventory — it never awaits a client (invariant #10).
    let inventory = Arc::clone(&state.discovery);
    let browser = Arc::clone(&state.discovery_browser);
    let broadcaster =
        DeviceBroadcaster::new(Arc::clone(&state.engine), Arc::clone(&state.device_status));
    let scan_types = service_types.clone();
    tokio::spawn(async move {
        run_scan(inventory, browser, broadcaster, scan_types, budget).await;
    });

    let body = ScanAccepted {
        operation_id: op.to_string(),
        service_types,
        budget_ms: u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
        note: "discovered services are an untrusted inventory; confirm-adopt via \
               POST /devices/{id} — discovery never creates a device"
            .to_owned(),
    };
    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

/// Run one bounded browse: classify each found service into the untrusted
/// inventory and publish a `device.discovered` event for it.
///
/// The browser's `browse` is potentially blocking (the `mdns-sd` daemon delivers
/// over a channel it drains for the budget), so it runs on a blocking thread —
/// this keeps the async runtime free and, crucially, off the engine path. Every
/// write here is to bounded control-plane state or the non-blocking broadcast
/// (invariant #10).
async fn run_scan(
    inventory: Arc<DiscoveryInventory>,
    browser: Arc<dyn DiscoveryBrowser>,
    broadcaster: DeviceBroadcaster,
    service_types: Vec<String>,
    budget: Duration,
) {
    let found = tokio::task::spawn_blocking(move || browser.browse(&service_types, budget))
        .await
        .unwrap_or_default();
    let expires_at = Instant::now() + DEFAULT_ENTRY_TTL;
    for raw in &found {
        let service = DiscoveredService::from_raw(raw, None, expires_at);
        // Publish the untrusted row first (so a realtime client sees it), then
        // record it in the inventory the GET reads.
        let primary = service.primary();
        broadcaster.discovered(
            service.driver_kind.as_str(),
            &primary.address,
            primary.family,
            Some(service.name.clone()),
        );
        inventory.upsert(service);
    }
}

/// `GET /api/v1/discovery/devices` — the current untrusted inventory snapshot
/// (role: read).
///
/// Returns the discovered services found by the most recent scans, AAAA-first
/// with IPv4 labelled legacy, stale (TTL-expired) rows purged on read. These are
/// **hints**, not devices: adopting one is the separate `POST /devices/{id}`
/// confirm-adopt referencing a discovered address (ADR-0041).
#[cfg_attr(
    feature = "openapi",
    utoipa::path(
        get,
        path = "/api/v1/discovery/devices",
        tag = "discovery",
        responses(
            (status = 200, description = "The untrusted discovery inventory (hints requiring explicit confirm-adopt; never devices).", body = [DiscoveredService]),
            (status = 401, description = "Missing or invalid credentials.", body = crate::problem::Problem),
            (status = 403, description = "Not authorized to read discovery.", body = crate::problem::Problem),
        ),
    )
)]
pub(crate) async fn list_discovered(
    State(state): State<AppState>,
    principal: Principal,
) -> ControlResult<Json<Vec<DiscoveredService>>> {
    principal.role.require(Action::Read)?;
    Ok(Json(state.discovery.snapshot()))
}
