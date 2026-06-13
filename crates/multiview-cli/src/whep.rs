//! The engine-backed **live WHEP preview egress** provider (ADR-P006), gated
//! behind the cli `webrtc-native` feature.
//!
//! This wires the native `multiview_webrtc::whep_egress::WhepEgress` transport
//! (the single str0m owner, ADR-0048) into the control plane's
//! [`multiview_control::WhepProvider`] seam so a browser can WHEP-play a live
//! preview tap over real DTLS/SRTP, with audio, on all scopes:
//!
//! * **program** — sampled from the wait-free [`ProgramSlot`] the engine loop
//!   publishes the composited NV12 canvas into (a *pre-encode canvas approx*,
//!   ADR-P005), encoded with a real low-latency H.264 (preferred) or VP8 encoder;
//! * **input** — sampled from the per-source last-good [`TileStore`] the JPEG
//!   preview already reads; and
//! * **output** — the same sampling seam (a canvas-approx tap of the rendition's
//!   source frame); the real-encoded-bitstream fan-out tap is the ADR-P006 PRV-5b
//!   upgrade, not required for the live egress to function.
//!
//! Optional **Opus audio** rides the same seam when the offer negotiates it: a
//! per-session [`OpusEncoder`] fed the program audio bus.
//!
//! ## Isolation (invariant #10 — the cardinal preview rule)
//!
//! Every media source here **samples** a wait-free latest-frame slot / lock-free
//! store; it never blocks, paces, or back-pressures the engine. The encode +
//! egress run on a dedicated driver task that owns the shared UDP socket and only
//! ever pushes into bounded **drop-oldest** `SampleFeed`s — a stalled or absent
//! browser merely loses the oldest samples and can never stall the program path
//! or other sessions. The driver never `.await`s a client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::Nv12Image;
use multiview_control::{WhepAnswer, WhepProvider, WhepReject, WhepScope};
use multiview_core::time::MediaTime;
use multiview_engine::{MonotonicTimeSource, TimeSource};
use multiview_ffmpeg::codec::VideoCodec;
use multiview_ffmpeg::encode::{VideoEncodeTarget, VideoEncoder};
use multiview_ffmpeg::encode_options::{
    preview_gop_frames, preview_h264_options, preview_vp8_options,
};
use multiview_preview::whep::transport::{
    sample_feed, EncodedSample, PreviewMediaSource, SampleFeed, SampleKind, SampleSink, SessionId,
    WhepTransport,
};
use multiview_preview::whep::PreviewCodec;
use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::WebRtcEndpoint;
use multiview_webrtc::turn::TurnRelayDriver;
use multiview_webrtc::whep_egress::WhepEgress;

use crate::live_sources::SharedStores;
use crate::preview::ProgramSlot;

/// The preview egress frame rate cap (ADR-P006: ≤ 15 fps).
const PREVIEW_FPS: u32 = 15;
/// The longest-edge cap for the preview encode (ADR-P006: ≤ 1280).
const PREVIEW_LONGEST_EDGE: u32 = 1280;
/// The preview video bitrate budget (bits/sec).
const PREVIEW_BITRATE: usize = 1_500_000;
/// The 90 kHz video RTP clock tick per preview frame (90000 / 15).
const VIDEO_TS_STEP: u32 = 90_000 / PREVIEW_FPS;
/// The driver tick: ≤ preview cadence, so a session pumps at least per frame and
/// ICE/DTLS timers advance promptly.
const DRIVER_TICK: Duration = Duration::from_millis(20);

/// A real preview video encoder (H.264 preferred, VP8 fallback) behind a mutex so
/// the `&self` `PreviewMediaSource` pump can drive its `&mut`-only libav context.
///
/// Lazily opened on the first frame (when the frame geometry is known), scaled to
/// the preview longest-edge cap and re-stamped on the 90 kHz RTP clock.
struct PreviewVideoEncoder {
    codec: PreviewCodec,
    inner: Mutex<Option<VideoEncoder>>,
    /// The next 90 kHz RTP timestamp to stamp (monotonic; one step per frame).
    rtp_ts: AtomicU32,
    /// Whether the first emitted access unit (the keyframe) has been produced.
    first_done: AtomicBool,
}

