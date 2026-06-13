//! The `zowietek` **poller actor** (DEV-A4, ADR-M009): the supervised
//! control-plane task that drives the DEV-A3 [`DeviceLifecycle`] through
//! `ADOPTING → ONLINE → DEGRADED → UNREACHABLE → reconnect`, opens the breaker on
//! `AUTH_FAILED` (no reconnect storm), enumerates the three facets into the
//! driver registry the projection routes read, and dispatches `set-mode`
//! convergence. Every test drives the poller through the **scripted transport
//! seam**, so the whole actor is socket-free — no real device, no `zowietek`
//! network feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    // Test prose names the lifecycle states in plain caps (ADOPTING/ONLINE/…)
    // for readability; not API items needing backticks.
    clippy::doc_markdown,
    // The harness returns a 4-tuple of shared handles — a test helper, not a
    // public type worth a named alias.
    clippy::type_complexity
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_control::devices::zowietek::client::{ScriptedReply, ScriptedTransport};
use multiview_control::devices::zowietek::poller::{
    PollerConfig, PollerControl, PollerStep, ZowietekPoller,
};
use multiview_control::devices::zowietek::ZowietekDriver;
use multiview_control::devices::{DeviceBroadcaster, DeviceDriverRegistry, DeviceStatusRegistry};
use multiview_engine::EnginePublisher;
use multiview_events::{DeviceState, Event};
use serde_json::json;

/// A broadcaster + the registries a poller drives, all control-plane.
fn harness() -> (
    Arc<EnginePublisher<serde_json::Value, Event>>,
    DeviceBroadcaster,
    Arc<DeviceStatusRegistry>,
    Arc<DeviceDriverRegistry>,
) {
    let engine = Arc::new(EnginePublisher::new(256));
    let status = Arc::new(DeviceStatusRegistry::new());
    let drivers = Arc::new(DeviceDriverRegistry::new());
    let broadcaster = DeviceBroadcaster::new(Arc::clone(&engine), Arc::clone(&status));
    (engine, broadcaster, status, drivers)
}

fn login_ok() -> ScriptedReply {
    ScriptedReply::json(
        json!({ "rsp": "succeed", "status": "00000", "data": { "uuid": "u", "type": 0 } }),
    )
}

fn venc_encoder() -> ScriptedReply {
    ScriptedReply::json(
        json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "encoder" } }),
    )
}

fn poller(
    device_id: &str,
    transport: &ScriptedTransport,
    broadcaster: &DeviceBroadcaster,
    status: &Arc<DeviceStatusRegistry>,
    drivers: &Arc<DeviceDriverRegistry>,
) -> ZowietekPoller<ScriptedTransport> {
    let driver = ZowietekDriver::new(
        device_id,
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(drivers),
        "admin",
        "admin",
    );
    ZowietekPoller::new(
        device_id,
        driver,
        Arc::clone(status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
}

/// The actor drives ADOPTING → ONLINE through the DeviceLifecycle (not an ad-hoc
/// status set), publishing ONLINE, and enumerates the facets on adopt.
#[tokio::test]
async fn adopt_step_drives_lifecycle_to_online_and_enumerates_facets() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // The poller's adopt enumerates the source facet (no I/O) and, for an
    // encoder, the source candidates land in the driver registry.
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    let step = poller.adopt_step().await;
    assert_eq!(step, PollerStep::Online, "a clean probe drives to ONLINE");
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "the lifecycle (driven by ProbeOk) published ONLINE"
    );
    // The source facet enumerated the served RTSP mounts into the driver
    // registry the projection route reads — at runtime, not the empty placeholder.
    assert!(
        !drivers.source_candidates("dev-a").is_empty(),
        "adopt enumerated the source candidates into the driver registry"
    );
}

/// A device-reported unhealthy stream drives ONLINE → DEGRADED via a DeviceFault
/// lifecycle event (the transition table), then recovery returns it to ONLINE.
#[tokio::test]
async fn poll_step_drives_online_to_degraded_then_recovers() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // First poll: an unhealthy stream → DEGRADED.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": false } ] }
        })),
    );
    // Second poll: healthy again → recover to ONLINE.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": true } ] }
        })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    assert_eq!(poller.poll_step().await, PollerStep::Degraded);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Degraded),
        "a device-reported fault drives ONLINE → DEGRADED through the lifecycle"
    );

    assert_eq!(poller.poll_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "a recovered stream drives DEGRADED → ONLINE"
    );
}

