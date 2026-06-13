//! WHIP ingest wiring (ADR-T014) — feature `webrtc-native`.
//!
//! Connects the three already-built halves into a running ingest tile:
//!
//! * the **control** [`WhipProvider`](multiview_control::WhipProvider) seam (the
//!   route layer calls `negotiate`/`release`),
//! * the **`multiview-webrtc`** native endpoint
//!   ([`WhipEndpoint`](multiview_webrtc::transport::WhipEndpoint) /
//!   [`WhipHandle`](multiview_webrtc::transport::WhipHandle)) that terminates
//!   ICE/DTLS/SRTP and surfaces decrypted RTP on a per-session
//!   [`RtpRing`](multiview_webrtc::transport::RtpRing),
//! * the **ingest** decode loop ([`drive_webrtc`]) that pulls that ring through
//!   the pure `WebRtcProducer`, decodes H.264 → NV12 and Opus → 48 kHz PCM via
//!   `multiview-ffmpeg`, normalizes timestamps (`PtsNormalizer`/`RtpAudioRebaser`),
//!   and publishes into the source's last-good `TileStore` / `AudioStore`.
//!
//! ## The publisher rendezvous
//!
//! A configured `webrtc` source's tile exists from run start (its decode loop is
//! spawned like every other source), but the **publisher arrives later** — and
//! "reconnect" means accepting the *next* `POST`. The [`WhipRegistry`] is the
//! rendezvous: `negotiate` publishes the freshly-negotiated [`RtpRing`] into the
//! source's slot, and [`drive_webrtc`] samples the slot — riding `NO_SIGNAL`
//! until a ring appears, then pumping it until the publisher goes (the ring
//! ends), then back to waiting. One publisher per source (the `409` is enforced
//! in the endpoint).
//!
//! ## Isolation (invariants #1 / #2 / #10)
//!
//! Every hand-off is a bounded drop-oldest ring; the decode loop only ever
//! *writes* the lock-free stores and never blocks the output clock. A silent or
//! dead publisher simply stops filling the ring and the tile holds last-good →
//! `NO_SIGNAL`. Nothing here can pace or back-pressure the engine.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use multiview_control::{WhipAnswer, WhipAuth, WhipProvider, WhipReject};
use multiview_webrtc::error::WebRtcError;
use multiview_webrtc::transport::{RtpRing, WhipHandle};

/// Per-source publish authorization: the configured token + whether audio is
/// accepted (from `SourceKind::Webrtc`). A source absent from the map is not a
/// configured `webrtc` source and is refused.
#[derive(Debug, Clone)]
pub struct WebrtcSourcePolicy {
    /// The optional per-source bearer token. `None` ⇒ a Write-scope API key is
    /// required (the route resolves that into [`WhipAuth::write_key`]).
    pub token: Option<String>,
    /// Whether the publisher's Opus audio m-line is accepted (else answered
    /// inactive).
    pub audio: bool,
}

/// The shared per-source publisher rendezvous: the slot `negotiate` writes a
/// freshly-negotiated [`RtpRing`] into and [`drive_webrtc`] reads.
///
/// Lock-guarded over a short critical section neither side holds across I/O.
#[derive(Debug, Default, Clone)]
pub struct WhipRegistry {
    inner: Arc<Mutex<HashMap<String, RtpRing>>>,
}

impl WhipRegistry {
    /// A fresh empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a freshly-negotiated ring for `source_id` (replacing any prior —
    /// the prior publisher's ring is closed so its drive loop ends cleanly).
    pub fn publish(&self, source_id: &str, ring: RtpRing) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(prev) = map.insert(source_id.to_owned(), ring) {
                prev.close();
            }
        }
    }

    /// Take the current ring for `source_id`, if a publisher is connected. The
    /// drive loop calls this; once taken it owns the ring until it ends.
    #[must_use]
    pub fn take(&self, source_id: &str) -> Option<RtpRing> {
        self.inner.lock().ok().and_then(|mut m| m.remove(source_id))
    }
}

