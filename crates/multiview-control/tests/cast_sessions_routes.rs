//! End-to-end tests for the ephemeral Cast sessions surface (DEV-D2,
//! ADR-M011): `/api/v1/cast/sessions` start/list/stop, the save-as-device
//! promotion into a normal `Device{driver: cast}` registry entry, and the
//! receiver-namespace volume control — all driven through the real router
//! with a scripted cast session factory (socket-free). Sessions are
//! EPHEMERAL: runtime-only, never part of the devices store, never exported.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::devices::cast::media::{CastDelivery, CastMediaTarget, HlsSegmentFormat};
use multiview_control::devices::cast::runtime::CastSessionFactory;
use multiview_control::devices::cast::session::{
    CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound,
};
use multiview_control::devices::DevicePollerRegistry;
use support::{
    body_json, delete_if_match, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    VIEWER_TOKEN,
};

/// The delivery map the routes resolve outputs against: two HLS renditions.
fn delivery() -> Arc<CastDelivery> {
    let mut d = CastDelivery::new();
    d.insert(
        "out-a",
        CastMediaTarget {
            url: "http://192.0.2.7:8080/hls/out-a/a.m3u8".to_owned(),
            format: HlsSegmentFormat::MpegTs,
        },
    );
    d.insert(
        "out-b",
        CastMediaTarget {
            url: "http://192.0.2.7:8080/hls/out-b/b.m3u8".to_owned(),
            format: HlsSegmentFormat::Fmp4,
        },
    );
    Arc::new(d)
}

/// A poller registry with a scripted cast factory: every cast spawn gets a
/// connector whose connects are refused (the actor then supervises reconnects
/// — irrelevant here; the routes only need a real spawned actor).
fn scripted_cast_registry() -> Arc<DevicePollerRegistry> {
    let factory = CastSessionFactory::new(
        Arc::new(ScriptedConnector::new(vec![])),
        delivery(),
        CastSessionConfig::test_fast(),
    );
    Arc::new(DevicePollerRegistry::with_factory(Arc::new(factory)))
}

/// A harness whose state carries the scripted cast registry + delivery map.
fn cast_harness() -> support::Harness {
    harness_with(|state| {
        state
            .with_device_pollers(scripted_cast_registry())
            .with_cast_delivery(delivery())
    })
}

/// A start-session request body.
fn start_body(output: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "address": "[2001:db8::20]:8009",
        "name": "Lounge TV"
    });
    if let Some(output) = output {
        body["output"] = serde_json::Value::String(output.to_owned());
    }
    body
}

