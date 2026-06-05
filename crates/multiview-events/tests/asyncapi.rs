#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Tests for `AsyncAPI` 3.0 document generation from the multiview-events types
//! (ADR-RT006). Asserts the generated document:
//!   - is valid JSON with `asyncapi: "3.0.0"`
//!   - contains the versioned envelope schema
//!   - declares the documented channels (`ws`, `sse`) + messages for key event types
//!   - is deterministic (calling twice yields identical output)

use multiview_events::asyncapi;
use serde_json::Value;

fn generated_doc() -> Value {
    let json_str = asyncapi::generate_asyncapi_document();
    serde_json::from_str(&json_str).unwrap()
}

#[test]
fn asyncapi_version_is_3_0_0() {
    let doc = generated_doc();
    assert_eq!(
        doc.get("asyncapi").unwrap(),
        "3.0.0",
        "asyncAPI version must be 3.0.0"
    );
}

#[test]
fn document_has_info_block() {
    let doc = generated_doc();
    let info = doc.get("info").unwrap();
    assert!(info.get("title").is_some(), "info block must have a title");
    assert!(
        info.get("version").is_some(),
        "info block must have a version"
    );
}

#[test]
fn document_declares_ws_and_sse_channels() {
    let doc = generated_doc();
    let channels = doc.get("channels").unwrap().as_object().unwrap();
    assert!(
        channels.contains_key("ws"),
        "channels must include `ws` for /api/v1/ws"
    );
    assert!(
        channels.contains_key("sse"),
        "channels must include `sse` for /api/v1/events"
    );
}

#[test]
fn ws_channel_has_websocket_binding() {
    let doc = generated_doc();
    let ws_channel = doc.pointer("/channels/ws").unwrap().as_object().unwrap();
    // The WS binding is injected by post-process because asyncapi-rust lacks it.
    let bindings = ws_channel.get("bindings").unwrap().as_object().unwrap();
    assert!(
        bindings.contains_key("ws"),
        "ws channel must have a ws binding (post-processed)"
    );
}

#[test]
fn messages_block_contains_envelope_schema() {
    let doc = generated_doc();
    let messages = doc.get("messages").unwrap().as_object().unwrap();
    assert!(
        messages.contains_key("Envelope"),
        "messages must contain an Envelope message definition"
    );
    let envelope_msg = &messages["Envelope"];
    // The payload schema references or inlines the envelope fields.
    let payload = envelope_msg.get("payload").unwrap();
    assert!(
        payload.is_object(),
        "Envelope message must have a payload schema"
    );
}

#[test]
fn messages_block_contains_key_event_types() {
    let doc = generated_doc();
    let messages = doc.get("messages").unwrap().as_object().unwrap();

    // The core data events from the brief §3 (ADR-RT002 discriminated union).
    for key in &[
        "TileState",
        "AudioMeter",
        "OutputStatus",
        "Alert",
        "InputConnection",
        "JobProgress",
    ] {
        assert!(
            messages.contains_key(*key),
            "messages must contain `{key}` (event type from the wire contract)"
        );
    }
}

#[test]
fn envelope_schema_has_required_fields() {
    let doc = generated_doc();
    let envelope_payload = doc.pointer("/messages/Envelope/payload").unwrap();
    let empty = vec![];
    let required = envelope_payload
        .get("required")
        .and_then(|r| r.as_array())
        .unwrap_or(&empty)
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();

    // The mandatory envelope fields from the realtime-api brief §2.
    for field in &["v", "t", "topic", "seq", "ts"] {
        assert!(
            required.contains(field),
            "envelope schema must mark `{field}` as required"
        );
    }
}

#[test]
fn document_is_deterministic() {
    // Re-running the generator must yield an identical string (no timestamps,
    // no non-deterministic ordering — idempotency requirement from SUR-6 acceptance).
    let first = asyncapi::generate_asyncapi_document();
    let second = asyncapi::generate_asyncapi_document();
    assert_eq!(
        first, second,
        "AsyncAPI document generation must be deterministic"
    );
}

#[test]
fn document_references_realtime_paths() {
    let doc = generated_doc();
    // Both WS and SSE server paths must appear somewhere in the document.
    let doc_str = serde_json::to_string(&doc).unwrap();
    assert!(
        doc_str.contains("/api/v1/ws"),
        "document must reference the WS endpoint /api/v1/ws"
    );
    assert!(
        doc_str.contains("/api/v1/events"),
        "document must reference the SSE endpoint /api/v1/events"
    );
}

#[test]
fn audio_meter_message_notes_high_rate() {
    // AudioMeter is the sole high-rate conflated topic; its message description
    // must mention this so integrators know to expect conflation.
    let doc = generated_doc();
    let meter_msg = doc.pointer("/messages/AudioMeter").unwrap();
    let desc = meter_msg
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    assert!(
        desc.to_lowercase().contains("conflat")
            || desc.to_lowercase().contains("high-rate")
            || desc.to_lowercase().contains("high rate"),
        "AudioMeter message must document its high-rate / conflated nature; got: {desc:?}"
    );
}
