//! The native ICE / DTLS / SRTP transport, built on the sans-IO `str0m` engine
//! (feature `native`, ADR-0048 §3–§7).
//!
//! This module owns the three things ADR-0048 pins behind the `native` gate:
//!
//! * [`Session`] — one peer's `str0m` [`Rtc`](str0m::Rtc) instance wrapped in a
//!   thin, **sans-IO** shell: it never touches a socket. The driver hands it
//!   received datagrams ([`Session::handle_datagram`]) and pulls the datagrams it
//!   wants to send ([`Session::poll_transmit`]) plus its next wake
//!   ([`Session::poll_timeout`]). This is what makes the whole stack
//!   CI-testable in memory — two `Session`s complete a full ICE+DTLS handshake
//!   and exchange SRTP media through an in-process byte shuttle, no network.
//! * [`WebRtcEndpoint`] — the one process-wide endpoint owning the single
//!   dual-stack UDP socket (`[::]`, `IPV6_V6ONLY=false`, ADR-0042) and the per-run
//!   [`DtlsCert`](str0m::crypto::DtlsCert). All sessions multiplex on it.
//! * The **TURN relay candidate** wiring: the in-crate [`crate::turn`] client
//!   allocates a relay (the operator's NAT-traversal requirement) and the relayed
//!   address is registered with `str0m` as a [`Candidate::relayed`].
//!
//! ## Isolation (invariant #10)
//!
//! Nothing here is on the engine tick path. The driver never `.await`s a peer
//! (UDP send is non-blocking); media crosses bounded drop-oldest rings owned by
//! the consumer lanes. A wedged peer loses only its own session's media.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, Frequency, MediaKind as Str0mMediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

use crate::config::EndpointConfig;
use crate::error::{Result, WebRtcError};

/// The 90 kHz RTP clock for video (ADR-0048 codec matrix; invariant #3 rationals).
const VIDEO_CLOCK_HZ: Frequency = Frequency::NINETY_KHZ;

/// The 48 kHz RTP clock for Opus audio (RFC 7587; ADR-P006). Opus always rides a
/// 48 kHz RTP clock regardless of the internal sample rate.
const AUDIO_CLOCK_HZ: Frequency = Frequency::FORTY_EIGHT_KHZ;

/// The kind of media a session carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaKind {
    /// Video (H.264 / VP8, 90 kHz).
    Video,
    /// Audio (Opus, 48 kHz).
    Audio,
}

impl MediaKind {
    const fn to_str0m(self) -> Str0mMediaKind {
        match self {
            Self::Video => Str0mMediaKind::Video,
            Self::Audio => Str0mMediaKind::Audio,
        }
    }
}

/// Per-session knobs. Defaults match a self-hosted full-ICE answerer.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Whether the session offers/answers H.264 video (default `true`).
    pub enable_h264: bool,
    /// Whether the session offers/answers VP8 video (default `true`).
    pub enable_vp8: bool,
    /// Whether the session offers/answers Opus audio (default `true`).
    pub enable_opus: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            enable_h264: true,
            enable_vp8: true,
            enable_opus: true,
        }
    }
}

/// One peer's `str0m` session, driven sans-IO.
///
/// Construct with [`Session::new`]; add reachability with
/// [`Session::add_host_candidate`] / [`Session::add_relay_candidate`]; negotiate
/// with [`Session::create_offer`] + [`Session::accept_answer`] (offerer) or
/// [`Session::accept_offer`] (answerer); then pump
/// [`Session::poll_transmit`] / [`Session::handle_datagram`] /
/// [`Session::poll_timeout`].
pub struct Session {
    rtc: Rtc,
    /// The pending offer awaiting an answer (offerer side only).
    pending: Option<SdpPendingOffer>,
    /// The local media mids in add order, with their kind.
    media: Vec<(Mid, MediaKind)>,
    /// Whether ICE+DTLS have completed.
    connected: bool,
    /// Datagrams the engine wants sent, drained from a single `poll_output` pass
    /// (the proven sans-IO drive shape — never poll the engine twice per tick).
    outbound: VecDeque<(SocketAddr, Vec<u8>)>,
    /// The next wake instant from the last drive pass.
    next_timeout: Option<Instant>,
    /// Decrypted media frames surfaced by the engine, oldest first (bounded).
    received: VecDeque<ReceivedMedia>,
    /// Total decrypted media frames ever surfaced (monotonic; for assertions).
    received_total: u64,
    /// Pending keyframe requests (PLI/FIR) from the remote peer, coalesced.
    keyframe_requested: bool,
}

