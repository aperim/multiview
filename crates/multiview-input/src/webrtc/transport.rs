//! WebRTC ingest transport — **feature `webrtc`, compile-verified only**.
//!
//! This is the thin shell that would drive a real ICE/DTLS/SRTP session from the
//! pure SDP negotiation in the parent module. It is gated behind the
//! off-by-default **`webrtc`** feature because a real session needs a network
//! stack (ICE candidate gathering, STUN/TURN), a DTLS handshake, and SRTP
//! depacketization — none of which exist in this devcontainer. The code here is
//! therefore **compile-verified only**; its correctness rests entirely on the
//! pure, fully-tested [`super::SessionDescription`] negotiator it consumes.
//!
//! It deliberately does **no** native FFI and pulls in no external WebRTC crate
//! (which would make the default build non-pure / non-LGPL-clean). The actual
//! media-engine binding is supplied by the application layer when the full
//! transport is wired up; this shell only owns the negotiated session and the
//! receive-task lifecycle, and — like every ingest path — it is *sampled*, never
//! pacing the output clock (invariants #1 / #10).

use crate::webrtc::NegotiatedSession;

/// A WebRTC receive session shell.
///
/// Holds the negotiated audio/video sections (from [`super::SessionDescription::negotiate_answer`])
/// that a concrete media engine would bind sockets and SRTP contexts to. The
/// engine binding itself is provided by the application layer; this type exists
/// so the gated build has a compilable surface and a place to attach the
/// pure-negotiated result.
#[derive(Debug, Clone)]
pub struct WebRtcSession {
    negotiated: NegotiatedSession,
}

impl WebRtcSession {
    /// Construct a session shell around a negotiated answer.
    #[must_use]
    pub fn new(negotiated: NegotiatedSession) -> Self {
        Self { negotiated }
    }

    /// The negotiated media sections this session will receive.
    #[must_use]
    pub fn negotiated(&self) -> &NegotiatedSession {
        &self.negotiated
    }
}
