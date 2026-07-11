//! ST 2110 UDP receive transport — **feature `st2110`**.
//!
//! This module owns the *testable core* of an ST 2110 receive source: the
//! application-layer [`PacketSource`] seam that delivers post-receive RTP packet
//! units, the ST 2022-7 sequence-keyed dedup over that seam
//! ([`DualPathPacketSource`]), and the
//! [`St2110Producer`]: a [`FrameProducer`](crate::source::FrameProducer) that
//! drives the IN-1 [`FrameAssembler`](crate::st2110::assembler::FrameAssembler)
//! and rebases the RTP 90 kHz timestamps onto the nanosecond timeline (via the
//! [`PtsNormalizer`](crate::normalize) the [`IngestPump`] holds).
//!
//! ## The seam vs. the NIC
//!
//! The pure depacketizers in the parent module ([`super::rtp`] / [`super::v20`]
//! …) turn bytes into typed values; the [`St2110Producer`] turns a stream of
//! those into produced frames. Both are driven by **injected** packet units and
//! are fully unit-tested with no NIC. The actual UDP sockets — [`RtpReceiver`]
//! and the two-path [`DualPathReceiver`] — bind a `tokio` `UdpSocket`, join a
//! multicast group, and need a real ST 2110 network (a PTP-disciplined NIC, an
//! IGMP-joined multicast source); this devcontainer has none, so that socket
//! path is exercised only by the gated/`#[ignore]`d live test.
//! [`channel_bridge`](RtpReceiver::channel_bridge) connects the async socket to
//! the sync seam through a **bounded, drop-oldest** channel so a stalled reader
//! never back-pressures the receive task (invariant #10).
//!
//! ## PTP / ST 2059 (informs, never paces)
//!
//! Per [streaming-gotchas §5] the master clock stays `CLOCK_MONOTONIC`; the
//! output is **never** slaved to an input. The RTP 90 kHz media clock is used
//! for per-input PTS only, and the first frame anchors to the master clock's
//! "now" exactly like every other source (the [`IngestPump`] does this). A PTP
//! epoch may later *inform* the rebase anchor, but full ST 2059 lock is out of
//! scope here (the brief's "free-run the rest" guidance).
//!
//! ## No native FFI (pure / LGPL-clean default)
//!
//! It deliberately does **no** native FFI (uses `tokio`'s safe `UdpSocket`),
//! keeping `multiview-input` `unsafe_code = forbid`, and it never paces the output
//! clock: received datagrams are depacketized and handed to the ingest pipeline
//! (which writes the last-good-frame store), exactly like every other source
//! (invariants #1 / #10).
//!
//! [`IngestPump`]: crate::source::IngestPump
//! [streaming-gotchas §5]: ../../../docs/research/streaming-gotchas.md

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::net::UdpSocket;

use multiview_core::color::ColorInfo;
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::{MediaTime, Rational};

use crate::error::{Error, Result};
use crate::normalize::WrapBits;
use crate::source::{FrameProducer, ProducedFrame};
use crate::st2022_7::{HitlessReconstructor, Path, PushOutcome};
use crate::st2110::assembler::{FrameAssembler, PacketUnit, RasterGeometry};
use crate::st2110::rtp::RtpPacket;
use crate::st2110::v20::V20Payload;

/// The RTP media clock rate ST 2110-20 video rides on (90 kHz, RFC 4175).
pub const VIDEO_CLOCK_RATE: u32 = 90_000;

/// The largest UDP datagram an ST 2110 receiver will buffer. ST 2110-20 uses a
/// ~1500-byte standard MTU (or up to ~9000 jumbo); 9 KiB covers both with margin.
pub const MAX_DATAGRAM: usize = 9216;

/// A single-path ST 2110 RTP receive socket.
///
/// Binds a UDP socket and (optionally) joins a source-specific multicast group,
/// then yields raw RTP packets one datagram at a time. Parsing is delegated to
/// the pure [`RtpPacket::parse`]; this type owns only the socket and a receive
/// buffer.
#[derive(Debug)]
pub struct RtpReceiver {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl RtpReceiver {
    /// Bind a receive socket to `local`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 bind {local}: {e}")))?;
        Ok(Self {
            socket,
            buf: vec![0u8; MAX_DATAGRAM],
        })
    }

