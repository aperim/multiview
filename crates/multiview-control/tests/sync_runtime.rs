#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
//! The sync-group runtime registry (DEV-C3, ADR-M010): the latest-wins
//! control-plane projection that derives a group's **achieved tier = weakest
//! member** from each member's live clock quality, tracks per-member measured
//! skew, runs the per-member drift-alarm hysteresis, and exposes a read-only
//! status projection (like the device status registry). Pure control-plane,
//! seeded from config, never persisted/exported.

use multiview_config::SyncGroup;
use multiview_control::devices::sync_drift::{DriftHysteresis, DriftTransition};
use multiview_control::devices::sync_runtime::SyncGroupRuntime;
use multiview_core::time::MediaTime;
use multiview_events::{AchievedSync, ClockQuality, SyncCapability};
use serde_json::json;

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

/// `SyncGroup` / `SyncMember` are `#[non_exhaustive]`, so build them via serde
/// (the canonical config-as-code shape) rather than struct literals.
fn group(id: &str, target: u32, members: &[(&str, u32)]) -> SyncGroup {
    let members: Vec<serde_json::Value> = members
        .iter()
        .map(|(device, offset)| json!({ "device": device, "offset_ms": offset }))
        .collect();
    serde_json::from_value(json!({
        "id": id,
        "mode": "auto",
        "target_skew_ms": target,
        "members": members,
    }))
    .expect("a valid sync-group document")
}

/// A freshly-seeded group with no measurements yet claims `None` (honest: we
/// have measured nothing, so we claim nothing) and lists its members with their
/// configured offsets.
#[test]
fn seeded_group_with_no_measurements_claims_none() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[group("wall", 50, &[("node-l", 0), ("node-r", 100)])]);
    let status = rt.status("wall").expect("a seeded group");
    assert_eq!(status.group, "wall");
    assert_eq!(status.achieved, AchievedSync::None);
    assert_eq!(status.target_skew_ms, 50);
    assert_eq!(status.members.len(), 2);
    let left = status.members.iter().find(|m| m.device == "node-l").unwrap();
    assert_eq!(left.offset_ms, 0);
    assert!(left.measured_skew_ms.is_none());
    assert!(!left.drift_alarm);
    let right = status.members.iter().find(|m| m.device == "node-r").unwrap();
    assert_eq!(right.offset_ms, 100);
}

/// The achieved tier is the WEAKEST member: two frame-accurate locked nodes
/// claim frame-accurate.
#[test]
fn all_frame_accurate_members_make_group_frame_accurate() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[group("wall", 50, &[("node-l", 0), ("node-r", 0)])]);
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(5.0),
        MediaTime::ZERO,
    );
    rt.observe(
        "wall",
        "node-r",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(7.0),
        MediaTime::ZERO,
    );
    let status = rt.status("wall").unwrap();
    assert_eq!(status.achieved, AchievedSync::FrameAccurate);
    // The worst measured member skew is surfaced.
    assert_eq!(status.measured_skew_ms, Some(7.0));
}

/// One free-running member drags the group down: never over-claimed.
#[test]
fn one_freerun_member_caps_group_at_bounded() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[group("wall", 50, &[("node-l", 0), ("node-r", 0)])]);
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(5.0),
        MediaTime::ZERO,
    );
    rt.observe(
        "wall",
        "node-r",
        SyncCapability::FrameAccurate,
        ClockQuality::Freerun,
        Some(8.0),
        MediaTime::ZERO,
    );
    let status = rt.status("wall").unwrap();
    // node-r degrades to bounded; the group is the weakest member.
    assert_eq!(status.achieved, AchievedSync::BoundedSkew);
    let right = status.members.iter().find(|m| m.device == "node-r").unwrap();
    assert_eq!(right.achieved, Some(AchievedSync::BoundedSkew));
    // And the limiting member is named.
    assert_eq!(status.limited_by.as_deref(), Some("node-r"));
}

