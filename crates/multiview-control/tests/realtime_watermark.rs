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

use multiview_control::devices::{DeviceBroadcaster, DeviceStatusRegistry, PublishStatusSeam};
use multiview_control::SessionStream;
use multiview_engine::EnginePublisher;
use multiview_events::{
    AddressFamily, Alert, AlertSeverity, DeviceDiscovered, DeviceError, DeviceMode, DeviceState,
    DeviceStatus, Event, FrameKind, ImpactClass, LifecycleState, ModePhase, TileState,
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

/// A `device.discovered` event — a lossless lifecycle row that carries NO registry
/// id and is in NEITHER the engine state blob nor the `DeviceStatus` registry.
fn device_discovered(driver: &str) -> Event {
    Event::DeviceDiscovered(DeviceDiscovered::new(
        driver.to_owned(),
        "http://[fd00:db8::42]".to_owned(),
        AddressFamily::Ipv6,
    ))
}

/// A `device.mode` event — a lossless lifecycle transition, in no connect snapshot.
fn device_mode(device_id: &str, mode: &str) -> Event {
    Event::DeviceMode(DeviceMode {
        device_id: device_id.to_owned(),
        mode: mode.to_owned(),
        phase: ModePhase::Finished,
        impact: ImpactClass::Device,
        detail: None,
    })
}

/// A `device.error` event — a lossless lifecycle signal, in no connect snapshot.
fn device_error(device_id: &str, message: &str) -> Event {
    Event::DeviceError(DeviceError {
        device_id: device_id.to_owned(),
        code: None,
        message: message.to_owned(),
    })
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

/// CRITICAL LOST-DELTA GUARD (PR #230 review-panel finding): lossless lifecycle
/// events that appear in NO connect snapshot frame — `device.discovered`,
/// `device.mode`, `device.error` (none is in `current_engine_snapshot()` NOR the
/// `DeviceStatus` registry; `device.discovered` has no registry id at all) — MUST
/// be delivered even when they land in the pre-watermark window. A *global*
/// `seq <= watermark` drop would permanently lose them: they are in no snapshot to
/// heal from and carry no seq the client can resume — strictly worse than the
/// duplicate the watermark fixes. This FAILS on the un-scoped (global) watermark
/// and passes once the drop is scoped to snapshot-backed variants (ADR-RT009).
#[tokio::test]
async fn watermark_never_drops_lossless_lifecycle_events() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();

    // Three lossless lifecycle events land in the subscribe->snapshot window.
    // Not one is reproduced by any connect snapshot frame.
    let _d1 = engine.publish_event(device_discovered("ndi-source"));
    let _d2 = engine.publish_event(device_mode("dev-a", "decoder"));
    let _d3 = engine.publish_event(device_error("dev-a", "probe failed"));
    let watermark = engine.events.sequence();

    let mut session =
        SessionStream::new(sub, "sess-lossless", None).with_snapshot_watermark(watermark);
    // Snapshot first, exactly as the transports emit it — it reproduces NONE of
    // the three lifecycle events above.
    let _hello = session.snapshot_frame(engine.state.sequence());

    // All three are pre-watermark (`seq <= watermark`) but lossless — the
    // watermark must NOT drop them. They arrive as the first three deltas, in
    // order, at gapless per-connection seqs 1,2,3.
    let delivered = drain(&mut session, 3).await;
    assert_eq!(
        delivered.len(),
        3,
        "lossless lifecycle events in the pre-watermark window must never be dropped by the watermark"
    );
    assert_eq!(delivered[0].envelope.seq.get(), 1);
    assert_eq!(delivered[2].envelope.seq.get(), 3);
    assert!(
        matches!(delivered[0].envelope.payload, Event::DeviceDiscovered(_)),
        "device.discovered must survive the watermark window"
    );
    assert!(
        matches!(delivered[1].envelope.payload, Event::DeviceMode(_)),
        "device.mode must survive the watermark window"
    );
    assert!(
        matches!(delivered[2].envelope.payload, Event::DeviceError(_)),
        "device.error must survive the watermark window"
    );
}

/// FINDING (a) strengthened with an actual snapshot inspection: a snapshot-backed
/// event (`tile.state`, reproduced in the `TilesSnapshot`) that lands in the
/// pre-watermark window IS in the built snapshot AND is suppressed as a delta — no
/// duplicate, no backward roll. The test BUILDS the tiles snapshot frame and
/// asserts it actually contains the tile, proving the no-duplicate premise the
/// watermark relies on (not just the suppression mechanics).
#[tokio::test]
async fn watermark_drops_a_tile_state_the_built_snapshot_reproduces() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let sub = engine.subscribe();

    // A tile.state transition lands in the window; the engine state blob the
    // connect snapshot reads reflects the resulting state (LIVE) — modelling the
    // tick-path state-then-event ordering (state_of precedes event_of).
    let _s1 = engine.publish_event(tile("cam", LifecycleState::NoSignal, LifecycleState::Live));
    let watermark = engine.events.sequence();
    let blob = serde_json::json!({
        "tiles": [ { "id": "tile-cam", "state": "LIVE", "input": "cam" } ]
    });

    let mut session = SessionStream::new(sub, "sess-snap", None).with_snapshot_watermark(watermark);
    let _hello = session.snapshot_frame(engine.state.sequence());

    // The built tiles snapshot REPRODUCES the tile's current state — so the
    // pre-watermark tile.state delta is redundant (it is already in the snapshot).
    let tiles_snap = session
        .tiles_snapshot_frame(&blob, engine.state.sequence())
        .expect("a tiles snapshot frame is built");
    match &tiles_snap.envelope.payload {
        Event::TilesSnapshot(s) => assert!(
            s.tiles.iter().any(|t| t.input.as_deref() == Some("cam")),
            "the connect snapshot must reproduce the tile the pre-watermark delta describes"
        ),
        other => panic!("expected a tiles snapshot, got {other:?}"),
    }

    // A fresh post-watermark event proves the stream still flows past the drop.
    let _s2 = engine.publish_event(alert("after"));

    // The pre-watermark tile.state (in the snapshot) is suppressed; only the
    // post-watermark alert is delivered. Two snapshot frames issued per-connection
    // seqs 0 ($hello) and 1 (tiles), so the first delta is gapless seq 2.
    let delivered = drain(&mut session, 2).await;
    assert_eq!(
        delivered.len(),
        1,
        "the snapshot-backed tile.state delta must not be re-delivered"
    );
    assert_eq!(
        delivered[0].envelope.seq.get(),
        2,
        "first delta follows the two snapshot frames (seqs 0,1) — gapless"
    );
    assert!(matches!(
        delivered[0].envelope.payload,
        Event::AlertRaised(_)
    ));
}

