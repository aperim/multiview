//! CONSPECT-3 never-off-air chaos gate (extends the CONSPECT-2 gate to the
//! heartbeat task; ADR-0096, invariants #1/#10).
//!
//! THE SACRED CONSTRAINT: the device-licensing heartbeat is best-effort and
//! **physically incapable of touching the output clock**. These tests run the
//! real software output clock WHILE a heartbeat task against a misbehaving
//! in-process server is **SIGKILL'd (aborted) / stalled / partitioned**, and
//! assert the output clock still emits exactly one frame per tick and never
//! falters, and the last-good lease state is unchanged afterwards.
//!
//! The store is seeded EMPTY here (last-good == no lease) — a hostile heartbeat
//! that aborts or only ever errors can install nothing and remove nothing, so
//! the store stays empty and the output clock is untouched. The
//! crate-level tests (`multiview-licence`'s `heartbeat_client` suite) cover the
//! seeded-lease "withhold keeps last-good Active" path against a real fake that
//! mints verified leases; this gate is specifically the engine-clock isolation
//! property, with no extra dev-dependencies pulled into the cli.
//!
//! Everything runs under a hard `tokio::time::timeout` so a hung loop fails CI
//! fast rather than hanging it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_cli::run::SoftwareEngine;
use multiview_config::MultiviewConfig;
use multiview_engine::{CooperativePacer, ManualTimeSource};
use multiview_licence::heartbeat::{
    DeviceIdentity, HeartbeatClient, HeartbeatConfig, HeartbeatError, HeartbeatRequest,
    HeartbeatResponse, LicenceServer, LicensingKeys, PinnedRoot,
};
use multiview_licence::{EnforcementLevel, LeaseStore};

/// The live production ECDSA-P256 root point (base64url uncompressed) — a valid
/// point so `PinnedRoot` parses; the hostile server never returns a lease, so it
/// is never actually used to verify anything here.
const ROOT_PUB_B64URL: &str =
    "BN4f6BIHOFZmFqXp9YM1U65bJTGpOOob1I9X8C_FpfJOWanTCs4Z3c-l8C1wqH4g8Rl01VNkQNC78XixViLiwRY";

/// A small 1x1 software config that builds + runs deterministically.
fn small_config() -> MultiviewConfig {
    let toml = r##"
schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/hb_gate.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    let cfg = MultiviewConfig::load_from_toml(toml).expect("parse");
    cfg.validate().expect("validate");
    cfg
}

/// A misbehaving in-process licence server: it either stalls forever or returns
/// a partition error. It never returns a valid lease — the point is that a
/// HOSTILE heartbeat cannot touch the output clock or the last-good state.
struct HostileServer {
    /// When true, every call hangs until cancelled (a stall / black hole).
    stall: AtomicBool,
    /// Set true once a stalled call is genuinely parked in flight (so a test can
    /// wait for a real concurrent stall rather than racing).
    in_flight: AtomicBool,
}

impl HostileServer {
    fn stalling() -> Self {
        Self {
            stall: AtomicBool::new(true),
            in_flight: AtomicBool::new(false),
        }
    }
    fn partitioning() -> Self {
        Self {
            stall: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
        }
    }
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }
    async fn maybe_stall(&self) {
        if self.stall.load(Ordering::SeqCst) {
            // Hang "forever" — the test aborts the task; never resolves on its own.
            self.in_flight.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    }
}

impl LicenceServer for HostileServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
    async fn heartbeat(
        &self,
        _org: &str,
        _req: HeartbeatRequest,
        _idem: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        self.maybe_stall().await;
        Err(HeartbeatError::Transport("partitioned".to_owned()))
    }
}

fn dummy_root() -> PinnedRoot {
    PinnedRoot::from_base64url(ROOT_PUB_B64URL).expect("the production root point parses")
}

fn identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_test".to_owned(),
        instance_id: "inst_test".to_owned(),
        binding_id: Some("ib_test".to_owned()),
        fingerprint_digest: "0".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_test".to_owned(),
        instance_discriminator_hash: "disc_test".to_owned(),
        instance_discriminator_digest: "1".repeat(64),
        app_version: "test".to_owned(),
        device_public_key_b64url: "a2V5".to_owned(),
    }
}

fn client(server: Arc<HostileServer>, store: Arc<LeaseStore>) -> HeartbeatClient<HostileServer> {
    HeartbeatClient::new(
        server,
        store,
        dummy_root(),
        HeartbeatConfig {
            org_id: "org_test".to_owned(),
            min_interval: Duration::from_millis(1),
            ..HeartbeatConfig::default()
        },
        identity(),
    )
}

