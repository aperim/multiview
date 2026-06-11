//! The Cast **session actor** (DEV-D2, ADR-M011) driven socket-free over the
//! scripted channel seam: connect → CONNECT → LAUNCH `CC1AD845` → CONNECT to
//! the app transport → LOAD; heartbeat PING every 10 s with the session dead
//! after 20 s without inbound traffic and 5 s reconnect retries; re-LOAD on
//! media IDLE; "preempted" surfaced (never fought) when another sender takes
//! the device; receiver-namespace volume/stop controls. The lifecycle states
//! it publishes are exactly the DEV-A3 `DeviceLifecycle` outputs.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Test prose names lifecycle states in plain caps (ONLINE/DEGRADED) and
    // protocol verbs (LOAD/PING) for readability.
    clippy::doc_markdown,
    // Test helpers take owned `serde_json::Value`s for terse call sites.
    clippy::needless_pass_by_value
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::devices::cast::media::{CastMediaTarget, HlsSegmentFormat};
use multiview_control::devices::cast::protocol::{
    CastFrame, NS_CONNECTION, NS_HEARTBEAT, NS_MEDIA, NS_RECEIVER, PLATFORM_RECEIVER_ID, SENDER_ID,
};
use multiview_control::devices::cast::session::{
    CastSessionActor, CastSessionConfig, ScriptedChannel, ScriptedConnector, ScriptedInbound,
    SentFrames,
};
use multiview_control::devices::{
    DeviceBroadcaster, DeviceStatusRegistry, PollerControl, PollerStep,
};
use multiview_control::EngineStateSnapshot;
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};

/// The media target every test session LOADs.
fn media() -> CastMediaTarget {
    CastMediaTarget {
        url: "http://192.0.2.7:8080/hls/program/program.m3u8".to_owned(),
        format: HlsSegmentFormat::MpegTs,
    }
}

/// Build the broadcaster + the engine publisher (to subscribe to events).
fn broadcaster() -> (
    Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    DeviceBroadcaster,
) {
    let engine = Arc::new(EnginePublisher::new(64));
    let status = Arc::new(DeviceStatusRegistry::new());
    let b = DeviceBroadcaster::new(Arc::clone(&engine), status);
    (engine, b)
}

/// An inbound frame from the device.
fn from_device(namespace: &str, payload: serde_json::Value) -> CastFrame {
    CastFrame {
        namespace: namespace.to_owned(),
        source: PLATFORM_RECEIVER_ID.to_owned(),
        destination: SENDER_ID.to_owned(),
        payload: payload.to_string(),
    }
}

/// A RECEIVER_STATUS carrying the Default Media Receiver app on transport
/// `t-1` / session `s-1`.
fn receiver_status_with_app() -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({
            "type": "RECEIVER_STATUS",
            "requestId": 0,
            "status": { "applications": [{
                "appId": "CC1AD845",
                "sessionId": "s-1",
                "transportId": "t-1",
                "displayName": "Default Media Receiver"
            }] }
        }),
    )
}

/// A RECEIVER_STATUS with **no** applications: another sender stopped or
/// replaced our app (the preemption signal).
fn receiver_status_without_app() -> CastFrame {
    from_device(
        NS_RECEIVER,
        serde_json::json!({ "type": "RECEIVER_STATUS", "requestId": 0, "status": {} }),
    )
}

/// A MEDIA_STATUS in `player_state` (optionally with an idle reason).
fn media_status(player_state: &str, idle_reason: Option<&str>) -> CastFrame {
    let mut entry = serde_json::json!({ "mediaSessionId": 1, "playerState": player_state });
    if let Some(reason) = idle_reason {
        entry["idleReason"] = serde_json::Value::String(reason.to_owned());
    }
    from_device(
        NS_MEDIA,
        serde_json::json!({ "type": "MEDIA_STATUS", "requestId": 0, "status": [entry] }),
    )
}

/// The payload `type` tokens of the frames sent on a channel, in order.
fn sent_types(sent: &SentFrames) -> Vec<(String, String)> {
    sent.lock()
        .expect("sent log lock")
        .iter()
        .map(|f| {
            let body: serde_json::Value =
                serde_json::from_str(&f.payload).expect("sent payload is JSON");
            (
                f.namespace.clone(),
                body["type"].as_str().unwrap_or("?").to_owned(),
            )
        })
        .collect()
}

/// Count the sent frames whose payload `type` equals `kind`.
fn count_sent(sent: &SentFrames, kind: &str) -> usize {
    sent_types(sent)
        .into_iter()
        .filter(|(_, t)| t == kind)
        .count()
}

