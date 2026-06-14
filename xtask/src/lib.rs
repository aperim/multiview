//! Library surface for the Multiview developer-automation `xtask` crate.
//!
//! The binary ([`main`](../main/index.html)) is a thin CLI dispatcher; the
//! reusable, unit-testable logic lives here: the [`iptv`] test-source selection
//! tool (`soak-iptv` task) and the [`soak`] acceptance-soak report renderer
//! (`soak-report` task).
#![warn(missing_docs)]

pub mod iptv;
pub mod soak;