/// The cli's [`WhipProvider`] over the native [`WhipHandle`].
///
/// `negotiate` authorizes against the per-source [`WebrtcSourcePolicy`] (token
/// or Write key), drives the endpoint negotiation, and rendezvous the resulting
/// [`RtpRing`] into the [`WhipRegistry`] for the source's [`drive_webrtc`] loop.
pub struct CliWhipProvider {
    handle: WhipHandle,
    registry: WhipRegistry,
    /// Per-source publish policy (token + audio), keyed by source id.
    policies: HashMap<String, WebrtcSourcePolicy>,
}

impl std::fmt::Debug for CliWhipProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliWhipProvider")
            .field("sources", &self.policies.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl CliWhipProvider {
    /// Build a provider over `handle`, with the `policies` for every configured
    /// `webrtc` source and the shared `registry` the drive loops read.
    #[must_use]
    pub fn new(
        handle: WhipHandle,
        registry: WhipRegistry,
        policies: HashMap<String, WebrtcSourcePolicy>,
    ) -> Self {
        Self {
            handle,
            registry,
            policies,
        }
    }

    /// Authorize a publish against the source policy: the per-source token match
    /// or a Write API key. Returns the policy on success.
    fn authorize<'p>(
        policy: &'p WebrtcSourcePolicy,
        auth: &WhipAuth,
    ) -> Result<&'p WebrtcSourcePolicy, WhipReject> {
        let token_ok = match &policy.token {
            Some(token) => auth.bearer.as_deref() == Some(token.as_str()),
            None => false,
        };
        if auth.write_key || token_ok {
            Ok(policy)
        } else if auth.bearer.is_none() {
            Err(WhipReject::Unauthorized)
        } else {
            Err(WhipReject::Forbidden)
        }
    }

    /// Map a crate [`WebRtcError`] from the endpoint onto a [`WhipReject`].
    fn map_endpoint_error(err: &WebRtcError) -> WhipReject {
        match err {
            WebRtcError::MalformedSdp(detail) => WhipReject::Malformed((*detail).to_owned()),
            WebRtcError::NoCompatibleCodec => WhipReject::NoCompatibleCodec,
            WebRtcError::PublisherConflict(_) => WhipReject::Conflict,
            // Capacity, config (no reachable candidate), transport, socket, turn:
            // the publisher cannot be admitted right now — shed honestly.
            _ => WhipReject::Unavailable,
        }
    }
}

impl WhipProvider for CliWhipProvider {
    fn negotiate(
        &self,
        source_id: &str,
        offer: &str,
        auth: &WhipAuth,
    ) -> Result<WhipAnswer, WhipReject> {
        // Only a configured `webrtc` source can be published to.
        let Some(policy) = self.policies.get(source_id) else {
            // Not a webrtc source — never anonymous, so a missing/insufficient
            // credential is the dominant signal; otherwise it is simply not a
            // publish target. Map to Unauthorized when no credential, else
            // Forbidden (a valid credential for a non-existent publish target).
            return Err(if auth.bearer.is_none() && !auth.write_key {
                WhipReject::Unauthorized
            } else {
                WhipReject::Forbidden
            });
        };
        let policy = Self::authorize(policy, auth)?;

        let negotiated = self
            .handle
            .negotiate(source_id, offer, policy.audio)
            .map_err(|e| Self::map_endpoint_error(&e))?;

        // Rendezvous the ring to the source's drive loop, then answer.
        self.registry.publish(source_id, negotiated.ring);
        Ok(WhipAnswer {
            session_id: negotiated.session_id.as_str().to_owned(),
            sdp: negotiated.answer_sdp,
        })
    }

    fn release(&self, source_id: &str, session_id: &str, _auth: &WhipAuth) -> bool {
        self.handle.release(source_id, session_id)
    }