#[tokio::test]
async fn connect_step_runs_connect_launch_load_in_order() {
    let (_engine, b) = broadcaster();
    let (channel, sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        // Queued behind the establishment: the first media status report.
        ScriptedInbound::Frame(media_status("PLAYING", None)),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );

    assert_eq!(actor.state(), DeviceState::Adopting, "starts ADOPTING");
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Online, "established session is ONLINE");
    assert_eq!(actor.state(), DeviceState::Online);

    // The exact ADR-M011 establishment sequence, in order: virtual CONNECT to
    // the platform receiver → LAUNCH the Default Media Receiver → CONNECT to
    // the app's transport → LOAD the HLS rendition.
    let types = sent_types(&sent);
    assert_eq!(
        types,
        vec![
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_RECEIVER.to_owned(), "LAUNCH".to_owned()),
            (NS_CONNECTION.to_owned(), "CONNECT".to_owned()),
            (NS_MEDIA.to_owned(), "LOAD".to_owned()),
        ]
    );
    // The app-transport CONNECT and the LOAD address the app, not the platform.
    let frames = sent.lock().expect("sent log lock").clone();
    assert_eq!(frames[2].destination, "t-1");
    assert_eq!(frames[3].destination, "t-1");

    // Until a MEDIA_STATUS arrives the published mode is the honest "loading".
    let status = actor.published_status().expect("a published status");
    assert_eq!(status.state, DeviceState::Online);
    assert_eq!(status.mode.as_deref(), Some("loading"));

    // The first MEDIA_STATUS maps the player state onto the conflated status.
    let step = actor.pump_step().await;
    assert_eq!(step, PollerStep::Online);
    let status = actor.published_status().expect("a published status");
    assert_eq!(status.mode.as_deref(), Some("playing"));
}

#[tokio::test]
async fn connect_refused_is_unreachable() {
    let (_engine, b) = broadcaster();
    // No scripted channels: every connect attempt is refused.
    let connector = ScriptedConnector::new(vec![]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Unreachable);
    assert_eq!(actor.state(), DeviceState::Unreachable);
}

#[tokio::test]
async fn launch_error_is_a_device_fault() {
    let (_engine, b) = broadcaster();
    let (channel, _sent) = ScriptedChannel::new(vec![ScriptedInbound::Frame(from_device(
        NS_RECEIVER,
        serde_json::json!({ "type": "LAUNCH_ERROR", "reason": "NOT_FOUND" }),
    ))]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let step = actor.connect_step().await;
    assert_eq!(step, PollerStep::Degraded, "launch refused = device fault");
    assert_eq!(actor.state(), DeviceState::Degraded);
}

#[tokio::test(start_paused = true)]
async fn heartbeat_pings_and_expiry_reconnects() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let (ch2, sent2) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "[2001:db8::20]:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    // After the first channel goes silent: PINGs ride every 10 s, the session
    // is declared dead 20 s after the last inbound traffic, the reconnect
    // retries 5 s later and re-establishes (LAUNCH + LOAD again).
    tokio::time::sleep(Duration::from_secs(40)).await;

    assert!(
        count_sent(&sent1, "PING") >= 1,
        "PINGs ride the first channel every 10 s: {:?}",
        sent_types(&sent1)
    );
    assert_eq!(
        connects.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the dead session reconnected exactly once"
    );
    assert_eq!(count_sent(&sent2, "LAUNCH"), 1, "re-LAUNCH on reconnect");
    assert_eq!(count_sent(&sent2, "LOAD"), 1, "re-LOAD on reconnect");
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Online),
        "re-established session is ONLINE again"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn media_idle_reloads_after_the_reload_delay() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Frame(media_status("IDLE", Some("ERROR"))),
        // The re-LOAD answer arrives well after the (5 s) reload delay.
        ScriptedInbound::Wait(Duration::from_secs(8)),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    // Shortly after the IDLE lands: DEGRADED, mode "idle", and only the
    // original LOAD so far (the supervisor waits the reload delay).
    tokio::time::sleep(Duration::from_secs(3)).await;
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Degraded, "IDLE degrades");
    assert_eq!(snapshot.mode.as_deref(), Some("idle"));
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // After the reload delay the supervisor re-LOADs; the next PLAYING
    // recovers the session to ONLINE.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(count_sent(&sent1, "LOAD"), 2, "re-LOAD on IDLE");
    let snapshot = status.snapshot("cast-1").expect("a status row");
    assert_eq!(snapshot.state, DeviceState::Online);
    assert_eq!(snapshot.mode.as_deref(), Some("playing"));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn preemption_is_surfaced_and_never_fought() {
    let (engine, b) = broadcaster();
    let mut events = engine.subscribe();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Wait(Duration::from_secs(2)),
        // Another sender took the device: our app is gone from the status.
        ScriptedInbound::Frame(receiver_status_without_app()),
        ScriptedInbound::Hang,
    ]);
    // The post-preemption reconnect (after the heartbeat dies on the silent
    // channel) only re-establishes the management channel — it must NOT
    // re-LAUNCH/re-LOAD over the other sender's session.
    let (ch2, sent2) = ScriptedChannel::new(vec![ScriptedInbound::Hang]);
    let connector = ScriptedConnector::new(vec![ch1, ch2]);
    let connects = connector.connect_count();
    let status = Arc::clone(b.registry());
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();

    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        status.state("cast-1"),
        Some(DeviceState::Degraded),
        "preempted session is DEGRADED (the channel is up, the media is not ours)"
    );
    // Exactly the original LAUNCH/LOAD — we never fight the other sender.
    assert_eq!(count_sent(&sent1, "LAUNCH"), 1);
    assert_eq!(count_sent(&sent1, "LOAD"), 1);

    // A `device.error` naming the preemption rode the lossless lane.
    let mut saw_preempted = false;
    while let Ok(envelope) = events.try_recv() {
        if let Event::DeviceError(e) = &*envelope.event {
            if e.device_id == "cast-1" && e.message.contains("preempted") {
                saw_preempted = true;
            }
        }
    }
    assert!(saw_preempted, "device.error surfaces the preemption");

    // Ride past the heartbeat expiry: the reconnect re-establishes the
    // channel but stays hands-off (no LAUNCH/LOAD) and remains DEGRADED.
    tokio::time::sleep(Duration::from_secs(40)).await;
    assert_eq!(connects.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        count_sent(&sent2, "LAUNCH"),
        0,
        "never re-LAUNCH after preemption"
    );
    assert_eq!(
        count_sent(&sent2, "LOAD"),
        0,
        "never re-LOAD after preemption"
    );
    assert_eq!(status.state("cast-1"), Some(DeviceState::Degraded));
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn stop_control_stops_the_receiver_app_and_exits() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(handle.try_dispatch(PollerControl::StopCast));
    let task = handle.into_join_handle();
    // The actor STOPs the receiver app and exits voluntarily (the task
    // completes — it is not aborted).
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the actor exits after StopCast")
        .expect("the actor exits cleanly, not by abort");
    let frames = sent1.lock().expect("sent log lock").clone();
    let stop = frames
        .iter()
        .find(|f| f.payload.contains("\"STOP\""))
        .expect("a receiver STOP was sent");
    assert_eq!(stop.namespace, NS_RECEIVER);
    let body: serde_json::Value = serde_json::from_str(&stop.payload).expect("STOP body");
    assert_eq!(body["sessionId"], "s-1", "STOP names the receiver session");
}