/// THE GATE: a SIGKILL'd (aborted) heartbeat task cannot touch the output clock
/// or the lease store. We build a compliant engine, spawn a heartbeat loop, ABORT
/// it mid-flight (the in-process analogue of SIGKILL), and drive the engine — one
/// frame per tick, never faltered, and the store state is unchanged.
#[tokio::test]
async fn a_sigkilled_heartbeat_task_never_touches_the_output_clock() {
    let store = Arc::new(LeaseStore::new());
    assert!(
        store.status().is_none(),
        "store starts empty (last-good = none)"
    );

    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // Spawn a stalling heartbeat loop, then abort it (SIGKILL analogue).
    let server = Arc::new(HostileServer::stalling());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });
    tokio::time::sleep(Duration::from_millis(5)).await;
    handle.abort();
    let _ = handle.await; // joins Cancelled.

    // The engine drives to completion regardless — one frame per tick, no falter.
    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 30),
    )
    .await
    .expect("the engine run must not hang")
    .expect("the engine drives regardless of the heartbeat task");
    assert_eq!(report.frames, 30, "one frame per tick");
    assert_eq!(report.ticks, 30);
    assert!(!report.faltered, "the output clock never falters");

    // A dead heartbeat task installs nothing and removes nothing.
    assert!(
        store.status().is_none(),
        "the store is unchanged by the dead heartbeat task"
    );
}

/// A partitioned (always-erroring) heartbeat loop, running CONCURRENTLY with the
/// output clock for the whole run, cannot slow it or change the store.
#[tokio::test]
async fn a_partitioned_heartbeat_loop_runs_alongside_the_clock_without_effect() {
    let store = Arc::new(LeaseStore::new());

    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // A heartbeat loop that errors every cycle (partition), churning the whole
    // time the engine runs. It can only ever WITHHOLD a lease — never install or
    // remove one.
    let server = Arc::new(HostileServer::partitioning());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang under heartbeat churn")
    .expect("the engine drives regardless");
    assert_eq!(
        report.frames, 50,
        "one frame per tick under heartbeat churn"
    );
    assert!(!report.faltered, "the output clock never falters");

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "a partitioned heartbeat never changes the store (withholds, never tightens)"
    );
}

/// THE IN-FLIGHT STALL GATE: the output clock keeps ticking WHILE a heartbeat
/// call is genuinely BLACK-HOLED in flight (not aborted, not erroring — parked
/// forever mid-call). This is the strongest invariant-#10 proof: a stalled
/// licence-server call cannot back-pressure the output clock.
#[tokio::test]
async fn the_output_clock_ticks_while_a_heartbeat_call_is_stalled_in_flight() {
    let store = Arc::new(LeaseStore::new());
    let mut engine = SoftwareEngine::build_gated(&small_config(), Some(EnforcementLevel::Active))
        .expect("build");

    // Start a heartbeat whose first server call black-holes forever.
    let server = Arc::new(HostileServer::stalling());
    let hb = client(Arc::clone(&server), Arc::clone(&store));
    let handle = tokio::spawn(async move { hb.run_forever().await });

    // Wait until the heartbeat call is genuinely parked in flight (not a race).
    let parked = tokio::time::timeout(Duration::from_secs(5), async {
        while !server.is_in_flight() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        parked.is_ok(),
        "the heartbeat call must reach an in-flight stall"
    );
    assert!(
        !handle.is_finished(),
        "the heartbeat is stalled (has not returned)"
    );

    // WHILE the heartbeat is stalled in flight, drive the output clock. It must
    // emit exactly one frame per tick and never falter.
    let time = Arc::new(ManualTimeSource::new());
    let report = tokio::time::timeout(
        Duration::from_secs(10),
        engine.run_for(time, CooperativePacer, 50),
    )
    .await
    .expect("the engine run must not hang while a heartbeat call is stalled")
    .expect("the engine drives regardless of an in-flight stalled heartbeat");
    assert_eq!(
        report.frames, 50,
        "one frame per tick while a heartbeat call is stalled in flight"
    );
    assert!(
        !report.faltered,
        "the output clock never falters during the stall"
    );
    assert!(
        !handle.is_finished(),
        "the heartbeat is STILL stalled while the clock ran the whole way"
    );

    handle.abort();
    let _ = handle.await;
    assert!(
        store.status().is_none(),
        "the stalled heartbeat installed/removed nothing"
    );
}
