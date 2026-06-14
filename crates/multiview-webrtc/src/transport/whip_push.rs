//! The `whip_push` **output client** — **feature `native`** (ADR-0049 §5.2,
//! ADR-0048 §7).
//!
//! Multiview is the RFC 9725 **client**: it publishes the program to a remote
//! WHIP ingest. The client builds a **sendonly** offer with host candidates +
//! `a=setup:actpass` (the answerer chooses the DTLS role — RFC 5763 answerers
//! typically pick `active`, so Multiview commonly runs the DTLS **server** role),
//! `POST`s it with the configured Bearer, applies the answer, and sample-writes
//! the program AUs drained from the shared
//! [`EgressFeed`](crate::egress::EgressFeed). It is **supervised with backoff
//! reconnect** exactly like the RTMP/SRT push clients.
//!
//! ## What is pure vs live
//!
//! [`WhipPushOffer`] (the sendonly-actpass-host-candidates offer) and
//! [`PushBackoff`] (the supervised reconnect schedule) are **pure** and CI-tested
//! offline; the offer→answer→connected lifecycle and media egress are proven in
//! memory over the `Session` shuttle (`tests/egress_native.rs`). The HTTP `POST`
//! to a real WHIP origin + the live UDP socket loop ([`WhipPushClient::run`]) are
//! the live legs, hardware-gated (a real OBS/MediaMTX ingest), not run in CI.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The push client is a fan-out consumer: program AUs cross only the bounded
//! drop-oldest [`EgressFeed`], and the driver never `.await`s the remote (UDP
//! send is non-blocking). A stalled or dead WHIP target loses *its* media and
//! triggers a supervised reconnect; it can never stall the encode-once fan-out or
//! the output clock. PTS are re-stamped from the tick at encode (invariant #3).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::egress::{EgressFeed, EgressMedia};
use crate::error::{Result, WebRtcError};
use crate::transport::{MediaKind, Session, SessionConfig, WebRtcEndpoint};

/// The maximum UDP datagram the recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the push driver wakes to advance timers, drain the feed, and send.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// The maximum HTTP redirect depth the WHIP `POST` follows (ADR-0049 §5.2:
/// 307/308, https-only, depth-capped at 3).
pub const MAX_REDIRECTS: u8 = 3;

/// A `whip_push` client SDP offer: a sendonly H.264 (+ optional Opus) offer with
/// host candidates and `a=setup:actpass`, built with no socket (str0m is
/// sans-IO). The driver `POST`s [`WhipPushOffer::sdp`] to the remote WHIP origin.
#[derive(Debug)]
pub struct WhipPushOffer {
    /// The offer SDP string.
    pub sdp: String,
    /// The underlying sans-IO session awaiting the remote's answer
    /// ([`Session::accept_answer`]) — the driver owns it across the handshake.
    pub session: Session,
}

impl WhipPushOffer {
    /// Build the sendonly publish offer for `host_candidates` (IPv6-first), with
    /// an Opus `m=audio` line iff `audio`.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if a candidate is invalid or the change set
    /// produced no offer.
    pub fn create(host_candidates: &[SocketAddr], audio: bool) -> Result<Self> {
        let now = Instant::now();
        let mut session = Session::new(&SessionConfig::push(), now);
        let mut gathered = 0usize;
        for addr in host_candidates {
            if addr.ip().is_unspecified() {
                continue;
            }
            session.add_host_candidate(*addr)?;
            gathered += 1;
        }
        if gathered == 0 {
            return Err(WebRtcError::Config(
                "whip_push has no reachable host candidate: set webrtc.advertised_addresses"
                    .to_owned(),
            ));
        }
        let kinds: &[MediaKind] = if audio {
            &[MediaKind::Video, MediaKind::Audio]
        } else {
            &[MediaKind::Video]
        };
        // The push client is the offerer: sendonly + str0m's default actpass.
        let sdp = session.create_offer(kinds)?;
        Ok(Self { sdp, session })
    }
}