    /// Join an IPv4 multicast `group` on the interface address `interface`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the membership cannot be joined.
    pub fn join_multicast_v4(
        &self,
        group: std::net::Ipv4Addr,
        interface: std::net::Ipv4Addr,
    ) -> Result<()> {
        self.socket
            .join_multicast_v4(group, interface)
            .map_err(|e| Error::Ingest(format!("st2110 join {group}: {e}")))
    }

    /// Receive one datagram and return the number of payload bytes read into the
    /// internal buffer; the bytes are then available via [`RtpReceiver::buffer`].
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the receive fails.
    pub async fn recv(&mut self) -> Result<usize> {
        let n = self
            .socket
            .recv(&mut self.buf)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 recv: {e}")))?;
        Ok(n)
    }

    /// The first `len` bytes of the last received datagram.
    #[must_use]
    pub fn buffer(&self, len: usize) -> &[u8] {
        let end = len.min(self.buf.len());
        self.buf.get(..end).unwrap_or(&[])
    }

    /// Receive one datagram and parse it as an RTP packet, returning the parsed
    /// view borrowed from the internal buffer.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or an RTP parse failure.
    pub async fn recv_rtp(&mut self) -> Result<RtpPacket<'_>> {
        let n = self.recv().await?;
        let end = n.min(self.buf.len());
        let bytes = self.buf.get(..end).unwrap_or(&[]);
        RtpPacket::parse(bytes).map_err(|e| Error::Ingest(format!("st2110 rtp: {e}")))
    }

    /// Drive this socket into a bounded, drop-oldest [`ChannelPacketSource`].
    ///
    /// Returns the sync [`PacketSource`] half plus an async future that loops
    /// `recv_rtp`, converts each datagram into an [`St2110Packet`], and enqueues
    /// it. When the channel is full the **oldest** queued unit is dropped so the
    /// receive loop never blocks on a slow reader (invariant #10): an ST 2110
    /// receiver must never let downstream stall back-pressure the wire. The
    /// future ends (clean end-of-stream) if the receiver half is dropped or a
    /// socket error occurs; it is the live, NIC-bound path and is exercised only
    /// by the gated live test.
    #[must_use]
    pub fn channel_bridge(mut self, capacity: usize) -> (ChannelPacketSource, ReceiveLoop) {
        let (sink, source) = ChannelPacketSource::bounded(capacity);
        let task = async move {
            loop {
                // Stop as soon as the sync reader is gone — nothing to feed.
                if sink.is_closed() {
                    return;
                }
                let unit = match self.recv_rtp().await {
                    Ok(packet) => St2110Packet::from_rtp(&packet),
                    // A socket/parse error ends the loop; the supervisor
                    // reconnects. Dropping the sink signals clean EOS to the
                    // sync side.
                    Err(_) => return,
                };
                // Genuine drop-oldest on a full ring (ADR-0033 §7): the push
                // never blocks and never grows — the freshest media is retained
                // and a stalled reader can never back-pressure the wire
                // (invariant #10).
                sink.push(unit);
            }
        };
        (source, Box::pin(task))
    }
}

/// The async receive future returned by [`RtpReceiver::channel_bridge`]: it must
/// be polled (e.g. `tokio::spawn`ed) to drive the live socket into the bounded
/// channel. It never resolves until the socket faults or the reader is dropped.
pub type ReceiveLoop = core::pin::Pin<Box<dyn core::future::Future<Output = ()> + Send>>;

/// A two-path ST 2022-7 receiver: two [`RtpReceiver`]s feeding one
/// [`HitlessReconstructor`] keyed by RTP sequence number.
///
/// `P` is the per-packet payload the caller extracts from each datagram (e.g. an
/// owned copy of the RTP payload). The reconstructor lives in the always-compiled
/// pure module; this struct is only the socket wiring around it.
#[derive(Debug)]
pub struct DualPathReceiver<P> {
    path_a: RtpReceiver,
    path_b: RtpReceiver,
    reconstructor: HitlessReconstructor<P>,
}

