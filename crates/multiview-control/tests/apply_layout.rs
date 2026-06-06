//! Tests for the `POST /api/v1/commands/apply-layout` command route (CTL-4):
//! `202 Accepted` + operation id reaching the engine, write-role enforcement,
//! the bounded command bus shedding to `503` under saturation, and the route
//! appearing in the served `OpenAPI` document.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use serde_json::json;
use support::{
    body_json, harness, harness_with_capacity, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN,
};

#[tokio::test]
async fn apply_layout_returns_202_with_op_id() {
    let mut h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    let op = body["operation_id"].as_str().expect("operation_id present");
    assert!(!op.is_empty(), "a non-empty operation id is returned");
    assert_eq!(body["kind"], "apply_layout");

    // The engine drains the command at its leisure (non-blocking): exactly one
    // ApplyLayout reached the engine, carrying the requested layout id and the
    // same correlation id the client received.
    let drained = h.commands.try_drain();
    assert_eq!(drained.len(), 1, "exactly one command reached the engine");
    match &drained[0] {
        multiview_control::Command::ApplyLayout { op: drained_op, layout } => {
            assert_eq!(layout, "grid-3x3");
            assert_eq!(
                drained_op.as_str(),
                op,
                "the engine sees the same correlation id the client got"
            );
        }
        other => panic!("expected ApplyLayout, got {other:?}"),
    }
}

#[tokio::test]
async fn apply_layout_requires_write_role() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            VIEWER_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a viewer is read-only and cannot apply a layout"
    );
}

#[tokio::test]
async fn apply_layout_sheds_503_when_bus_full() {
    // Capacity 1, and the engine never drains: the first command fills the bus,
    // and a second apply-layout must be shed (503), never block — proving the
    // handler only try_submits and can never back-pressure the engine (inv #10).
    let h = harness_with_capacity(1);

    let resp1 = send(
        &h.router,
        post_json("/api/v1/commands/start", OPERATOR_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp1.status(), StatusCode::ACCEPTED);

    let resp2 = send(
        &h.router,
        post_json(
            "/api/v1/commands/apply-layout",
            OPERATOR_TOKEN,
            &json!({ "layout": "grid-3x3" }),
        ),
    )
    .await;
    assert_eq!(
        resp2.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a full bus sheds the request rather than blocking the engine"
    );
    let problem = body_json(resp2).await;
    assert_eq!(problem["type"], "/problems/engine-busy");
}

#[test]
fn apply_layout_path_is_documented_in_openapi() {
    use multiview_control::openapi::ApiDoc;
    use utoipa::OpenApi;

    let doc = ApiDoc::openapi();
    let json = serde_json::to_value(&doc).expect("OpenAPI serializes");
    let paths = &json["paths"];

    let item = paths
        .get("/api/v1/commands/apply-layout")
        .expect("POST /api/v1/commands/apply-layout is in the OpenAPI document");
    let post = item
        .get("post")
        .expect("the apply-layout path documents its POST operation");

    // The accepted (202) outcome is advertised so the generated SPA client knows
    // the command is asynchronous (result arrives on the realtime stream).
    assert!(
        post["responses"].get("202").is_some(),
        "apply-layout documents the 202 Accepted response"
    );

    // It is also enumerated in the static REST-surface list the crate advertises.
    let routes: Vec<&str> = ApiDoc::rest_routes().iter().map(|(_, p)| *p).collect();
    assert!(
        routes.contains(&"/api/v1/commands/apply-layout"),
        "apply-layout route enumerated in rest_routes()"
    );
}