impl PreviewVideoEncoder {
    fn new(codec: PreviewCodec) -> Self {
        Self {
            codec,
            inner: Mutex::new(None),
            rtp_ts: AtomicU32::new(0),
            first_done: AtomicBool::new(false),
        }
    }

    /// The scaled (longest-edge-capped, even) preview dimensions for `image`.
    ///
    /// Integer-only scaling (the workspace bans silent `as` conversions): scale
    /// each dimension by `PREVIEW_LONGEST_EDGE / longest` using `u64` math, then
    /// floor to an even value (NV12 chroma) with a floor of 2.
    fn target_dims(image: &Nv12Image) -> (u32, u32) {
        let (w, h) = (image.width().max(2), image.height().max(2));
        let longest = w.max(h);
        if longest <= PREVIEW_LONGEST_EDGE {
            return (w & !1, h & !1);
        }
        let cap = u64::from(PREVIEW_LONGEST_EDGE);
        let long64 = u64::from(longest).max(1);
        let scale = |v: u32| -> u32 {
            let scaled = u64::from(v).saturating_mul(cap) / long64;
            let clamped = u32::try_from(scaled).unwrap_or(PREVIEW_LONGEST_EDGE);
            (clamped.max(2)) & !1
        };
        (scale(w), scale(h))
    }

    /// Encode one NV12 frame into an [`EncodedSample`] (or `None` if the encoder
    /// buffered it). The frame is fed at its native size; the encoder's configured
    /// output size handles the scale. Best-effort: an encode fault yields `None`
    /// (preview never propagates a failure).
    fn encode(&self, image: &Nv12Image) -> Option<EncodedSample> {
        let Ok(mut guard) = self.inner.lock() else {
            return None;
        };
        if guard.is_none() {
            *guard = self.open(image);
        }
        let encoder = guard.as_mut()?;
        let video = nv12_to_video(image).ok()?;
        encoder.send_frame(&video).ok()?;
        let packet = encoder.receive_packet().ok()??;
        let data = packet.data()?;
        let first = !self.first_done.swap(true, Ordering::AcqRel);
        let ts = self.rtp_ts.fetch_add(VIDEO_TS_STEP, Ordering::AcqRel);
        Some(EncodedSample {
            data: Arc::from(data),
            rtp_timestamp: ts,
            keyframe: first || packet.is_key(),
            kind: SampleKind::Video,
        })
    }

    /// Open the libav encoder for `image`'s (scaled) geometry with the fixed
    /// low-latency preview profile (zerolatency, B-frames off, repeat-headers,
    /// 2 s GOP — ADR-P006), choosing a runtime-resolved encoder.
    fn open(&self, image: &Nv12Image) -> Option<VideoEncoder> {
        use ffmpeg_next::format::Pixel;
        use multiview_core::time::Rational;
        let (w, h) = Self::target_dims(image);
        // The preview cadence as an exact rational (invariant #3: never float fps).
        let fps = Rational::new(i64::from(PREVIEW_FPS), 1);
        let (encoder_name, options) = match self.codec {
            PreviewCodec::H264 => {
                let name = multiview_ffmpeg::codec::select_encoder(VideoCodec::H264)?;
                (name, preview_h264_options(name, fps))
            }
            PreviewCodec::Vp8 => {
                let name = multiview_ffmpeg::codec::select_encoder(VideoCodec::Vp8)?;
                (name, preview_vp8_options(fps))
            }
            // A future preview codec with no encoder wired here yields no encoder
            // (the session was already rejected at negotiate if unsupported).
            _ => return None,
        };
        let target = VideoEncodeTarget {
            codec_name: encoder_name.to_owned(),
            width: w,
            height: h,
            format: Pixel::NV12,
            // The encoder time-base is the reciprocal of the cadence (1/fps).
            time_base: Rational::new(1, i64::from(PREVIEW_FPS)),
            bit_rate: PREVIEW_BITRATE,
            gop: preview_gop_frames(fps),
            cuda_device: None,
        };
        VideoEncoder::new_with_options(&target, &options).ok()
    }
}

