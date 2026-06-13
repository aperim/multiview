//! The conflating device broadcaster (ADR-RT007, invariant #10).
//!
//! Every device event originates here, in the **control plane** — never the
//! engine. The broadcaster is the single producer the future driver pollers
//! (DEV-A4/A5) call to publish into the engine's drop-oldest event broadcast
//! (`EnginePublisher::publish_event`, a non-blocking `broadcast::Sender::send`)
//! and to update the latest-wins [`DeviceStatusRegistry`].
//!
//! ## Isolation proof (invariant #10)
//!
//! The engine **never produces, forwards, or awaits** a device event — it only
//! owns the broadcast channel the control plane publishes into. Publishing is a
//! wait-free `send` that drops the oldest buffered event for a slow subscriber
//! and never blocks the producer; the status lane is conflated latest-wins in
//! the [`DeviceStatusRegistry`] (N devices × ~1 Hz polling never grows a queue),
//! and the lifecycle lane is bounded by the broadcast ring. So nothing on this
//! topic can back-pressure the engine, by construction — the same proof shape
//! as every other control-plane producer (alarms, tally).
//!
//! ## Conflation policy
//!
//! `device.status` (and `timing.status`) are conflated, latest-wins telemetry
//! **excluded from the lossless replay ring**; the device lifecycle events
//! (`device.adopted`/`.removed`/`.mode`/`.error`/`.sync`) and the cast-session
//! membership events (`cast.session.started`/`.removed`, DEV-D3.1) are
//! lossless. The
//! session pump applies `topic.is_high_rate() || event.is_conflated()` to
//! decide ring-exclusion per event type. This broadcaster's job is only to
//! *publish* those events with the correct types + the registry update; the
//! pump (in [`crate::realtime`]) enforces the ring rule on resume.

use std::sync::Arc;

use multiview_config::DeviceDriver;
use multiview_engine::EnginePublisher;
use multiview_events::{
    CastSessionRemoved, CastSessionStarted, DeviceAdopted, DeviceError, DeviceMode, DeviceRemoved,
    DeviceState, DeviceStatus, DeviceSync, Event, ImpactClass, ModePhase, SyncChange,
    SyncGroupTestPattern,
};

use super::registry::DeviceStatusRegistry;
use crate::state::EngineStateSnapshot;

/// Publishes device lifecycle + status events into the engine's drop-oldest
/// broadcast and keeps the latest-wins [`DeviceStatusRegistry`] current.
///
/// Cheap to clone (`Arc` handles); a driver actor holds one and is the only
/// thing that ever publishes a device event.
#[derive(Clone)]
pub struct DeviceBroadcaster {
    engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    registry: Arc<DeviceStatusRegistry>,
}

impl DeviceBroadcaster {
    /// Build a broadcaster over the engine's event publisher and the shared
    /// status registry.
    #[must_use]
    pub fn new(
        engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
        registry: Arc<DeviceStatusRegistry>,
    ) -> Self {
        Self { engine, registry }
    }

    /// The shared status registry this broadcaster updates.
    #[must_use]
    pub fn registry(&self) -> &Arc<DeviceStatusRegistry> {
        &self.registry
    }

    /// Publish `device.adopted` (lossless lifecycle) and seed the device's
    /// runtime status in `ADOPTING`. The `driver` wire string is taken from
    /// [`DeviceDriver::as_str`] — never hand-typed (ADR-RT007 guard).
    ///
    /// Returns the engine sequence number the event was published at.
    // The publish is the point; the returned seq is informational (a resume
    // cursor / test anchor), so a caller may legitimately ignore it — not a
    // `must_use` value.
    #[allow(clippy::must_use_candidate)]
    pub fn adopted(&self, device_id: &str, driver: DeviceDriver, name: Option<String>) -> u64 {
        self.registry.ensure(device_id);
        self.engine
            .publish_event(Event::DeviceAdopted(DeviceAdopted {
                device_id: device_id.to_owned(),
                driver: driver.as_str().to_owned(),
                name,
            }))
    }

