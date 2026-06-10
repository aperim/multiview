//! DEV-C1 (ADR-M010): the `timing.status` publication — `{stream_id, epoch,
//! link_offset_ns, clock_source, clock_quality, groups}` pushed onto the
//! engine's drop-oldest outbound broadcast (the same channel the control WS
//! pump forwards, conflated latest-wins per ADR-RT007) at ~1 Hz.
//!
//! Pins:
//! * `publish_once` publishes a correct `Event::TimingStatus` AND mirrors the
//!   epoch into the shared HLS PDT cell (one anchor, every surface agrees);
//! * the event is conflated (ring-excluded latest-wins telemetry);
//! * isolation (inv #10): a never-reading subscriber lags (drop-oldest) and
//!   can never block the publisher;
//! * the spawned task anchors lazily off the run's epoch-anchor slot and
//!   publishes without ever touching the engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::timing_status::{self, TimingStatusOptions};
use multiview_control::EngineStateSnapshot;
use multiview_engine::epoch::{
    EpochAnchor, EpochPolicy, EpochSampler, EpochSamplerConfig, WallClockSampler, WallSample,
};
use multiview_engine::sysref::{NtpQuery, NtpReading, SystemRefConfig};
use multiview_engine::{EnginePublisher, ManualTimeSource, StopSignal, TryRecvError};
use multiview_events::{ClockQuality, ClockSource, Event};
use multiview_output::SharedEpoch;

/// A deterministic wall sampler.
struct FakeWall {
    mono_mid_ns: i64,
    wall_ns: i64,
}

impl WallClockSampler for FakeWall {
    fn sample(&mut self) -> WallSample {
        WallSample {
            mono_before_ns: self.mono_mid_ns,
            wall_ns: self.wall_ns,
            mono_after_ns: self.mono_mid_ns,
        }
    }
}

struct NoNtp;

impl NtpQuery for NoNtp {
    fn read(&mut self) -> Option<NtpReading> {
        None
    }
}

fn sampler(seed_nanos: i64, wall_ns: i64) -> EpochSampler<FakeWall, NoNtp> {
    EpochSampler::new(
        seed_nanos,
        FakeWall {
            mono_mid_ns: seed_nanos,
            wall_ns,
        },
        NoNtp,
        EpochSamplerConfig {
            policy: EpochPolicy::new_default(),
            sys: SystemRefConfig::new_default(),
        },
    )
}

#[test]
fn publish_once_emits_a_correct_timing_status_and_mirrors_the_hls_epoch() {
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(8);
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let mut s = sampler(0, 1_781_049_600_000_000_000);

    let status =
        timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 150_000_000);

    let evt = sub.try_recv().expect("one published event");
    let Event::TimingStatus(ts) = &*evt.event else {
        panic!("expected timing.status, got {:?}", evt.event);
    };
    assert_eq!(ts.stream_id, "main");
    assert_eq!(ts.link_offset_ns, 150_000_000);
    assert_eq!(ts.epoch, status.epoch);
    assert_eq!(
        ts.epoch.wall_at(0),
        1_781_049_600_000_000_000,
        "tick 0 maps to the sampled wall instant (seed == mono_mid here)"
    );
    assert_eq!(ts.clock_source, ClockSource::System);
    assert_eq!(ts.clock_quality, ClockQuality::Locked);
    assert!(ts.groups.is_empty(), "no sync groups are measured yet");
    // Conflated latest-wins telemetry, excluded from the lossless replay ring.
    assert!(evt.event.is_conflated());
    // The SAME epoch feeds the HLS PDT path — one anchor, every surface agrees.
    assert_eq!(hls_epoch.get(), Some(status.epoch));
}

#[test]
fn a_stalled_subscriber_lags_and_never_blocks_the_publisher() {
    // Ring depth 2; publish 10 epochs while the subscriber never reads. The
    // publisher must complete every publish (drop-oldest), and the subscriber
    // observes a Lagged gap — never a full ring back-pressuring the producer.
    let publisher: EnginePublisher<EngineStateSnapshot, Event> = EnginePublisher::new(2);
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let mut s = sampler(0, 1_781_049_600_000_000_000);
    for _ in 0..10 {
        let _ = timing_status::publish_once(&mut s, &publisher, &hls_epoch, "main", 0);
    }
    match sub.try_recv() {
        Err(TryRecvError::Lagged(missed)) => {
            assert!(missed >= 8, "drop-oldest must have shed the backlog");
        }
        other => panic!("expected a Lagged gap, got {other:?}"),
    }
    // After the lag the subscriber resumes at the oldest retained event.
    assert!(sub.try_recv().is_ok());
}

#[tokio::test]
async fn the_spawned_task_publishes_from_the_anchor_slot() {
    let publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>> =
        Arc::new(EnginePublisher::new(8));
    let mut sub = publisher.subscribe();
    let hls_epoch = SharedEpoch::new();
    let stop = StopSignal::new();

    // The run publishes its anchor (seed + the run's monotonic source) into the
    // slot when the clock seeds; the task picks it up lazily.
    let slot = timing_status::anchor_slot();
    let time: Arc<dyn multiview_engine::TimeSource> = Arc::new(ManualTimeSource::new());
    slot.store(Some(Arc::new(EpochAnchor::new(time, 0))));

    let handle = timing_status::spawn(
        Arc::clone(&publisher),
        Arc::clone(&slot),
        hls_epoch.clone(),
        TimingStatusOptions {
            stream_id: "main".to_owned(),
            link_offset_ns: 150_000_000,
            ptp_phc: None,
        },
        stop.clone(),
    );

    // The first publication arrives within the first sampling period.
    let evt = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("a timing.status within the first period")
        .expect("subscription alive");
    let Event::TimingStatus(ts) = &*evt.event else {
        panic!("expected timing.status, got {:?}", evt.event);
    };
    assert_eq!(ts.stream_id, "main");
    assert_eq!(ts.link_offset_ns, 150_000_000);
    assert!(hls_epoch.get().is_some(), "the HLS PDT cell is fed too");

    stop.stop();
    tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("task stops on the StopSignal")
        .expect("task join");
}
