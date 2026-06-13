//! The sync-group **runtime** registry (DEV-C3, ADR-M010): the latest-wins
//! control-plane projection of how each presentation-sync group is *actually*
//! performing.
//!
//! A [`SyncGroup`](multiview_config::SyncGroup) is durable config (the
//! `sync-groups` resource store); this registry is the runtime *measurement* of
//! it — never persisted or exported, exactly like the device status registry.
//! For each member it tracks the live clock quality (from the C1 `timing.status`
//! / per-device clock discipline), the measured presentation skew, and a
//! per-member [`DriftMonitor`] (dwell hysteresis). From those it derives:
//!
//! * the **achieved tier = weakest member** ([`weakest_achieved`]), computed at
//!   runtime and displayed immediately, never over-claimed; and
//! * the per-member + per-group **drift-alarm** state.
//!
//! It is plain control-plane state behind a `Mutex`; the lock guards only this
//! map and is never held by the engine, so it cannot back-pressure the engine
//! (invariant #10). The `timing.status` producer reads [`all_skews`] to fill the
//! group skew lane; the status route reads [`status`] for `/sync-groups/{id}/status`.
//!
//! [`weakest_achieved`]: multiview_events::sync_tier::weakest_achieved
//! [`all_skews`]: SyncGroupRuntime::all_skews
//! [`status`]: SyncGroupRuntime::status

use std::collections::HashMap;
use std::sync::Mutex;

use multiview_config::SyncGroup;
use multiview_core::time::MediaTime;
use multiview_events::sync_tier::{member_achieved_tier, weakest_achieved};
use multiview_events::{AchievedSync, ClockQuality, SyncCapability, SyncGroupSkew};
use serde::{Deserialize, Serialize};

use super::sync_drift::{DriftHysteresis, DriftMonitor, DriftTransition};

/// One member's read-only runtime status inside a [`SyncGroupStatus`].
///
/// The `OpenAPI` schema is the separate [`SyncGroupStatusDoc`] mirror in
/// [`crate::openapi_schemas`] (enum fields render as strings there), so this
/// wire type carries no utoipa derive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncMemberStatus {
    /// The member device id.
    pub device: String,
    /// The configured per-member presentation offset trim (milliseconds, AES67
    /// link-offset semantics applied to video).
    pub offset_ms: u32,
    /// The tier this member actually achieves right now (its capability ceiling
    /// degraded by live clock quality). `None` until a clock quality is seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub achieved: Option<AchievedSync>,
    /// The member's measured presentation skew (milliseconds), where measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_skew_ms: Option<f32>,
    /// Whether this member's drift alarm is currently raised (its skew exceeded
    /// the group target past the dwell and has not yet recovered).
    pub drift_alarm: bool,
}

impl SyncMemberStatus {
    /// The achieved tier this member contributes to the weakest-member fold.
    ///
    /// A member with no clock-quality reading yet contributes [`AchievedSync::None`]:
    /// we have measured nothing, so the honest contribution is "unsynchronized"
    /// — under-claiming, never over-claiming.
    fn contributed_tier(&self) -> AchievedSync {
        self.achieved.unwrap_or(AchievedSync::None)
    }
}

/// A sync group's read-only runtime status — the projection behind
/// `GET /sync-groups/{id}/status` and the `device.sync` / `timing.status`
/// telemetry. Derived state only; never persisted.
///
/// The `OpenAPI` schema is the separate [`SyncGroupStatusDoc`] mirror in
/// [`crate::openapi_schemas`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncGroupStatus {
    /// The sync-group id.
    pub group: String,
    /// The group's configured drift-alarm threshold (milliseconds).
    pub target_skew_ms: u32,
    /// The tier the group actually achieves — the **weakest member's** tier,
    /// never over-claimed (ADR-M010).
    pub achieved: AchievedSync,
    /// The single member that limits the group's tier, named only when exactly
    /// one member sits at the weakest tier while others are strictly better. A
    /// shared weakest tier names nobody (singling one out would be arbitrary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limited_by: Option<String>,
    /// The worst measured member skew across the group (milliseconds), where any
    /// member has a measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_skew_ms: Option<f32>,
    /// Whether any member's drift alarm is currently raised.
    pub drift_alarm: bool,
    /// Each member's runtime status.
    pub members: Vec<SyncMemberStatus>,
}