#[tokio::test]
async fn start_list_get_stop_round_trips() {
    let h = cast_harness();

    // Start an ad-hoc session.
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(Some("out-b"))),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();
    assert!(
        id.starts_with("cast-session-"),
        "session ids are namespaced: {id}"
    );
    assert_eq!(created["address"], "[2001:db8::20]:8009");
    assert_eq!(created["output"], "out-b");
    assert_eq!(created["name"], "Lounge TV");
    assert_eq!(
        created["media_url"], "http://192.0.2.7:8080/hls/out-b/b.m3u8",
        "the resolved device-reachable media URL is visible"
    );

    // It lists (read role suffices) and fetches by id with a live state.
    let resp = send(&h.router, get("/api/v1/cast/sessions", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    assert_eq!(list.as_array().expect("an array").len(), 1);
    assert_eq!(list[0]["id"], id.as_str());
    assert!(list[0]["state"].is_string(), "a lifecycle state rides along");

    let resp = send(
        &h.router,
        get(&format!("/api/v1/cast/sessions/{id}"), VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Stop tears it down: gone from the list, 404 thereafter.
    let resp = send(
        &h.router,
        delete_if_match(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&h.router, get("/api/v1/cast/sessions", VIEWER_TOKEN)).await;
    let list = body_json(resp).await;
    assert!(list.as_array().expect("an array").is_empty());
    let resp = send(
        &h.router,
        get(&format!("/api/v1/cast/sessions/{id}"), VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn start_without_an_output_casts_the_first_rendition() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(created["output"], "out-a", "the first declared rendition");
    assert_eq!(created["media_url"], "http://192.0.2.7:8080/hls/out-a/a.m3u8");
}

#[tokio::test]
async fn start_with_an_unknown_output_is_a_validation_problem() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("nope")),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("nope"),
        "the problem names the unknown output: {problem}"
    );
}

#[tokio::test]
async fn start_with_a_bad_address_is_a_validation_problem() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &serde_json::json!({ "address": "not a host:port" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn start_without_a_cast_driver_build_is_a_conflict() {
    // The default harness: no cast factory installed (the default registry's
    // no-op factory) — the route reports the missing live driver honestly
    // instead of recording a session that casts nothing.
    let h = harness_with(|state| state.with_cast_delivery(delivery()));
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn start_without_a_delivery_map_is_a_conflict() {
    // No `cast_media_base` configured: no device-reachable URL can be derived.
    let h = harness_with(|state| state.with_device_pollers(scripted_cast_registry()));
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn sessions_are_ephemeral_never_in_the_devices_store() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The devices collection (what config export serializes) stays empty:
    // ad-hoc sessions are runtime-only (ADR-M011: never exported).
    let resp = send(&h.router, get("/api/v1/devices", OPERATOR_TOKEN)).await;
    let devices = body_json(resp).await;
    assert!(
        devices.as_array().expect("an array").is_empty(),
        "an ephemeral session must never appear in the devices store"
    );
}

#[tokio::test]
async fn save_promotes_the_session_to_a_cast_device() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("out-b")),
        ),
    )
    .await;
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    // Promote: a normal Device{driver: cast} registry entry is created
    // carrying the session's address + rendition assignment, and the
    // ephemeral session is gone (one actor remains, keyed by the device id).
    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/cast/sessions/{id}/save"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "device_id": "dev-lounge", "display_name": "Lounge TV" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let device = body_json(resp).await;
    assert_eq!(device["id"], "dev-lounge");
    assert_eq!(device["body"]["driver"], "cast");
    assert_eq!(device["body"]["address"], "[2001:db8::20]:8009");
    assert_eq!(device["body"]["display"]["assign"]["output"], "out-b");

    // The device store now carries it (it WILL export — desired state only).
    let resp = send(&h.router, get("/api/v1/devices/dev-lounge", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The ephemeral session is gone.
    let resp = send(&h.router, get("/api/v1/cast/sessions", OPERATOR_TOKEN)).await;
    let list = body_json(resp).await;
    assert!(list.as_array().expect("an array").is_empty());

    // Saving the same id again conflicts (the device exists).
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    let created = body_json(resp).await;
    let id2 = created["id"].as_str().expect("a session id").to_owned();
    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/cast/sessions/{id2}/save"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "device_id": "dev-lounge" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn volume_dispatches_to_the_running_session() {
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/cast/sessions/{id}/volume"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "level_percent": 42 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let accepted = body_json(resp).await;
    assert!(accepted["operation_id"].is_string());

    // Out-of-range volume is a validation problem.
    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/cast/sessions/{id}/volume"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "level_percent": 101 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // An unknown session is a 404.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions/cast-session-nope/volume",
            OPERATOR_TOKEN,
            &serde_json::json!({ "level_percent": 10 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_surface_requires_authentication_and_write_roles() {
    let h = cast_harness();

    // Viewer can read but not start/stop.
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", VIEWER_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(&h.router, get("/api/v1/cast/sessions", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // No token at all is a 401.
    let resp = send(
        &h.router,
        axum::http::Request::builder()
            .method("GET")
            .uri("/api/v1/cast/sessions")
            .body(axum::body::Body::empty())
            .expect("a request"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Admin may stop too (role: write includes admin).
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", ADMIN_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn stop_clears_the_tombstone_so_session_ids_stay_bounded() {
    use multiview_control::devices::{
        DeviceBroadcaster, DeviceDriverRegistry, DeviceStatusRegistry, PollerWiring,
    };

    // White-box of the route contract: after DELETE, the poller registry's
    // tombstone for the (never-reused, UUID-fresh) session id is cleared so
    // the tombstone set cannot grow without bound under churning sessions.
    let registry = scripted_cast_registry();
    let registry_probe = Arc::clone(&registry);
    let h = harness_with(move |state| {
        state
            .with_device_pollers(registry)
            .with_cast_delivery(delivery())
    });

    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();
    let resp = send(
        &h.router,
        delete_if_match(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The id is no longer tombstoned: a (hypothetical) fresh start for the
    // same id is accepted by the registry — DELETE cleared the tombstone
    // after the deterministic stop, so churning sessions never grow the set.
    let dev: multiview_config::Device = serde_json::from_value(serde_json::json!({
        "id": id,
        "driver": "cast",
        "address": "[2001:db8::20]:8009",
        "display": { "assign": { "output": "out-a" } }
    }))
    .expect("a valid device");
    let engine = Arc::new(
        multiview_engine::EnginePublisher::<
            multiview_control::EngineStateSnapshot,
            multiview_events::Event,
        >::new(8),
    );
    let wiring = PollerWiring {
        broadcaster: DeviceBroadcaster::new(engine, Arc::new(DeviceStatusRegistry::new())),
        drivers: Arc::new(DeviceDriverRegistry::new()),
    };
    assert!(
        registry_probe.start(&dev, &wiring),
        "the DELETE cleared the session id's tombstone"
    );
    registry_probe.stop(&id).await;
}