impl<P> DualPathReceiver<P> {
    /// Build a dual-path receiver from two bound sockets and a reorder-window
    /// capacity.
    #[must_use]
    pub fn new(path_a: RtpReceiver, path_b: RtpReceiver, window: usize) -> Self {
        Self {
            path_a,
            path_b,
            reconstructor: HitlessReconstructor::new(window),
        }
    }

    /// Receive concurrently from both paths; whichever delivers first is parsed,
    /// its payload extracted by `extract`, pushed into the reconstructor, and the
    /// merged in-order packets returned.
    ///
    /// `extract` maps a parsed [`RtpPacket`] to the caller's payload `P` (e.g. an
    /// owned payload copy). The merge/de-dup/reorder is performed by the pure
    /// reconstructor.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or RTP parse failure on the path that
    /// fired.
    pub async fn recv_merged<F>(&mut self, mut extract: F) -> Result<Vec<P>>
    where
        F: FnMut(&RtpPacket<'_>) -> P,
    {
        let (path, packet) = tokio::select! {
            r = self.path_a.recv_rtp() => (Path::A, r?),
            r = self.path_b.recv_rtp() => (Path::B, r?),
        };
        let payload = extract(&packet);
        let _ = self
            .reconstructor
            .push(path, packet.header.sequence, payload);
        Ok(self.reconstructor.drain())
    }

    /// Borrow the underlying reconstructor (for metrics / window inspection).
    #[must_use]
    pub const fn reconstructor(&self) -> &HitlessReconstructor<P> {
        &self.reconstructor
    }
}

/// One post-receive ST 2110-20 RTP packet unit handed across the
/// [`PacketSource`] seam.
///
/// This is the boundary between the (NIC-bound, gated) socket layer and the
/// pure depacketize -> assemble -> frame logic: the socket parses the RTP fixed
/// header and hands these fields here; the producer never sees the wire framing
/// again, only this typed unit. `payload` is an **owned** copy of the RTP
/// payload (the bytes after the fixed header) the SRD segments point into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct St2110Packet {
    /// The RFC 4175 marker bit: `true` flags the **last packet of a frame**.
    pub marker: bool,
    /// The 90 kHz RTP media timestamp (one value per video frame).
    pub timestamp: u32,
    /// The 16-bit RTP sequence number (gap detection / 2022-7 dedup / reorder).
    pub sequence: u16,
    /// The owned RTP payload bytes (after the fixed header) the SRD segments of
    /// the depacketized [`V20Payload`] index into.
    pub payload: Vec<u8>,
}

impl St2110Packet {
    /// Build a packet unit from a parsed [`RtpPacket`], copying its payload.
    ///
    /// This is how the socket layer crosses the seam: it owns the borrowed
    /// payload so the unit can outlive the receive buffer.
    #[must_use]
    pub fn from_rtp(packet: &RtpPacket<'_>) -> Self {
        Self {
            marker: packet.header.marker,
            timestamp: packet.header.timestamp,
            sequence: packet.header.sequence,
            payload: packet.payload.to_vec(),
        }
    }
}

/// The application-layer packet-source seam.
///
/// A concrete implementation delivers post-receive ST 2110-20 RTP packet units.
/// It is driven cooperatively by [`St2110Producer`]: each
/// [`poll_packet`](PacketSource::poll_packet) returns the next available unit,
/// `Ok(None)` at clean end-of-stream (or when nothing is currently ready), or an
/// error the supervisor reacts to. It must **never block the caller** waiting on
/// the network — a source with nothing ready returns `Ok(None)` and is held
/// (invariants #1 / #10).
pub trait PacketSource {
    /// Pull the next post-receive packet unit.
    ///
    /// Returns `Ok(Some(unit))` for a unit, `Ok(None)` at clean end-of-stream
    /// (or when nothing is currently ready), or an error for a fault the
    /// supervisor should react to (reconnect).
    ///
    /// # Errors
    ///
    /// An [`Error`] when the underlying source faults (a socket error, an RTP
    /// parse failure). The caller treats this as a connection fault and applies
    /// the supervised-reconnect backoff rather than crashing the engine.
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>>;
}

