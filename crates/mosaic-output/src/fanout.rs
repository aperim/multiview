//! Encode-once-mux-many fan-out routing model (invariant #7).
//!
//! Mosaic composites once and **encodes the canvas once per rendition**, then
//! fans the *same* coded packets to every transport (RTSP, HLS, NDI, RTMP/SRT
//! push). A separate encode exists only when codec, resolution, or bitrate
//! differ — never per tile.
//!
//! This module is the pure-Rust **routing/registration data model**: a
//! [`PacketRouter`] maps each [`RenditionId`] to a set of registered
//! [`PacketSink`]s and, on [`PacketRouter::route`], hands every sink an
//! `Arc<EncodedPacket>` — the *same* allocation, by reference. The concrete
//! sinks (the RTSP server, the LL-HLS origin, the NDI sender, the push clients)
//! are feature-gated and implement [`PacketSink`]; they are out of scope here.
//!
//! ## Isolation (invariant #10)
//!
//! [`PacketSink::deliver`] is a synchronous, non-blocking hand-off: an
//! implementation MUST enqueue the reference into its own bounded, drop-oldest
//! buffer and return immediately. It must never block, `.await`, or
//! back-pressure the caller — the engine drives [`PacketRouter::route`] on the
//! output clock and can never be allowed to stall on a slow consumer.
use std::collections::HashMap;
use std::sync::Arc;

/// Stable identifier for an output rendition (one encode at a given
/// codec/resolution/bitrate).
///
/// Renditions are the unit of fan-out: all sinks registered under the same
/// [`RenditionId`] receive that rendition's packets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RenditionId(String);

impl RenditionId {
    /// Construct a rendition id from any string-like value.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RenditionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The kind of coded payload an [`EncodedPacket`] carries.
///
/// The router treats every kind identically (it fans references out
/// unconditionally); the kind is metadata for sinks that need it (e.g. a
/// segmenter that starts a new CMAF part on a keyframe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PacketKind {
    /// A video keyframe (IDR) — a valid segment/part boundary.
    VideoKeyframe,
    /// A non-keyframe video packet (P/B frame).
    VideoDelta,
    /// An audio packet.
    Audio,
}

impl PacketKind {
    /// Whether this packet is a keyframe (a valid segment boundary).
    #[must_use]
    pub const fn is_keyframe(self) -> bool {
        matches!(self, Self::VideoKeyframe)
    }
}

/// One encoded packet emitted by a rendition's encoder.
///
/// Carries timestamps **already re-stamped from the output tick counter**
/// (invariant #3 — raw input PTS never reaches a muxer) and the coded payload
/// as a shared, immutable byte slice. Wrapped in an [`Arc`] by the caller so a
/// single allocation fans out to many sinks without copying.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// The rendition that produced this packet.
    pub rendition: RenditionId,
    /// The payload kind.
    pub kind: PacketKind,
    /// Presentation timestamp, in the output timebase (re-stamped from the
    /// tick counter).
    pub pts: i64,
    /// Decode timestamp, in the output timebase.
    pub dts: i64,
    /// Packet duration, in the output timebase.
    pub duration: i64,
    /// Coded payload. Shared and immutable so the same bytes serve every sink.
    pub data: Arc<[u8]>,
}

/// A transport that consumes a rendition's encoded packets.
///
/// Implementors are the feature-gated servers/clients (RTSP, HLS origin, NDI,
/// RTMP/SRT). [`PacketSink::deliver`] is a **non-blocking** hand-off (see the
/// module-level isolation note).
pub trait PacketSink: Send + Sync {
    /// Stable sink identifier, unique within a rendition.
    fn sink_id(&self) -> &str;

    /// Deliver a packet reference. MUST NOT block or back-pressure the caller:
    /// enqueue the reference into a bounded drop-oldest buffer and return.
    fn deliver(&self, packet: &Arc<EncodedPacket>);
}

/// Routes a single encoded packet stream to many transport sinks, grouped by
/// rendition (the encode-once-mux-many fan-out, invariant #7).
#[derive(Default)]
pub struct PacketRouter {
    renditions: HashMap<RenditionId, Vec<Arc<dyn PacketSink>>>,
}

impl PacketRouter {
    /// Create an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self {
            renditions: HashMap::new(),
        }
    }

    /// Register a sink under a rendition. If a sink with the same
    /// [`PacketSink::sink_id`] is already registered under that rendition it is
    /// replaced (re-registration is idempotent in identity).
    pub fn register(&mut self, rendition: RenditionId, sink: Arc<dyn PacketSink>) {
        let sinks = self.renditions.entry(rendition).or_default();
        let id = sink.sink_id().to_owned();
        if let Some(existing) = sinks.iter_mut().find(|s| s.sink_id() == id) {
            *existing = sink;
        } else {
            sinks.push(sink);
        }
    }

    /// Deregister the sink with `sink_id` from `rendition`. Returns `true` if a
    /// sink was removed.
    pub fn deregister(&mut self, rendition: &RenditionId, sink_id: &str) -> bool {
        let Some(sinks) = self.renditions.get_mut(rendition) else {
            return false;
        };
        let before = sinks.len();
        sinks.retain(|s| s.sink_id() != sink_id);
        let removed = sinks.len() != before;
        if sinks.is_empty() {
            self.renditions.remove(rendition);
        }
        removed
    }

    /// Route a packet to every sink registered under its rendition, handing
    /// each the **same** `Arc<EncodedPacket>` allocation. Returns the number of
    /// sinks the packet was delivered to (`0` if the rendition has none).
    ///
    /// This never fails and never blocks: an absent rendition or a slow sink
    /// must not stall the engine (invariants #1/#10).
    // reason: `route`'s purpose is the side effect (delivering to sinks); the
    // returned sink count is a diagnostic the engine may legitimately ignore,
    // so it must NOT be `#[must_use]`.
    #[allow(clippy::must_use_candidate)]
    pub fn route(&self, packet: &Arc<EncodedPacket>) -> usize {
        let Some(sinks) = self.renditions.get(&packet.rendition) else {
            return 0;
        };
        for sink in sinks {
            sink.deliver(packet);
        }
        sinks.len()
    }

    /// Number of sinks registered under `rendition`.
    #[must_use]
    pub fn sink_count(&self, rendition: &RenditionId) -> usize {
        self.renditions.get(rendition).map_or(0, Vec::len)
    }

    /// Number of distinct renditions with at least one registered sink.
    #[must_use]
    pub fn rendition_count(&self) -> usize {
        self.renditions.len()
    }
}
