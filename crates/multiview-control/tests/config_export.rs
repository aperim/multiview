//! `GET /api/v1/config/export` (ADR-W015): compose the live resource stores
//! (working layout + sources + outputs + overlays) into a full
//! `MultiviewConfig` document and return it as TOML, closing the UI → config
//! file loop. The composed document is validated as a whole before render.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::{header, StatusCode};
use serde_json::json;
use support::{get, harness, post_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// Seed a working layout + one source + one output through the public API,
/// mirroring what `seed_resources` does for a config-driven run.
async fn seed(h: &support::Harness) {
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/working",
            OPERATOR_TOKEN,
            &json!({
                "name": "working",
                "body": {
                    "canvas": { "width": 1920, "height": 1080, "fps": "30/1" },
                    "layout": { "kind": "grid", "columns": 2, "rows": 2 },
                    "cells": [
                        { "id": "a", "source": "cam1" }
                    ]
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
            &json!({
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
            "/api/v1/outputs/web1",
            OPERATOR_TOKEN,
            &json!({
                "name": "LL-HLS",
                "body": { "id": "web1", "kind": "ll_hls", "path": "/var/lib/multiview/hls", "codec": "h264" }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "output seed must land");
}

#[tokio::test]
async fn export_renders_the_stores_as_valid_toml() {
    let h = harness();
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .expect("export has a content type")
            .to_str()
            .unwrap(),
        "application/toml"
    );

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // The document must itself round-trip through the canonical config type.
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(&text).expect("export is a valid MultiviewConfig document");
    assert_eq!(parsed.canvas.width, 1920);
    assert_eq!(parsed.sources.len(), 1, "the created source is exported");
    assert_eq!(parsed.sources[0].id, "cam1");
    assert_eq!(parsed.outputs.len(), 1, "the created output is exported");
    assert_eq!(parsed.cells.len(), 1, "the working layout cells are exported");
}

#[tokio::test]
async fn export_requires_authentication() {
    let h = harness();
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/v1/config/export")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = send(&h.router, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_without_a_working_layout_is_422() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = support::body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}