/// A two-path ST 2022-7 dedup over the [`PacketSource`] seam.
///
/// Wraps two [`PacketSource`]s (the redundant network paths) and one
/// [`HitlessReconstructor`] keyed by RTP sequence number: each
/// [`poll_packet`](PacketSource::poll_packet) drains both paths into the
/// reconstructor, which **de-duplicates** the copy of every sequence that
/// arrives on both paths and releases the merged units in sequence order. A
/// packet lost on one path but present on the other produces **no gap**; a
/// packet lost on *both* is a genuine gap the assembler surfaces downstream.
///
/// This is the pure, NIC-free analogue of [`DualPathReceiver::recv_merged`]: the
/// merge/dedup logic is identical (the same reconstructor), but driven by the
/// injectable sync seam so it is fully unit-tested without a socket.
pub struct DualPathPacketSource {
    path_a: Box<dyn PacketSource + Send>,
    path_b: Box<dyn PacketSource + Send>,
    reconstructor: HitlessReconstructor<St2110Packet>,
    /// Units released by the reconstructor but not yet returned (FIFO), so each
    /// `poll_packet` returns exactly one merged unit.
    pending: std::collections::VecDeque<St2110Packet>,
}

impl DualPathPacketSource {
    /// Build a dual-path dedup over two packet sources and a reorder-window
    /// `capacity` (distinct sequence numbers held before the window slides).
    #[must_use]
    pub fn new(
        path_a: Box<dyn PacketSource + Send>,
        path_b: Box<dyn PacketSource + Send>,
        capacity: usize,
    ) -> Self {
        Self {
            path_a,
            path_b,
            reconstructor: HitlessReconstructor::new(capacity),
            pending: std::collections::VecDeque::new(),
        }
    }

    /// Push one unit from `path` into the reconstructor (de-dup by sequence),
    /// then queue any newly-released merged units.
    fn ingest(&mut self, path: Path, unit: St2110Packet) {
        let seq = unit.sequence;
        // The reconstructor owns the dedup: a second copy of `seq` from the other
        // path returns `Duplicate` and is dropped here.
        if self.reconstructor.push(path, seq, unit) == PushOutcome::Accepted {
            for released in self.reconstructor.drain() {
                self.pending.push_back(released);
            }
        }
    }
}

impl PacketSource for DualPathPacketSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>> {
        // Drain both paths into the reconstructor until a merged unit is ready or
        // neither path has anything to deliver. Each pull is non-blocking; a
        // `None` from a path means "nothing ready this tick" (it is NOT latched
        // as permanent end-of-stream — a live channel source returns `None`
        // whenever its buffer is momentarily empty).
        loop {
            if let Some(unit) = self.pending.pop_front() {
                return Ok(Some(unit));
            }
            let mut progressed = false;
            if let Some(unit) = self.path_a.poll_packet()? {
                self.ingest(Path::A, unit);
                progressed = true;
            }
            if let Some(unit) = self.path_b.poll_packet()? {
                self.ingest(Path::B, unit);
                progressed = true;
            }
            if !progressed {
                // A lull: neither path delivered. Release everything the reorder
                // window is still holding back — the network has caught up, so
                // there is nothing newer to reorder against, and a frame's final
                // packet must not be stranded waiting (invariant #1). This is
                // correct whether the lull is transient (a live channel) or the
                // genuine end of an injected stream.
                for released in self.reconstructor.flush() {
                    self.pending.push_back(released);
                }
                return Ok(self.pending.pop_front());
            }
        }
    }
}

impl core::fmt::Debug for DualPathPacketSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DualPathPacketSource")
            .field("buffered", &self.reconstructor.buffered())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

/// Shared bounded-ring state behind the [`PacketSink`] producer half and the
/// [`ChannelPacketSource`] consumer half.
///
/// A plain [`VecDeque`](std::collections::VecDeque) guarded by a `Mutex` held
/// only for the O(1) push/pop — never across a socket call or an `.await` — so
/// the *property* it provides is genuine bounded **drop-oldest**: a full ring
/// sheds its oldest unit instead of growing, and neither half ever blocks the
/// other (invariants #1 / #5 / #10). Mirrors the display-audio sink's
/// `AudioFifo`.
#[derive(Debug)]
struct PacketRing {
    queue: std::collections::VecDeque<St2110Packet>,
    capacity: usize,
    dropped: u64,
    /// Set when the consuming [`ChannelPacketSource`] is dropped, so the
    /// producing receive task learns the reader is gone and can stop.
    consumer_gone: bool,
}

