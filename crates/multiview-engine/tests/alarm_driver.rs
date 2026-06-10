//! End-to-end driver test for the config-probe → analyser → X.733 alarm seam
//! (ADR-MV001, M10).
//!
//! These tests pin the *production path*, not just the constructor: a
//! config-authored [`Probe`] declaration is turned into a live analyser **with
//! the operator's threshold and detection zone** (not a hardcoded default) and a
//! per-tick driver runs that analyser over sampled luma, drives the X.733
//! [`AlarmStateMachine`], and emits a raise/clear transition with a full
//! [`AlarmRecord`]. This is the wiring the review correctly flagged as missing:
//! `from_probe` had no production caller and the analyser threshold/zone were
//! dropped on the floor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::probe::{DetectionZone, Dwell, Probe, ProbeKind};
use multiview_core::alarm::{AlarmKind, AlarmScope, PerceivedSeverity};
use multiview_core::time::MediaTime;
use multiview_engine::alarm::driver::{AlarmDriver, ProbeFrame, ProbeRunner};
use multiview_engine::alarm::AlarmTransition;
use multiview_engine::probe::LumaView;

const W: u32 = 32;
const H: u32 = 18;

fn px(n: u32) -> usize {
    usize::try_from(n).expect("frame index fits usize")
}

/// A tightly-packed `W x H` luma plane filled with a constant value.
fn flat(value: u8) -> Vec<u8> {
    vec![value; px(W * H)]
}

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

fn black_probe(luma_threshold: u8, zone: DetectionZone, dwell: Dwell) -> Probe {
    Probe::new(
        "blk",
        "cam-1",
        ProbeKind::black(luma_threshold, zone),
        dwell,
        PerceivedSeverity::Major,
        false,
    )
}

#[test]
fn runner_honours_the_config_declared_luma_threshold() {
    // The config declares a luma threshold of 40. A field at 30 (<= 40) is black;
    // a field at 50 (> 40) is not. A hardcoded default threshold of 16 would get
    // BOTH wrong (30 and 50 are both > 16 → never black), so this fails unless the
    // config threshold is actually threaded into the analyser.
    let probe = black_probe(40, DetectionZone::default(), Dwell::new(0, 0));
    let mut runner = ProbeRunner::from_probe(&probe);

    let dark = flat(30);
    let dark_view = LumaView::packed(&dark, W, H).unwrap();
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&dark_view, None), ms(0)),
        AlarmTransition::Raised,
        "field at 30 is at/below the declared threshold 40 → black raises"
    );

    let bright = flat(50);
    let bright_view = LumaView::packed(&bright, W, H).unwrap();
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&bright_view, None), ms(1)),
        AlarmTransition::Cleared,
        "field at 50 is above the declared threshold 40 → clears (zero dwell-down)"
    );
}

#[test]
fn runner_honours_the_config_declared_detection_zone() {
    // A zone covering only the LEFT half of the frame. The left half is black
    // (luma 0), the right half is bright (luma 200). With a left-half zone the
    // mean luma the analyser sees is 0 → black. A full-frame default zone would
    // see ~100 (> threshold) → NOT black, so this fails unless the config zone is
    // threaded through.
    let left_half = DetectionZone::new(0.0, 0.0, 0.5, 1.0);
    let probe = black_probe(16, left_half, Dwell::new(0, 0));
    let mut runner = ProbeRunner::from_probe(&probe);

    let mut buf = vec![200u8; px(W * H)];
    for y in 0..H {
        for x in 0..(W / 2) {
            buf[px(y * W + x)] = 0;
        }
    }
    let view = LumaView::packed(&buf, W, H).unwrap();
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&view, None), ms(0)),
        AlarmTransition::Raised,
        "left-half zone sees only the black half → raises despite the bright right half"
    );
}

#[test]
fn runner_drives_dwell_to_raise_then_clear() {
    // Dwell-up 100 ms, dwell-down 200 ms: a sustained black field raises only at
    // the dwell-up boundary, and recovery clears only at the dwell-down boundary.
    let probe = black_probe(16, DetectionZone::default(), Dwell::new(100, 200));
    let mut runner = ProbeRunner::from_probe(&probe);

    let dark = flat(8);
    let dark_view = LumaView::packed(&dark, W, H).unwrap();
    let bright = flat(120);
    let bright_view = LumaView::packed(&bright, W, H).unwrap();

    // Black present from t=0; still pending until 100 ms elapse.
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&dark_view, None), ms(0)),
        AlarmTransition::None
    );
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&dark_view, None), ms(99)),
        AlarmTransition::None
    );
    assert!(!runner.machine().is_active());
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&dark_view, None), ms(100)),
        AlarmTransition::Raised
    );
    assert!(runner.machine().is_active());
    assert_eq!(
        runner.machine().current_severity(),
        PerceivedSeverity::Major
    );

    // Recovery at t=110: holds Raised through the 200 ms dwell-down, clears at the
    // boundary (since=110 → 310).
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&bright_view, None), ms(110)),
        AlarmTransition::None
    );
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&bright_view, None), ms(309)),
        AlarmTransition::None
    );
    assert!(runner.machine().is_active());
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&bright_view, None), ms(310)),
        AlarmTransition::Cleared
    );
    assert!(!runner.machine().is_active());
}