/// A [`PreviewMediaSource`] that samples a wait-free NV12 latest-frame slot,
/// encodes each sampled frame with a real preview encoder, and pushes the encoded
/// sample into a bounded **drop-oldest** feed the transport drains.
///
/// The slot is the engine's program/preview slot (program scope) or a per-input
/// last-good store snapshot (input/output scope). Sampling is non-blocking and
/// drop-oldest at both ends — invariant #10.
struct SlotMediaSource {
    encoder: PreviewVideoEncoder,
    slot: SlotReader,
    sink: SampleSink,
    feed: Mutex<Option<SampleFeed>>,
    codec: PreviewCodec,
    /// The optional Opus audio sink/feed pair, present only when the session's
    /// offer negotiated Opus audio (ADR-P006). Audio is fed from the program bus.
    audio: Mutex<Option<SampleFeed>>,
}

/// How a [`SlotMediaSource`] reads its latest NV12 frame — wait-free for every
/// scope (a slot load or a lock-free store read).
enum SlotReader {
    /// The program/preview slot the engine loop publishes the composited canvas
    /// into (program scope).
    Program(ProgramSlot),
    /// A per-input last-good store, read at the current wall-clock instant
    /// (input/output scope).
    Input {
        stores: SharedStores,
        id: String,
        clock: MonotonicTimeSource,
    },
}

impl SlotReader {
    /// Load the latest NV12 frame, or `None` if none is available yet.
    fn load(&self) -> Option<Arc<Nv12Image>> {
        match self {
            Self::Program(slot) => slot.load_full(),
            Self::Input { stores, id, clock } => {
                let stores = stores.load();
                let store = stores.get(id)?;
                let now = MediaTime::from_nanos(clock.now_nanos());
                let read = store.read_at(now);
                read.frame().map(Arc::clone)
            }
        }
    }
}

impl SlotMediaSource {
    /// Build a video source over `slot`. `audio` is the consumer end of a
    /// drop-oldest Opus feed whose producer the driver pumps from a program-PCM
    /// preview slot — `None` when the scope has no audio source wired (the
    /// ADR-P006 contract: a session whose scope has no audio source leaves audio
    /// absent, never a dead feed). The video-only program/input scopes pass
    /// `None`; the audio producer slot is plumbed from the engine program bus as a
    /// follow-on without changing this seam (the transport already carries Opus —
    /// proven in `multiview-webrtc`'s offline egress test).
    fn new(codec: PreviewCodec, slot: SlotReader, audio: Option<SampleFeed>) -> Self {
        // Shallow drop-oldest preview rings (ADR-P001: depth 1–3).
        let (sink, feed) = sample_feed(3);
        Self {
            encoder: PreviewVideoEncoder::new(codec),
            slot,
            sink,
            feed: Mutex::new(Some(feed)),
            codec,
            audio: Mutex::new(audio),
        }
    }

    /// Sample the latest frame and, if present, encode + push it (drop-oldest).
    /// Called by the driver at preview cadence. Non-blocking; never paces the
    /// engine (it samples a wait-free slot).
    fn pump_once(&self) {
        let Some(image) = self.slot.load() else {
            return;
        };
        if let Some(sample) = self.encoder.encode(&image) {
            let _ = self.sink.push(sample);
        }
    }
}

impl PreviewMediaSource for SlotMediaSource {
    fn codec(&self) -> PreviewCodec {
        self.codec
    }
    fn feed(&self) -> SampleFeed {
        self.feed
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .unwrap_or_else(|| sample_feed(1).1)
    }
    fn audio_feed(&self) -> Option<SampleFeed> {
        self.audio.lock().ok().and_then(|mut g| g.take())
    }
}

/// A live WHEP egress session's media sources, kept alive by the driver so its
/// `pump_once` runs each tick and its feeds are drained by the transport.
struct LiveSession {
    media: Arc<SlotMediaSource>,
    scope_label: String,
}

/// The shared driver state: the native egress transport + the live media sources.
#[derive(Default)]
struct DriverState {
    sessions: HashMap<String, LiveSession>,
}

