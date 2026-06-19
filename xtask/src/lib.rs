//! Library surface for the Multiview developer-automation `xtask` crate.
//!
//! The binary ([`main`](../main/index.html)) is a thin CLI dispatcher; the
//! reusable, unit-testable logic lives here: the [`iptv`] test-source selection
//! tool used by the `soak-iptv` task, and the [`soak`] acceptance-soak report
//! renderer (`soak-report` task) over the pure `multiview-telemetry::soak`
//! analyzer (DEV-C4, ADR-R012).
#![warn(missing_docs)]

pub mod iptv;
pub mod soak;