    /// Publish `device.removed` (lossless lifecycle) and drop the device's
    /// runtime status.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn removed(&self, device_id: &str) -> u64 {
        self.registry.forget(device_id);
        self.engine
            .publish_event(Event::DeviceRemoved(DeviceRemoved::new(device_id)))
    }

    /// Publish a conflated `device.status` snapshot (latest-wins) for
    /// `device_id` in `state`, updating the registry first so a resuming client
    /// re-snapshots the latest value (the conflated lane is ring-excluded).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn status(&self, device_id: &str, state: DeviceState) -> u64 {
        let status = DeviceStatus::new(device_id, state);
        self.publish_status(status)
    }

    /// Publish an explicit conflated `device.status` snapshot (latest-wins).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn publish_status(&self, status: DeviceStatus) -> u64 {
        self.registry.set_status(status.clone());
        self.engine.publish_event(Event::DeviceStatus(status))
    }

    /// Publish `device.mode` with phase `Started` for a mode convergence whose
    /// device-side (DEV-class) impact was declared before apply (ADR-M009).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn mode_started(&self, device_id: &str, mode: &str) -> u64 {
        self.publish_mode(device_id, mode, ModePhase::Started)
    }

    /// Publish `device.mode` with phase `Finished` once a convergence completed
    /// (the device reached the requested mode; ADR-M009).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn mode_finished(&self, device_id: &str, mode: &str) -> u64 {
        self.publish_mode(device_id, mode, ModePhase::Finished)
    }

    /// Publish `device.mode` with phase `Failed` when a convergence could not
    /// complete (the driver re-converges per its supervision policy; ADR-M009).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn mode_failed(&self, device_id: &str, mode: &str) -> u64 {
        self.publish_mode(device_id, mode, ModePhase::Failed)
    }

    /// Publish a `device.mode` event in `phase`, carrying the DEV-class impact
    /// and the declared-impact statement (ADR-M009).
    fn publish_mode(&self, device_id: &str, mode: &str, phase: ModePhase) -> u64 {
        self.engine.publish_event(Event::DeviceMode(DeviceMode {
            device_id: device_id.to_owned(),
            mode: mode.to_owned(),
            phase,
            impact: ImpactClass::Device,
            detail: Some(mode_impact_detail(device_id, mode)),
        }))
    }

    /// Publish `device.discovered` (lossless lifecycle): one **untrusted**
    /// discovery-inventory row found by an mDNS scan (DEV-A5 / ADR-0041).
    ///
    /// This is **not** an adoption: it does not touch the status registry and
    /// carries no registry id — a discovered service is a hint requiring explicit
    /// confirm-adopt (`POST /devices/{id}`). The `driver` string is the discovered
    /// driver-kind wire token (e.g. `ndi-source`); `address` is the IPv6-first
    /// management address and `family` labels IPv4 as legacy (ADR-0042).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn discovered(
        &self,
        driver: &str,
        address: &str,
        family: multiview_events::AddressFamily,
        name: Option<String>,
    ) -> u64 {
        self.engine.publish_event(Event::DeviceDiscovered(
            multiview_events::DeviceDiscovered {
                driver: driver.to_owned(),
                address: address.to_owned(),
                family,
                name,
            },
        ))
    }

    /// Publish `device.error` (lossless lifecycle).
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn error(&self, device_id: &str, message: &str) -> u64 {
        self.engine.publish_event(Event::DeviceError(DeviceError {
            device_id: device_id.to_owned(),
            code: None,
            message: message.to_owned(),
        }))
    }

    /// Publish `device.sync` carrying a drift-alarm threshold crossing (DEV-C3):
    /// a member's measured skew moved past (`exceeded = true`, after the dwell)
    /// or back inside (`exceeded = false`) the group's target. The drift
    /// roll-up/hysteresis lives in the sync-group runtime; this is the publish.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn sync_drift(
        &self,
        device_id: &str,
        group: &str,
        measured_skew_ms: f32,
        target_skew_ms: u32,
        exceeded: bool,
    ) -> u64 {
        self.engine.publish_event(Event::DeviceSync(DeviceSync {
            device_id: device_id.to_owned(),
            group: group.to_owned(),
            change: SyncChange::Drift {
                measured_skew_ms,
                target_skew_ms,
                exceeded,
            },
        }))
    }

    /// Publish `device.sync` carrying a member's presentation-offset trim
    /// (DEV-C3): the member `device_id` joined `group` with `offset_ms` of
    /// AES67-link-offset trim. A member node consumes this to trim its
    /// presentation buffer at a frame boundary (Class-1 apply; the engine output
    /// cadence is untouched — ADR-M010). This is the control-plane side of the
    /// offset seam; the node adds the trim to its `link_offset`.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn sync_member_offset(&self, device_id: &str, group: &str, offset_ms: i64) -> u64 {
        self.engine.publish_event(Event::DeviceSync(DeviceSync {
            device_id: device_id.to_owned(),
            group: group.to_owned(),
            change: SyncChange::Joined { offset_ms },
        }))
    }

    /// Publish `sync.test-pattern` (lossless lifecycle, DEV-C3): the group's
    /// member devices should render a burnt-in frame counter + binary flash for
    /// `duration_ms` so the displays can be photographed/OCR'd for frame-accuracy
    /// verification. A pure publish — the engine program output is untouched.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn sync_test_pattern(
        &self,
        group: &str,
        duration_ms: u32,
        frame_counter: bool,
        flash_period_ms: u32,
    ) -> u64 {
        self.engine
            .publish_event(Event::SyncGroupTestPattern(SyncGroupTestPattern {
                group: group.to_owned(),
                duration_ms,
                frame_counter,
                flash_period_ms,
            }))
    }

    /// Publish `cast.session.started` (lossless lifecycle, DEV-D3.1): an
    /// ephemeral cast session joined the runtime session list. A **pure
    /// publish** — the cast routes own the status-registry seeding (`ensure`)
    /// and the session-store insert, exactly as they did before this event.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn cast_session_started(
        &self,
        session_id: &str,
        name: Option<String>,
        address: &str,
        output: &str,
    ) -> u64 {
        self.engine
            .publish_event(Event::CastSessionStarted(CastSessionStarted {
                session_id: session_id.to_owned(),
                name,
                address: address.to_owned(),
                output: output.to_owned(),
            }))
    }

    /// Publish `cast.session.removed` (lossless lifecycle, DEV-D3.1): the
    /// session was stopped, or promoted to a saved device. A pure publish —
    /// the routes own the registry/store teardown.
    #[allow(clippy::must_use_candidate)] // seq is informational; see `adopted`.
    pub fn cast_session_removed(&self, session_id: &str) -> u64 {
        self.engine
            .publish_event(Event::CastSessionRemoved(CastSessionRemoved::new(
                session_id,
            )))
    }
}