/// The engine-backed live WHEP egress provider.
///
/// Owns the native [`WhepEgress`] transport and the [`WebRtcEndpoint`] (the single
/// dual-stack UDP socket), drives them on a dedicated task, and builds a real
/// per-scope [`SlotMediaSource`] on each `negotiate`.
pub struct CliWhepProvider {
    egress: Arc<WhepEgress>,
    state: Arc<Mutex<DriverState>>,
    program: ProgramSlot,
    stores: SharedStores,
    /// Whether the live native transport actually came up (the socket bound). When
    /// `false` the provider sheds every focus to the JPEG fallback and the
    /// capabilities endpoint advertises `webrtc: false`.
    available: bool,
}

impl std::fmt::Debug for CliWhepProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliWhepProvider")
            .field("available", &self.available)
            .finish_non_exhaustive()
    }
}

impl CliWhepProvider {
    /// Build the provider, bind the shared UDP socket from `config`, and spawn the
    /// driver task. If the socket cannot bind, the provider stays *unavailable*
    /// (the SPA sheds to JPEG honestly) rather than failing the run.
    #[must_use]
    pub fn spawn(config: EndpointConfig, program: ProgramSlot, stores: SharedStores) -> Self {
        let state = Arc::new(Mutex::new(DriverState::default()));
        // Build the TURN relay driver from the configured ICE servers BEFORE the
        // endpoint consumes `config` (the operator's NAT-traversal path, ADR-0048
        // §5.1): one in-crate TURN client per configured TURN server, driven sans-
        // IO inside the egress driver over the SAME UDP socket as the media. Empty
        // when no TURN server is configured (host + advertised candidates only).
        let turn = TurnRelayDriver::from_config(&config, Instant::now());
        let endpoint = match WebRtcEndpoint::bind(config) {
            Ok(ep) => ep,
            Err(err) => {
                tracing::warn!(%err, "WHEP egress endpoint bind failed; preview sheds to JPEG");
                return Self {
                    egress: Arc::new(WhepEgress::new()),
                    state,
                    program,
                    stores,
                    available: false,
                };
            }
        };
        // Register the bound host candidate so the answer carries reachability.
        // TURN relay candidates are learned at runtime by the driver's TURN
        // client(s) and published into the egress via `learn_relay` (ADR-0048
        // §5.1), so a browser behind NAT can WHEP-play via the operator's relay.
        let host = endpoint
            .host_candidates()
            .ok()
            .and_then(|c| c.into_iter().next());
        let egress = Arc::new(match host {
            Some(addr) => WhepEgress::with_host_candidate(addr),
            None => WhepEgress::new(),
        });
        // The local socket address relayed traffic egresses from (for the relay
        // candidate's `raddr`); the bound host candidate, or the unspecified bind
        // address as a fallback.
        let local = host.unwrap_or_else(|| {
            endpoint
                .local_addr()
                .unwrap_or_else(|_| std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)))
        });
        spawn_driver(Arc::clone(&egress), Arc::clone(&state), endpoint, turn, local);
        Self {
            egress,
            state,
            program,
            stores,
            available: true,
        }
    }

    /// Build a [`SlotMediaSource`] for `scope`, choosing the wait-free reader. No
    /// program-PCM preview slot is wired yet, so audio is left absent (the
    /// ADR-P006 "no audio source ⇒ audio absent" contract); the transport already
    /// carries Opus when fed (proven offline in `multiview-webrtc`).
    fn media_for(&self, scope: &WhepScope, codec: PreviewCodec) -> Arc<SlotMediaSource> {
        let reader = match scope {
            WhepScope::Input(id) => SlotReader::Input {
                stores: self.stores.clone(),
                id: id.clone(),
                clock: MonotonicTimeSource::new(),
            },
            // Program, Output, and any future scope sample the program/preview
            // slot (the canvas-approx tap; the real-rendition fan-out is the
            // ADR-P006 PRV-5b upgrade).
            WhepScope::Program | WhepScope::Output(_) | _ => {
                SlotReader::Program(Arc::clone(&self.program))
            }
        };
        Arc::new(SlotMediaSource::new(codec, reader, None))
    }
}