/// One member's full runtime: its read-only status plus the drift state machine
/// (the machine is internal — it never serialises).
#[derive(Debug, Clone)]
struct MemberRuntime {
    status: SyncMemberStatus,
    drift: DriftMonitor,
}

/// One group's runtime: its config-derived knobs plus per-member runtimes, in
/// stable config order.
#[derive(Debug, Clone)]
struct GroupRuntime {
    target_skew_ms: u32,
    members: Vec<MemberRuntime>,
}

impl GroupRuntime {
    /// The group's achieved tier (weakest member) and the sole limiting member.
    fn achieved_and_limiter(&self) -> (AchievedSync, Option<String>) {
        let achieved = weakest_achieved(self.members.iter().map(|m| m.status.contributed_tier()));
        // Name a limiter only when exactly one member sits at the weakest tier
        // and the group has more than one member (a shared weakest names nobody).
        let at_weakest: Vec<&MemberRuntime> = self
            .members
            .iter()
            .filter(|m| m.status.contributed_tier() == achieved)
            .collect();
        let limited_by = if self.members.len() > 1 && at_weakest.len() == 1 {
            at_weakest.first().map(|m| m.status.device.clone())
        } else {
            None
        };
        (achieved, limited_by)
    }

    /// The worst (largest) measured member skew, where any member has one.
    fn worst_skew(&self) -> Option<f32> {
        self.members
            .iter()
            .filter_map(|m| m.status.measured_skew_ms)
            .fold(None, |acc, skew| match acc {
                Some(worst) if worst >= skew => Some(worst),
                _ => Some(skew),
            })
    }

    /// Build the read-only status projection for this group.
    fn status(&self, group: &str) -> SyncGroupStatus {
        let (achieved, limited_by) = self.achieved_and_limiter();
        SyncGroupStatus {
            group: group.to_owned(),
            target_skew_ms: self.target_skew_ms,
            achieved,
            limited_by,
            measured_skew_ms: self.worst_skew(),
            drift_alarm: self.members.iter().any(|m| m.status.drift_alarm),
            members: self.members.iter().map(|m| m.status.clone()).collect(),
        }
    }
}

/// The latest-wins runtime registry over every configured sync group.
///
/// Seeded from config with [`seed`](Self::seed); driven per status sample with
/// [`observe`](Self::observe); read with [`status`](Self::status) /
/// [`all_skews`](Self::all_skews). Plain control-plane state behind a `Mutex`.
#[derive(Debug)]
pub struct SyncGroupRuntime {
    hysteresis: DriftHysteresis,
    groups: Mutex<HashMap<String, GroupRuntime>>,
}

impl Default for SyncGroupRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncGroupRuntime {
    /// A fresh, empty runtime with the default drift hysteresis.
    #[must_use]
    pub fn new() -> Self {
        Self::with_hysteresis(DriftHysteresis::default())
    }

    /// A fresh, empty runtime with explicit drift hysteresis (tests pin short
    /// dwells; production uses the default).
    #[must_use]
    pub fn with_hysteresis(hysteresis: DriftHysteresis) -> Self {
        Self {
            hysteresis,
            groups: Mutex::new(HashMap::new()),
        }
    }

    /// Lock the inner map, recovering from a poisoned lock (a panic in another
    /// request must not wedge the control plane).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, GroupRuntime>> {
        match self.groups.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Seed (or re-seed) the registry from the configured groups. A re-apply
    /// **replaces** the group set: a dropped group is forgotten, a new group
    /// appears unmeasured, and a surviving group keeps its members' runtime
    /// measurements + drift state where the member still exists (config re-apply
    /// must not blank live telemetry). A member's configured `offset_ms` is
    /// always refreshed from the new config.
    pub fn seed(&self, groups: &[SyncGroup]) {
        let mut guard = self.lock();
        let mut next: HashMap<String, GroupRuntime> = HashMap::with_capacity(groups.len());
        for group in groups {
            let prior = guard.get(&group.id);
            let members = group
                .members
                .iter()
                .map(|member| {
                    // Preserve a surviving member's live runtime; refresh its
                    // configured offset from the new document.
                    let prior_member = prior
                        .and_then(|g| g.members.iter().find(|m| m.status.device == member.device));
                    match prior_member {
                        Some(existing) => {
                            let mut status = existing.status.clone();
                            status.offset_ms = member.offset_ms;
                            MemberRuntime {
                                status,
                                drift: existing.drift.clone(),
                            }
                        }
                        None => MemberRuntime {
                            status: SyncMemberStatus {
                                device: member.device.clone(),
                                offset_ms: member.offset_ms,
                                achieved: None,
                                measured_skew_ms: None,
                                drift_alarm: false,
                            },
                            drift: DriftMonitor::new(self.hysteresis),
                        },
                    }
                })
                .collect();
            next.insert(
                group.id.clone(),
                GroupRuntime {
                    target_skew_ms: group.target_skew_ms,
                    members,
                },
            );
        }
        *guard = next;
    }