/// A decrypted media frame surfaced from the remote peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedMedia {
    /// The RTP payload type the frame arrived on.
    pub payload_type: u8,
    /// The decrypted frame bytes (one access unit / Opus frame).
    pub data: Vec<u8>,
}

/// The bound on buffered decrypted frames before the oldest is dropped
/// (drop-oldest, invariant #10 — a slow consumer never grows memory).
const MAX_RECEIVED_BUFFER: usize = 256;

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("connected", &self.connected)
            .field("media", &self.media)
            .field("received_buffered", &self.received.len())
            .field("received_total", &self.received_total)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Build a session with a fresh self-signed DTLS certificate, started at
    /// `now`. Full ICE (never ice-lite) per ADR-0048 §5.
    #[must_use]
    pub fn new(config: &SessionConfig, now: Instant) -> Self {
        let rtc = Rtc::builder()
            .set_ice_lite(false)
            .clear_codecs()
            .enable_h264(config.enable_h264)
            .enable_vp8(config.enable_vp8)
            .enable_opus(config.enable_opus)
            .build(now);
        Self {
            rtc,
            pending: None,
            media: Vec::new(),
            connected: false,
            outbound: VecDeque::new(),
            next_timeout: None,
            received: VecDeque::new(),
            received_total: 0,
            keyframe_requested: false,
        }
    }

    /// Drive the engine: drain `poll_output` **fully** in one pass — `Transmit`
    /// → the outbound queue, `Event` → dispatch, `Timeout` → record + stop. This
    /// is the canonical str0m sans-IO loop (ADR-0048 §7); polling the engine more
    /// than once per logical tick desynchronises ICE/DTLS, so every public driver
    /// method funnels through here.
    fn drive(&mut self, now: Instant) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(transmit)) => {
                    let bytes: Vec<u8> = transmit.contents.into();
                    self.outbound.push_back((transmit.destination, bytes));
                }
                Ok(Output::Timeout(t)) => {
                    self.next_timeout = Some(t.max(now));
                    return;
                }
                Ok(Output::Event(event)) => self.absorb_event(event),
                Err(_e) => {
                    // A hot-path fault disconnects this session, never the engine.
                    self.rtc.disconnect();
                    self.connected = false;
                    self.next_timeout = Some(now + Duration::from_secs(1));
                    return;
                }
            }
        }
    }

    /// Register a host candidate (a locally reachable address) — IPv6-first per
    /// ADR-0042 (the caller orders v6 before v4).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if `addr` is not a valid host candidate.
    pub fn add_host_candidate(&mut self, addr: SocketAddr) -> Result<()> {
        let candidate = Candidate::host(addr, Protocol::Udp)
            .map_err(|e| WebRtcError::Transport(format!("host candidate {addr}: {e}")))?;
        self.rtc.add_local_candidate(candidate);
        Ok(())
    }

    /// Register a TURN **relay** candidate: `relayed` is the address the TURN
    /// server allocated, `local` the socket the relayed traffic egresses from.
    /// This is the operator's NAT-traversal path (ADR-0048 §5.1, the
    /// 2026-06-13 amendment that brought the in-crate TURN client into scope).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if the relay candidate is invalid.
    pub fn add_relay_candidate(&mut self, relayed: SocketAddr, local: SocketAddr) -> Result<()> {
        let candidate = Candidate::relayed(relayed, local, Protocol::Udp)
            .map_err(|e| WebRtcError::Transport(format!("relay candidate {relayed}: {e}")))?;
        self.rtc.add_local_candidate(candidate);
        Ok(())
    }

    /// Create an SDP offer adding the given sendonly media, returning the offer
    /// SDP string. The matching pending offer is stashed for
    /// [`Session::accept_answer`].
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if the change set produced no offer.
    pub fn create_offer(&mut self, kinds: &[MediaKind]) -> Result<String> {
        let mut change = self.rtc.sdp_api();
        for kind in kinds {
            let mid = change.add_media(kind.to_str0m(), Direction::SendOnly, None, None, None);
            self.media.push((mid, *kind));
        }
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| WebRtcError::Transport("offer produced no changes".to_owned()))?;
        self.pending = Some(pending);
        Ok(offer.to_sdp_string())
    }

    /// Accept a remote SDP offer and return **str0m's own complete answer SDP**
    /// (BUNDLE / mid / rtcp-mux / fmtp; ADR-0048 §10).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::MalformedSdp`] if the offer does not parse;
    /// [`WebRtcError::Transport`] if str0m rejects it.
    pub fn accept_offer(&mut self, offer_sdp: &str) -> Result<String> {
        let offer = SdpOffer::from_sdp_string(offer_sdp)
            .map_err(|_e| WebRtcError::MalformedSdp("offer SDP did not parse"))?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|e| WebRtcError::Transport(format!("accept_offer: {e}")))?;
        Ok(answer.to_sdp_string())
    }

    /// Apply the remote answer to a pending offer (offerer side).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if there is no pending offer or str0m rejects
    /// the answer; [`WebRtcError::MalformedSdp`] if the answer does not parse.
    pub fn accept_answer(&mut self, answer_sdp: &str) -> Result<()> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| WebRtcError::Transport("no pending offer for answer".to_owned()))?;
        let answer = SdpAnswer::from_sdp_string(answer_sdp)
            .map_err(|_e| WebRtcError::MalformedSdp("answer SDP did not parse"))?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| WebRtcError::Transport(format!("accept_answer: {e}")))
    }

    /// Whether ICE and DTLS have completed (the session can carry media).
    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Whether the session's `Rtc` is still alive (not disconnected/failed).
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.rtc.is_alive()
    }

    /// Pull the next datagram the session wants to send. Returns
    /// `(destination, payload)` or `None` when the outbound queue is empty.
    ///
    /// This is the sans-IO send side: the driver sends `payload` to
    /// `destination` over the shared UDP socket and loops until `None`. Datagrams
    /// are produced by the single [`Session::drive`] pass that
    /// [`Session::handle_datagram`] / [`Session::handle_timeout`] run, so the
    /// engine is never polled twice per tick.
    #[must_use = "the returned datagram must actually be sent"]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        let _ = now;
        self.outbound.pop_front()
    }

    /// The next instant the session wants to be polled (its earliest timer). The
    /// driver sleeps until then or until a datagram arrives, whichever first.
    #[must_use]
    pub fn poll_timeout(&mut self, now: Instant) -> Instant {
        self.next_timeout
            .unwrap_or_else(|| now + Duration::from_secs(1))
    }

    /// Feed a received datagram into the session.
    ///
    /// `source` is the remote address the datagram came from, `destination` the
    /// local socket address it arrived on (the dual-stack socket's local addr).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if str0m rejects the input. A non-STUN datagram
    /// from an unknown peer is ignored (returns `Ok(())`).
    pub fn handle_datagram(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) -> Result<()> {
        let receive = match Receive::new(Protocol::Udp, source, destination, payload) {
            Ok(r) => r,
            // A datagram str0m can't even parse as STUN/DTLS/RTP is not for us.
            Err(_e) => return Ok(()),
        };
        let input = Input::Receive(now, receive);
        if !self.rtc.accepts(&input) {
            // Not for this session (ufrag/peer demux miss) — silently ignore.
            return Ok(());
        }
        self.rtc
            .handle_input(input)
            .map_err(|e| WebRtcError::Transport(format!("handle_input: {e}")))?;
        self.drive(now);
        Ok(())
    }

    /// Advance the session's timers without any network input (the driver calls
    /// this when a timeout fires with no datagram pending).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if str0m rejects the timeout input.
    pub fn handle_timeout(&mut self, now: Instant) -> Result<()> {
        self.rtc
            .handle_input(Input::Timeout(now))
            .map_err(|e| WebRtcError::Transport(format!("timeout: {e}")))?;
        self.drive(now);
        Ok(())
    }

    /// Write one encoded video sample (an access unit) into the first video
    /// media; str0m packetizes it into SRTP.
    ///
    /// `rtp_timestamp` is the sample's presentation time in **90 kHz** RTP units
    /// (invariant #3: exact rationals, never float fps) — every distinct sample
    /// carries its own advancing timestamp, never a pinned 0. `keyframe` records
    /// that the access unit begins an IDR; str0m's H.264/VP8 packetizers derive
    /// the actual keyframe marking from the NAL/VP8 payload itself, so the flag is
    /// informational at this seam (the egress loop uses it to gate first-packet
    /// delivery on a sync point).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if there is no video media, no negotiated
    /// payload type, or the write fails.
    pub fn write_video_sample(
        &mut self,
        data: &[u8],
        keyframe: bool,
        rtp_timestamp: u32,
        now: Instant,
    ) -> Result<()> {
        let _ = keyframe;
        self.write_sample(MediaKind::Video, data, u64::from(rtp_timestamp), now)
    }

    /// Write one encoded **Opus** audio frame into the first audio media; str0m
    /// packetizes it into SRTP. `rtp_timestamp` is in **48 kHz** RTP units (RFC
    /// 7587 fixes the Opus RTP clock at 48 kHz regardless of the internal sample
    /// rate; ADR-P006).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Transport`] if there is no audio media, no negotiated
    /// payload type, or the write fails.
    pub fn write_audio_sample(
        &mut self,
        data: &[u8],
        rtp_timestamp: u32,
        now: Instant,
    ) -> Result<()> {
        self.write_sample(MediaKind::Audio, data, u64::from(rtp_timestamp), now)
    }

    /// Common write path for both media kinds: resolve the mid + negotiated PT,
    /// re-stamp the RTP time on the kind's clock, hand the access unit to str0m's
    /// sample-mode writer, and drive one packetization pass.
    fn write_sample(
        &mut self,
        kind: MediaKind,
        data: &[u8],
        rtp_timestamp: u64,
        now: Instant,
    ) -> Result<()> {
        let mid = self
            .media
            .iter()
            .find(|(_, k)| *k == kind)
            .map(|(mid, _)| *mid)
            .ok_or_else(|| WebRtcError::Transport(format!("no {kind:?} media to write")))?;
        let clock = match kind {
            MediaKind::Video => VIDEO_CLOCK_HZ,
            MediaKind::Audio => AUDIO_CLOCK_HZ,
        };
        let rtp_time = MediaTime::new(rtp_timestamp, clock);
        let pt = self
            .rtc
            .media(mid)
            .and_then(|m| m.remote_pts().first().copied())
            .ok_or_else(|| WebRtcError::Transport("no negotiated payload type".to_owned()))?;
        let writer = self
            .rtc
            .writer(mid)
            .ok_or_else(|| WebRtcError::Transport("no writer for mid".to_owned()))?;
        writer
            .write(pt, now, rtp_time, data.to_vec())
            .map_err(|e| WebRtcError::Transport(format!("write: {e}")))?;
        // Drive so str0m packetizes the sample into outbound SRTP now.
        self.drive(now);
        Ok(())
    }

    /// Pop the oldest decrypted media frame surfaced from the remote peer.
    #[must_use]
    pub fn take_received_media(&mut self) -> Option<ReceivedMedia> {
        self.received.pop_front()
    }

    /// Count of decrypted media frames ever surfaced (monotonic).
    #[must_use]
    pub fn received_media_count(&self) -> u64 {
        self.received_total
    }

    /// Take (and clear) the coalesced keyframe-request flag — the consumer maps
    /// this to a rate-limited force-IDR toward its encoder (ADR-0048 §10).
    #[must_use]
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_requested)
    }

    /// Disconnect the session (releases the `Rtc`).
    pub fn disconnect(&mut self) {
        self.rtc.disconnect();
        self.connected = false;
    }

    fn absorb_event(&mut self, event: Event) {
        match event {
            Event::Connected => self.connected = true,
            Event::MediaData(data) => {
                self.received_total = self.received_total.saturating_add(1);
                if self.received.len() >= MAX_RECEIVED_BUFFER {
                    let _ = self.received.pop_front();
                }
                self.received.push_back(ReceivedMedia {
                    payload_type: *data.pt,
                    data: data.data,
                });
            }
            Event::KeyframeRequest(_) => self.keyframe_requested = true,
            // Other events (stats, ICE state transitions, channel data) are not
            // load-bearing for the transport core.
            _ => {}
        }
    }
}