/// The supervised reconnect schedule for a `whip_push` client — exponential
/// backoff with a floor and a cap, reset on a successful connect (mirrors the
/// RTMP/SRT push supervision).
#[derive(Debug)]
pub struct PushBackoff {
    /// The current delay; doubles each failure, capped at [`Self::MAX_DELAY`].
    current: Duration,
}

impl PushBackoff {
    /// The initial (and floor) retry delay.
    pub const MIN_DELAY: Duration = Duration::from_millis(500);
    /// The ceiling the backoff never exceeds (a dead origin retries at most this
    /// slowly — bounded, never unbounded).
    pub const MAX_DELAY: Duration = Duration::from_secs(30);

    /// A fresh backoff at the floor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: Self::MIN_DELAY,
        }
    }

    /// The next retry delay, then double it (capped at [`Self::MAX_DELAY`]).
    #[must_use]
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current.saturating_mul(2)).min(Self::MAX_DELAY);
        delay
    }

    /// Reset to the floor after a successful connect.
    pub fn reset(&mut self) {
        self.current = Self::MIN_DELAY;
    }
}

impl Default for PushBackoff {
    fn default() -> Self {
        Self::new()
    }
}

/// The live `whip_push` client endpoint: the bound socket + the publish session.
///
/// Build with [`WhipPushClient::bind`], then drive its [`run`](Self::run) loop on
/// a tokio task. The HTTP signalling against the remote origin is performed by a
/// [`WhipSignaller`] the cli supplies (control owns the reqwest dependency, not
/// this crate); the client drives ICE/DTLS/SRTP and the program egress.
pub struct WhipPushClient {
    endpoint: WebRtcEndpoint,
    feed: EgressFeed,
    audio: bool,
}

/// The remote answer + session resource the WHIP `POST` resolved.
#[derive(Debug, Clone)]
pub struct WhipPushAnswer {
    /// The remote's SDP answer body.
    pub answer_sdp: String,
    /// The session resource URL the client `DELETE`s to stop (resolved against
    /// the post-redirect effective URL, ADR-0049 §5.2).
    pub resource_url: Option<String>,
}

/// The HTTP signalling seam the cli implements over reqwest: `POST` the offer to
/// the remote WHIP origin (Bearer, https-only 307/308 redirects preserving method
/// + headers, depth-capped) and resolve the answer + session `Location`.
///
/// Kept as a trait so this crate never grows an HTTP-client dependency (the same
/// posture as the control `WhipProvider` seam) and the lifecycle is unit-tested
/// with an in-memory shuttle.
pub trait WhipSignaller: Send + Sync {
    /// `POST` `offer_sdp` to the configured WHIP URL with the optional Bearer
    /// `token`, returning the remote's answer + session resource.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] on a network / HTTP error, an https→http
    /// downgrade, exceeding the redirect depth, or a non-`201` status.
    fn post_offer(&self, offer_sdp: &str) -> Result<WhipPushAnswer>;

    /// `DELETE` the session resource (best-effort teardown). A failure is logged,
    /// never propagated (the remote also times the session out).
    fn delete_resource(&self, resource_url: &str);
}