    /// Fold one member observation into the runtime: its live clock quality
    /// (which sets its achieved tier via [`member_achieved_tier`]), its measured
    /// skew (`None` when unmeasured), and the current media time (driving the
    /// drift dwell). Returns the drift-alarm transition (so the caller can
    /// publish a `device.sync` drift event). A no-op returning
    /// [`DriftTransition::None`] for an unknown group or member.
    pub fn observe(
        &self,
        group: &str,
        device: &str,
        capability: SyncCapability,
        quality: ClockQuality,
        measured_skew_ms: Option<f32>,
        now: MediaTime,
    ) -> DriftTransition {
        let mut guard = self.lock();
        let Some(group_rt) = guard.get_mut(group) else {
            return DriftTransition::None;
        };
        let target = group_rt.target_skew_ms;
        let Some(member) = group_rt
            .members
            .iter_mut()
            .find(|m| m.status.device == device)
        else {
            return DriftTransition::None;
        };
        member.status.achieved = Some(member_achieved_tier(capability, quality));
        member.status.measured_skew_ms = measured_skew_ms;
        // No measurement → the drift condition is treated as absent (NaN never
        // exceeds the target), so an unmeasured member cannot raise drift.
        let skew = measured_skew_ms.unwrap_or(f32::NAN);
        let transition = member.drift.observe(skew, target, now);
        member.status.drift_alarm = member.drift.is_alarmed();
        transition
    }

    /// The read-only status projection for `group`, or `None` if it is not a
    /// configured group.
    #[must_use]
    pub fn status(&self, group: &str) -> Option<SyncGroupStatus> {
        self.lock().get(group).map(|g| g.status(group))
    }

    /// Every group's measured skew/tier summary for the `timing.status`
    /// producer, id-sorted. The achieved tier is the weakest member's; the skew
    /// is the worst measured member skew (omitted when unmeasured).
    #[must_use]
    pub fn all_skews(&self) -> Vec<SyncGroupSkew> {
        let guard = self.lock();
        let mut out: Vec<SyncGroupSkew> = guard
            .iter()
            .map(|(id, group)| {
                let (achieved, _) = group.achieved_and_limiter();
                SyncGroupSkew {
                    group: id.clone(),
                    achieved,
                    measured_skew_ms: group.worst_skew(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.group.cmp(&b.group));
        out
    }

    /// Every member's configured presentation trim as `(group, device,
    /// offset_ms)`, id-sorted by group then member config order.
    ///
    /// The binary publishes each of these as a `device.sync` `Joined { offset_ms
    /// }` Class-1 apply event when the group membership is applied, so a member
    /// node trims its presentation buffer at a frame boundary (the engine output
    /// cadence is untouched — ADR-M010). This is the control-plane side of the
    /// offset seam; the node-side consumer adds the trim to its `link_offset`.
    #[must_use]
    pub fn member_offsets(&self) -> Vec<(String, String, u32)> {
        let guard = self.lock();
        let mut group_ids: Vec<&String> = guard.keys().collect();
        group_ids.sort();
        let mut out = Vec::new();
        for group_id in group_ids {
            if let Some(group) = guard.get(group_id) {
                for member in &group.members {
                    out.push((
                        group_id.clone(),
                        member.status.device.clone(),
                        member.status.offset_ms,
                    ));
                }
            }
        }
        out
    }
}
