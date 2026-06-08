//! The program-bus mix on the output clock (AUD-3, [`ProgramBus`]).
//!
//! [`ProgramBus`] composes the three audio primitives into the one operation the
//! output clock needs each tick:
//!
//! * [`SampleClock`](crate::cadence::SampleClock) — how many samples this tick
//!   owes (exact, drift-free; the audio side of invariant #1/#3);
//! * one [`AudioStore`](crate::store::AudioStore) per routed source — the
//!   last-good, silence-filling, lock-free read (a stalled source is silence,
//!   never a gap);
//! * the [`Mixer`](crate::mixer::Mixer) — sum the routed inputs at their program
//!   gains into the program bus.
//!
//! Each [`ProgramBus::tick`] pulls exactly `samples_per_tick` frames from every
//! source and returns the mixed program [`AudioBlock`] of exactly that length.
//! It does **no I/O and cannot block** — the stores are wait-free reads — so it
//! is safe to call on the output-clock hot loop (invariant #1), and it never
//! back-pressures any input (invariant #10). The encode + dual-stream mux of the
//! returned block is AUD-4.

use std::sync::Arc;

use multiview_core::time::Rational;

use crate::cadence::SampleClock;
use crate::format::{AudioBlock, AudioFormat};
use crate::mixer::{Mixer, RoutePoint};
use crate::store::AudioStore;

/// The program-bus mixer driven by the output clock.
///
/// Build one per run at the canonical program format with [`ProgramBus::new`],
/// register each source with [`ProgramBus::add_source`], then call
/// [`ProgramBus::tick`] once per output tick.
#[derive(Debug)]
pub struct ProgramBus {
    mixer: Mixer,
    clock: SampleClock,
    /// Per routed source: its mixer route point + the store the output clock
    /// pulls from. Shared (`Arc`) with the source's decode thread, which
    /// publishes into the same store.
    routes: Vec<(RoutePoint, Arc<AudioStore>)>,
    format: AudioFormat,
}

impl ProgramBus {
    /// Build an empty program bus at `format`, paced by output cadence `fps`.
    #[must_use]
    pub fn new(format: AudioFormat, fps: Rational) -> Self {
        Self {
            mixer: Mixer::new(format),
            clock: SampleClock::new(format.sample_rate(), fps),
            routes: Vec::new(),
            format,
        }
    }

    /// Route a source's audio store onto the program bus at linear `program_gain`,
    /// returning the [`RoutePoint`] that now reads it.
    ///
    /// The store must already be at the bus [`format`](Self::format) (every
    /// source decode resamples to the canonical program format upstream).
    ///
    /// This **appends** a new route; to swap which store an *existing* route
    /// point reads (an audio breakaway / re-point), use
    /// [`repoint`](Self::repoint).
    pub fn add_source(
        &mut self,
        id: impl Into<String>,
        store: Arc<AudioStore>,
        program_gain: f64,
    ) -> RoutePoint {
        let point = self.mixer.add_input(id);
        self.mixer.route_to_program(point, program_gain);
        self.routes.push((point, store));
        point
    }

    /// Re-point an **existing** route point onto a different `Arc<AudioStore>`
    /// (replace semantics) — the audio-breakaway primitive (RT-8a).
    ///
    /// Distinct from [`add_source`](Self::add_source)'s append: this swaps the
    /// store bound to a route point already on the bus, leaving its mixer strip
    /// (gain/route/id) untouched, so the program bus channel now reads the new
    /// source. To keep the switch **sample-aligned at the seam** (no silence gap,
    /// no climb from frame 0 through a warm store's evicted history), the new
    /// store's read cursor is seeked to its live edge
    /// ([`AudioStore::seek_to_live_edge`]) as part of the re-point.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::UnknownInput`](crate::error::AudioError::UnknownInput)
    /// if `point` is not a route on this bus — an honest error, never a panic and
    /// never a silent wrong-source bind.
    pub fn repoint(
        &mut self,
        point: RoutePoint,
        store: Arc<AudioStore>,
    ) -> crate::error::Result<()> {
        let slot = self
            .routes
            .iter_mut()
            .find(|(p, _)| *p == point)
            .map(|(_, s)| s)
            .ok_or(crate::error::AudioError::UnknownInput(point.index()))?;
        // Align the warm store to its live edge so the seam reads fresh audio,
        // not silence climbing from frame 0 through evicted history.
        store.seek_to_live_edge();
        *slot = store;
        Ok(())
    }

    /// The program-bus format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Mix one output tick of program audio.
    ///
    /// Advances the [`SampleClock`] by one tick to
    /// get this tick's exact sample budget, then mixes (see [`tick_to`](Self::tick_to)
    /// for the per-source pull). Use this when the consumer is guaranteed to call
    /// exactly once per output tick; under `DropOnOverload` (where some ticks are
    /// skipped) drive the bus by the absolute tick index with
    /// [`tick_to`](Self::tick_to) instead, so audio cannot drift from video
    /// (invariant #3).
    pub fn tick(&mut self) -> AudioBlock {
        let frames = self.clock.next_tick();
        self.mix(frames)
    }

    /// Mix program audio up to the absolute output **tick index** `target_tick`.
    ///
    /// Advances the [`SampleClock`] to `target_tick`,
    /// **catching up the samples for any ticks that were skipped** since the last
    /// call (e.g. a `DropOnOverload` gap), and returns an [`AudioBlock`] of
    /// exactly that many frames. Because the sample budget is a pure function of
    /// the tick index — not paced by surviving frames — the cumulative emitted
    /// samples stay locked to `SampleClock::total_at(target_tick)`, so audio
    /// never drifts away from the video tick timeline (invariant #3, RT-8a). A
    /// `target_tick` at or behind the clock is a no-op (returns a zero-length
    /// block; the clock never rewinds).
    ///
    /// This is the API RT-8b drives from the output tick index carried on each
    /// `StreamItem`; combined with [`repoint`](Self::repoint)'s live-edge seek it
    /// keeps an audio breakaway lip-synced across overload gaps.
    pub fn tick_to(&mut self, target_tick: u64) -> AudioBlock {
        let frames = self.clock.advance_to(target_tick);
        self.mix(frames)
    }

    /// Pull `frames` from each routed source's store and mix the program bus,
    /// returning an [`AudioBlock`] of exactly `frames` frames — never shorter,
    /// never absent (the audio continuity guarantee). Does no I/O and cannot
    /// block (the stores are wait-free reads), so it is safe on the output-clock
    /// hot loop (invariant #1) and never back-pressures any input (invariant #10).
    fn mix(&mut self, frames: usize) -> AudioBlock {
        // Disjoint field borrows: `routes` (read) + `mixer` (mutated) are
        // different fields, so the per-source pull-and-submit is borrow-clean.
        let Self {
            mixer,
            routes,
            format,
            ..
        } = self;
        for (point, store) in routes.iter() {
            // `read` always returns exactly `frames` frames at the store format
            // (= the bus format), so `submit` cannot fail on a format mismatch;
            // a (impossible) error degrades to dropping that input this tick
            // rather than panicking on the hot loop.
            let block = store.read(frames);
            let _ = mixer.submit(*point, block);
        }
        // The mix is `frames` long whenever any source submitted (each submits
        // exactly `frames`); with no sources it is empty — fall back to a full
        // budget of silence so the program bus is never short or absent.
        match mixer.mix_program() {
            Some(block) if block.frame_count() == frames => block,
            _ => AudioBlock::silence(*format, frames),
        }
    }
}
