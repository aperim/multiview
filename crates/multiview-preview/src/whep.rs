//! WHEP (WebRTC-HTTP Egress Protocol) **focus-session scaffold** — gated behind
//! the off-by-default `webrtc` feature.
//!
//! Click-to-focus promotes one entity from the cheap JPEG/MJPEG grid view to a
//! single low-latency WebRTC preview encode session (preview brief §4, the
//! `POST …/preview/whep` routes). This module owns the **pure** half of that:
//!
//! * parse the browser's WHEP **SDP offer**, extract the offered video codecs;
//! * **select** the preview encode codec (prefer H.264 baseline — one cheap
//!   hardware/software encode session — then VP8);
//! * enforce that the caller holds [`AccessScope::Focus`] (the concurrent-focus
//!   cap is only enforceable when focus is granted explicitly, not implied by a
//!   view token); and
//! * emit a minimal, well-formed **SDP answer** advertising the chosen payload
//!   type as the server's send-only preview media.
//!
//! The **actual ICE / DTLS / SRTP transport** (the native `webrtc`/`str0m`
//! stack, or a `MediaMTX` sidecar) is a *separately*-gated TODO: this scaffold
//! pulls **no** native dependency, so even the `webrtc`-feature build stays pure
//! Rust, green, and deny-clean until the transport lands. See the brief's
//! "Sidecar reuse" note.
//!
//! ## Isolation (invariant #10)
//!
//! A focus session is still strictly a *preview* consumer: it reads engine taps
//! lossily and is admission-controlled + sheddable-first. Nothing here touches
//! or awaits the protected output path; this module is pure SDP/codec logic.
use crate::token::AccessScope;

/// A preview-encode video codec selected from a WHEP offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PreviewCodec {
    /// H.264 — preferred: one cheap, widely-supported low-latency encode
    /// session (`skip_frame`-light, budgeted, sheddable first).
    H264,
    /// VP8 — fallback when the offer carries no H.264 (e.g. a restricted
    /// browser build).
    Vp8,
}

impl PreviewCodec {
    /// The SDP `rtpmap` encoding name for this codec (`<name>/90000`).
    #[must_use]
    pub const fn rtpmap_name(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Vp8 => "VP8",
        }
    }
}

/// Errors from negotiating a WHEP focus session.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhepError {
    /// The offer SDP was not well-formed enough to negotiate (no `v=` line, no
    /// video `m=` line, etc.).
    #[error("malformed WHEP offer: {reason}")]
    MalformedOffer {
        /// A short, non-secret description of why the offer was rejected.
        reason: &'static str,
    },

    /// The offer carried no video codec this build can preview-encode.
    #[error("WHEP offer advertises no supported preview codec")]
    NoSupportedCodec,

    /// The caller does not hold [`AccessScope::Focus`]; a view token can never
    /// open a focus session.
    #[error("WHEP focus requires Focus access, but {held:?} was presented")]
    AccessDenied {
        /// The access scope actually presented.
        held: AccessScope,
    },
}

/// One offered video codec: its dynamic payload type and encoding name.
#[derive(Debug, Clone, Copy)]
struct OfferedCodec {
    payload_type: u16,
    codec: PreviewCodec,
}

/// A negotiated WHEP focus session: the selected codec + the SDP answer.
///
/// Constructed by [`WhepSession::negotiate`]; the realtime transport that would
/// own the ICE/DTLS/SRTP peer connection attaches to this once the native stack
/// is wired (currently a TODO behind a further gate).
#[derive(Debug, Clone)]
pub struct WhepSession {
    codec: PreviewCodec,
    payload_type: u16,
    answer: String,
}

impl WhepSession {
    /// Negotiate a focus session from a browser WHEP `offer`, requiring
    /// `access` to be [`AccessScope::Focus`].
    ///
    /// Selects the preview encode codec (H.264 preferred, then VP8) from the
    /// offer's video `m=` line and returns the session carrying a well-formed
    /// send-only SDP answer.
    ///
    /// # Errors
    ///
    /// * [`WhepError::AccessDenied`] — `access` is not `Focus`.
    /// * [`WhepError::MalformedOffer`] — the offer is not parseable SDP with a
    ///   video media section.
    /// * [`WhepError::NoSupportedCodec`] — no offered video codec is supported.
    pub fn negotiate(offer: &str, access: AccessScope) -> Result<Self, WhepError> {
        if access != AccessScope::Focus {
            return Err(WhepError::AccessDenied { held: access });
        }
        let offered = parse_video_codecs(offer)?;
        let chosen = select_codec(&offered).ok_or(WhepError::NoSupportedCodec)?;
        let answer = build_answer_sdp(chosen);
        Ok(Self {
            codec: chosen.codec,
            payload_type: chosen.payload_type,
            answer,
        })
    }

