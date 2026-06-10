//! The production **alarm driver**: the seam that turns the run's config-declared
//! probes into live analysers and drives their X.733 state machines off the slow
//! control tick (ADR-MV001, M10).
//!
//! [`crate::alarm::state::AlarmStateMachine::from_probe`] builds the *lifecycle*
//! (severity + dwell + latch) from a config [`Probe`](multiview_config::probe::Probe),
//! but a probe declaration also carries the **analyser policy** — the
//! [`luma_threshold`](multiview_config::probe::ProbeKind::Black) /
//! [`difference_threshold`](multiview_config::probe::ProbeKind::Freeze) and the
//! [`DetectionZone`](multiview_config::probe::DetectionZone) — that decides *when
//! the instantaneous fault condition is present*. This module is the missing
//! production wiring: it
//!
//! 1. builds the engine analyser ([`BlackProbe`] / [`FreezeProbe`]) from the
//!    config probe's **operator-authored threshold and zone** — not a hardcoded
//!    default (the config→analyser seam); and
//! 2. drives, per control tick, the analyser over the cell's *already-sampled*
//!    last-good luma → [`AlarmStateMachine::observe`] → an emitted
//!    [`AlarmTransition`] + [`AlarmRecord`] for the telemetry/event layer.
//!
//! ## Isolation (invariant #1 + #10)
//!
//! Everything here is a pure function of injected, already-sampled inputs (a
//! borrowed [`LumaView`] the compositor already read from the lock-free store, and
//! an injected [`MediaTime`]). There are **no clocks, channels, sleeps or I/O**:
//! the engine drives this from its own slow control tick and the driver *returns*
//! transitions for the engine to publish at a frame boundary. A starved probe (no
//! luma sampled this tick) is simply not advanced — it can neither stall the
//! output clock nor back-pressure the engine. The driver holds no resource an
//! input or a client can hold.
//!
//! Audio probes ([`Silence`](multiview_config::probe::ProbeKind::Silence) /
//! [`Loudness`](multiview_config::probe::ProbeKind::Loudness)) carry a live state
//! machine too, but their *condition* comes from the audio meter rather than a
//! luma frame; [`ProbeRunner::observe_audio`] drives those from an
//! externally-measured `condition_present`, so the same lifecycle (dwell, latch,
//! severity) applies uniformly to every probe class.
use std::collections::HashMap;

use multiview_config::probe::{DetectionZone as ConfigZone, Probe, ProbeKind};
use multiview_core::alarm::AlarmRecord;
use multiview_core::time::MediaTime;

use crate::alarm::state::{AlarmStateMachine, AlarmTransition};
use crate::probe::{BlackConfig, BlackProbe, DetectionZone, FreezeConfig, FreezeProbe, LumaView};

/// One sampled frame handed to a probe runner for this tick: the cell's current
/// last-good luma view and, for freeze detection, the previous one.
///
/// Both views are **borrowed** — the engine already sampled them from the
/// lock-free [`TileStore`](multiview_framestore::TileStore) (the same slot the
/// compositor reads), so building a [`ProbeFrame`] is zero-copy and cannot block.
/// `previous` is [`None`] on the first frame a cell produces (or when no prior
/// frame is retained), in which case a freeze probe reports *not frozen* (fail
/// safe toward "live").
#[derive(Debug, Clone, Copy)]
pub struct ProbeFrame<'a> {
    current: &'a LumaView<'a>,
    previous: Option<&'a LumaView<'a>>,
}

impl<'a> ProbeFrame<'a> {
    /// A probe frame from the current luma view and an optional previous one (for
    /// freeze).
    #[must_use]
    pub const fn new(current: &'a LumaView<'a>, previous: Option<&'a LumaView<'a>>) -> Self {
        Self { current, previous }
    }

    /// The current luma view.
    #[must_use]
    pub const fn current(&self) -> &LumaView<'a> {
        self.current
    }

    /// The previous luma view, if any.
    #[must_use]
    pub const fn previous(&self) -> Option<&LumaView<'a>> {
        self.previous
    }
}

/// The engine analyser a config probe maps to, built from the probe's
/// operator-authored threshold and detection zone.
///
/// `Black`/`Freeze` are luma analysers driven by a [`ProbeFrame`]; `Audio` covers
/// silence/loudness, whose condition is measured by the audio meter and supplied
/// to [`ProbeRunner::observe_audio`] rather than derived from a frame.
#[derive(Debug, Clone, Copy)]
enum ProbeAnalyser {
    /// Black-picture analyser (mean luma over the zone at/below the threshold).
    Black(BlackProbe),
    /// Freeze analyser (changed-sample fraction over the zone at/below the
    /// threshold), needing the previous frame.
    Freeze(FreezeProbe),
    /// An audio-domain probe (silence/loudness): no luma analyser; its condition
    /// is supplied by the audio meter.
    Audio,
}

