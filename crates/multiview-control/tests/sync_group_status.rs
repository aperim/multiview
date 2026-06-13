#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
//! End-to-end tests for the DEV-C3 sync-group runtime surface:
//! `GET /sync-groups/{id}/status` (the read-only weakest-member achieved tier +
//! per-member skew + drift-alarm projection) and
//! `POST /sync-groups/{id}/test-pattern` (the burnt-in-counter + flash action,
//! `202` + operation id, publishing a `device.sync` test-pattern event). Driven
//! through the real router via `tower::oneshot`.

mod support;

use std::sync::Arc;

use axum::http::StatusCode;
use multiview_control::devices::sync_runtime::SyncGroupRuntime;
use multiview_core::time::MediaTime;
use multiview_events::{ClockQuality, Event, SyncCapability};
use serde_json::{json, Value};
use support::{body_json, get, harness_with, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// Build a runtime seeded with one two-member group, observe each member's live
/// clock quality, and install it on the harness state.
fn runtime_with_one_freerun_member() -> Arc<SyncGroupRuntime> {
    let rt = Arc::new(SyncGroupRuntime::new());
    let group: multiview_config::SyncGroup = serde_json::from_value(json!({
        "id": "lobby-wall",
        "mode": "auto",
        "target_skew_ms": 50,
        "members": [
            { "device": "node-left", "offset_ms": 0 },
            { "device": "node-right", "offset_ms": 120 }
        ]
    }))
    .unwrap();
    rt.seed(std::slice::from_ref(&group));
    rt.observe(
        "lobby-wall",
        "node-left",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(6.0),
        MediaTime::ZERO,
    );
    rt.observe(
        "lobby-wall",
        "node-right",
        SyncCapability::FrameAccurate,
        ClockQuality::Freerun,
        Some(9.0),
        MediaTime::ZERO,
    );
    rt
}

#[tokio::test]
async fn status_reports_weakest_member_tier_and_skew() {
    let rt = runtime_with_one_freerun_member();
    let h = harness_with(move |state| state.with_sync_runtime(Arc::clone(&rt)));
    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/lobby-wall/status", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = body_json(resp).await;
    assert_eq!(status["group"], "lobby-wall");
    // The free-running member caps the group at bounded skew — never over-claimed.
    assert_eq!(status["achieved"], "bounded-skew");
    assert_eq!(status["limited_by"], "node-right");
    assert_eq!(status["target_skew_ms"], 50);
    // The worst measured member skew is surfaced.
    assert_eq!(status["measured_skew_ms"], 9.0);
    assert_eq!(status["drift_alarm"], false);
    let members = status["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    let right = members
        .iter()
        .find(|m| m["device"] == "node-right")
        .unwrap();
    assert_eq!(right["achieved"], "bounded-skew");
    assert_eq!(right["offset_ms"], 120);
}

#[tokio::test]
async fn status_of_unknown_group_is_404() {
    let h = harness_with(|state| state);
    let resp = send(
        &h.router,
        get("/api/v1/sync-groups/no-such/status", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_pattern_returns_202_and_publishes_event() {
    let rt = runtime_with_one_freerun_member();
    let h = harness_with(move |state| state.with_sync_runtime(Arc::clone(&rt)));
    let mut sub = h.engine.subscribe();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall/test-pattern",
            OPERATOR_TOKEN,
            &json!({ "duration_s": 5, "frame_counter": true, "flash_period_ms": 500 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body: Value = body_json(resp).await;
    assert_eq!(body["kind"], "test-pattern");
    assert!(body["operation_id"].as_str().is_some());

    // A `sync.test-pattern` lifecycle event was published for the group,
    // carrying the burnt-in-counter + flash parameters.
    let mut saw_test_pattern = false;
    for _ in 0..16 {
        match sub.try_recv() {
            Ok(evt) => {
                if let Event::SyncGroupTestPattern(tp) = &*evt.event {
                    if tp.group == "lobby-wall" {
                        assert!(tp.frame_counter);
                        assert_eq!(tp.flash_period_ms, 500);
                        assert_eq!(tp.duration_ms, 5_000);
                        saw_test_pattern = true;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_test_pattern,
        "expected a sync.test-pattern event for the group"
    );
}

#[tokio::test]
async fn test_pattern_of_unknown_group_is_404() {
    let h = harness_with(|state| state);
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/no-such/test-pattern",
            OPERATOR_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_pattern_requires_write_role() {
    let rt = runtime_with_one_freerun_member();
    let h = harness_with(move |state| state.with_sync_runtime(Arc::clone(&rt)));
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sync-groups/lobby-wall/test-pattern",
            VIEWER_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