impl WhepProvider for CliWhepProvider {
    fn negotiate(&self, scope: &WhepScope, offer: &str) -> Result<WhepAnswer, WhepReject> {
        if !self.available {
            return Err(WhepReject::CapacityExceeded {
                fallback: "jpeg".to_owned(),
            });
        }
        // Select the preview codec from the offer (H.264 preferred, then VP8).
        let codec = select_offer_codec(offer)?;
        // The offer may negotiate Opus; the cli leaves audio absent until a
        // program-PCM preview slot is wired (ADR-P006 "no audio source" path).
        let _opus = offer_has_opus(offer);
        let media = self.media_for(scope, codec);
        let answer = self
            .egress
            .accept_session(offer, codec, media.as_ref())
            .map_err(|e| map_whep_error(&e))?;
        let session_id = answer.transport.session_id.as_str().to_owned();
        // Keep the media source alive + pumped by the driver for the session's life.
        if let Ok(mut state) = self.state.lock() {
            state.sessions.insert(
                session_id.clone(),
                LiveSession {
                    media,
                    scope_label: scope.label(),
                },
            );
        }
        Ok(WhepAnswer {
            session_id,
            sdp: answer.sdp_answer,
        })
    }

    fn release(&self, _scope: &WhepScope, session_id: &str) -> bool {
        let id = SessionId::new(session_id);
        let known = self.egress.session_state(&id).is_some();
        let _ = self.egress.close(&id);
        if let Ok(mut state) = self.state.lock() {
            state.sessions.remove(session_id);
        }
        known
    }

    fn active_sessions(&self) -> usize {
        self.state.lock().map_or(0, |s| s.sessions.len())
    }

    fn webrtc_available(&self) -> bool {
        self.available
    }
}

/// Spawn the egress driver: own the shared UDP socket, run the configured TURN
/// clients over it, pump every session's encode + egress at preview cadence, and
/// fan inbound datagrams to the sessions. Never `.await`s a client; a stalled
/// session loses only its own media (invariant #10).
fn spawn_driver(
    egress: Arc<WhepEgress>,
    state: Arc<Mutex<DriverState>>,
    endpoint: WebRtcEndpoint,
    mut turn: TurnRelayDriver,
    local: std::net::SocketAddr,
) {
    std::thread::Builder::new()
        .name("whep-egress-driver".to_owned())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let now = Instant::now();
                // 1. Drain inbound datagrams (non-blocking). A datagram from a
                //    configured TURN server feeds its client (allocation/refresh);
                //    any other datagram is media broadcast to the sessions.
                loop {
                    match endpoint.recv_from(&mut buf) {
                        Ok((len, source)) => {
                            let payload = buf.get(..len).unwrap_or(&[]);
                            if turn.feed(source, payload, now) {
                                continue;
                            }
                            let dst = endpoint.local_addr().unwrap_or(source);
                            let _ =
                                egress.handle_datagram_broadcast(source, dst, payload, now);
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                // 2. Drive the TURN client(s): send their queued datagrams and
                //    publish any newly-allocated relay into the egress so the next
                //    negotiated session offers it as a relay candidate (the
                //    operator's NAT-traversal path, ADR-0048 §5.1).
                pump_turn_relays(&mut turn, egress.as_ref(), local, now, |payload, dst| {
                    let _ = endpoint.send_to(payload, dst);
                });
                // 3. Pump each live session's encode (sample → encode → feed).
                if let Ok(state) = state.lock() {
                    for session in state.sessions.values() {
                        session.media.pump_once();
                        let _ = &session.scope_label;
                    }
                }
                // 4. Drive every session's egress and send the outbound datagrams.
                if let Ok(out) = egress.drive_all(now) {
                    for (dst, payload) in out {
                        let _ = endpoint.send_to(&payload, dst);
                    }
                }
                // 5. Park until the next session timer or the driver tick, whichever
                //    is sooner — never busy-spin (invariant #10: best-effort).
                let wake = egress.next_wake(now);
                let until = wake
                    .saturating_duration_since(Instant::now())
                    .min(DRIVER_TICK);
                std::thread::sleep(until.max(Duration::from_millis(1)));
            }
        })
        .ok();
}

