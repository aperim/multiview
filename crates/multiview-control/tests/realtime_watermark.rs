//! Connect-time broadcast watermark (ADR-RT009): an event already folded into the
//! connect snapshot must NOT be re-delivered as a delta.
//!
//! The transports subscribe to the engine broadcast BEFORE reading the engine
//! snapshot; events that race into that window are reflected in the snapshot AND
//! buffered in the subscription. Without a watermark they replay as deltas —
//! duplicate delivery, and (for a multi-transition object) a transient backward
//! roll of the client's state. The fix pairs the snapshot read with a broadcast
//! watermark (`events.sequence()`) and drops every subscribed event whose
//! `seq <= watermark` BEFORE issuing the per-connection seq, so the drop is
//! invisible to resume-by-seq and composes with the #211 object-scope filter.
//!
//! These tests drive `SessionStream` directly — the transport-agnostic core both
//! `run_ws_session` and `sse_handler` share — exactly like `tests/realtime.rs`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_control::SessionStream;
use multiview_engine::EnginePublisher;
use multiview_events::{
    Alert, AlertSeverity, DeviceState, DeviceStatus, Event, FrameKind, LifecycleState, TileState,
};

type Publisher = EnginePublisher<serde_json::Value, Event>;

/// An `alert.raised` event keyed by `key`.
fn alert(key: &str) -> Event {
    Event::AlertRaised(Alert {
        key: key.to_owned(),
        severity: AlertSeverity::Warning,
        title: "test".to_owned(),
        detail: None,
        active: true,
    })
}

/// A `tile.state` transition for `input` from `from` to `to`.
fn tile(input: &str, from: LifecycleState, to: LifecycleState) -> Event {
    Event::TileState(TileState {
        from,
        to,
        input: Some(input.to_owned()),
        trigger: "test".to_owned(),
    })
}

/// A `device.status` event scoped (by `object_authz_scope_id`) to `device_id`.
fn device_status(device_id: &str) -> Event {
    Event::DeviceStatus(DeviceStatus::new(device_id, DeviceState::Online))
}

/// Drain up to `polls` deltas from a session, collecting the ones actually
/// delivered (an `Ok(None)` is a suppressed/skipped event, not a delivery). Each
/// event is pre-buffered in the subscription, so `next_delta` never awaits — the
/// bounded poll count terminates deterministically without a timeout.
async fn drain(session: &mut SessionStream, polls: usize) -> Vec<multiview_control::RealtimeFrame> {
    let mut delivered = Vec::new();
    for _ in 0..polls {
        if let Some(frame) = session.next_delta().await.unwrap() {
            delivered.push(frame);
        }
    }
    delivered
}

/// The core defect: two transitions of one tile race into the subscribe→snapshot
/// window (so the snapshot already reflects the latest, `Live`), then a fresh
/// event lands after the watermark. The two pre-snapshot transitions
/// (`seq <= watermark`) must be dropped — no duplicate, no backward roll — and
/// only the post-watermark event delivered, at a GAPLESS per-connection seq
/// (the two drops consume no connection seq).
#[tokio::test]
async fn watermark_suppresses_deltas_at_or_before_the_connect_snapshot() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();

    // Two transitions of `cam` land in the subscribe→snapshot window; the
    // connect snapshot reflects the LATEST (`Live`).
    let _s1 = engine.publish_event(tile("cam", LifecycleState::Live, LifecycleState::NoSignal));
    let s2 = engine.publish_event(tile("cam", LifecycleState::NoSignal, LifecycleState::Live));

    // The watermark is the broadcast frontier captured with the snapshot.
    let watermark = engine.events.sequence();
    assert_eq!(watermark, s2, "watermark is the latest published event seq");

    let mut session = SessionStream::new(sub, "sess-wm", None).with_snapshot_watermark(watermark);

    // Snapshot first (per-connection seq 0), exactly as both transports emit it.
    let snap = session.snapshot_frame(engine.state.sequence());
    assert_eq!(snap.kind, FrameKind::Snapshot);
    assert_eq!(snap.envelope.seq.get(), 0);

    // A fresh event lands AFTER the watermark.
    let _s3 = engine.publish_event(alert("after-connect"));

    // s1,s2 (<= watermark) are dropped; only the post-watermark alert is
    // delivered — three polls drain the three buffered events.
    let delivered = drain(&mut session, 3).await;
    assert_eq!(
        delivered.len(),
        1,
        "the two pre-snapshot transitions must not replay as deltas"
    );
    let d = &delivered[0];
    assert_eq!(d.kind, FrameKind::Delta);
    assert_eq!(
        d.envelope.seq.get(),
        1,
        "per-connection seq stays gapless: the suppressed events consumed no seq"
    );
    match &d.envelope.payload {
        Event::AlertRaised(a) => assert_eq!(a.key, "after-connect"),
        other => panic!("expected the post-watermark alert, got {other:?}"),
    }
}

