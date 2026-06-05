//! # multiview-preview
//!
//! Preview taps (input / program / output) and the **isolation contract** for
//! Multiview's best-effort, read-only preview side-channel. The library target is
//! `multiview_preview`.
//!
//! Preview lets an operator watch any individual source, the composed program
//! canvas, or any real encoded output — and it is paid for **only while someone
//! is watching**. It NEVER inserts itself on the protected output path
//! (invariant #10): every tap reads frames that already exist via the engine's
//! lock-free, drop-oldest broadcast channels, so a slow, stalled, or absent
//! preview consumer can never back-pressure the decode / composite / encode /
//! mux path. See [`docs/research/preview-subsystem.md`] and ADR-P001..P005.
//!
//! [`docs/research/preview-subsystem.md`]: https://github.com/aperim/multiview/blob/main/docs/research/preview-subsystem.md
//!
//! ## What this crate provides (pure-Rust default build)
//!
//! * [`TapRegistry`] — a registry of preview taps with subscriber **refcounting**,
//!   **lazy-start** on the first subscriber, and **auto-stop** on the last leave.
//!   At most one tap per entity, fanned out to many viewers; cost is ~zero when
//!   nobody is watching. Each [`TapLease`] subscribes to the engine's drop-oldest
//!   broadcast and therefore cannot stall the engine.
//! * [`TokenIssuer`] — short-lived **HMAC-SHA256 signed access tokens** scoped to
//!   exactly one [`TapKey`] + [`AccessScope`], with an absolute expiry. Forged,
//!   tampered, expired, or wrong-tap tokens are rejected; the MAC is verified in
//!   constant time.
//! * [`MjpegStream`] / [`Snapshot`] / [`ThumbnailPlan`] — the `multipart/
//!   x-mixed-replace` MJPEG framing model, the single-JPEG snapshot model (with a
//!   content-derived `ETag`), and the clamped fps/dimension caps that keep preview
//!   cheap. [`JpegEncoder`] is the NV12→JPEG seam; the production default
//!   [`Nv12JpegEncoder`] is a real pure-Rust encoder (NV12 → packed `YCbCr` →
//!   baseline JPEG via the dependency-free `jpeg-encoder` crate), with a
//!   [`StubJpegEncoder`] kept for codec-free framing/refcount tests.
//!
//! ## Feature flags
//!
//! * `webrtc` (off by default) — gates the `whep` WHEP/WebRTC *focus* surface:
//!   the SDP offer/answer + preview-encoder-selection logic, **plus** the
//!   [`whep::transport`] seam — the [`whep::transport::WhepTransport`] trait, the
//!   session lifecycle state machine, the transport-supplied SDP answer
//!   attributes, and the bounded **drop-oldest** [`whep::transport::SampleFeed`]
//!   the preview encoder pushes through (invariant #10). The seam is socket-free
//!   and pulls **no** native dependency, so this feature build stays pure Rust;
//!   a native (str0m) ICE/DTLS/SRTP implementation of the seam lands behind a
//!   *further* gate (it needs UDP/STUN + DTLS certificates). The default build
//!   is the MJPEG/snapshot model plus signed tokens, with no native or GPU
//!   dependency.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod encode;
pub mod error;
pub mod framing;
pub mod tap;
pub mod token;
#[cfg(feature = "webrtc")]
pub mod whep;

pub use encode::Nv12JpegEncoder;
pub use error::{Error, Result};
pub use framing::{JpegEncoder, JpegError, MjpegStream, Snapshot, StubJpegEncoder, ThumbnailPlan};
pub use tap::{TapError, TapLease, TapRegistry};
pub use token::{
    AccessScope, PreviewToken, TapKey, TapScope, TokenClaims, TokenError, TokenIssuer,
};
