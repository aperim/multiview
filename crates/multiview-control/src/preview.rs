//! The live-preview provider seam.
//!
//! The control plane serves low-rate JPEG snapshots of the composited **program**
//! and of each **input** so the web UI can show live previews. The pixels live in
//! the engine/compositor (which this crate does not depend on), so the provider
//! is a trait the binary implements: it hands back already-encoded JPEG bytes,
//! keeping `multiview-control` free of any pixel-format or codec dependency.
//!
//! Like every other engine→control read path, a provider implementation must be
//! **isolation-safe** (invariant #10): it samples a wait-free latest-frame slot
//! and the lock-free per-input stores; it never blocks the engine. Encoding runs
//! on the request task (off the output-clock loop), not on the hot path.

use std::sync::Arc;

/// Supplies JPEG snapshots of the live program and inputs for the preview API.
///
/// `quality` is the JPEG quality (1–100). Each method returns `None` when no
/// frame is available yet (the route answers `503`), so a freshly-started engine
/// or an unknown input id degrades gracefully rather than erroring.
pub trait PreviewProvider: Send + Sync {
    /// The latest composited **program** frame as JPEG, or `None` if none yet.
    fn program_jpeg(&self, quality: u8) -> Option<Vec<u8>>;

    /// The latest frame of the input `id` as JPEG, or `None` if the input is
    /// unknown or has produced no frame.
    fn input_jpeg(&self, id: &str, quality: u8) -> Option<Vec<u8>>;

    /// The ids of the inputs that can be previewed (for the UI to enumerate
    /// thumbnails). May be empty.
    fn input_ids(&self) -> Vec<String>;
}

/// The default provider used when the binary wires no live preview (e.g. the
/// in-memory test harness): every snapshot is absent.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPreview;

impl PreviewProvider for NoPreview {
    fn program_jpeg(&self, _quality: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_jpeg(&self, _id: &str, _quality: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A shared [`PreviewProvider`] handle, as stored in [`crate::AppState`].
pub type SharedPreview = Arc<dyn PreviewProvider>;

/// The default shared provider ([`NoPreview`]).
#[must_use]
pub fn no_preview() -> SharedPreview {
    Arc::new(NoPreview)
}
