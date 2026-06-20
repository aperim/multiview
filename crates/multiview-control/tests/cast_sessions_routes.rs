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
use multiview_control::devices::cast::protocol::{
    CastFrame, NS_MEDIA, NS_RECEIVER, PLATFORM_RECEIVER_ID, SENDER_ID,
};
use multiview_control::devices::cast::runtime::CastSessionFactory;
use multiview_control::devices::cast::session::{
    CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound,
};
use multiview_control::devices::DevicePollerRegistry;
use support::{
    body_json, delete_if_match, get, harness_with, post_json, send, ADMIN_TOKEN, OPERATOR_TOKEN,
    SCOPED_TOKEN, VIEWER_TOKEN,
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
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("out-b")),
        ),
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
    assert!(
        list[0]["state"].is_string(),
        "a lifecycle state rides along"
    );

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
    assert_eq!(
        created["media_url"],
        "http://192.0.2.7:8080/hls/out-a/a.m3u8"
    );
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

/// BOLA (OWASP API1, conventions §H / ADR-W005): the save-as-device promotion
/// touches TWO objects — the target `device_id` it creates AND the path session
/// `id` it reads + retires. A principal scoped to its own object allowlist that
/// is authorized for the target device but NOT for the session id must be
/// **denied** the promotion: otherwise a scoped operator can promote another
/// tenant's running session into a device it controls.
///
/// `SCOPED_TOKEN` is an operator scoped to the object allowlist
/// `["scoped-layout"]`. Saving with `device_id: "scoped-layout"` clears the
/// device-id authorization (that id is in the allowlist), so the ONLY thing that
/// can deny this request is authorizing the session path id — which is a
/// `cast-session-…` id outside the allowlist. The request must be a `403`,
/// exactly as `get`/`stop`/`volume` deny an unauthorized session id.
#[tokio::test]
async fn save_denies_a_session_outside_the_scoped_allowlist() {
    let h = cast_harness();

    // An operator starts an ad-hoc session (it owns a `cast-session-…` id the
    // scoped principal is NOT authorized for).
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("out-b")),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    // The scoped operator is authorized for object "scoped-layout" but NOT for
    // the session id. It targets a device id INSIDE its allowlist, so the
    // device-id authorization passes; the session path id is the lone gate.
    let resp = send(
        &h.router,
        post_json(
            &format!("/api/v1/cast/sessions/{id}/save"),
            SCOPED_TOKEN,
            &serde_json::json!({ "device_id": "scoped-layout" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a scoped principal must not promote a session id outside its allowlist (BOLA)"
    );
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/forbidden");

    // The session was NOT promoted: it is still an ephemeral session (the
    // denied request had no side effect), and no device was created.
    let resp = send(&h.router, get("/api/v1/cast/sessions", ADMIN_TOKEN)).await;
    let list = body_json(resp).await;
    assert_eq!(
        list.as_array().expect("an array").len(),
        1,
        "the denied save must not have retired the ephemeral session"
    );
    let resp = send(&h.router, get("/api/v1/devices/scoped-layout", ADMIN_TOKEN)).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the denied save must not have created the promoted device"
    );
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
    let engine = Arc::new(multiview_engine::EnginePublisher::<
        multiview_control::EngineStateSnapshot,
        multiview_events::Event,
    >::new(8));
    let wiring = PollerWiring {
        broadcaster: DeviceBroadcaster::new(engine, Arc::new(DeviceStatusRegistry::new())),
        drivers: Arc::new(DeviceDriverRegistry::new()),
        cast_sessions: std::sync::Arc::new(
            multiview_control::devices::cast::store::CastSessionStore::new(),
        ),
        clock: std::sync::Arc::new(|| multiview_core::time::MediaTime::from_nanos(0)),
    };
    assert!(
        registry_probe.start(&dev, &wiring),
        "the DELETE cleared the session id's tombstone"
    );
    registry_probe.stop(&id).await;
}

#[tokio::test]
async fn ephemeral_sessions_never_reach_the_config_export() {
    // The start route's contract: an ad-hoc cast session is runtime-only —
    // it must NEVER ride `GET /config/export` (config-as-code carries only
    // adopted devices; a restart intentionally forgets ad-hoc sessions).
    let h = cast_harness();

    // Seed the minimum exportable document (a working layout + the source
    // and output it references), mirroring the config_export suite.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/working",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "name": "working",
                "body": {
                    "canvas": {
                        "width": 1920,
                        "height": 1080,
                        "fps": "30/1",
                        "pixel_format": "nv12",
                        "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": [{
                        "id": "a",
                        "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5 },
                        "source": { "input_id": "cam1" }
                    }]
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "layout seed must land");
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "source seed must land");
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/out-a",
            OPERATOR_TOKEN,
            &serde_json::json!({
                "name": "LL-HLS",
                "body": { "id": "out-a", "kind": "ll_hls", "path": "/var/lib/multiview/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "output seed must land");

    // A live ad-hoc session…
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("out-a")),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    // …lists live but is absent from the exported document.
    let resp = send(&h.router, get("/api/v1/cast/sessions", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await.as_array().expect("an array").len(),
        1,
        "the session is live"
    );

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("an export body");
    let text = String::from_utf8(body.to_vec()).expect("UTF-8 TOML");
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");
    assert!(
        parsed.devices.is_empty(),
        "no device rows: the ephemeral session must not be exported"
    );
    assert!(
        !text.contains(&id),
        "the session id never appears anywhere in the export"
    );
}

// ---------------------------------------------------------------------------
// DEV-D3.1: session started-at + membership lifecycle events.
// ---------------------------------------------------------------------------

/// An inbound frame from the device (scripted-channel test vocabulary,
/// mirroring the `cast_session.rs` builders).
fn from_device(namespace: &str, payload: &serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: payload.to_string(),
    }
}

/// A `RECEIVER_STATUS` carrying the launched Default Media Receiver.
fn receiver_status_with_app() -> CastFrame {
    from_device(
        NS_RECEIVER,
        &serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": "CC1AD845",
                "sessionId": "s-1",
                "transportId": "t-1",
                "displayName": "Default Media Receiver"
            }] }
        }),
    )
}

