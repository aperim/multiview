//! The safe `NdiOutput` sink seam.
//!
//! `NdiOutput` publishes the composited multiview as a single NDI source. It can
//! only be constructed with an **accepted** [`NdiLicense`] *and* an [`NdiApi`]
//! implementation — so the runtime license gate (ADR-0008 §7.5) is enforced by
//! the type system: there is no way to construct an NDI sender without acceptance.
//!
//! This is the **seam** (OUT-3 load-scaffolding): the drive loop, the
//! create-on-open, the per-frame send with tick-restamped timecode, and the
//! graceful close. The real SDK-function-table [`NdiApi`] and the NV12→UYVY
//! conversion that feeds it are OUT-4 / live-only; here the seam is exercised over
//! the deterministic [`super::api::FakeNdiApi`].

use super::api::{NdiApi, NdiSendError, NdiVideoFrame};
use super::license::NdiLicense;

/// A single-source NDI output sender, gated behind an accepted license.
///
/// Generic over the [`NdiApi`] seam so it is unit-testable over a fake table.
#[derive(Debug)]
pub struct NdiOutput<A: NdiApi> {
    api: A,
    name: String,
    // Held to prove (by construction) that the runtime license was accepted; the
    // audit record is reachable via `license()`.
    license: NdiLicense,
    open: bool,
}

impl<A: NdiApi> NdiOutput<A> {
    /// Create the NDI sender named `name`, driving the given [`NdiApi`].
    ///
    /// Requires an **accepted** [`NdiLicense`] — the only way to obtain one is
    /// through [`NdiLicense::accept`] / [`NdiLicense::from_setting`], so an
    /// unaccepted operator can never reach this constructor. On a create failure
    /// the sender is not opened and the typed error is returned (never a panic).
    ///
    /// # Errors
    /// [`NdiSendError::CreateFailed`] if the sender cannot be created.
    pub fn new(
        license: NdiLicense,
        mut api: A,
        name: impl Into<String>,
    ) -> Result<Self, NdiSendError> {
        let name = name.into();
        api.create_sender(&name)?;
        Ok(Self {
            api,
            name,
            license,
            open: true,
        })
    }

    /// The NDI source name other tools discover.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the sender is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The accepted license guard (its audit record is reachable for export).
    #[must_use]
    pub fn license(&self) -> &NdiLicense {
        &self.license
    }

    /// Push one host-memory frame to the NDI sender.
    ///
    /// The frame's timecode is expected to already be re-stamped from the tick
    /// counter (invariant #3) by the caller. A send after [`Self::close`] is a
    /// typed [`NdiSendError::Closed`], never a panic.
    ///
    /// # Errors
    /// [`NdiSendError`] if the sender is closed or the frame is invalid.
    pub fn send(&mut self, frame: &NdiVideoFrame<'_>) -> Result<(), NdiSendError> {
        if !self.open {
            return Err(NdiSendError::Closed);
        }
        self.api.send_video(frame)
    }

    /// Close the sender, destroying the SDK handle. Idempotent.
    pub fn close(&mut self) {
        if self.open {
            self.api.destroy_sender();
            self.open = false;
        }
    }

    /// Borrow the underlying API seam (for tests / introspection).
    #[must_use]
    pub fn api(&self) -> &A {
        &self.api
    }
}

impl<A: NdiApi> Drop for NdiOutput<A> {
    fn drop(&mut self) {
        // Ensure the SDK sender handle is released even if `close` was not called
        // explicitly. This is host-side cleanup only — never on the engine hot
        // path — so it cannot back-pressure the output clock (#10).
        self.close();
    }
}