/// The watermark composes with the #211 object-scope filter (ADR-W005/ADR-W025):
/// BOTH read-side drops run before `issue_seq`. A pre-watermark in-scope delta is
/// dropped by the watermark; a post-watermark out-of-scope delta is dropped by the
/// scope; only a post-watermark in-scope delta is delivered — proving the fix does
/// not regress the object-scope projection and both filters keep the seq gapless.
#[tokio::test]
async fn watermark_composes_with_the_object_scope_filter() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();

    // Pre-watermark: an in-scope AND an out-of-scope device status.
    let _s1 = engine.publish_event(device_status("dev-mine"));
    let _s2 = engine.publish_event(device_status("dev-other"));
    let watermark = engine.events.sequence();

    let mut session = SessionStream::new(sub, "sess-wm-scope", None)
        .with_object_scope(Some(vec!["dev-mine".to_owned()]))
        .with_snapshot_watermark(watermark);
    let _snap = session.snapshot_frame(engine.state.sequence());

    // Post-watermark: an in-scope then an out-of-scope device status.
    let _s3 = engine.publish_event(device_status("dev-mine"));
    let _s4 = engine.publish_event(device_status("dev-other"));

    // s1 (in-scope, <= watermark) dropped by the watermark; s2 dropped twice-over;
    // s3 (in-scope, > watermark) delivered; s4 (out-of-scope) dropped by scope.
    let delivered = drain(&mut session, 4).await;
    assert_eq!(
        delivered.len(),
        1,
        "only the post-watermark in-scope delta is delivered"
    );
    let d = &delivered[0];
    assert_eq!(
        d.envelope.seq.get(),
        1,
        "watermark + scope drops both precede issue_seq — seq stays gapless"
    );
    match &d.envelope.payload {
        Event::DeviceStatus(s) => assert_eq!(s.device_id, "dev-mine"),
        other => panic!("expected the in-scope device.status delta, got {other:?}"),
    }
}

/// A session with NO watermark (`with_snapshot_watermark` never called) is
/// unchanged: every event is delivered as a delta. Guards that the default /
/// resume path (`snapshot_watermark == None`) is untouched by the fix.
#[tokio::test]
async fn absent_watermark_delivers_every_delta_unchanged() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();

    let _s1 = engine.publish_event(alert("one"));
    let _s2 = engine.publish_event(alert("two"));

    let mut session = SessionStream::new(sub, "sess-no-wm", None);
    let _snap = session.snapshot_frame(engine.state.sequence());
    let _s3 = engine.publish_event(alert("three"));

    let delivered = drain(&mut session, 3).await;
    assert_eq!(
        delivered.len(),
        3,
        "with no watermark the stream delivers every delta, exactly as before"
    );
    assert_eq!(delivered[0].envelope.seq.get(), 1);
    assert_eq!(delivered[2].envelope.seq.get(), 3);
}