impl WhipPushClient {
    /// Bind the dual-stack socket and create the client over the program egress
    /// `feed`. `audio` is whether the output carries the program Opus rendition.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(
        config: crate::config::EndpointConfig,
        feed: EgressFeed,
        audio: bool,
    ) -> Result<Self> {
        let endpoint = WebRtcEndpoint::bind(config)?;
        Ok(Self {
            endpoint,
            feed,
            audio,
        })
    }

    /// The host candidates the publish offer advertises (IPv6-first).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        self.endpoint.host_candidates()
    }

    /// Run the supervised publish loop until `stop`: build a sendonly offer, hand
    /// it to `signaller` for the WHIP `POST`, apply the answer, then drive
    /// ICE/DTLS/SRTP and sample-write the program AUs drained from the feed. On a
    /// drop or signalling failure, back off (bounded) and reconnect. The live
    /// socket loop is hardware-gated; it never blocks the engine.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    pub async fn run<S: WhipSignaller>(self, signaller: S, stop: Arc<AtomicBool>) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        let host_candidates = self.endpoint.host_candidates()?;
        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;

        let mut backoff = PushBackoff::new();
        let mut buf = vec![0u8; RECV_BUFFER];

        while !stop.load(Ordering::Acquire) {
            // Build a fresh sendonly offer + negotiate with the remote origin.
            let Ok(offer) = WhipPushOffer::create(&host_candidates, self.audio) else {
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            };
            let mut session = offer.session;
            let answer = match signaller.post_offer(&offer.sdp) {
                Ok(answer) => answer,
                Err(err) => {
                    tracing::warn!(error = %err, "whip_push POST failed; backing off");
                    tokio::time::sleep(backoff.next_delay()).await;
                    continue;
                }
            };
            if session.accept_answer(&answer.answer_sdp).is_err() {
                tokio::time::sleep(backoff.next_delay()).await;
                continue;
            }
            backoff.reset();
            let mut saw_keyframe = false;
            let mut tick = tokio::time::interval(DRIVER_TICK);

            // Drive this session until ICE/DTLS dies or stop is raised.
            loop {
                if stop.load(Ordering::Acquire) {
                    if let Some(url) = &answer.resource_url {
                        signaller.delete_resource(url);
                    }
                    return Ok(());
                }
                tokio::select! {
                    recv = socket.recv_from(&mut buf) => {
                        let now = Instant::now();
                        if let Ok((len, src)) = recv {
                            if let Some(payload) = buf.get(..len) {
                                let _ = session.handle_datagram(src, local_addr, payload, now);
                            }
                        }
                        Self::drain_and_send(&socket, &mut session, &self.feed, &mut saw_keyframe, now).await;
                    }
                    _ = tick.tick() => {
                        let now = Instant::now();
                        let _ = session.handle_timeout(now);
                        Self::drain_and_send(&socket, &mut session, &self.feed, &mut saw_keyframe, now).await;
                    }
                }
                if !session.is_alive() {
                    // The remote dropped: tear the resource down (best-effort) and
                    // reconnect with backoff (supervised, like RTMP/SRT push).
                    if let Some(url) = &answer.resource_url {
                        signaller.delete_resource(url);
                    }
                    break;
                }
            }
            tokio::time::sleep(backoff.next_delay()).await;
        }
        Ok(())
    }

    /// Drain the egress feed and sample-write each program AU into the session,
    /// then flush its outbound datagrams onto the socket (non-blocking).
    async fn drain_and_send(
        socket: &tokio::net::UdpSocket,
        session: &mut Session,
        feed: &EgressFeed,
        saw_keyframe: &mut bool,
        now: Instant,
    ) {
        if session.is_connected() {
            for _ in 0..256 {
                let Some(sample) = feed.pop() else { break };
                match sample.media {
                    EgressMedia::Video => {
                        if sample.keyframe {
                            *saw_keyframe = true;
                        }
                        if !*saw_keyframe {
                            continue;
                        }
                        let _ = session.write_video_sample(
                            &sample.data,
                            sample.keyframe,
                            sample.rtp_timestamp,
                            now,
                        );
                    }
                    EgressMedia::Audio => {
                        let _ = session.write_audio_sample(&sample.data, sample.rtp_timestamp, now);
                    }
                }
            }
        }
        while let Some((dst, payload)) = session.poll_transmit(now) {
            let _ = socket.send_to(&payload, dst).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn backoff_caps_and_resets() {
        let mut b = PushBackoff::new();
        assert_eq!(b.next_delay(), PushBackoff::MIN_DELAY);
        for _ in 0..20 {
            let d = b.next_delay();
            assert!(d <= PushBackoff::MAX_DELAY);
        }
        b.reset();
        assert_eq!(b.next_delay(), PushBackoff::MIN_DELAY);
    }
}
