//! The shared **outbound presentation epoch** cell (ADR-M010, DEV-C1).
//!
//! One [`WallClockRef`] per program maps output-PTS nanoseconds to disciplined
//! wall-clock nanoseconds. The engine-side sampler derives and re-anchors it
//! (off the hot path, ~1 Hz) and **writes** it here; the egress consumers in
//! this crate **read** it:
//!
//! * the HLS [`LivePlaylist`](crate::hls::LivePlaylist) stamps
//!   `EXT-X-PROGRAM-DATE-TIME` from `epoch.wall_at(segment first PTS)`;
//! * the RTCP [`SrStamper`](crate::rtcp::SrStamper) stamps Sender-Report
//!   NTP↔RTP pairs from the same map.
//!
//! One anchor, every surface agrees — the load-bearing property of ADR-M010.
//!
//! ## Isolation (invariants #1/#10)
//!
//! The cell is read on egress threads only (the segmenter at segment-close
//! cadence, the RTSP seam when a report is built) — never on the output-clock
//! loop — and written at ~1 Hz by the sampler task. The interior `RwLock`
//! guards a 32-byte `Copy` value, so both sides hold it for nanoseconds; a
//! poisoned lock (a panicked writer elsewhere) degrades to the last stored
//! value rather than propagating the panic.

use std::sync::{Arc, RwLock};

use multiview_core::wallclock::WallClockRef;

/// A shared, latest-wins cell carrying the program's outbound presentation
/// epoch (`None` until the sampler first anchors it — consumers emit no
/// fabricated wall mapping before then).
#[derive(Debug, Clone, Default)]
pub struct SharedEpoch {
    cell: Arc<RwLock<Option<WallClockRef>>>,
}

impl SharedEpoch {
    /// An empty cell (no epoch published yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the latest epoch (latest-wins; overwrites any previous map).
    pub fn set(&self, epoch: WallClockRef) {
        let mut guard = match self.cell.write() {
            Ok(guard) => guard,
            // A poisoned lock means a panic elsewhere while writing; the data
            // is a Copy value and always coherent — keep serving it.
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(epoch);
    }

    /// Read the latest epoch, or `None` when nothing was published yet.
    #[must_use]
    pub fn get(&self) -> Option<WallClockRef> {
        match self.cell.read() {
            Ok(guard) => *guard,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}
