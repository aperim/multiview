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

    /// Route a source's audio store onto the program bus at linear `program_gain`.
    ///
    /// The store must already be at the bus [`format`](Self::format) (every
    /// source decode resamples to the canonical program format upstream).
    pub fn add_source(&mut self, id: impl Into<String>, store: Arc<AudioStore>, program_gain: f64) {
        let point = self.mixer.add_input(id);
        self.mixer.route_to_program(point, program_gain);
        self.routes.push((point, store));
    }

    /// The program-bus format.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Mix one output tick of program audio.
    ///
    /// Advances the [`SampleClock`](crate::cadence::SampleClock) to get this
    /// tick's exact sample budget, pulls that many frames from each routed
    /// source's store (silence for a stalled/empty source), mixes the program
    /// bus, and returns an [`AudioBlock`] of exactly the budgeted length — never
    /// shorter, never absent (the audio continuity guarantee).
    pub fn tick(&mut self) -> AudioBlock {
        let frames = self.clock.next_tick();
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