/// Map a config [`DetectionZone`](ConfigZone) (validated `0.0..=1.0` fractions) to
/// the engine [`DetectionZone`].
///
/// A config zone that somehow fails the engine's stricter constructor (it
/// should not for a validated probe) falls back to the full frame, so a
/// malformed zone degrades to "analyse the whole picture" rather than panicking
/// on the control tick.
#[must_use]
pub fn engine_zone(zone: ConfigZone) -> DetectionZone {
    DetectionZone::new(zone.x, zone.y, zone.w, zone.h).unwrap_or(DetectionZone::FULL)
}

/// Build the engine **black** analyser config from a config [`ProbeKind`],
/// threading the operator-authored `luma_threshold` (an 8-bit ceiling) and
/// detection zone into the engine analyser (the config→analyser seam — **not** a
/// hardcoded default). Returns [`None`] for a non-black kind.
///
/// The config threshold is on the same `0.0..=255.0` mean-luma scale the engine
/// analyser compares against, so the widen is exact (`f64::from`, no `as` cast).
#[must_use]
pub fn black_config_from_kind(kind: &ProbeKind) -> Option<BlackConfig> {
    match *kind {
        ProbeKind::Black {
            luma_threshold,
            zone,
        } => Some(BlackConfig {
            luma_threshold: f64::from(luma_threshold),
            zone: engine_zone(zone),
        }),
        _ => None,
    }
}

/// Build the engine **freeze** analyser config from a config [`ProbeKind`],
/// threading the operator-authored `difference_threshold` (a per-mille,
/// `0..=1000`) and detection zone into the engine analyser. Returns [`None`] for a
/// non-freeze kind.
///
/// The per-mille threshold maps to the engine's `0.0..=1.0` changed-sample
/// fraction by dividing by 1000 exactly (`f64::from`, no `as` cast). The remaining
/// engine knobs (the per-sample `diff_tolerance`) keep their defaults.
#[must_use]
pub fn freeze_config_from_kind(kind: &ProbeKind) -> Option<FreezeConfig> {
    match *kind {
        ProbeKind::Freeze {
            difference_threshold,
            zone,
        } => Some(FreezeConfig {
            change_threshold: f64::from(difference_threshold) / 1000.0,
            zone: engine_zone(zone),
            ..FreezeConfig::default()
        }),
        _ => None,
    }
}

impl ProbeAnalyser {
    /// Build the analyser from a config probe kind, threading its
    /// operator-authored threshold and detection zone into the engine analyser
    /// config (the config→analyser seam — **not** a hardcoded default).
    fn from_kind(kind: &ProbeKind) -> Self {
        if let Some(config) = black_config_from_kind(kind) {
            return Self::Black(BlackProbe::new(config));
        }
        if let Some(config) = freeze_config_from_kind(kind) {
            return Self::Freeze(FreezeProbe::new(config));
        }
        // Audio-domain probes, and any future non-luma probe kind (`ProbeKind` is
        // `#[non_exhaustive]`): no luma analyser. Their condition is supplied
        // externally (the audio meter / the future probe's own sampler), so the
        // video path never raises them — they degrade safely to "not driven by a
        // frame" rather than mis-detecting.
        Self::Audio
    }
}

/// A live, config-authored probe: its analyser (built from the operator's
/// threshold + zone) plus its X.733 [`AlarmStateMachine`] and the cell it watches.
///
/// Drive it once per control tick with the cell's sampled luma via
/// [`ProbeRunner::observe_video`] (black/freeze) or with an externally-measured
/// audio condition via [`ProbeRunner::observe_audio`] (silence/loudness); each
/// returns the [`AlarmTransition`] for that tick. The state machine is pure over
/// the injected [`MediaTime`], so the whole runner is deterministic and
/// allocation-free on the control tick (invariants #1 + #10).
#[derive(Debug, Clone)]
pub struct ProbeRunner {
    /// The cell id this probe watches (the run routes that cell's luma here).
    cell: String,
    analyser: ProbeAnalyser,
    machine: AlarmStateMachine,
}

impl ProbeRunner {
    /// Build a runner from an operator-authored config [`Probe`]: its analyser is
    /// built from the probe's threshold + zone, and its lifecycle from
    /// [`AlarmStateMachine::from_probe`] (severity + dwell + latch + scope).
    #[must_use]
    pub fn from_probe(probe: &Probe) -> Self {
        Self {
            cell: probe.cell.clone(),
            analyser: ProbeAnalyser::from_kind(&probe.kind),
            machine: AlarmStateMachine::from_probe(probe),
        }
    }

    /// The cell id this probe watches.
    #[must_use]
    pub fn cell(&self) -> &str {
        &self.cell
    }