/// The human-readable DEV-class impact statement declared before a mode
/// convergence (ADR-M009): the device restarts its pipeline; Multiview program
/// output is never interrupted.
#[must_use]
pub(crate) fn mode_impact_detail(device_id: &str, mode: &str) -> String {
    format!(
        "device {device_id} restarts its pipeline to converge to {mode:?}; bound sources from \
         this device ride the tile ladder to NO_SIGNAL during the switch; no Multiview outputs \
         are affected"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use multiview_config::DeviceDriver;
    use multiview_engine::EnginePublisher;
    use multiview_events::{DeviceState, Event};

    use super::DeviceBroadcaster;
    use crate::devices::registry::DeviceStatusRegistry;
    use crate::state::EngineStateSnapshot;

    fn broadcaster() -> (
        Arc<EnginePublisher<EngineStateSnapshot, Event>>,
        DeviceBroadcaster,
    ) {
        let engine = Arc::new(EnginePublisher::new(64));
        let registry = Arc::new(DeviceStatusRegistry::new());
        let b = DeviceBroadcaster::new(Arc::clone(&engine), registry);
        (engine, b)
    }

    #[test]
    fn adopted_seeds_adopting_and_uses_the_driver_wire_form() {
        let (engine, b) = broadcaster();
        let mut sub = engine.subscribe();
        b.adopted("dev-a", DeviceDriver::Zowietek, None);
        assert_eq!(b.registry().state("dev-a"), Some(DeviceState::Adopting));
        let evt = sub.try_recv().expect("an adopted event");
        match &*evt.event {
            Event::DeviceAdopted(a) => assert_eq!(a.driver, "zowietek"),
            other => panic!("expected device.adopted, got {other:?}"),
        }
    }

    #[test]
    fn status_is_latest_wins_in_the_registry() {
        let (_engine, b) = broadcaster();
        b.status("dev-a", DeviceState::Online);
        b.status("dev-a", DeviceState::Degraded);
        assert_eq!(b.registry().state("dev-a"), Some(DeviceState::Degraded));
    }

    #[test]
    fn sync_member_offset_publishes_a_joined_change_with_the_trim() {
        use multiview_events::SyncChange;
        let (engine, b) = broadcaster();
        let mut sub = engine.subscribe();
        b.sync_member_offset("node-left", "lobby-wall", 120);
        let evt = sub.try_recv().expect("a device.sync event");
        match &*evt.event {
            Event::DeviceSync(sync) => {
                assert_eq!(sync.device_id, "node-left");
                assert_eq!(sync.group, "lobby-wall");
                assert_eq!(sync.change, SyncChange::Joined { offset_ms: 120 });
            }
            other => panic!("expected device.sync, got {other:?}"),
        }
    }

    #[test]
    fn sync_drift_publishes_a_threshold_crossing() {
        use multiview_events::SyncChange;
        let (engine, b) = broadcaster();
        let mut sub = engine.subscribe();
        b.sync_drift("node-left", "lobby-wall", 180.5, 50, true);
        let evt = sub.try_recv().expect("a device.sync event");
        match &*evt.event {
            Event::DeviceSync(sync) => match &sync.change {
                SyncChange::Drift {
                    measured_skew_ms,
                    target_skew_ms,
                    exceeded,
                } => {
                    assert!((measured_skew_ms - 180.5).abs() < f32::EPSILON);
                    assert_eq!(*target_skew_ms, 50);
                    assert!(*exceeded);
                }
                other => panic!("expected a drift change, got {other:?}"),
            },
            other => panic!("expected device.sync, got {other:?}"),
        }
    }
}
