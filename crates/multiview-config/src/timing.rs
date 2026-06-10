//! The `[timing]` block (ADR-M010): outbound presentation-timing knobs.
//!
//! One outbound `WallClockRef` epoch is published per program (DEV-C1); this
//! block carries the two deployment-level knobs that go with it:
//!
//! * **`link_offset_ms`** — the fixed receiver-side presentation delay every
//!   epoch consumer adds to `wall_at(pts)` before presenting (AES67's
//!   link-offset rule applied to video). The default ≈ 2× max network jitter +
//!   decode time; **uniformity across nodes is the goal, not smallness** — a
//!   uniform 150 ms beats a per-node-minimised spread.
//! * **`ptp_phc`** — the optional PTP Hardware Clock device path (e.g.
//!   `/dev/ptp0`) the `ptp`-feature build samples to discipline the epoch's
//!   wall estimate (ADR-T012: PTP outranks the system clock while
//!   disciplined). Ignored — with a warning — by builds without the `ptp`
//!   feature. The PHC **never paces the output clock** (invariant #1): it
//!   disciplines a published estimate only.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The inclusive upper bound for [`TimingConfig::link_offset_ms`]: beyond 10 s
/// the value is a typo, not a presentation-delay policy (the same bound
/// rationale as the sync-group member-offset cap).
pub const MAX_LINK_OFFSET_MS: u32 = 10_000;

/// `[timing]` — the outbound presentation-timing knobs (ADR-M010).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TimingConfig {
    /// The fixed receiver-side presentation delay in milliseconds, added by
    /// every epoch consumer to `wall_at(pts)` (AES67 link-offset semantics
    /// applied to video; uniformity over smallness). Default **150 ms**.
    #[serde(default = "default_link_offset_ms")]
    pub link_offset_ms: u32,
    /// Optional PTP Hardware Clock device path (e.g. `/dev/ptp0`) sampled by
    /// the `ptp`-feature build to discipline the epoch's wall estimate.
    /// `None` ⇒ the chrony/NTP-disciplined system clock is the wall source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ptp_phc: Option<String>,
}

/// The default outbound link offset (ms): inside ADR-M010's typical
/// 100–300 ms envelope.
const fn default_link_offset_ms() -> u32 {
    150
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            link_offset_ms: default_link_offset_ms(),
            ptp_phc: None,
        }
    }
}

impl TimingConfig {
    /// The link offset in integer nanoseconds (exact: `ms × 1_000_000`).
    #[must_use]
    pub fn link_offset_ns(&self) -> i64 {
        i64::from(self.link_offset_ms).saturating_mul(1_000_000)
    }

    /// Validate the block: the link offset must be within the sane bound.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when `link_offset_ms` exceeds
    /// [`MAX_LINK_OFFSET_MS`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.link_offset_ms > MAX_LINK_OFFSET_MS {
            return Err(ConfigError::Validation(format!(
                "timing: link_offset_ms ({}) exceeds the {MAX_LINK_OFFSET_MS} ms bound — \
                 beyond it the value is a typo, not a presentation-delay policy",
                self.link_offset_ms
            )));
        }
        Ok(())
    }
}