#[test]
fn runner_honours_the_config_freeze_difference_threshold() {
    // A freeze probe declares a per-mille difference threshold of 5 (0.5 %). Two
    // identical frames have 0 changed samples (<= 0.5 %) → frozen. A frame where
    // ~6 % of samples changed is NOT frozen.
    let probe = Probe::new(
        "frz",
        "cam-1",
        ProbeKind::freeze(5, DetectionZone::default()),
        Dwell::new(0, 0),
        PerceivedSeverity::Minor,
        false,
    );
    let mut runner = ProbeRunner::from_probe(&probe);

    let a = flat(120);
    let a_view = LumaView::packed(&a, W, H).unwrap();
    // First frame: no previous → not frozen (no change metric yet).
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&a_view, None), ms(0)),
        AlarmTransition::None
    );
    // Identical successive frame → 0 % changed → frozen → raises (zero dwell-up).
    let b = flat(120);
    let b_view = LumaView::packed(&b, W, H).unwrap();
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&b_view, Some(&a_view)), ms(1)),
        AlarmTransition::Raised
    );
    assert_eq!(runner.machine().kind(), AlarmKind::Freeze);

    // A frame where many samples changed (> 0.5 %) → not frozen → clears.
    let mut c = vec![120u8; px(W * H)];
    // 10 % of samples flipped to black — well above 0.5 %.
    for px in c.iter_mut().take(px(W * H) / 10) {
        *px = 0;
    }
    let c_view = LumaView::packed(&c, W, H).unwrap();
    assert_eq!(
        runner.observe_video(&ProbeFrame::new(&c_view, Some(&b_view)), ms(2)),
        AlarmTransition::Cleared
    );
}

#[test]
fn driver_built_from_config_probes_raises_per_cell() {
    // A driver built from the run's config-declared probes routes each cell's
    // sampled luma to the matching probe(s) and drives their state machines. This
    // is the production per-tick path: feed a cell's luma each tick and read back
    // which alarms transitioned.
    let probes = vec![
        black_probe(16, DetectionZone::default(), Dwell::new(0, 0)),
        Probe::new(
            "blk-2",
            "cam-2",
            ProbeKind::black(16, DetectionZone::default()),
            Dwell::new(0, 0),
            PerceivedSeverity::Critical,
            false,
        ),
    ];
    let mut driver = AlarmDriver::from_probes(&probes);
    assert_eq!(driver.len(), 2);

    let dark = flat(4);
    let dark_view = LumaView::packed(&dark, W, H).unwrap();
    let bright = flat(200);
    let bright_view = LumaView::packed(&bright, W, H).unwrap();

    // cam-1 is black, cam-2 is bright this tick.
    let mut frames = std::collections::HashMap::new();
    frames.insert("cam-1".to_owned(), ProbeFrame::new(&dark_view, None));
    frames.insert("cam-2".to_owned(), ProbeFrame::new(&bright_view, None));
    let transitions = driver.observe_cells(&frames, ms(0));

    // Exactly the cam-1 probe raised; it carries the full record with the declared
    // severity and probe scope.
    assert_eq!(transitions.len(), 1);
    let (transition, record) = &transitions[0];
    assert_eq!(*transition, AlarmTransition::Raised);
    assert_eq!(record.id.as_str(), "blk");
    assert_eq!(record.kind, AlarmKind::Black);
    assert_eq!(record.severity, PerceivedSeverity::Major);
    assert_eq!(
        record.scope,
        AlarmScope::Probe {
            id: "blk".to_owned()
        }
    );

    // Next tick: cam-1 recovers (bright), cam-2 goes black → cam-1 clears, cam-2
    // raises at Critical.
    let mut frames = std::collections::HashMap::new();
    frames.insert("cam-1".to_owned(), ProbeFrame::new(&bright_view, None));
    frames.insert("cam-2".to_owned(), ProbeFrame::new(&dark_view, None));
    let transitions = driver.observe_cells(&frames, ms(1));
    assert_eq!(transitions.len(), 2);
    let kinds: std::collections::HashMap<&str, AlarmTransition> = transitions
        .iter()
        .map(|(t, r)| (r.id.as_str(), *t))
        .collect();
    assert_eq!(kinds.get("blk"), Some(&AlarmTransition::Cleared));
    assert_eq!(kinds.get("blk-2"), Some(&AlarmTransition::Raised));
}

#[test]
fn driver_skips_cells_with_no_frame_this_tick() {
    // A probe whose watched cell is starved (no luma sampled this tick) is simply
    // not advanced — never a panic, never a spurious transition (isolation #1/#10:
    // an absent input cannot drive the alarm engine into a wrong state).
    let probes = vec![black_probe(16, DetectionZone::default(), Dwell::new(0, 0))];
    let mut driver = AlarmDriver::from_probes(&probes);
    let frames: std::collections::HashMap<String, ProbeFrame<'_>> =
        std::collections::HashMap::new();
    let transitions = driver.observe_cells(&frames, ms(0));
    assert!(transitions.is_empty());
    assert!(!driver.runners()[0].machine().is_active());
}