/// The producer half of the receive bridge, handed to the async receive task.
///
/// [`push`](PacketSink::push) is a bounded, genuine **drop-oldest** enqueue:
/// on a full ring the *oldest* queued unit is evicted so the freshest media is
/// retained (ADR-0033 §7). It never blocks and never grows, so a stalled reader
/// can never back-pressure the receive task or the wire (invariant #10).
/// Cloneable so the receive task can hold it while the sync source drains the
/// other half.
#[derive(Debug, Clone)]
pub struct PacketSink {
    ring: Arc<Mutex<PacketRing>>,
}

impl PacketSink {
    /// Enqueue one unit, evicting the **oldest** queued unit first when the ring
    /// is already at capacity (genuine drop-oldest — ADR-0033 §7). Never blocks;
    /// the receive task is never back-pressured by a slow reader (invariant #10).
    pub fn push(&self, unit: St2110Packet) {
        let Ok(mut ring) = self.ring.lock() else {
            // A poisoned lock means a holder panicked; drop the unit rather than
            // propagate (the data plane holds last-good, never crashes).
            return;
        };
        // Genuine drop-oldest (ADR-0033 §7): a full ring evicts its OLDEST unit
        // before appending the newest, so the freshest media is retained and the
        // ring never grows past capacity.
        while ring.queue.len() >= ring.capacity {
            ring.queue.pop_front();
            ring.dropped = ring.dropped.saturating_add(1);
        }
        ring.queue.push_back(unit);
    }

    /// Whether the consuming [`ChannelPacketSource`] has been dropped (the reader
    /// is gone, so the receive task should stop).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.ring.lock().map_or(true, |ring| ring.consumer_gone)
    }
}

/// A bounded, drop-oldest packet source fed by an async receive task.
///
/// This is the seam the live (NIC-bound) [`RtpReceiver`] / [`DualPathReceiver`]
/// path crosses into the sync [`St2110Producer`]: the async receive loop pushes
/// units into a **bounded ring** via a [`PacketSink`]; this source drains the
/// front. A stalled reader can never back-pressure the sender — the sink drops
/// the *oldest* queued unit when the ring is full (invariant #10). It never
/// blocks the data plane: an empty ring yields `Ok(None)` (the producer re-polls
/// next tick), and a dropped sink (the receive task ended) simply drains to
/// empty and then yields `Ok(None)` (clean end-of-stream).
#[derive(Debug)]
pub struct ChannelPacketSource {
    ring: Arc<Mutex<PacketRing>>,
}

impl ChannelPacketSource {
    /// Build a bounded ring of `capacity` units, returning the [`PacketSink`]
    /// producer half (for the async receive task) and the sync [`PacketSource`]
    /// consumer half.
    ///
    /// The receive task calls [`PacketSink::push`], which on a full ring drops
    /// the **oldest** unit so a slow reader never stalls the receiver — the ring
    /// is bounded and never grows (invariant #10 / #5).
    #[must_use]
    pub fn bounded(capacity: usize) -> (PacketSink, Self) {
        let ring = Arc::new(Mutex::new(PacketRing {
            queue: std::collections::VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
            consumer_gone: false,
        }));
        (
            PacketSink {
                ring: Arc::clone(&ring),
            },
            Self { ring },
        )
    }

    /// The number of units dropped to drop-oldest overflow since construction
    /// (telemetry / test observability).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.ring.lock().map_or(0, |ring| ring.dropped)
    }
}

impl Drop for ChannelPacketSource {
    fn drop(&mut self) {
        if let Ok(mut ring) = self.ring.lock() {
            ring.consumer_gone = true;
        }
    }
}

impl PacketSource for ChannelPacketSource {
    fn poll_packet(&mut self) -> Result<Option<St2110Packet>> {
        // Pop the oldest buffered unit. An empty ring (nothing ready this tick —
        // hold, never block, invariant #1) and a dropped sink (the receive task
        // ended — clean end-of-stream) both surface to the non-blocking producer
        // as "no frame now".
        Ok(self
            .ring
            .lock()
            .ok()
            .and_then(|mut ring| ring.queue.pop_front()))
    }
}