/// The one process-wide native WebRTC endpoint (ADR-0048 §4).
///
/// Owns the single dual-stack UDP socket bound `[::]:udp_port`
/// (`IPV6_V6ONLY=false`, never `0.0.0.0`) and the per-run DTLS certificate. All
/// sessions multiplex on the socket; the driver demuxes by ICE ufrag (STUN) and
/// learned remote address (everything else).
#[derive(Debug)]
pub struct WebRtcEndpoint {
    config: EndpointConfig,
    socket: std::net::UdpSocket,
}

impl WebRtcEndpoint {
    /// Bind the single dual-stack UDP socket and create the endpoint.
    ///
    /// Binds `[::]:udp_port` with `IPV6_V6ONLY=false` so a single socket serves
    /// both IPv6 and IPv4-mapped peers (ADR-0042). `udp_port = 0` picks an
    /// ephemeral port (used by the hardware-gated tests).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Config`] if the configuration is invalid, or
    /// [`WebRtcError::Socket`] if the bind fails.
    pub fn bind(config: EndpointConfig) -> Result<Self> {
        config.validate()?;
        let bind_addr = config.bind_addr();
        let socket = bind_dual_stack(bind_addr)?;
        Ok(Self { config, socket })
    }

    /// The local address the media socket is bound to.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|source| WebRtcError::Socket {
                addr: self.config.bind_addr(),
                source,
            })
    }

    /// The endpoint configuration.
    #[must_use]
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// The gathered host candidate addresses, IPv6-first (ADR-0042): the bound
    /// socket's address plus any configured `advertised_addresses` (NAT 1:1 /
    /// Docker), with the bound port applied to bare advertised IPs.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        let local = self.local_addr()?;
        let mut out: Vec<SocketAddr> = vec![local];
        for ip in &self.config.advertised_addresses {
            out.push(SocketAddr::new(*ip, local.port()));
        }
        // IPv6-first: stable sort with v6 before v4.
        out.sort_by_key(|a| u8::from(a.is_ipv4()));
        Ok(out)
    }
}

