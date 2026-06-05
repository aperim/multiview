//! ST 2110-20 video **frame assembler** (pure, always compiled).
//!
//! STUB — IN-1 red commit. The behaviour is implemented in the follow-up green
//! commit; this stub only fixes the public surface so the failing tests compile.

/// Raster geometry the assembler reassembles SRD segments into.
#[derive(Debug, Clone, Copy)]
pub struct RasterGeometry;

impl RasterGeometry {
    /// Construct a raster geometry. STUB: always yields a placeholder.
    #[must_use]
    pub fn new(_width: u32, _height: u32, _bytes_per_line: usize) -> Option<Self> {
        Some(Self)
    }
}

/// One depacketized ST 2110-20 packet handed to the assembler.
#[derive(Debug, Clone)]
pub struct PacketUnit {
    /// RFC 4175 end-of-frame marker bit.
    pub marker: bool,
    /// 90 kHz RTP media timestamp.
    pub timestamp: u32,
    /// 16-bit RTP sequence number.
    pub sequence: u16,
    /// The raw RTP payload the SRD segments point into.
    pub payload: Vec<u8>,
    /// The depacketized -20 payload (SRD segments).
    pub payload_v20: crate::st2110::v20::V20Payload,
}

/// A reassembled (or partial) raster.
#[derive(Debug, Clone)]
pub struct AssembledFrame {
    /// Whether a marker bit closed the frame.
    pub complete: bool,
    /// Whether a sequence gap / loss was detected within the frame.
    pub discontinuity: bool,
    /// The 90 kHz RTP timestamp of the frame, as a producer-timebase raw pts.
    pub raw_pts: i64,
    /// Number of raster lines written into `pixels`.
    pub lines_written: usize,
    /// The line-addressed pixel buffer.
    pub pixels: Vec<u8>,
}

/// The ST 2110-20 frame assembler. STUB.
#[derive(Debug)]
pub struct FrameAssembler;

impl FrameAssembler {
    /// Construct an assembler for `_geometry`. STUB.
    #[must_use]
    pub fn new(_geometry: RasterGeometry) -> Self {
        Self
    }

    /// Push one depacketized packet. STUB: never closes a frame.
    pub fn push(&mut self, _unit: &PacketUnit) -> Option<AssembledFrame> {
        None
    }

    /// Drain a pending partial frame at EOS. STUB.
    pub fn finish(self) -> Option<AssembledFrame> {
        None
    }
}
