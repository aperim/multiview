//! WebRTC ingest: a pure, testable model of a **negotiated** media session, with
//! the ICE/DTLS/SRTP transport behind the off-by-default `webrtc` feature.
//!
//! WHIP-style WebRTC contribution negotiates its media session with an SDP
//! offer/answer exchange. Production negotiation is performed by **str0m** in the
//! `multiview-webrtc` crate (ADR-0048); this crate never parses SDP. What lives
//! here is the pure, typed **result** of that negotiation ([`NegotiatedSession`]
//! and friends) plus the gated pure depacketizer/router seam that turns
//! decrypted RTP into typed compressed media events.
//!
//! ## Feature gating (no socket here)
//!
//! The ICE agent, DTLS handshake, and SRTP depacketization need a real network
//! stack and a crypto stack; they live behind the off-by-default **`webrtc`**
//! feature (compiled only with that feature enabled), keeping the default
//! build pure-Rust, native-dep-free, and LGPL-clean. The gated half is still
//! itself pure and exhaustively tested over *injected* packets: the
//! [`transport`] module owns the application-layer `MediaEngine` seam and the
//! keyframe-gated H.264 depacketizer (RFC 6184); the [`opus`] module owns the
//! Opus depacketizer (RFC 7587); and the [`route`] module dispatches decrypted
//! RTP by negotiated payload type into typed compressed media events (ADR-T014).
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Negotiated media feeds the last-good stores like every other ingest path — it
//! is *sampled*, never *pacing*. Nothing here blocks the output clock.

#[cfg(feature = "webrtc")]
pub mod opus;
#[cfg(feature = "webrtc")]
pub mod route;
#[cfg(feature = "webrtc")]
pub mod transport;

mod sdp;

pub use sdp::{Codec, MediaKind, NegotiatedMedia, NegotiatedSession, SdpDirection};