/// Drive the shared [`TurnRelayDriver`] one pass: send each queued TURN datagram
/// via `send` and publish any newly-allocated relay into `egress` (with `local`
/// as the address the relayed traffic egresses from), so every subsequently-
/// negotiated WHEP session offers it as a relay candidate (ADR-0048 §5.1).
///
/// Pure over the `send` closure (the socket is the caller's) so the driver's TURN
/// pump is offline-testable without binding a socket — mirroring the WHIP
/// endpoint's `pump_turn`. A no-op when no TURN server is configured.
fn pump_turn_relays<S: FnMut(&[u8], std::net::SocketAddr)>(
    turn: &mut TurnRelayDriver,
    egress: &WhepEgress,
    local: std::net::SocketAddr,
    now: Instant,
    mut send: S,
) {
    while let Some((dst, payload)) = turn.poll_transmit(now) {
        send(&payload, dst);
    }
    for relay in turn.take_new_relays() {
        egress.learn_relay(relay, local);
    }
}

/// Select the preview codec from a WHEP offer (H.264 preferred, then VP8),
/// mapping the absence of a supported codec to the route's `415`.
fn select_offer_codec(offer: &str) -> Result<PreviewCodec, WhepReject> {
    let mut in_video = false;
    let mut has_h264 = false;
    let mut has_vp8 = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_video = rest.starts_with("video");
            continue;
        }
        if !in_video {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(name) = rtpmap
                .split_whitespace()
                .nth(1)
                .and_then(|m| m.split('/').next())
            {
                if name.eq_ignore_ascii_case("H264") {
                    has_h264 = true;
                } else if name.eq_ignore_ascii_case("VP8") {
                    has_vp8 = true;
                }
            }
        }
    }
    if has_h264 && multiview_ffmpeg::codec::can_encode(VideoCodec::H264) {
        Ok(PreviewCodec::H264)
    } else if has_vp8 && multiview_ffmpeg::codec::can_encode(VideoCodec::Vp8) {
        Ok(PreviewCodec::Vp8)
    } else {
        Err(WhepReject::UnsupportedCodec)
    }
}

