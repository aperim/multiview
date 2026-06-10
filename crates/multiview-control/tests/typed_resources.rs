//! Typed resource validation (ADR-W015): source/output/overlay bodies must
//! deserialize against the canonical `multiview_config` types at the API
//! boundary — invalid documents are rejected with `422 /problems/validation`
//! carrying the offending field path, and valid mutations declare their apply
//! semantics via the `X-Multiview-Apply` header.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{body_json, harness, post_json, put_json, send, OPERATOR_TOKEN};

const APPLY_HEADER: &str = "x-multiview-apply";

#[tokio::test]
async fn create_source_with_unknown_kind_is_422_with_field_detail() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "flux-capacitor" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    let detail = problem["detail"].as_str().expect("detail is present");
    assert!(
        detail.contains("flux-capacitor") || detail.contains("kind"),
        "detail names the offending field/variant, got: {detail}"
    );
}

#[tokio::test]
async fn create_source_missing_required_field_is_422() {
    let h = harness();
    // An rtsp source without its `url` must be rejected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "cam1", "kind": "rtsp" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
    assert!(
        problem["detail"].as_str().unwrap_or("").contains("url"),
        "detail names the missing field, got: {}",
        problem["detail"]
    );
}

#[tokio::test]
async fn create_source_with_mismatched_body_id_is_422() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "id": "other", "kind": "bars" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn create_source_without_body_id_inherits_the_path_id() {
    let h = harness();
    // The body `id` may be omitted; the resource id from the path is used.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Cam 1",
                "body": { "kind": "bars" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created = body_json(resp).await;
    assert_eq!(created["body"]["id"], "cam1", "the path id is injected");
}

#[tokio::test]
async fn valid_source_mutations_declare_restart_apply_semantics() {
    let h = harness();
    let body = json!({
        "name": "Cam 1",
        "body": { "id": "cam1", "kind": "rtsp", "url": "rtsp://[2001:db8::1]/cam1" }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/sources/cam1", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers()
            .get(APPLY_HEADER)
            .expect("create declares apply semantics")
            .to_str()
            .unwrap(),
        "restart"
    );

    let resp = send(
        &h.router,
        put_json(
            "/api/v1/sources/cam1",
            OPERATOR_TOKEN,
            Some("W/\"1\""),
            &body,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(APPLY_HEADER)
            .expect("update declares apply semantics")
            .to_str()
            .unwrap(),
        "restart"
    );
}

#[tokio::test]
async fn create_output_missing_required_field_is_422() {
    let h = harness();
    // An rtmp output without its destination `url` must be rejected.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/push1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Push 1",
                "body": { "id": "push1", "kind": "rtmp" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn create_valid_ll_hls_output_succeeds_with_apply_header() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/outputs/web1",
            OPERATOR_TOKEN,
            &json!({
                "name": "LL-HLS",
                "body": { "id": "web1", "kind": "ll_hls", "path": "/var/lib/multiview/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(
        resp.headers().get(APPLY_HEADER).unwrap().to_str().unwrap(),
        "restart"
    );
}

#[tokio::test]
async fn create_overlay_with_invalid_shape_is_422() {
    let h = harness();
    // An overlay body must at least be an object with a string `kind`.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clock1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Clock",
                "body": { "id": "clock1", "kind": 7 }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn create_valid_overlay_succeeds() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/overlays/clock1",
            OPERATOR_TOKEN,
            &json!({
                "name": "Clock",
                "body": { "id": "clock1", "kind": "clock", "target": "canvas" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}
