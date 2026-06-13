#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! The pure achieved-tier vocabulary (DEV-C3, ADR-M010): a member's achieved
//! sync tier is its probed [`SyncCapability`] **degraded** by its live clock
//! quality, and a group's achieved tier is the **weakest member's** — displayed
//! immediately and never over-claimed. These tests pin the mapping and the
//! weakest-member ordering as the wire-honest source of truth both the control
//! status projection and the `timing.status` producer derive from.

use multiview_events::sync_tier::{member_achieved_tier, weakest_achieved};
use multiview_events::{AchievedSync, ClockQuality, SyncCapability};

/// A frame-accurate node on a locked clock achieves frame-accuracy.
#[test]
fn frame_accurate_capability_locked_clock_is_frame_accurate() {
    assert_eq!(
        member_achieved_tier(SyncCapability::FrameAccurate, ClockQuality::Locked),
        AchievedSync::FrameAccurate
    );
}

/// A frame-accurate node coasting in holdover still presents the same frame
/// index from the last good epoch — frame-accurate (the affine map stays valid
/// when stale; ADR-M010).
#[test]
fn frame_accurate_capability_holdover_stays_frame_accurate() {
    assert_eq!(
        member_achieved_tier(SyncCapability::FrameAccurate, ClockQuality::Holdover),
        AchievedSync::FrameAccurate
    );
}

/// A frame-accurate node that is only *acquiring* a lock cannot yet claim the
/// same frame index — it degrades to bounded skew, never over-claimed.
#[test]
fn frame_accurate_capability_acquiring_degrades_to_bounded() {
    assert_eq!(
        member_achieved_tier(SyncCapability::FrameAccurate, ClockQuality::Acquiring),
        AchievedSync::BoundedSkew
    );
}

/// A frame-accurate node on a free-running clock has no disciplined reference:
/// it cannot be frame-accurate, only bounded — honesty over aspiration.
#[test]
fn frame_accurate_capability_freerun_degrades_to_bounded() {
    assert_eq!(
        member_achieved_tier(SyncCapability::FrameAccurate, ClockQuality::Freerun),
        AchievedSync::BoundedSkew
    );
}

/// An offset-only (vendor decoder) member is bounded-skew at best, regardless
/// of clock discipline — its capability ceiling caps the achieved tier.
#[test]
fn offset_only_capability_is_capped_at_bounded() {
    for quality in [
        ClockQuality::Locked,
        ClockQuality::Holdover,
        ClockQuality::Acquiring,
        ClockQuality::Freerun,
    ] {
        assert_eq!(
            member_achieved_tier(SyncCapability::OffsetOnly, quality),
            AchievedSync::BoundedSkew
        );
    }
}

/// A member with no sync mechanism (Cast-class) achieves nothing, whatever the
/// clock — never part of a synchronized canvas (Tier D).
#[test]
fn none_capability_is_always_none() {
    for quality in [
        ClockQuality::Locked,
        ClockQuality::Holdover,
        ClockQuality::Acquiring,
        ClockQuality::Freerun,
    ] {
        assert_eq!(
            member_achieved_tier(SyncCapability::None, quality),
            AchievedSync::None
        );
    }
}

/// The group tier is the WEAKEST member: one bounded member drags a
/// frame-accurate group down to bounded.
#[test]
fn weakest_member_drags_group_down() {
    assert_eq!(
        weakest_achieved([AchievedSync::FrameAccurate, AchievedSync::BoundedSkew]),
        AchievedSync::BoundedSkew
    );
}

/// One unsynchronized member makes the whole group unsynchronized — the most
/// load-bearing honesty rule (never over-claim).
#[test]
fn one_none_member_makes_group_none() {
    assert_eq!(
        weakest_achieved([
            AchievedSync::FrameAccurate,
            AchievedSync::FrameAccurate,
            AchievedSync::None,
        ]),
        AchievedSync::None
    );
}

/// An all-frame-accurate group claims frame-accuracy.
#[test]
fn all_frame_accurate_group_is_frame_accurate() {
    assert_eq!(
        weakest_achieved([AchievedSync::FrameAccurate, AchievedSync::FrameAccurate]),
        AchievedSync::FrameAccurate
    );
}

