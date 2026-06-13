//! End-to-end tests for the display-node enrollment/pairing surface (DEV-B6,
//! ADR-0045 / managed-devices brief §4): enrollment-token minting (one-time
//! display, hashed at rest, TTL'd), `POST /devices/enroll` (token → enrolled;
//! no token → 6-char pairing code), `POST /devices/pair` (operator completes
//! screen pairing), the keypair-signed `POST /devices/{id}/heartbeat`, and the
//! `GET /devices/{id}/display-heads` projection (ADR-M009 facet (c)) — all
//! driven through the real router via `tower::oneshot`, socket-free.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::map_unwrap_or
)]

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use multiview_control::devices::enroll::{canonical_message, NodeEnrollState};
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness, harness_with, post_json, send, ADMIN_TOKEN,
    OPERATOR_TOKEN, VIEWER_TOKEN,
};

/// The deterministic node keypair used across these tests.
fn node_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// A second, distinct node keypair.
fn other_key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32])
}

/// The base64 form of a signing key's public half (the enroll wire form).
fn public_key_b64(key: &SigningKey) -> String {
    BASE64.encode(key.verifying_key().to_bytes())
}

/// A well-formed enroll request body for `key`, with one EDID-derived head.
fn enroll_body(token: Option<&str>, key: &SigningKey) -> serde_json::Value {
    json!({
        "token": token,
        "public_key": public_key_b64(key),
        "model": "hp-t630",
        "node_name": "Lobby left",
        "heads": [{
            "id": "head-0",
            "connector": "HDMI-A-1",
            "width": 1920,
            "height": 1080,
            "refresh_millihertz": 60_000,
            "connected": true
        }]
    })
}

/// POST a JSON body **without** any Authorization header (the node side of
/// enrollment is authenticated by token/keypair, never by an operator key).
fn post_json_unauth(path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .expect("request should build")
}