/// A [`FrameProducer`] over a [`PacketSource`]: pulls ST 2110-20 RTP packet
/// units, depacketizes each ([`V20Payload::parse`]) and feeds the IN-1
/// [`FrameAssembler`], and yields a produced frame per reassembled raster for
/// the [`IngestPump`].
///
/// This is the IN-2 bridge for ST 2110: it does **non-blocking pulls only** from
/// the source and never paces the output clock. The assembler's bounded raster
/// buffers drop, never grow (invariants #1 / #2 / #5). The RTP 90 kHz timestamp
/// is surfaced as the producer's raw PTS; [`St2110Producer::wrap_bits`] reports
/// [`WrapBits::Rtp32`] so the [`PtsNormalizer`](crate::normalize) rebases it onto
/// the nanosecond timeline correctly (the float-free 90 kHz -> ns conversion).
///
/// [`IngestPump`]: crate::source::IngestPump
pub struct St2110Producer {
    source: Box<dyn PacketSource + Send>,
    assembler: FrameAssembler,
}

impl St2110Producer {
    /// Build a producer over an application-supplied [`PacketSource`],
    /// reassembling into `geometry`.
    #[must_use]
    pub fn new(source: Box<dyn PacketSource + Send>, geometry: RasterGeometry) -> Self {
        Self {
            source,
            assembler: FrameAssembler::new(geometry),
        }
    }

    /// The raster geometry this producer reassembles into.
    #[must_use]
    pub const fn geometry(&self) -> RasterGeometry {
        self.assembler.geometry()
    }

    /// Depacketize one packet unit and push it into the assembler, returning a
    /// closed frame mapped to a [`ProducedFrame`] when one becomes available.
    ///
    /// A malformed -20 payload is dropped (it never closes a frame and never
    /// panics) rather than faulting the source — a single bad datagram must not
    /// stall the stream (invariants #1 / #2).
    fn push(&mut self, packet: &St2110Packet) -> Option<ProducedFrame> {
        let payload_v20 = V20Payload::parse(&packet.payload, packet.sequence).ok()?;
        let unit = PacketUnit {
            marker: packet.marker,
            timestamp: packet.timestamp,
            sequence: packet.sequence,
            payload: packet.payload.clone(),
            payload_v20,
        };
        let assembled = self.assembler.push(&unit)?;
        let geometry = self.assembler.geometry();
        Some(ProducedFrame {
            pixels: assembled.pixels,
            // The verbatim 90 kHz RTP timestamp; the pump rebases it via the
            // normalizer ([`WrapBits::Rtp32`]) onto the ns timeline.
            raw_pts: Some(assembled.raw_pts),
            // A sequence gap / lost-marker partial re-anchors the normalizer.
            discontinuity: assembled.discontinuity,
            meta: FrameMeta {
                pts: MediaTime::ZERO,
                width: geometry.width(),
                height: geometry.height(),
                format: PixelFormat::Nv12,
                color: ColorInfo::default(),
            },
        })
    }
}

impl core::fmt::Debug for St2110Producer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("St2110Producer")
            .field("geometry", &self.assembler.geometry())
            .finish_non_exhaustive()
    }
}

impl FrameProducer for St2110Producer {
    fn next_frame(&mut self) -> Result<Option<ProducedFrame>> {
        // Pull from the source until a complete raster emerges or the source
        // signals clean end-of-stream. Each pull is non-blocking; an empty pull
        // ends this call (the pump re-polls on the next tick) rather than
        // spinning.
        loop {
            let Some(packet) = self.source.poll_packet()? else {
                return Ok(None);
            };
            if let Some(frame) = self.push(&packet) {
                return Ok(Some(frame));
            }
        }
    }

    fn timebase(&self) -> Rational {
        // ST 2110-20 video rides a 90 kHz RTP media clock.
        Rational::new(1, i64::from(VIDEO_CLOCK_RATE))
    }

    fn cadence(&self) -> Rational {
        // No cadence is carried in the RTP stream itself; assume 30 fps for the
        // genpts fallback (frames carry real 90 kHz timestamps in practice, so
        // this only matters for a timestamp-less packet, which does not occur in
        // ST 2110-20).
        Rational::new(30, 1)
    }

    fn wrap_bits(&self) -> WrapBits {
        WrapBits::Rtp32
    }
}