    fn active_sessions(&self) -> usize {
        self.handle.live_publisher_count()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use multiview_webrtc::transport::ReceivedRtp;

    fn policy(token: Option<&str>) -> WebrtcSourcePolicy {
        WebrtcSourcePolicy {
            token: token.map(str::to_owned),
            audio: true,
        }
    }

    #[test]
    fn authorize_accepts_token_or_write_key() {
        let p = policy(Some("s3cret"));
        // Matching token.
        assert!(CliWhipProvider::authorize(
            &p,
            &WhipAuth {
                bearer: Some("s3cret".to_owned()),
                write_key: false
            }
        )
        .is_ok());
        // Write key, any/no bearer.
        assert!(CliWhipProvider::authorize(
            &p,
            &WhipAuth {
                bearer: None,
                write_key: true
            }
        )
        .is_ok());
        // No credential -> 401.
        assert!(matches!(
            CliWhipProvider::authorize(&p, &WhipAuth::default()),
            Err(WhipReject::Unauthorized)
        ));
        // Wrong token, not a write key -> 403.
        assert!(matches!(
            CliWhipProvider::authorize(
                &p,
                &WhipAuth {
                    bearer: Some("nope".to_owned()),
                    write_key: false
                }
            ),
            Err(WhipReject::Forbidden)
        ));
    }

    #[test]
    fn token_less_source_requires_write_key() {
        let p = policy(None);
        // A token-less source rejects a bearer that is not a Write key.
        assert!(matches!(
            CliWhipProvider::authorize(
                &p,
                &WhipAuth {
                    bearer: Some("anything".to_owned()),
                    write_key: false
                }
            ),
            Err(WhipReject::Forbidden)
        ));
        assert!(CliWhipProvider::authorize(
            &p,
            &WhipAuth {
                bearer: Some("admin.key".to_owned()),
                write_key: true
            }
        )
        .is_ok());
    }

    #[test]
    fn registry_rendezvous_publishes_and_takes_once() {
        let reg = WhipRegistry::new();
        assert!(reg.take("cam-1").is_none(), "no publisher yet");
        let ring = RtpRing::new();
        ring.push(ReceivedRtp {
            payload_type: 96,
            sequence: 1,
            timestamp: 90_000,
            marker: true,
            ssrc: 7,
            payload: vec![0x65, 0x10],
        });
        reg.publish("cam-1", ring);
        let taken = reg.take("cam-1").expect("the drive loop takes the ring");
        assert_eq!(
            taken.len(),
            1,
            "the taken ring carries the published packet"
        );
        // Taken once: a second take finds nothing until the next publish.
        assert!(reg.take("cam-1").is_none());
    }

    #[test]
    fn registry_replacing_a_ring_closes_the_prior() {
        let reg = WhipRegistry::new();
        let first = RtpRing::new();
        reg.publish("cam-1", first.clone());
        // A new publisher replaces the slot; the prior ring is closed so its
        // (now-orphaned) drive loop ends cleanly rather than leaking.
        let second = RtpRing::new();
        reg.publish("cam-1", second);
        assert!(
            first.is_ended(),
            "the replaced ring is closed (drained EOS)"
        );
    }

    #[test]
    fn map_endpoint_error_covers_the_signalling_rows() {
        assert!(matches!(
            CliWhipProvider::map_endpoint_error(&WebRtcError::NoCompatibleCodec),
            WhipReject::NoCompatibleCodec
        ));
        assert!(matches!(
            CliWhipProvider::map_endpoint_error(&WebRtcError::PublisherConflict("x".to_owned())),
            WhipReject::Conflict
        ));
        assert!(matches!(
            CliWhipProvider::map_endpoint_error(&WebRtcError::AtCapacity),
            WhipReject::Unavailable
        ));
        assert!(matches!(
            CliWhipProvider::map_endpoint_error(&WebRtcError::MalformedSdp("bad")),
            WhipReject::Malformed(_)
        ));
    }
}