/// Mint an enrollment token as admin, returning `(token_id, token)`.
async fn mint_token(router: &axum::Router) -> (String, String) {
    let resp = send(
        router,
        post_json("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    (
        body["token_id"].as_str().unwrap().to_owned(),
        body["token"].as_str().unwrap().to_owned(),
    )
}

/// The current UNIX time in seconds (the heartbeat signature timestamp base).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a keypair-signed heartbeat request (the node-auth scheme: the node id,
/// a strictly-increasing UNIX timestamp, and an Ed25519 signature over the
/// canonical message, in `X-Multiview-Node-*` headers).
fn signed_heartbeat(
    device_id: &str,
    ts: u64,
    body: &serde_json::Value,
    key: &SigningKey,
) -> Request<Body> {
    let bytes = serde_json::to_vec(body).unwrap();
    let path = format!("/api/v1/devices/{device_id}/heartbeat");
    let message = canonical_message("POST", &path, device_id, ts, &bytes);
    let signature = key.sign(message.as_bytes());
    Request::builder()
        .method("POST")
        .uri(&path)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-multiview-node-id", device_id)
        .header("x-multiview-node-ts", ts.to_string())
        .header(
            "x-multiview-node-signature",
            BASE64.encode(signature.to_bytes()),
        )
        .body(Body::from(bytes))
        .expect("request should build")
}

/// A minimal heartbeat body re-reporting one head.
fn heartbeat_body() -> serde_json::Value {
    json!({
        "heads": [{
            "id": "head-0",
            "connector": "HDMI-A-1",
            "width": 1920,
            "height": 1080,
            "refresh_millihertz": 60_000,
            "connected": true
        }],
        "temperature_c": 41.5
    })
}

// ---------------------------------------------------------------------------
// Enrollment tokens: minting (one-time display), listing (hashed at rest),
// revocation.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn minting_an_enrollment_token_requires_admin() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/enrollment-tokens",
            OPERATOR_TOKEN,
            &json!({}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = send(
        &h.router,
        post_json("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN, &json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    let token = body["token"].as_str().expect("the token is displayed once");
    let token_id = body["token_id"].as_str().unwrap();
    assert!(
        token_id.starts_with("enr-"),
        "token ids carry the enr- prefix: {token_id}"
    );
    assert!(
        token.starts_with(&format!("{token_id}.")),
        "the bearer form is <token_id>.<secret>: {token}"
    );
    assert!(
        body["expires_epoch_s"].as_u64().unwrap() > body["created_epoch_s"].as_u64().unwrap(),
        "a fresh token expires in the future"
    );
}

#[tokio::test]
async fn token_list_shows_metadata_but_never_the_secret() {
    let h = harness();
    let (token_id, token) = mint_token(&h.router).await;

    let resp = send(
        &h.router,
        get("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    let entries = listed.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["token_id"], token_id.as_str());
    assert_eq!(entries[0]["state"], "pending");
    // Hashed at rest: the full bearer token (its secret half) is one-time
    // display only — the list never carries it in any field.
    let serialized = serde_json::to_string(&listed).unwrap();
    let secret = token.split_once('.').unwrap().1;
    assert!(
        !serialized.contains(secret),
        "the token secret must never appear in the list"
    );

    // Listing is an admin surface.
    let resp = send(
        &h.router,
        get("/api/v1/devices/enrollment-tokens", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn ttl_out_of_range_is_rejected() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/enrollment-tokens",
            ADMIN_TOKEN,
            &json!({ "ttl_secs": 5 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn revoked_token_is_rejected_at_enroll() {
    let h = harness();
    let (token_id, token) = mint_token(&h.router).await;

    let resp = send(
        &h.router,
        delete_if_match(
            &format!("/api/v1/devices/enrollment-tokens/{token_id}"),
            ADMIN_TOKEN,
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = send(
        &h.router,
        get("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN),
    )
    .await;
    let listed = body_json(resp).await;
    assert_eq!(listed[0]["state"], "revoked");
}

#[tokio::test]
async fn expired_token_is_rejected_at_enroll() {
    // A fake millisecond clock so the test can cross the TTL deterministically.
    let clock = Arc::new(AtomicU64::new(1_000_000_000_000));
    let read = Arc::clone(&clock);
    let enroll_state = Arc::new(NodeEnrollState::with_clock(Arc::new(move || {
        read.load(Ordering::Relaxed)
    })));
    let h = harness_with(move |s| s.with_node_enroll(enroll_state));

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/enrollment-tokens",
            ADMIN_TOKEN,
            &json!({ "ttl_secs": 60 }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let token = body_json(resp).await["token"].as_str().unwrap().to_owned();

    // 61 seconds later the token has expired.
    clock.fetch_add(61_000, Ordering::Relaxed);
    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = send(
        &h.router,
        get("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN),
    )
    .await;
    let listed = body_json(resp).await;
    assert_eq!(listed[0]["state"], "expired");
}

// ---------------------------------------------------------------------------
// Zero-touch enrollment: token → enrolled device, one-time consumption.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enroll_with_token_creates_an_online_displaynode_device() {
    let h = harness();
    let (token_id, token) = mint_token(&h.router).await;

    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "enrolled");
    let device_id = body["device_id"].as_str().unwrap().to_owned();
    assert!(
        body["heartbeat_secs"].as_u64().unwrap() > 0,
        "the node is told its heartbeat cadence"
    );

    // The device exists, is a displaynode, and carries the bound public key
    // (the keypair binding is config-as-code durable state).
    let resp = send(
        &h.router,
        get(&format!("/api/v1/devices/{device_id}"), OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let device = body_json(resp).await;
    assert_eq!(device["body"]["driver"], "displaynode");
    assert_eq!(
        device["body"]["enrollment"]["public_key"],
        public_key_b64(&node_key())
    );
    assert_eq!(device["name"], "Lobby left");

    // It appears ONLINE (the brief: "appears in Devices already ONLINE").
    let resp = send(
        &h.router,
        get(&format!("/api/v1/devices/{device_id}/status"), OPERATOR_TOKEN),
    )
    .await;
    let status = body_json(resp).await;
    assert_eq!(status["state"], "ONLINE");

    // The display-head projection carries the EDID-derived head list.
    let resp = send(
        &h.router,
        get(
            &format!("/api/v1/devices/{device_id}/display-heads"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let heads = body_json(resp).await;
    assert_eq!(heads.as_array().unwrap().len(), 1);
    assert_eq!(heads[0]["connector"], "HDMI-A-1");
    assert_eq!(heads[0]["refresh_millihertz"], 60_000);

    // The token is consumed: one-time use, attributed to the device.
    let resp = send(
        &h.router,
        get("/api/v1/devices/enrollment-tokens", ADMIN_TOKEN),
    )
    .await;
    let listed = body_json(resp).await;
    assert_eq!(listed[0]["token_id"], token_id.as_str());
    assert_eq!(listed[0]["state"], "used");
    assert_eq!(listed[0]["used_by"], device_id.as_str());
}

#[tokio::test]
async fn enroll_is_idempotent_for_an_already_enrolled_key() {
    let h = harness();
    let (_, token) = mint_token(&h.router).await;

    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    let first = body_json(resp).await;
    let device_id = first["device_id"].as_str().unwrap().to_owned();

    // A re-enroll from the same keypair needs no token at all (e.g. the node
    // rebooted with the one-time token already consumed) and maps to the SAME
    // device — never a duplicate record.
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let again = body_json(resp).await;
    assert_eq!(again["status"], "enrolled");
    assert_eq!(again["device_id"].as_str().unwrap(), device_id);
}

#[tokio::test]
async fn a_used_token_is_rejected_for_a_different_key() {
    let h = harness();
    let (_, token) = mint_token(&h.router).await;

    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // One-time use: a second node presenting the same token is refused.
    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &other_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn enroll_rejects_a_malformed_public_key() {
    let h = harness();
    let (_, token) = mint_token(&h.router).await;
    let mut body = enroll_body(Some(&token), &node_key());
    body["public_key"] = json!("not-base64!!!");
    let resp = send(&h.router, post_json_unauth("/api/v1/devices/enroll", &body)).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Screen pairing: no token → 6-char code; the operator completes in the SPA.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enroll_without_token_yields_a_pairing_code_and_pair_completes() {
    let h = harness();

    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let pending = body_json(resp).await;
    assert_eq!(pending["status"], "pairing");
    let code = pending["pairing_code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 6, "the pairing code is six characters: {code}");
    // The code alphabet excludes the ambiguous 0/O/1/I (it is read off a
    // screen and typed by the operator — WCAG-honest legibility).
    assert!(
        code.chars()
            .all(|c| "ABCDEFGHJKLMNPQRSTUVWXYZ23456789".contains(c)),
        "code {code} uses the unambiguous alphabet"
    );
    assert!(pending["retry_secs"].as_u64().unwrap() > 0);

    // Polling again returns the SAME code (the card on the screen stays valid).
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(body_json(resp).await["pairing_code"].as_str().unwrap(), code);

    // The operator sees the pending request (metadata only — never the code or
    // the key, which stay on the node's screen / in the registry).
    let resp = send(
        &h.router,
        get("/api/v1/devices/pairing-requests", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let requests = body_json(resp).await;
    assert_eq!(requests.as_array().unwrap().len(), 1);
    assert_eq!(requests[0]["model"], "hp-t630");
    let serialized = serde_json::to_string(&requests).unwrap();
    assert!(
        !serialized.contains(&code),
        "the pairing code must not leak through the pending list"
    );

    // The operator completes pairing with the code read off the node's screen.
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/pair",
            OPERATOR_TOKEN,
            &json!({ "code": code, "device_id": "node-lobby", "display_name": "Lobby node" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let paired = body_json(resp).await;
    assert_eq!(paired["device_id"], "node-lobby");

    // The node's next poll flips to enrolled with the operator-chosen id.
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let enrolled = body_json(resp).await;
    assert_eq!(enrolled["status"], "enrolled");
    assert_eq!(enrolled["device_id"], "node-lobby");

    // The paired node is a managed device bound to the keypair.
    let resp = send(&h.router, get("/api/v1/devices/node-lobby", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let device = body_json(resp).await;
    assert_eq!(device["body"]["driver"], "displaynode");
    assert_eq!(
        device["body"]["enrollment"]["public_key"],
        public_key_b64(&node_key())
    );
}

#[tokio::test]
async fn pairing_codes_are_case_insensitive_to_type() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    let code = body_json(resp).await["pairing_code"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/pair",
            OPERATOR_TOKEN,
            &json!({ "code": code.to_lowercase() }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn pair_with_an_unknown_code_is_not_found_and_viewer_is_forbidden() {
    let h = harness();
    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/pair",
            OPERATOR_TOKEN,
            &json!({ "code": "ZZZZZZ" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/pair",
            VIEWER_TOKEN,
            &json!({ "code": "ZZZZZZ" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pair_refuses_a_device_id_that_already_exists() {
    let h = harness();
    // An existing device occupies the id.
    let body = json!({
        "name": "occupied",
        "body": { "id": "node-lobby", "driver": "zowietek", "address": "http://[fd00:db8::1]" }
    });
    let resp = send(
        &h.router,
        post_json("/api/v1/devices/node-lobby", OPERATOR_TOKEN, &body),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    let code = body_json(resp).await["pairing_code"]
        .as_str()
        .unwrap()
        .to_owned();

    let resp = send(
        &h.router,
        post_json(
            "/api/v1/devices/pair",
            OPERATOR_TOKEN,
            &json!({ "code": code, "device_id": "node-lobby" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_pending_pairing_table_is_bounded() {
    let h = harness();
    // Fill the bounded pending table (cap 32) with distinct node keys.
    for i in 0..32u8 {
        let key = SigningKey::from_bytes(&[i.wrapping_add(100); 32]);
        let resp = send(
            &h.router,
            post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &key)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED, "request {i} pends");
    }
    // The 33rd distinct key is shed with 429 — bounded memory, never growth.
    let key = SigningKey::from_bytes(&[200u8; 32]);
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &key)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// Keypair-signed heartbeats: liveness + assignment delivery.
// ---------------------------------------------------------------------------

/// Enroll the test node with a fresh token, returning its device id.
async fn enroll_node(h: &support::Harness) -> String {
    let (_, token) = mint_token(&h.router).await;
    let resp = send(
        &h.router,
        post_json_unauth(
            "/api/v1/devices/enroll",
            &enroll_body(Some(&token), &node_key()),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["device_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_signed_heartbeat_returns_the_display_assignment() {
    let h = harness();
    let device_id = enroll_node(&h).await;

    // The operator assigns this node a wall head (the config-sketch binding).
    let resp = send(
        &h.router,
        get(&format!("/api/v1/devices/{device_id}"), OPERATOR_TOKEN),
    )
    .await;
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let device = body_json(resp).await;
    let mut body = device["body"].clone();
    body["display"] = json!({ "assign": { "wall_head": "head-l" } });
    let update = json!({ "name": device["name"], "body": body });
    let resp = send(
        &h.router,
        support::put_json_if_match(
            &format!("/api/v1/devices/{device_id}"),
            OPERATOR_TOKEN,
            &update,
            &etag,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A correctly signed heartbeat answers with the current assignment.
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, now_secs(), &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let answer = body_json(resp).await;
    assert_eq!(answer["assignment"], json!({ "wall_head": "head-l" }));
    assert!(answer["heartbeat_secs"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn heartbeats_with_a_wrong_key_stale_or_replayed_timestamp_are_rejected() {
    let h = harness();
    let device_id = enroll_node(&h).await;
    let ts = now_secs();

    // Signed by a key that is not the enrolled one.
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, ts, &heartbeat_body(), &other_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Outside the freshness window (an hour old).
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, ts - 3_600, &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // A valid heartbeat…
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, ts, &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // …then an exact replay of the same timestamp is refused (strictly
    // increasing per device — replay defense).
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, ts, &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // And an older timestamp likewise.
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, ts - 1, &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_heartbeat_for_an_unknown_device_is_unauthorized() {
    let h = harness();
    let resp = send(
        &h.router,
        signed_heartbeat("node-ghost", now_secs(), &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn heartbeat_updates_the_display_head_projection() {
    let h = harness();
    let device_id = enroll_node(&h).await;

    // The node re-reports its heads: one disconnected now.
    let mut body = heartbeat_body();
    body["heads"][0]["connected"] = json!(false);
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, now_secs(), &body, &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(
        &h.router,
        get(
            &format!("/api/v1/devices/{device_id}/display-heads"),
            OPERATOR_TOKEN,
        ),
    )
    .await;
    let heads = body_json(resp).await;
    assert_eq!(heads[0]["connected"], false);
}

// ---------------------------------------------------------------------------
// Projection + lifecycle cleanup.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn display_heads_for_an_unknown_device_is_not_found() {
    let h = harness();
    let resp = send(
        &h.router,
        get("/api/v1/devices/node-ghost/display-heads", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_the_device_forgets_the_node_identity() {
    let h = harness();
    let device_id = enroll_node(&h).await;

    let resp = send(
        &h.router,
        get(&format!("/api/v1/devices/{device_id}"), ADMIN_TOKEN),
    )
    .await;
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let resp = send(
        &h.router,
        delete_if_match(
            &format!("/api/v1/devices/{device_id}"),
            ADMIN_TOKEN,
            Some(&etag),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // The keypair no longer authenticates a heartbeat…
    let resp = send(
        &h.router,
        signed_heartbeat(&device_id, now_secs(), &heartbeat_body(), &node_key()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // …and a fresh enroll from that key is back to the pairing flow (the
    // binding was genuinely dropped, not cached).
    let resp = send(
        &h.router,
        post_json_unauth("/api/v1/devices/enroll", &enroll_body(None, &node_key())),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
}