    /// The underlying X.733 state machine (for the record snapshot / introspection).
    #[must_use]
    pub const fn machine(&self) -> &AlarmStateMachine {
        &self.machine
    }

    /// Snapshot the current X.733 [`AlarmRecord`] at media time `now`.
    #[must_use]
    pub fn record(&self, now: MediaTime) -> AlarmRecord {
        self.machine.record(now)
    }

    /// Drive the runner one control tick over the cell's sampled `frame`, at media
    /// time `now`.
    ///
    /// Runs the luma analyser to get the instantaneous condition, then steps the
    /// X.733 machine (dwell/latch/hysteresis), returning whether the active state
    /// changed this tick. An **audio** probe ignores the frame and never raises
    /// here — drive it with [`ProbeRunner::observe_audio`] instead — so feeding
    /// every cell's luma to every runner is safe (a silence probe simply reports
    /// `AlarmTransition::None`).
    pub fn observe_video(&mut self, frame: &ProbeFrame<'_>, now: MediaTime) -> AlarmTransition {
        let present = match self.analyser {
            ProbeAnalyser::Black(probe) => probe.detect(frame.current).condition_present,
            ProbeAnalyser::Freeze(probe) => match frame.previous {
                // Freeze needs two frames; with no previous frame it is, by
                // definition, not frozen (fail safe toward "live").
                Some(previous) => probe.detect(frame.current, previous).condition_present,
                None => false,
            },
            // An audio probe is not driven by luma — hold its condition absent on
            // the video path so it is never spuriously raised by a frame.
            ProbeAnalyser::Audio => false,
        };
        self.machine.observe(present, now)
    }

    /// Drive an **audio** probe (silence/loudness) one control tick from an
    /// externally-measured `condition_present` (from the audio meter), at media
    /// time `now`.
    ///
    /// This is the same X.733 lifecycle as the video path; only the *source* of
    /// the instantaneous condition differs. A non-audio probe ignores the audio
    /// reading and reports `AlarmTransition::None`, so routing an audio meter
    /// reading to every runner is safe.
    pub fn observe_audio(&mut self, condition_present: bool, now: MediaTime) -> AlarmTransition {
        let present = matches!(self.analyser, ProbeAnalyser::Audio) && condition_present;
        self.machine.observe(present, now)
    }
}

/// The set of live probe runners for a run, built from its config-declared
/// probes, driven once per control tick.
///
/// This is the production driver the engine attaches off its slow control tick:
/// [`AlarmDriver::observe_cells`] takes the cells' sampled luma for this tick and
/// returns the alarms that raised/cleared (each with its full
/// [`AlarmRecord`]) for the telemetry/event layer to publish. It owns no clock,
/// no channel and no I/O — it is a pure value machine over the injected
/// [`MediaTime`] and the already-sampled frames (invariants #1 + #10).
#[derive(Debug, Clone)]
pub struct AlarmDriver {
    runners: Vec<ProbeRunner>,
}

impl AlarmDriver {
    /// Build the driver from the run's config-declared probes (read-only; the
    /// engine never mutates the config).
    #[must_use]
    pub fn from_probes(probes: &[Probe]) -> Self {
        Self {
            runners: probes.iter().map(ProbeRunner::from_probe).collect(),
        }
    }

    /// The number of probe runners (one per declared probe).
    #[must_use]
    pub fn len(&self) -> usize {
        self.runners.len()
    }

    /// Whether the driver has no probes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runners.is_empty()
    }

    /// The probe runners (introspection / record snapshots).
    #[must_use]
    pub fn runners(&self) -> &[ProbeRunner] {
        &self.runners
    }

    /// Drive every **video** probe one control tick from the per-cell sampled luma
    /// `frames` (`cell_id -> ProbeFrame`), at media time `now`.
    ///
    /// Routes each runner's watched cell's frame to it; a probe whose cell has no
    /// frame this tick (a starved/absent input) is **not advanced** — never a
    /// panic, never a spurious transition. Returns one `(transition, record)` pair
    /// for each probe whose active state **changed** this tick (raised or
    /// cleared); steady-state ticks return an empty vector, so the engine publishes
    /// only on change (not a per-tick flood — inv #10).
    #[must_use]
    pub fn observe_cells(
        &mut self,
        frames: &HashMap<String, ProbeFrame<'_>>,
        now: MediaTime,
    ) -> Vec<(AlarmTransition, AlarmRecord)> {
        let mut out = Vec::new();
        for runner in &mut self.runners {
            let Some(frame) = frames.get(&runner.cell) else {
                continue;
            };
            let transition = runner.observe_video(frame, now);
            if transition != AlarmTransition::None {
                out.push((transition, runner.record(now)));
            }
        }
        out
    }
}