/// A dropped socket mid-poll rides ONLINE → UNREACHABLE, and a supervised
/// reconnect re-establishes the channel and re-converges to ONLINE.
#[tokio::test]
async fn poll_socket_drop_rides_to_unreachable_then_reconnects() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // Poll drops the socket → UNREACHABLE.
    transport.push("streamplay", ScriptedReply::socket_dropped());
    // Reconnect: login + probe succeed again → ONLINE.
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    assert_eq!(poller.poll_step().await, PollerStep::Unreachable);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Unreachable),
        "a dropped socket rides ONLINE → UNREACHABLE through the lifecycle"
    );

    // Supervised reconnect: the breaker is closed (not AUTH_FAILED), so a
    // reconnect attempt re-establishes the channel.
    assert_eq!(poller.reconnect_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "supervised reconnect re-converges UNREACHABLE → ONLINE"
    );
}

/// AUTH_FAILED opens the breaker: the actor does NOT re-login in a storm. A
/// reconnect attempt while the breaker is open is a no-op that issues NO login
/// request, and only a secret update re-arms a probe.
#[tokio::test]
async fn auth_failed_opens_the_breaker_no_reconnect_storm() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt login is rejected with bad credentials → AUTH_FAILED.
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "login failed", "status": "00002" })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    assert_eq!(poller.adopt_step().await, PollerStep::AuthFailed);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "bad credentials open the breaker (AUTH_FAILED)"
    );
    let after_adopt = transport.request_count("system");
    assert_eq!(
        after_adopt, 1,
        "exactly one login attempt was made on adopt"
    );

    // The breaker is OPEN: reconnect attempts must NOT re-login (no storm).
    for _ in 0..5 {
        let step = poller.reconnect_step().await;
        assert_eq!(
            step,
            PollerStep::BreakerOpen,
            "a reconnect while AUTH_FAILED is a no-op (the breaker is open)"
        );
    }
    assert_eq!(
        transport.request_count("system"),
        after_adopt,
        "the open breaker issued NO further login requests (no reconnect storm)"
    );
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "the device stays AUTH_FAILED while the breaker is open"
    );

    // A secret update re-arms a probe (lifecycle AUTH_FAILED → ADOPTING).
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    poller.secret_updated();
    assert_eq!(
        poller.adopt_step().await,
        PollerStep::Online,
        "a secret update re-arms the probe and the next adopt succeeds"
    );
}

/// A `PollerControl::Reboot` command (DEV-A4 fix 2) issues a real fire-and-forget
/// reboot to the device: a `system`-module write is sent and the expected socket
/// drop is ridden (no error). This proves the reboot verb is WIRED end-to-end to
/// the transport, not a no-op stub.
#[tokio::test]
async fn reboot_command_fires_fire_and_forget_to_the_device() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // The reboot request: the device drops the socket with no HTTP response
    // (the verified reboot hazard) — fire_and_forget treats this as expected.
    transport.push("system", ScriptedReply::socket_dropped());
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    let system_before = transport.request_count("system");

    poller.handle_control(PollerControl::Reboot).await;

    // A reboot write was actually issued to the device's `system` module — the
    // verb dispatches to the live transport, it is not a swallowed no-op.
    assert_eq!(
        transport.request_count("system"),
        system_before + 1,
        "reboot issues one fire-and-forget /system write to the device"
    );
    let last = transport.last_request().expect("a request was recorded");
    assert_eq!(last.module, "system", "reboot targets the system module");
    assert_eq!(
        last.body.get("opt").and_then(serde_json::Value::as_str),
        Some("reboot"),
        "the reboot request carries the reboot opt: {:?}",
        last.body
    );
}

/// A `PollerControl::SetMode` command to an ONLINE device records the desired
/// mode and dispatches the convergence — but with no grounded decode-table index
/// (DEV-A4 fix 3) the convergence REFUSES rather than issue a global stop, so the
/// poller issues ZERO `/streamplay` wire writes and defers. The set-mode is still
/// handled (the desire is recorded for the next grounded pass); what it must
/// never do is global-stop decode on a production unit.
#[tokio::test]
async fn set_mode_command_records_intent_and_defers_without_a_global_stop() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);

    // Dispatch a set-mode command through the actor's control channel handling.
    poller
        .handle_control(PollerControl::SetMode {
            mode: "decoder".to_owned(),
        })
        .await;

    // The desire was recorded (deferred to a future grounded convergence pass)…
    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "set-mode records the operator's desired mode"
    );
    // …and NO global decode-table mutation was issued: with no grounded index the
    // convergence refuses rather than fire the global `streamplay`/stop.
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "set-mode on an ONLINE box with no grounded index issues ZERO /streamplay writes"
    );
}