/// Bind a UDP socket dual-stack (`IPV6_V6ONLY=false`) at `addr` (ADR-0042). The
/// addr must be IPv6 (`[::]`); IPv4-only binds are rejected by construction.
fn bind_dual_stack(addr: SocketAddr) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol as S2Protocol, Socket, Type};
    let IpAddr::V6(_) = addr.ip() else {
        return Err(WebRtcError::Config(
            "media socket must bind an IPv6 dual-stack address ([::]), never 0.0.0.0".to_owned(),
        ));
    };
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(S2Protocol::UDP))
        .map_err(|source| WebRtcError::Socket { addr, source })?;
    // Dual-stack: accept IPv4-mapped peers on the same v6 socket.
    socket
        .set_only_v6(false)
        .map_err(|source| WebRtcError::Socket { addr, source })?;
    socket
        .set_nonblocking(true)
        .map_err(|source| WebRtcError::Socket { addr, source })?;
    socket
        .bind(&addr.into())
        .map_err(|source| WebRtcError::Socket { addr, source })?;
    Ok(socket.into())
}

/// The unspecified dual-stack bind IP (`[::]`), exported for callers that build
/// a bind address without an [`EndpointConfig`].
#[must_use]
pub const fn dual_stack_unspecified() -> IpAddr {
    IpAddr::V6(Ipv6Addr::UNSPECIFIED)
}