/// Whether the offer carries an Opus audio m-line at 48 kHz (ADR-P006).
fn offer_has_opus(offer: &str) -> bool {
    let mut in_audio = false;
    for raw in offer.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("m=") {
            in_audio = rest.starts_with("audio");
            continue;
        }
        if !in_audio {
            continue;
        }
        if let Some(rtpmap) = line.strip_prefix("a=rtpmap:") {
            if let Some(m) = rtpmap.split_whitespace().nth(1) {
                let mut f = m.split('/');
                if f.next().is_some_and(|n| n.eq_ignore_ascii_case("opus"))
                    && f.next() == Some("48000")
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Map a transport [`WhepError`] onto the control-plane [`WhepReject`].
fn map_whep_error(err: &multiview_preview::whep::WhepError) -> WhepReject {
    use multiview_preview::whep::WhepError;
    match err {
        WhepError::NoSupportedCodec => WhepReject::UnsupportedCodec,
        WhepError::MalformedOffer { reason } => WhepReject::Malformed((*reason).to_owned()),
        // Access/lifecycle faults (and any future variant) shed honestly to JPEG —
        // a preview-only refusal that never reflects or affects the engine.
        WhepError::AccessDenied { .. } | WhepError::IllegalTransition { .. } | _ => {
            WhepReject::CapacityExceeded {
                fallback: "jpeg".to_owned(),
            }
        }
    }
}

/// Convert an [`Nv12Image`] into a libav NV12 [`Video`] frame for the encoder
/// (safe `ffmpeg-next` value API only — no FFI in this crate).
fn nv12_to_video(image: &Nv12Image) -> Result<ffmpeg_next::util::frame::Video, ()> {
    use ffmpeg_next::format::Pixel;
    use ffmpeg_next::util::frame::Video;
    let w = image.width();
    let h = image.height();
    let mut frame = Video::new(Pixel::NV12, w, h);
    let wu = usize::try_from(w).map_err(|_| ())?;
    let hu = usize::try_from(h).map_err(|_| ())?;
    let y_stride = frame.stride(0);
    let uv_stride = frame.stride(1);
    copy_plane(frame.data_mut(0), y_stride, image.y_plane(), wu, hu)?;
    copy_plane(frame.data_mut(1), uv_stride, image.uv_plane(), wu, hu / 2)?;
    Ok(frame)
}

/// Copy `rows` rows of `row_bytes` from a tightly-packed `src` into a
/// (possibly stride-padded) `dst` plane.
fn copy_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    row_bytes: usize,
    rows: usize,
) -> Result<(), ()> {
    if dst_stride < row_bytes {
        return Err(());
    }
    for row in 0..rows {
        let s = src
            .get(row * row_bytes..(row * row_bytes + row_bytes))
            .ok_or(())?;
        let d = dst
            .get_mut(row * dst_stride..(row * dst_stride + row_bytes))
            .ok_or(())?;
        d.copy_from_slice(s);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use std::net::SocketAddr;
    use std::time::Instant;

    use multiview_webrtc::config::{EndpointConfig, IceServer, TurnCredentials};
    use multiview_webrtc::turn::message::{long_term_key, Attribute, Class, Method, StunMessage};
    use multiview_webrtc::turn::TurnRelayDriver;
    use multiview_webrtc::whep_egress::WhepEgress;

    use super::pump_turn_relays;

    /// A WHEP offer carrying a recvonly H.264 video m-line (str0m needs real ICE
    /// credentials + a DTLS fingerprint + `setup:actpass` to answer).
    const VIDEO_OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP6 ::1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP6 ::\r\n\
a=ice-ufrag:tEsT\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=ice-options:trickle\r\n\
a=fingerprint:sha-256 \
6F:8E:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7:\
F8:09:1A:2B:3C:4D:5E:6F:70:81:92:A3:B4:C5:D6:E7\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

    /// A minimal, allocate-only fake TURN server (the long-term-credential 401
    /// challenge + an authenticated Allocate success carrying XOR-RELAYED-ADDRESS).
    /// Purpose-built for this wiring test; the full RFC 5766 surface (Refresh /
    /// CreatePermission / ChannelBind / Data) is exercised by the `multiview-webrtc`
    /// `fake_turn` fixture — here we only need the relay to be learned.
    struct MiniTurnServer {
        relay: SocketAddr,
        username: String,
        password: String,
        realm: String,
        nonce: String,
        saw_auth: bool,
    }

    impl MiniTurnServer {
        fn new(relay: SocketAddr, username: &str, password: &str, realm: &str) -> Self {
            Self {
                relay,
                username: username.to_owned(),
                password: password.to_owned(),
                realm: realm.to_owned(),
                nonce: "nonce-xyz".to_owned(),
                saw_auth: false,
            }
        }

        fn key(&self) -> Vec<u8> {
            long_term_key(&self.username, &self.realm, &self.password)
        }

        fn handle(&mut self, datagram: &[u8]) -> Option<Vec<u8>> {
            let msg = StunMessage::parse(datagram).expect("client sends valid STUN");
            if msg.class() != Class::Request || msg.method() != Method::Allocate {
                return None;
            }
            let authed = msg
                .attributes()
                .iter()
                .any(|a| matches!(a, Attribute::Username(_)));
            if !authed || !msg.verify_integrity(&self.key()) {
                let mut reply = StunMessage::with_transaction(
                    Class::Error,
                    Method::Allocate,
                    msg.transaction_id(),
                );
                reply.push(Attribute::ErrorCode {
                    code: 401,
                    reason: "Unauthorized".to_owned(),
                });
                reply.push(Attribute::Realm(self.realm.clone()));
                reply.push(Attribute::Nonce(self.nonce.clone()));
                return Some(reply.to_bytes(None));
            }
            self.saw_auth = true;
            let mut reply =
                StunMessage::with_transaction(Class::Success, Method::Allocate, msg.transaction_id());
            reply.push(Attribute::XorRelayedAddress(self.relay));
            reply.push(Attribute::Lifetime(600));
            reply.push(Attribute::Username(self.username.clone()));
            reply.push(Attribute::Realm(self.realm.clone()));
            reply.push(Attribute::Nonce(self.nonce.clone()));
            Some(reply.to_bytes(Some(&self.key())))
        }
    }

    fn relay_candidate_count(candidates: &[String]) -> usize {
        candidates.iter().filter(|c| c.contains("typ relay")).count()
    }

    #[test]
    fn whep_driver_pump_runs_turn_and_publishes_the_relay_to_the_egress() {
        // ITEM-1 of PR #141: the cli WHEP egress driver runs its configured TURN
        // client over the SAME socket via `pump_turn_relays`, and a relay it
        // allocates is published into the WhepEgress so the next negotiated
        // session offers a `typ relay` candidate (browser-behind-NAT WHEP-play).
        let now = Instant::now();
        let server_addr: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        let relay_addr: SocketAddr = "[2001:db8::1]:49152".parse().unwrap();
        let host: SocketAddr = "[2001:db8::abc]:8189".parse().unwrap();
        let mut server = MiniTurnServer::new(relay_addr, "alice", "s3cret", "example.org");

        let config = EndpointConfig {
            ice_servers: vec![IceServer::turn(
                server_addr,
                TurnCredentials::long_term("alice", "s3cret"),
            )],
            ..EndpointConfig::default()
        };
        let mut turn = TurnRelayDriver::from_config(&config, now);
        assert_eq!(turn.client_count(), 1, "the WHEP driver built a TURN client");
        let egress = WhepEgress::with_host_candidate(host);

        // Drive the pump in a shuttle: the closure captures the TURN datagrams and
        // feeds them to the fake server, whose replies we feed back into the
        // driver before the next pump (mirroring the live recv→feed loop).
        let mut pending_replies: Vec<Vec<u8>> = Vec::new();
        for _ in 0..16 {
            for reply in pending_replies.drain(..) {
                assert!(
                    turn.feed(server_addr, &reply, now),
                    "the TURN-server reply is consumed by the driver"
                );
            }
            pump_turn_relays(&mut turn, &egress, host, now, |payload, dst| {
                assert_eq!(dst, server_addr);
                if let Some(reply) = server.handle(payload) {
                    pending_replies.push(reply);
                }
            });
            // A relay learned ⇒ a subsequent session offers it; check early-exit.
            let media = crate::whep::tests::dummy_media();
            if let Ok(answer) = egress.accept_session(
                VIDEO_OFFER,
                multiview_preview::whep::PreviewCodec::H264,
                media.as_ref(),
            ) {
                if relay_candidate_count(&answer.transport.candidates) >= 1 {
                    assert!(server.saw_auth, "the Allocate was authenticated");
                    return; // success
                }
            }
        }
        panic!("the WHEP driver never published the TURN relay to the egress");
    }

    #[test]
    fn whep_driver_pump_is_a_noop_without_turn_and_never_blocks() {
        // INVARIANT #10: with no TURN configured the pump sends nothing and
        // publishes nothing — and it completes immediately (it never blocks the
        // egress driver thread, so a slow/absent TURN server can't stall preview).
        let now = Instant::now();
        let host: SocketAddr = "[2001:db8::abc]:8189".parse().unwrap();
        let mut turn = TurnRelayDriver::from_config(&EndpointConfig::default(), now);
        assert!(turn.is_empty());
        let egress = WhepEgress::with_host_candidate(host);

        let started = Instant::now();
        let mut sent = 0usize;
        pump_turn_relays(&mut turn, &egress, host, now, |_p, _d| sent += 1);
        assert_eq!(sent, 0, "no TURN server ⇒ no TURN datagrams");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "the pump never blocks"
        );

        let media = dummy_media();
        let answer = egress
            .accept_session(
                VIDEO_OFFER,
                multiview_preview::whep::PreviewCodec::H264,
                media.as_ref(),
            )
            .expect("accept");
        assert_eq!(
            relay_candidate_count(&answer.transport.candidates),
            0,
            "no relay candidate without TURN"
        );
    }

    /// A no-op preview media source for the egress `accept_session` in these
    /// wiring tests (the relay candidate is what we assert, not media flow).
    pub(super) fn dummy_media() -> std::sync::Arc<DummyMedia> {
        std::sync::Arc::new(DummyMedia)
    }

    pub(super) struct DummyMedia;

    impl multiview_preview::whep::transport::PreviewMediaSource for DummyMedia {
        fn codec(&self) -> multiview_preview::whep::PreviewCodec {
            multiview_preview::whep::PreviewCodec::H264
        }
        fn feed(&self) -> multiview_preview::whep::transport::SampleFeed {
            multiview_preview::whep::transport::sample_feed(1).1
        }
        fn audio_feed(&self) -> Option<multiview_preview::whep::transport::SampleFeed> {
            None
        }
    }
}