/// SAFETY (DEV-A4 fix 1): an operator `set-mode` while the device is NOT ONLINE
/// (here: DEGRADED, with a cached session) records the intent but issues ZERO
/// decode-table wire writes — it must NOT fire `converge_mode` through a session
/// the device's current state has not validated. The un-gated code would issue a
/// close+open `/streamplay` pair against a degraded device; the gate defers it.
#[tokio::test]
async fn set_mode_while_degraded_records_intent_and_issues_no_wire_writes() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // One poll reports an unhealthy stream → DEGRADED (the channel is up, the
    // session is cached, but the device is not ONLINE).
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({
            "rsp": "succeed", "status": "00000",
            "data": { "streams": [ { "healthy": false } ] }
        })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(poller.poll_step().await, PollerStep::Degraded);
    let before = transport.request_count("streamplay");

    // An operator set-mode arrives while DEGRADED.
    poller
        .handle_control(PollerControl::SetMode {
            mode: "decoder".to_owned(),
        })
        .await;

    // The intent was recorded (deferred to the next ONLINE pass)…
    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "set-mode records the desired mode even when not ONLINE"
    );
    // …but NO decode-table wire write was issued: convergence is ONLINE-gated, so
    // a stray set-mode can never mutate decode through a non-validated session.
    assert_eq!(
        transport.request_count("streamplay"),
        before,
        "set-mode while DEGRADED issues ZERO decode-table wire writes (gated, deferred)"
    );
}

/// SAFETY (DEV-A4 fix 1): an operator `set-mode` while the device is UNREACHABLE
/// records the intent but issues ZERO wire writes — the channel is down, so the
/// convergence must not be attempted (it is deferred to the next ONLINE pass).
#[tokio::test]
async fn set_mode_while_unreachable_records_intent_and_issues_no_wire_writes() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // The poll drops the socket → UNREACHABLE.
    transport.push("streamplay", ScriptedReply::socket_dropped());
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(poller.poll_step().await, PollerStep::Unreachable);
    let before = transport.request_count("streamplay");

    poller
        .handle_control(PollerControl::SetMode {
            mode: "decoder".to_owned(),
        })
        .await;

    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "set-mode records the desired mode even when UNREACHABLE"
    );
    assert_eq!(
        transport.request_count("streamplay"),
        before,
        "set-mode while UNREACHABLE issues ZERO wire writes (gated, deferred)"
    );
}

/// SAFETY (DEV-A4 fix 1): an operator `set-mode` while AUTH_FAILED (the A3 auth
/// breaker is OPEN, but a session is still cached from the earlier ONLINE pass)
/// records the intent but issues ZERO wire writes — a set-mode must NEVER bypass
/// the breaker to push decode-table writes through a session the device
/// repudiated.
#[tokio::test]
async fn set_mode_while_auth_failed_records_intent_and_bypasses_no_breaker() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // A poll comes back with a credential-rejection status → AUTH_FAILED (the
    // breaker opens). A session is still cached from the successful adopt above.
    transport.push(
        "streamplay",
        ScriptedReply::json(json!({ "rsp": "auth failed", "status": "00002" })),
    );
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);
    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(poller.poll_step().await, PollerStep::AuthFailed);
    assert_eq!(status.state("dev-a"), Some(DeviceState::AuthFailed));
    let before = transport.request_count("streamplay");

    poller
        .handle_control(PollerControl::SetMode {
            mode: "decoder".to_owned(),
        })
        .await;

    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "set-mode records the desired mode even while AUTH_FAILED"
    );
    assert_eq!(
        transport.request_count("streamplay"),
        before,
        "set-mode while AUTH_FAILED issues ZERO wire writes — the breaker is not bypassed"
    );
}

/// SAFETY (DEV-A4 fix 3): a device adopted with `desired_mode` records the
/// desire when adopt reaches ONLINE, but — with no grounded decode-table index —
/// the convergence REFUSES rather than fire a global `/streamplay` stop. Adopting
/// a production unit must NEVER global-stop its decode; the switch is deferred to
/// a future grounded pass. (Before fix 3, adopt issued a close+open global stop.)
#[tokio::test]
async fn adopt_with_desired_mode_records_intent_and_does_not_global_stop() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    // The box powers up in encoder mode; desired_mode is decoder → a switch is
    // wanted, but it must not be applied via a global stop.
    transport.push("venc", venc_encoder());
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::Online),
        "adopt drove the lifecycle to ONLINE"
    );
    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "the declared desired_mode is recorded for a future grounded convergence"
    );
    // No global decode-table mutation: the convergence refused for lack of a
    // grounded index, so adopt issued ZERO /streamplay switch.
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "adopt with desired_mode and no grounded index issues ZERO /streamplay writes"
    );
}

/// A device with NO `desired_mode` does not converge on adopt: no `/streamplay`
/// close/open switch is issued (the device keeps whatever mode it powered up in).
#[tokio::test]
async fn adopt_without_desired_mode_does_not_converge() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // No desired_mode → the convergence path must not run; the poller helper
    // builds a poller with desired_mode None.
    let mut poller = poller("dev-a", &transport, &broadcaster, &status, &drivers);

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "no desired_mode ⇒ adopt issues no mode-convergence /streamplay switch"
    );
}