/// An empty member set claims nothing (a group with no members cannot be
/// synchronized) — under-claiming is always safe.
#[test]
fn empty_group_claims_none() {
    assert_eq!(weakest_achieved([]), AchievedSync::None);
}

/// The ordering is a total order best→worst: `FrameAccurate` < `BoundedSkew` <
/// `None`, so `weakest` is a pure maximum over that order.
#[test]
fn weakest_is_order_independent() {
    let forward = weakest_achieved([
        AchievedSync::FrameAccurate,
        AchievedSync::BoundedSkew,
        AchievedSync::None,
    ]);
    let reverse = weakest_achieved([
        AchievedSync::None,
        AchievedSync::BoundedSkew,
        AchievedSync::FrameAccurate,
    ]);
    assert_eq!(forward, AchievedSync::None);
    assert_eq!(reverse, AchievedSync::None);
}

use proptest::prelude::{prop_oneof, Just, Strategy};
use proptest::{prop_assert, prop_assert_eq, proptest};

/// A strategy over the three achieved tiers.
fn any_tier() -> impl Strategy<Value = AchievedSync> {
    prop_oneof![
        Just(AchievedSync::FrameAccurate),
        Just(AchievedSync::BoundedSkew),
        Just(AchievedSync::None),
    ]
}

/// A strategy over the four clock-quality states.
fn any_quality() -> impl Strategy<Value = ClockQuality> {
    prop_oneof![
        Just(ClockQuality::Locked),
        Just(ClockQuality::Holdover),
        Just(ClockQuality::Acquiring),
        Just(ClockQuality::Freerun),
    ]
}

/// A strategy over the three sync capabilities.
fn any_capability() -> impl Strategy<Value = SyncCapability> {
    prop_oneof![
        Just(SyncCapability::FrameAccurate),
        Just(SyncCapability::OffsetOnly),
        Just(SyncCapability::None),
    ]
}

proptest! {
    /// The weakest tier is never *better* than any member's tier — over-claiming
    /// is impossible by construction (the load-bearing honesty property).
    #[test]
    fn weakest_never_over_claims(tiers in proptest::collection::vec(any_tier(), 1..16)) {
        let weakest = weakest_achieved(tiers.clone());
        for tier in &tiers {
            // `>=` in the best→worst order means "at least as weak as".
            prop_assert!(weakest >= *tier);
        }
        // And the weakest tier is actually present among the members (it is the
        // maximum, not a fabricated value).
        prop_assert!(tiers.contains(&weakest));
    }

    /// Adding a member can only ever keep the group the same or make it weaker —
    /// monotone under union, never stronger.
    #[test]
    fn weakest_is_monotone_under_adding_a_member(
        tiers in proptest::collection::vec(any_tier(), 1..16),
        extra in any_tier(),
    ) {
        let before = weakest_achieved(tiers.clone());
        let mut with_extra = tiers;
        with_extra.push(extra);
        let after = weakest_achieved(with_extra);
        prop_assert!(after >= before);
    }

    /// A member never over-claims: the achieved tier is never better than the
    /// capability ceiling (frame-accurate is the only capability that can ever
    /// reach the frame-accurate tier, and only on a disciplined clock).
    #[test]
    fn member_never_exceeds_its_capability_ceiling(
        capability in any_capability(),
        quality in any_quality(),
    ) {
        let achieved = member_achieved_tier(capability, quality);
        let ceiling = match capability {
            SyncCapability::FrameAccurate => AchievedSync::FrameAccurate,
            SyncCapability::OffsetOnly => AchievedSync::BoundedSkew,
            // `SyncCapability` is `#[non_exhaustive]`; an unknown future
            // capability under-claims (the safe ceiling).
            _ => AchievedSync::None,
        };
        // `achieved` is never strictly better (smaller) than the ceiling.
        prop_assert!(achieved >= ceiling);
    }

    /// A None capability achieves None whatever the clock; a frame-accurate
    /// capability on a locked clock achieves frame-accuracy.
    #[test]
    fn member_tier_endpoints(quality in any_quality()) {
        prop_assert_eq!(
            member_achieved_tier(SyncCapability::None, quality),
            AchievedSync::None
        );
        prop_assert_eq!(
            member_achieved_tier(SyncCapability::FrameAccurate, ClockQuality::Locked),
            AchievedSync::FrameAccurate
        );
    }
}