/// A sustained over-target member raises its drift alarm after the dwell, and
/// the transition is reported so the broadcaster can publish it.
#[test]
fn sustained_over_target_raises_member_drift_alarm() {
    let rt = SyncGroupRuntime::with_hysteresis(DriftHysteresis::new(ms(2_000), ms(3_000)));
    rt.seed(&[group("wall", 50, &[("node-l", 0)])]);
    let t0 = rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(80.0),
        MediaTime::ZERO,
    );
    assert_eq!(t0, DriftTransition::None);
    let t1 = rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(80.0),
        ms(2_000),
    );
    assert_eq!(t1, DriftTransition::Raised);
    let status = rt.status("wall").unwrap();
    let left = status.members.iter().find(|m| m.device == "node-l").unwrap();
    assert!(left.drift_alarm);
}

/// Recovery for the clear dwell clears the alarm.
#[test]
fn sustained_recovery_clears_member_drift_alarm() {
    let rt = SyncGroupRuntime::with_hysteresis(DriftHysteresis::new(ms(2_000), ms(3_000)));
    rt.seed(&[group("wall", 50, &[("node-l", 0)])]);
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(80.0),
        MediaTime::ZERO,
    );
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(80.0),
        ms(2_000),
    );
    // Recovery begins at t=2 s (first sample under target → starts clear dwell).
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(10.0),
        ms(2_000),
    );
    // 3 s of continuous recovery (dwell_down) → clears at t=5 s.
    let cleared = rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(10.0),
        ms(5_000),
    );
    assert_eq!(cleared, DriftTransition::Cleared);
    let status = rt.status("wall").unwrap();
    let left = status.members.iter().find(|m| m.device == "node-l").unwrap();
    assert!(!left.drift_alarm);
}

/// An observation for an unknown group or unknown member is a no-op (returns
/// `None` transition) — runtime state never invents groups config did not
/// declare.
#[test]
fn observation_for_unknown_group_or_member_is_a_noop() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[group("wall", 50, &[("node-l", 0)])]);
    assert_eq!(
        rt.observe(
            "no-such-group",
            "node-l",
            SyncCapability::FrameAccurate,
            ClockQuality::Locked,
            Some(5.0),
            MediaTime::ZERO,
        ),
        DriftTransition::None
    );
    assert_eq!(
        rt.observe(
            "wall",
            "no-such-member",
            SyncCapability::FrameAccurate,
            ClockQuality::Locked,
            Some(5.0),
            MediaTime::ZERO,
        ),
        DriftTransition::None
    );
    assert!(rt.status("no-such-group").is_none());
}

/// Re-seeding replaces the group set (config re-apply): a dropped group is
/// forgotten, a new one appears, and a surviving group keeps its runtime
/// measurements.
#[test]
fn reseed_replaces_groups_and_preserves_survivors() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[group("wall", 50, &[("node-l", 0)])]);
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(12.0),
        MediaTime::ZERO,
    );
    rt.seed(&[
        group("wall", 50, &[("node-l", 0)]),
        group("foyer", 80, &[("node-f", 0)]),
    ]);
    // The survivor keeps its measurement.
    let wall = rt.status("wall").unwrap();
    assert_eq!(wall.measured_skew_ms, Some(12.0));
    // The new group exists, unmeasured.
    assert!(rt.status("foyer").is_some());
    // Dropping `wall` forgets it.
    rt.seed(&[group("foyer", 80, &[("node-f", 0)])]);
    assert!(rt.status("wall").is_none());
}

/// `all_skews` summarises every group for the `timing.status` producer:
/// achieved tier + worst measured skew per group, id-sorted.
#[test]
fn all_skews_summarises_every_group_for_timing_status() {
    let rt = SyncGroupRuntime::new();
    rt.seed(&[
        group("wall", 50, &[("node-l", 0)]),
        group("foyer", 80, &[("node-f", 0)]),
    ]);
    rt.observe(
        "wall",
        "node-l",
        SyncCapability::FrameAccurate,
        ClockQuality::Locked,
        Some(9.0),
        MediaTime::ZERO,
    );
    let skews = rt.all_skews();
    assert_eq!(skews.len(), 2);
    // Id-sorted: foyer before wall.
    assert_eq!(skews[0].group, "foyer");
    assert_eq!(skews[1].group, "wall");
    assert_eq!(skews[1].achieved, AchievedSync::FrameAccurate);
    assert_eq!(skews[1].measured_skew_ms, Some(9.0));
    // The unmeasured group claims none with no skew.
    assert_eq!(skews[0].achieved, AchievedSync::None);
    assert!(skews[0].measured_skew_ms.is_none());
}