#[tokio::test(start_paused = true)]
async fn volume_control_rides_the_receiver_namespace() {
    let (_engine, b) = broadcaster();
    let (ch1, sent1) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(media_status("PLAYING", None)),
        ScriptedInbound::Hang,
    ]);
    let connector = ScriptedConnector::new(vec![ch1]);
    let actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    let handle = actor.spawn();
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(handle.try_dispatch(PollerControl::SetVolume { percent: 42 }));
    tokio::time::sleep(Duration::from_secs(1)).await;

    let frames = sent1.lock().expect("sent log lock").clone();
    let set = frames
        .iter()
        .find(|f| f.payload.contains("SET_VOLUME"))
        .expect("a SET_VOLUME was sent");
    assert_eq!(set.namespace, NS_RECEIVER);
    let body: serde_json::Value = serde_json::from_str(&set.payload).expect("SET_VOLUME body");
    let level = body["volume"]["level"].as_f64().expect("a unit level");
    assert!((level - 0.42).abs() < 1e-9, "42% = level 0.42, got {level}");
    drop(handle);
}

#[tokio::test]
async fn inbound_device_ping_is_answered_with_pong() {
    let (_engine, b) = broadcaster();
    let (channel, sent) = ScriptedChannel::new(vec![
        ScriptedInbound::Frame(receiver_status_with_app()),
        ScriptedInbound::Frame(from_device(
            NS_HEARTBEAT,
            serde_json::json!({"type": "PING"}),
        )),
    ]);
    let connector = ScriptedConnector::new(vec![channel]);
    let mut actor = CastSessionActor::new(
        "cast-1",
        connector,
        "192.0.2.20:8009",
        media(),
        b,
        CastSessionConfig::default(),
    );
    assert_eq!(actor.connect_step().await, PollerStep::Online);
    let _ = actor.pump_step().await;
    assert_eq!(
        count_sent(&sent, "PONG"),
        1,
        "the device's own PING is answered: {:?}",
        sent_types(&sent)
    );
}