/// A `MEDIA_STATUS` with one active (PLAYING) media session.
fn media_status_playing() -> CastFrame {
    from_device(
        NS_MEDIA,
        &serde_json::json!({
            "type": "MEDIA_STATUS",
            "requestId": 0,
            "status": [{ "mediaSessionId": 1, "playerState": "PLAYING" }]
        }),
    )
}

#[tokio::test]
async fn start_and_stop_publish_cast_session_lifecycle_events() {
    // Gap 2 (DEV-D3.1): session-list MEMBERSHIP changes must ride the lossless
    // devices lane immediately — never only the SPA's 15 s REST re-poll.
    let h = cast_harness();
    let mut events = h.engine.subscribe();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/cast/sessions",
            OPERATOR_TOKEN,
            &start_body(Some("out-b")),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    let mut started = None;
    while let Ok(envelope) = events.try_recv() {
        if let multiview_events::Event::CastSessionStarted(s) = &*envelope.event {
            started = Some(s.clone());
        }
    }
    let started = started.expect("POST published cast.session.started");
    assert_eq!(started.session_id, id);
    assert_eq!(started.name.as_deref(), Some("Lounge TV"));
    assert_eq!(started.address, "[2001:db8::20]:8009");
    assert_eq!(started.output, "out-b");

    let resp = send(
        &h.router,
        delete_if_match(&format!("/api/v1/cast/sessions/{id}"), OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let mut removed = false;
    while let Ok(envelope) = events.try_recv() {
        if let multiview_events::Event::CastSessionRemoved(r) = &*envelope.event {
            removed = removed || r.session_id == id;
        }
    }
    assert!(removed, "DELETE published cast.session.removed");
}

#[tokio::test]
async fn save_promotion_publishes_cast_session_removed() {
    // The save-as-device promotion retires the EPHEMERAL record (playback
    // continues under the device id): membership changed, so the removal
    // event rides the lane here too.
    let h = cast_harness();
    let mut events = h.engine.subscribe();

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
            &format!("/api/v1/cast/sessions/{id}/save"),
            OPERATOR_TOKEN,
            &serde_json::json!({ "device_id": "dev-save-events" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let mut removed = false;
    while let Ok(envelope) = events.try_recv() {
        if let multiview_events::Event::CastSessionRemoved(r) = &*envelope.event {
            removed = removed || r.session_id == id;
        }
    }
    assert!(
        removed,
        "save published cast.session.removed for the session id"
    );
}

#[tokio::test]
async fn a_refused_start_publishes_no_lifecycle_event() {
    // The no-live-driver 409 records nothing — and must announce nothing.
    let h = harness_with(|state| state.with_cast_delivery(delivery()));
    let mut events = h.engine.subscribe();
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    while let Ok(envelope) = events.try_recv() {
        assert!(
            !matches!(
                &*envelope.event,
                multiview_events::Event::CastSessionStarted(_)
            ),
            "a refused start must not announce a session"
        );
    }
}

#[tokio::test]
async fn started_unix_ns_is_absent_until_the_receiver_accepts_the_load() {
    // Gap 1 (DEV-D3.1): the served doc carries the start stamp ONLY once the
    // receiver accepted the LOAD. This harness's connector refuses every
    // connect, so no LOAD is ever accepted — the field must stay absent
    // (stamping at REST-accept time would lie about failed loads).
    let h = cast_harness();
    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();
    assert!(
        created.get("started_unix_ns").is_none(),
        "no stamp before the LOAD is accepted: {created}"
    );

    let resp = send(
        &h.router,
        get(&format!("/api/v1/cast/sessions/{id}"), VIEWER_TOKEN),
    )
    .await;
    let doc = body_json(resp).await;
    assert!(
        doc.get("started_unix_ns").is_none(),
        "GET mirrors the absent stamp: {doc}"
    );
}

#[tokio::test]
async fn started_unix_ns_appears_once_the_receiver_accepts_the_load() {
    // A full scripted establishment: connect → CONNECT → LAUNCH →
    // RECEIVER_STATUS → CONNECT → LOAD → MEDIA_STATUS(PLAYING). The accept
    // point stamps the session record from the control plane's injectable
    // clock (the same `AckClock` the audit log stamps with, Unix
    // nanoseconds), and the REST docs expose it as `started_unix_ns`.
    const NOW_UNIX_NS: i64 = 1_765_000_000_123_456_789;
    let (channel, _sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status_playing()),
        ScriptedInbound::Hang,
    ]);
    let factory = CastSessionFactory::new(
        Arc::new(ScriptedConnector::new(vec![channel])),
        delivery(),
        CastSessionConfig::test_fast(),
    );
    let registry = Arc::new(DevicePollerRegistry::with_factory(Arc::new(factory)));
    let h = harness_with(move |state| {
        state
            .with_device_pollers(registry)
            .with_cast_delivery(delivery())
            .with_ack_clock(Arc::new(|| {
                multiview_core::time::MediaTime::from_nanos(NOW_UNIX_NS)
            }))
    });

    let resp = send(
        &h.router,
        post_json("/api/v1/cast/sessions", OPERATOR_TOKEN, &start_body(None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    let id = created["id"].as_str().expect("a session id").to_owned();

    // The supervised actor establishes asynchronously (test_fast cadences):
    // poll the GET until the accept-point stamp lands.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let stamped = loop {
        let resp = send(
            &h.router,
            get(&format!("/api/v1/cast/sessions/{id}"), VIEWER_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let doc = body_json(resp).await;
        if let Some(value) = doc
            .get("started_unix_ns")
            .and_then(serde_json::Value::as_i64)
        {
            break value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the LOAD-accept stamp never landed: {doc}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    assert_eq!(
        stamped, NOW_UNIX_NS,
        "the stamp is the injectable control-plane clock at the accept point"
    );

    // The list view carries the same stamp.
    let resp = send(&h.router, get("/api/v1/cast/sessions", VIEWER_TOKEN)).await;
    let list = body_json(resp).await;
    assert_eq!(list[0]["started_unix_ns"].as_i64(), Some(NOW_UNIX_NS));
}
