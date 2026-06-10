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
use support::{get, harness, post_json, put_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

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
                    "canvas": {
                        "width": 1920,
                        "height": 1080,
                        "fps": "30/1",
                        "pixel_format": "nv12",
                        "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": [
                        {
                            "id": "a",
                            "rect": { "x": 0.0, "y": 0.0, "w": 0.5, "h": 0.5 },
                            "source": { "input_id": "cam1" }
                        }
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
    assert_eq!(
        parsed.cells.len(),
        1,
        "the working layout cells are exported"
    );
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

#[tokio::test]
async fn export_retains_base_config_sections_the_stores_do_not_carry() {
    // Review B1: exporting must not destroy authored sections (control,
    // placement, probes, …) — a restart with the exported file would otherwise
    // lose the management listener itself.
    let base = json!({
        "schema_version": 1,
        "canvas": {
            "width": 1280, "height": 720, "fps": "25/1",
            "pixel_format": "nv12", "background": "#000000",
            "color": { "profile": "sdr-bt709-limited" }
        },
        "layout": { "kind": "absolute" },
        "cells": [],
        "sources": [],
        "outputs": [ { "kind": "hls", "path": "/srv/hls", "codec": "h264" } ],
        "control": { "listen": "[::1]:8087" },
        "placement": { "reserve_headroom": 0.2 },
        "probes": []
    });
    let h = support::harness_with(|state| state.with_base_document(base.clone()));
    seed(&h).await;

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        text.contains("[control]") && text.contains("[::1]:8087"),
        "the [control] section survives export:\n{text}"
    );
    assert!(text.contains("[placement"), "placement survives export");
    // Store-backed sections override the base: the seeded source/output/canvas
    // win over the base document's.
    let parsed: multiview_config::MultiviewConfig = toml::from_str(&text).unwrap();
    assert_eq!(parsed.canvas.width, 1920, "working-layout canvas wins");
    assert_eq!(parsed.sources.len(), 1, "store sources win");
    assert_eq!(parsed.outputs.len(), 1, "store outputs win");
}

#[tokio::test]
async fn export_prefers_the_seeded_working_layout_over_alphabetical_order() {
    // Review M1: with several layouts carrying a canvas, the export must use
    // the designated working layout, not the id-sorted first.
    let h = support::harness_with(|state| state.with_working_layout_id("schema_v1"));
    seed(&h).await; // seeds layout id "working" (carries 1920x1080)

    // An alphabetically-earlier decoy with a different canvas.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/aaa-decoy",
            OPERATOR_TOKEN,
            &json!({
                "name": "decoy",
                "body": {
                    "canvas": {
                        "width": 640, "height": 360, "fps": "25/1",
                        "pixel_format": "nv12", "background": "#000000",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": []
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The designated working layout id ("schema_v1") doesn't exist yet — so
    // create it too, mirroring the seeded-name flow.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/layouts/schema_v1",
            OPERATOR_TOKEN,
            &json!({
                "name": "schema_v1",
                "body": {
                    "canvas": {
                        "width": 3840, "height": 2160, "fps": "30/1",
                        "pixel_format": "nv12", "background": "#101014",
                        "color": { "profile": "sdr-bt709-limited" }
                    },
                    "layout": { "kind": "absolute" },
                    "cells": []
                }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: multiview_config::MultiviewConfig =
        toml::from_str(core::str::from_utf8(&body).unwrap()).unwrap();
    assert_eq!(
        parsed.canvas.width, 3840,
        "the designated working layout wins over the alphabetical decoy"
    );
}

#[tokio::test]
async fn export_carries_a_download_disposition() {
    let h = harness();
    seed(&h).await;
    let resp = send(&h.router, get("/api/v1/config/export", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-disposition")
            .expect("export offers a filename")
            .to_str()
            .unwrap(),
        "attachment; filename=\"multiview.toml\""
    );
}

#[tokio::test]
async fn the_export_segment_is_reserved_in_the_versioning_namespace() {
    // `/api/v1/config/export` (static) wins over `/config/{target}` — the
    // literal target name "export" is reserved by design (ADR-W015): GET
    // returns the export document, and committing to a target named "export"
    // is a 405, never a silent versioning write.
    let h = harness();
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/config/export",
            OPERATOR_TOKEN,
            None,
            &json!({ "document": {}, "message": "nope" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}
