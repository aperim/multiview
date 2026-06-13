//! The pure achieved-tier vocabulary (DEV-C3, ADR-M010): how a member's live
//! sync tier is derived and how a group's tier folds to the **weakest member**.
//!
//! ADR-M010's published tier table (S/A/B/C/D) collapses to the honest wire
//! vocabulary [`AchievedSync`] (`frame-accurate` / `bounded-skew` / `none`). A
//! member's achieved tier is its probed [`SyncCapability`] — the ceiling — then
//! **degraded** by its live [`ClockQuality`]: a frame-accurate node that is only
//! *acquiring* a lock or *free-running* cannot present the same frame index
//! everywhere, so it honestly reports bounded skew rather than over-claiming. A
//! group's achieved tier is the **weakest member's**, computed at runtime and
//! displayed immediately (never over-claimed): one unsynchronized member makes
//! the whole group unsynchronized.
//!
//! Both functions are pure and total, so they are the one source of truth the
//! control-plane status projection and the `timing.status` producer derive
//! from, and they are property-tested for the weakest-member ordering.

use crate::event::{AchievedSync, ClockQuality, SyncCapability};

/// The achieved sync tier of a single member: its probed [`SyncCapability`]
/// ceiling **degraded** by the live [`ClockQuality`] of the disciplining clock.
///
/// * [`SyncCapability::FrameAccurate`] (our display nodes) keeps frame-accuracy
///   only while the clock is [`Locked`](ClockQuality::Locked) or coasting in
///   [`Holdover`](ClockQuality::Holdover) — the affine epoch stays valid when
///   stale, so the same frame index is still presented (ADR-M010). While
///   [`Acquiring`](ClockQuality::Acquiring) a lock or running
///   [`Freerun`](ClockQuality::Freerun) (undisciplined) there is no trustworthy
///   reference, so it degrades to [`AchievedSync::BoundedSkew`] — never claimed
///   frame-accurate.
/// * [`SyncCapability::OffsetOnly`] (vendor decoders) is capped at
///   [`AchievedSync::BoundedSkew`] whatever the clock: it accepts only a fixed
///   offset trim, never frame-locking.
/// * [`SyncCapability::None`] (Cast-class) achieves [`AchievedSync::None`]
///   whatever the clock: never part of a synchronized canvas.
#[must_use]
pub fn member_achieved_tier(capability: SyncCapability, quality: ClockQuality) -> AchievedSync {
    match capability {
        SyncCapability::FrameAccurate => match quality {
            ClockQuality::Locked | ClockQuality::Holdover => AchievedSync::FrameAccurate,
            // No disciplined reference yet (acquiring) or at all (free-run):
            // honest bounded skew, never an over-claimed frame index.
            ClockQuality::Acquiring | ClockQuality::Freerun => AchievedSync::BoundedSkew,
        },
        SyncCapability::OffsetOnly => AchievedSync::BoundedSkew,
        SyncCapability::None => AchievedSync::None,
    }
}

/// The achieved sync tier of a **group**: the weakest member's tier.
///
/// [`AchievedSync`] is ordered best → worst (`FrameAccurate < BoundedSkew <
/// None`), so the weakest member is the *maximum* over the set. An empty member
/// set claims [`AchievedSync::None`] — a group with no members cannot be
/// synchronized, and under-claiming is always safe. This is the one rule that
/// must never over-claim: a single bounded or unsynchronized member drags the
/// whole group down, displayed immediately.
#[must_use]
pub fn weakest_achieved<I: IntoIterator<Item = AchievedSync>>(members: I) -> AchievedSync {
    members.into_iter().max().unwrap_or(AchievedSync::None)
}
