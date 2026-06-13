//! Honest, IPv6-first SDP helpers (ADR-0048 §10, ADR-0042) and ICE candidate
//! priority ordering.
//!
//! On the native path the answer returned to a WHIP/WHEP client is **str0m's own
//! complete answer SDP** (correct BUNDLE / mid / rtcp-mux / fmtp). This module
//! owns the *fallback / fake-path* answer used by the pure (str0m-free) build and
//! by tests: it is structurally faithful — `c=IN IP6 ::` (never the
//! `c=IN IP4 0.0.0.0` placeholder the old scaffold emitted), with `a=mid` and
//! `a=rtcp-mux` — so the fake path emits the same dialect a browser sees.
//!
//! It also owns the **IPv6-first candidate priority ordering**: across families
//! IPv6 leads IPv4 at every tier; within a family host > server-reflexive > relay.
//! TURN relay candidates (the operator's NAT-traversal last resort) are *ordered*
//! lowest — never omitted.

/// The parameters needed to render an honest answer SDP for the fallback path.
#[derive(Debug, Clone)]
pub struct AnswerParams {
    /// ICE username fragment (`a=ice-ufrag`).
    pub ice_ufrag: String,
    /// ICE password (`a=ice-pwd`).
    pub ice_pwd: String,
    /// DTLS fingerprint algorithm (lower-case, e.g. `sha-256`).
    pub fingerprint_algorithm: String,
    /// DTLS fingerprint colon-hex value.
    pub fingerprint_value: String,
    /// The negotiated video payload type.
    pub video_payload_type: u8,
    /// The negotiated video codec encoding name (e.g. `H264`, `VP8`).
    pub video_codec: String,
    /// The negotiated audio payload type, if audio is carried.
    pub audio_payload_type: Option<u8>,
    /// The negotiated audio codec encoding name, if audio is carried.
    pub audio_codec: Option<String>,
}

impl AnswerParams {
    /// Render the answer SDP. Always IPv6-first (`c=IN IP6 ::`), BUNDLE,
    /// `a=mid`/`a=rtcp-mux`, `a=setup:passive` (the server is the DTLS server).
    #[must_use]
    pub fn build_sdp(&self) -> String {
        use std::fmt::Write as _;

        let mut sdp = String::new();
        // Session-level. IPv6-first per ADR-0042: origin + connection are IPv6.
        sdp.push_str("v=0\r\n");
        sdp.push_str("o=- 0 0 IN IP6 ::\r\n");
        sdp.push_str("s=-\r\n");
        sdp.push_str("t=0 0\r\n");
        // BUNDLE both media on one transport (mid 0 = video, 1 = audio).
        if self.audio_payload_type.is_some() {
            sdp.push_str("a=group:BUNDLE 0 1\r\n");
        } else {
            sdp.push_str("a=group:BUNDLE 0\r\n");
        }
        sdp.push_str("a=msid-semantic: WMS *\r\n");

        // Video m-line (mid 0). `write!` to a String is infallible.
        let _ = write!(
            sdp,
            "m=video 9 UDP/TLS/RTP/SAVPF {}\r\n",
            self.video_payload_type
        );
        sdp.push_str("c=IN IP6 ::\r\n");
        sdp.push_str("a=rtcp-mux\r\n");
        sdp.push_str("a=mid:0\r\n");
        sdp.push_str("a=sendonly\r\n");
        self.push_ice_dtls(&mut sdp);
        let _ = write!(
            sdp,
            "a=rtpmap:{} {}/90000\r\n",
            self.video_payload_type, self.video_codec
        );

        // Audio m-line (mid 1), if carried.
        if let (Some(pt), Some(codec)) = (self.audio_payload_type, self.audio_codec.as_ref()) {
            let _ = write!(sdp, "m=audio 9 UDP/TLS/RTP/SAVPF {pt}\r\n");
            sdp.push_str("c=IN IP6 ::\r\n");
            sdp.push_str("a=rtcp-mux\r\n");
            sdp.push_str("a=mid:1\r\n");
            sdp.push_str("a=sendonly\r\n");
            self.push_ice_dtls(&mut sdp);
            let _ = write!(sdp, "a=rtpmap:{pt} {codec}/48000/2\r\n");
        }
        sdp
    }

    fn push_ice_dtls(&self, sdp: &mut String) {
        use std::fmt::Write as _;

        let _ = write!(sdp, "a=ice-ufrag:{}\r\n", self.ice_ufrag);
        let _ = write!(sdp, "a=ice-pwd:{}\r\n", self.ice_pwd);
        let _ = write!(
            sdp,
            "a=fingerprint:{} {}\r\n",
            self.fingerprint_algorithm, self.fingerprint_value
        );
        // The answerer is the DTLS server (the client offered actpass / active).
        sdp.push_str("a=setup:passive\r\n");
    }
}

/// A class of ICE candidate, used for IPv6-first priority ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CandidateClass {
    /// IPv6 host candidate.
    HostV6,
    /// IPv4 host candidate.
    HostV4,
    /// IPv6 server-reflexive (STUN-discovered) candidate.
    ServerReflexiveV6,
    /// IPv4 server-reflexive candidate.
    ServerReflexiveV4,
    /// IPv6 TURN relay candidate.
    RelayV6,
    /// IPv4 TURN relay candidate.
    RelayV4,
}

impl CandidateClass {
    /// A sort key: lower sorts first (higher ICE priority). Across families IPv6
    /// leads IPv4 at each tier; within a tier host > srflx > relay.
    const fn rank(self) -> u8 {
        match self {
            Self::HostV6 => 0,
            Self::HostV4 => 1,
            Self::ServerReflexiveV6 => 2,
            Self::ServerReflexiveV4 => 3,
            Self::RelayV6 => 4,
            Self::RelayV4 => 5,
        }
    }
}

/// Order candidate classes IPv6-first (ADR-0042 / ADR-0048 §5). Every input class
/// is preserved — TURN relay candidates are ordered lowest, never dropped.
#[must_use]
pub fn candidate_priority_order(classes: &[CandidateClass]) -> Vec<CandidateClass> {
    let mut out = classes.to_vec();
    out.sort_by_key(|c| c.rank());
    out
}
