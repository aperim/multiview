//! Read-only structured log-tail REST surface (ADR-0060 §5.2): `GET
//! /api/v1/logs` returns recent buffered records from the bounded drop-oldest
//! `LogRing`, filterable by `resource_id` / `kind` / `level` / `since` / `limit`.
//! Role: read. RFC 9457 problems on bad filters / auth. The ring is bounded and
//! never back-pressures the engine (invariant #10).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_telemetry::{LogLevel, LogRecord, LogResourceKind};
use support::{body_json, get, harness, send, VIEWER_TOKEN};

fn record(seq: u64, resource_id: Option<&str>, level: LogLevel, message: &str) -> LogRecord {
    LogRecord {
        seq,
        timestamp_ms: 1_700_000_000_000 + seq,
        level,
        target: "libav".to_owned(),
        message: message.to_owned(),
        run_id: Some("run-1".to_owned()),
        resource_kind: resource_id.map(|_| LogResourceKind::Source),
        resource_id: resource_id.map(str::to_owned),
        label: None,
        component: Some("hevc".to_owned()),
        repeated: None,
    }
}

#[tokio::test]
async fn lists_recent_records_for_a_viewer() {
    let h = harness();
    h.logs
        .push(record(0, Some("cnn"), LogLevel::Info, "cnn opening"));
    h.logs
        .push(record(1, Some("cnn"), LogLevel::Error, "cnn RPS error"));

    let resp = send(&h.router, get("/api/v1/logs", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["resource_id"], "cnn");
    assert_eq!(arr[1]["level"], "error");
    assert_eq!(arr[1]["component"], "hevc");
}

#[tokio::test]
async fn filters_by_resource_id_and_minimum_level() {
    let h = harness();
    h.logs
        .push(record(0, Some("cnn"), LogLevel::Info, "cnn info"));
    h.logs
        .push(record(1, Some("cnn"), LogLevel::Error, "cnn error"));
    h.logs
        .push(record(2, Some("bbc"), LogLevel::Error, "bbc error"));

    let resp = send(
        &h.router,
        get("/api/v1/logs?resource_id=cnn&level=warn", VIEWER_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{arr:?}");
    assert_eq!(arr[0]["message"], "cnn error");
}

#[tokio::test]
async fn filters_by_since_cursor() {
    let h = harness();
    h.logs.push(record(0, None, LogLevel::Info, "first"));
    h.logs.push(record(1, None, LogLevel::Info, "second"));
    h.logs.push(record(2, None, LogLevel::Info, "third"));

    let resp = send(&h.router, get("/api/v1/logs?since=0", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 2, "records strictly after seq 0");
    assert_eq!(arr[0]["message"], "second");
}

#[tokio::test]
async fn limit_caps_the_returned_count() {
    let h = harness();
    for i in 0..10 {
        h.logs
            .push(record(i, None, LogLevel::Info, &format!("line {i}")));
    }
    let resp = send(&h.router, get("/api/v1/logs?limit=3", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[2]["message"], "line 9");
}

#[tokio::test]
async fn unknown_level_is_a_422_problem() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs?level=bogus", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/validation");
}

#[tokio::test]
async fn unknown_kind_is_a_422_problem() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs?kind=bogus", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unauthenticated_listing_is_401() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn empty_ring_returns_empty_list() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/logs", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    assert!(arr.as_array().unwrap().is_empty());
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_log_record_mirror_matches_the_telemetry_serde_shape() {
    // The OpenAPI mirror (LogRecordDoc) must serialise to the SAME JSON shape as
    // the real telemetry LogRecord it documents, or the published contract would
    // lie. Round-trip a real record's JSON THROUGH the mirror — including the
    // unattributed case (resource_id omitted) and the libav case (component set).
    use multiview_control::openapi_schemas::LogRecordDoc;

    let attributed = record(7, Some("cnn"), LogLevel::Error, "cnn RPS error");
    let unattributed = record(8, None, LogLevel::Info, "context-free line");
    for real in [attributed, unattributed] {
        let real_json = serde_json::to_value(&real).unwrap();
        let doc: LogRecordDoc = serde_json::from_value(real_json.clone()).unwrap();
        let doc_json = serde_json::to_value(&doc).unwrap();
        assert_eq!(
            real_json, doc_json,
            "the OpenAPI mirror must match the telemetry LogRecord serde shape"
        );
    }
}