    /// The codec selected for the preview encode session.
    #[must_use]
    pub const fn codec(&self) -> PreviewCodec {
        self.codec
    }

    /// The RTP dynamic payload type the answer binds the codec to.
    #[must_use]
    pub const fn payload_type(&self) -> u16 {
        self.payload_type
    }

    /// The SDP **answer** to return to the WHEP client (`201 Created` body).
    #[must_use]
    pub fn answer_sdp(&self) -> &str {
        &self.answer
    }
}

/// Parse the video `m=` section of an SDP offer into the codecs it advertises.
///
/// Total and allocation-light. Only genuinely unparseable input (no SDP `v=`
/// version line) is [`WhepError::MalformedOffer`]; a well-formed SDP that simply
/// carries no supported video codec (e.g. audio-only) returns an **empty** list,
/// which the caller maps to [`WhepError::NoSupportedCodec`] — that distinction
/// drives the right `400` vs `415/`-style response.
fn parse_video_codecs(offer: &str) -> Result<Vec<OfferedCodec>, WhepError> {
    if !offer.contains("v=0") && !offer.starts_with("v=") {
        return Err(WhepError::MalformedOffer {
            reason: "no SDP version line",
        });
    }

    let mut in_video = false;
    let mut codecs = Vec::new();
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_video = rest.starts_with("video");
            continue;
        }
        if !in_video {
            continue;
        }
        // a=rtpmap:<pt> <NAME>/<clock>[/<channels>]
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(parsed) = parse_rtpmap(rtpmap) {
                codecs.push(parsed);
            }
        }
    }
    Ok(codecs)
}

/// Parse one `rtpmap` body (`"<pt> <NAME>/<clock>..."`) into an [`OfferedCodec`]
/// if its encoding name is one we can preview-encode.
fn parse_rtpmap(body: &str) -> Option<OfferedCodec> {
    let mut parts = body.split_whitespace();
    let pt = parts.next()?.parse::<u16>().ok()?;
    let mapping = parts.next()?;
    let name = mapping.split('/').next()?;
    let codec = match name.to_ascii_uppercase().as_str() {
        "H264" => PreviewCodec::H264,
        "VP8" => PreviewCodec::Vp8,
        _ => return None,
    };
    Some(OfferedCodec {
        payload_type: pt,
        codec,
    })
}

/// Select the preview encode codec: prefer H.264 (cheapest, most portable
/// session), else the first VP8.
fn select_codec(offered: &[OfferedCodec]) -> Option<OfferedCodec> {
    offered
        .iter()
        .find(|c| c.codec == PreviewCodec::H264)
        .or_else(|| offered.iter().find(|c| c.codec == PreviewCodec::Vp8))
        .copied()
}

/// Build a minimal, well-formed **send-only** SDP answer advertising the chosen
/// codec/payload type as the server's preview media.
fn build_answer_sdp(chosen: OfferedCodec) -> String {
    let pt = chosen.payload_type;
    let name = chosen.codec.rtpmap_name();
    // Send-only: the preview server transmits, the client receives. A real
    // transport fills in ICE ufrag/pwd, DTLS fingerprint, and the bundle/mid
    // attributes; this scaffold yields the codec-negotiation skeleton.
    format!(
        "v=0\r\n\
o=multiview-preview 0 0 IN IP4 0.0.0.0\r\n\
s=multiview-preview\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF {pt}\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:{pt} {name}/90000\r\n\
a=sendonly\r\n"
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn parse_rtpmap_ignores_unknown_codecs() {
        assert!(parse_rtpmap("111 opus/48000/2").is_none());
        let h264 = parse_rtpmap("96 H264/90000").unwrap();
        assert_eq!(h264.codec, PreviewCodec::H264);
        assert_eq!(h264.payload_type, 96);
    }

    #[test]
    fn select_prefers_h264_over_vp8() {
        let offered = [
            OfferedCodec {
                payload_type: 97,
                codec: PreviewCodec::Vp8,
            },
            OfferedCodec {
                payload_type: 96,
                codec: PreviewCodec::H264,
            },
        ];
        assert_eq!(select_codec(&offered).unwrap().codec, PreviewCodec::H264);
    }
}