/// `desired_mode` convergence is suppressed while the AUTH_FAILED breaker is
/// open: a bad-credential adopt opens the breaker and issues NO convergence
/// (only a state that actually reached ONLINE may converge).
#[tokio::test]
async fn auth_failed_suppresses_desired_mode_convergence() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt login rejected → AUTH_FAILED (the breaker opens).
    transport.push(
        "system",
        ScriptedReply::json(json!({ "rsp": "login failed", "status": "00002" })),
    );
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::AuthFailed);
    assert_eq!(
        status.state("dev-a"),
        Some(DeviceState::AuthFailed),
        "bad credentials opened the breaker"
    );
    assert_eq!(
        transport.request_count("streamplay"),
        0,
        "AUTH_FAILED ⇒ no convergence (the device never reached ONLINE)"
    );
}

/// SAFETY (DEV-A4 fix 3): a supervised reconnect that reaches ONLINE re-records
/// the `desired_mode` but, with no grounded decode-table index, issues NO mutating
/// `/streamplay` stop/start — a box that rebooted into the wrong mode is NOT
/// "fixed" by a global stop that would halt all decode on a production unit. The
/// reconnect may still read the decode table (enumeration), but it never issues a
/// `stop` or `start` opt. (Before fix 3, reconnect global-stopped to switch.)
#[tokio::test]
async fn reconnect_does_not_global_stop_to_reconverge() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    // Adopt: the box is ALREADY in decoder mode, so adopt converges no-op.
    transport.push("system", login_ok());
    transport.push(
        "venc",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "workmode": "decoder" } }),
        ),
    );
    // Adopt enumerates the decoder output targets (decode-mode box) — a getinfo
    // READ, never a stop/start.
    transport.push(
        "streamplay",
        ScriptedReply::json(
            json!({ "rsp": "succeed", "status": "00000", "data": { "entries": [] } }),
        ),
    );
    // Poll drops the socket → UNREACHABLE.
    transport.push("streamplay", ScriptedReply::socket_dropped());
    // Reconnect: the box came back in ENCODER mode (it rebooted). The desired
    // mode is decoder, but the reconnect must NOT global-stop to switch it.
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let mut poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    )
    .with_desired_mode(Some("decoder".to_owned()));

    assert_eq!(poller.adopt_step().await, PollerStep::Online);
    assert_eq!(poller.poll_step().await, PollerStep::Unreachable);
    assert_eq!(poller.reconnect_step().await, PollerStep::Online);

    // The desire is still recorded after reconnect…
    assert_eq!(
        poller.desired_mode(),
        Some("decoder"),
        "reconnect keeps the recorded desired_mode"
    );
    // …and NO mutating stop/start was ever issued on /streamplay (only reads).
    let order = transport.streamplay_request_order();
    let mutating = order
        .iter()
        .filter(|r| {
            matches!(
                r.body.get("opt").and_then(serde_json::Value::as_str),
                Some("stop" | "start")
            )
        })
        .count();
    assert_eq!(
        mutating, 0,
        "reconnect issued ZERO mutating /streamplay stop/start (no global stop); order = {order:?}"
    );
}

/// The spawned actor task drives the full lifecycle end-to-end over its control
/// channel and stops cleanly when the handle is dropped (no leaked task).
#[tokio::test]
async fn spawned_actor_runs_and_stops_on_handle_drop() {
    let (_engine, broadcaster, status, drivers) = harness();
    let transport = ScriptedTransport::new();
    transport.push("system", login_ok());
    transport.push("venc", venc_encoder());
    // A steady stream of healthy polls keeps it ONLINE.
    for _ in 0..32 {
        transport.push(
            "streamplay",
            ScriptedReply::json(json!({
                "rsp": "succeed", "status": "00000",
                "data": { "streams": [ { "healthy": true } ] }
            })),
        );
    }
    let driver = ZowietekDriver::new(
        "dev-a",
        Arc::new(transport.clone()),
        broadcaster.clone(),
        Arc::clone(&drivers),
        "admin",
        "admin",
    );
    let poller = ZowietekPoller::new(
        "dev-a",
        driver,
        Arc::clone(&status),
        "[fd00:db8::1]",
        PollerConfig::test_fast(),
    );
    let handle = poller.spawn();

    // Wait until the actor adopts and publishes ONLINE.
    let online = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if status.state("dev-a") == Some(DeviceState::Online) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        online.is_ok(),
        "the spawned actor adopts the device to ONLINE"
    );
    assert!(
        !drivers.source_candidates("dev-a").is_empty(),
        "the spawned actor populated the driver registry the projection route reads"
    );

    // Dropping the handle stops the task (abort on drop) — proven by the task
    // being finished shortly after.
    let join = handle.into_join_handle();
    join.abort();
    let _ = join.await;
}