/// FINDING (a) — the #230 residual, now EXERCISED rather than modelled. The
/// tests above prove the watermark's *suppression mechanics*, but they MODEL the
/// fold-then-publish ordering the safety argument rests on: e.g.
/// [`watermark_drops_a_tile_state_the_built_snapshot_reproduces`] publishes only
/// the event and then hand-writes the `blob` asserting the state is `LIVE`. A
/// regression that reordered the real producer — publishing the event BEFORE
/// writing the state/registry — would slip past them, yet it is exactly what
/// breaks ADR-RT009: dropping a `seq <= watermark` delta is only safe because the
/// producer folds state BEFORE publishing, so the connect snapshot already holds
/// it.
///
/// This test EXERCISES the one real fold-then-publish path reachable inside
/// `multiview-control`: [`DeviceBroadcaster::publish_status`], which writes the
/// `DeviceStatusRegistry` (the device-snapshot source) THEN publishes the
/// `device.status` event. It drives the REAL registry, the REAL publish, and the
/// REAL `SessionStream::devices_snapshot_frames` + watermark drop — no modelled
/// blob — and asserts the connect flow never loses the device across the
/// watermark boundary.
///
/// It is DETERMINISTIC (not loop-and-hope, rule 18) via a `_test-seams`
/// rendezvous installed BETWEEN the registry write and the event publish: with
/// the publisher parked there, the observer captures the watermark and the device
/// snapshot in that exact window. Under the shipped state-then-event order the
/// registry already holds the device (the snapshot reproduces it) and the event
/// is not yet visible (watermark below its seq, so the delta is delivered, not
/// dropped) — the device is present. A reorder (event before the registry write)
/// makes the observer capture the event's seq while the registry snapshot is
/// still empty, so the `seq <= watermark` drop loses the device with no snapshot
/// to heal from and this test FAILS. Proven by locally reordering
/// `publish_status` (event before `set_status`) → RED, then reverting → GREEN.
///
/// The `tile.state` fold-then-publish (the engine tick, `runtime.rs`) is
/// compositor/engine-driven and not reachable from a control-crate test; a
/// cli-level exercise of it is tracked as a follow-up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_status_state_then_event_never_loses_the_device_across_the_watermark() {
    let engine: Arc<Publisher> = Arc::new(EnginePublisher::new(64));
    let registry = Arc::new(DeviceStatusRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&registry));

    // Subscribe BEFORE the publish, exactly as the transports do: the racing
    // device.status must be buffered in this subscription so it would replay as a
    // delta absent the watermark.
    let sub = engine.subscribe();

    // The rendezvous parks publish_status between the registry write and the
    // event publish.
    let seam = PublishStatusSeam::new();
    let publisher = broadcaster.clone().with_publish_status_seam(seam.clone());

    // Drive the REAL fold-then-publish on its own thread; it parks at the seam.
    let handle = std::thread::spawn(move || {
        publisher.publish_status(DeviceStatus::new("cam-1", DeviceState::Online))
    });

    // Wait until the publisher is parked. Under the shipped order the registry
    // write has happened and the event is NOT yet published.
    seam.wait_until_parked();

    // Capture the connect watermark THEN the snapshot, exactly as a connecting
    // session does — reading the REAL registry, no modelled blob.
    let watermark = engine.events.sequence();
    let mut session = SessionStream::new(sub, "sess-fold", None).with_snapshot_watermark(watermark);
    // Emit the connect frames the real handler emits: the `$hello`, then one
    // device-status snapshot frame per tracked device (read from the registry).
    let _hello = session.snapshot_frame(engine.state.sequence());
    let device_snapshot = session.devices_snapshot_frames(&registry, engine.state.sequence());

    // Release the publisher; it publishes the device.status event into the
    // subscription and returns the event's engine seq.
    seam.release();
    let event_seq = handle.join().expect("the publish thread completes");

    // Drain the single buffered delta (dropped by the watermark, or delivered).
    let deltas = drain(&mut session, 1).await;

    let in_snapshot = device_snapshot
        .iter()
        .any(|f| is_cam1_status(&f.envelope.payload));
    let in_deltas = deltas.iter().any(|f| is_cam1_status(&f.envelope.payload));

    // The lost-delta guard: the device must be reflected at LEAST once — in the
    // connect snapshot or as a delivered delta — never lost. (A duplicate across
    // both is the tolerable at-least-once residual, RT003; a LOSS is the #230
    // defect the watermark must never cause.)
    assert!(
        in_snapshot || in_deltas,
        "device.status for cam-1 must survive the connect watermark boundary \
         (snapshot={in_snapshot}, delta={in_deltas}); a reorder publishing the \
         event before the registry write loses it — the #230 defect"
    );

    // Pin the shipped state-then-event SHAPE so the pass is not a coincidence:
    // the registry write preceded the publish, so (a) the watermark captured with
    // the event not-yet-visible is BELOW the event seq, and (b) the connect
    // snapshot already reproduces the device.
    assert!(
        watermark < event_seq,
        "state-then-event: the watermark captured with the event not-yet-published \
         must be below the event seq (watermark={watermark}, event_seq={event_seq})"
    );
    assert!(
        in_snapshot,
        "state-then-event: the registry write preceded the publish, so the connect \
         snapshot must already reproduce cam-1"
    );
}

/// True for a `device.status` event for `cam-1`.
fn is_cam1_status(event: &Event) -> bool {
    matches!(event, Event::DeviceStatus(s) if s.device_id == "cam-1")
}
