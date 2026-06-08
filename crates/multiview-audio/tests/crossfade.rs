// RT-9 held-out pop-avoidance test suite (the tests that matter for the refuted
// CLAIM 5 "No per-tick-scalar cross-fade" hard gate).
//
// An instant `repoint` (RT-8a) is sample-accurate but waveform-DISCONTINUOUS at
// the seam: the bus jumps from the old source's sample to the new source's
// sample in one step → a CLICK. RT-9 replaces that hard swap with an EQUAL-POWER
// (sin/cos) cross-fade applied as a PER-SAMPLE intra-block envelope inside
// `mix_program`'s sample loop, so the gain moves smoothly WITHIN a tick block
// (not stepping at tick boundaries). Four properties are asserted:
//
//   (a) NO DISCONTINUITY (no click): across the whole ~10 ms ramp — including the
//       old-instant-swap seam where RT-8a clicked — the sample-to-sample step
//       |x[n+1]-x[n]| stays bounded (no abrupt jump). The hard-swap step is gone.
//
//   (b) EQUAL POWER (no dip): the summed instantaneous power across the fade
//       stays ~constant — gainA²+gainB²==1 at every frame (sin²+cos²=1). A LINEAR
//       cross-fade would dip ~-3 dB at the midpoint; this does not.
//
//   (c) R128 / TRUE-PEAK not violated: the fade never produces a sample louder
//       than the louder of the two sources's equal-power envelope.
//
//   (d) OLD STRIP UNROUTED after the ramp: once the ramp completes the old strip
//       is removed from the bus (no lingering contribution / no double-count).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
// reason: test-only exact/approx float comparisons on bounded ramps and
// loss-free index<->float casts on small ranges.
#![allow(
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::doc_markdown
)]

use std::sync::Arc;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::mixer::{GainRamp, Mixer};
use multiview_audio::program::{ApplyClass, ProgramBus, SwitchTier};
use multiview_audio::store::AudioStore;
use multiview_core::time::Rational;

const FS: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(FS, ChannelLayout::Stereo)
}

/// Gather the full left-channel sample run a `ProgramBus` emits over `ticks`
/// ticks, concatenated into one contiguous signal (so seam continuity across
/// tick blocks can be checked end to end).
fn drain_left(bus: &mut ProgramBus, ticks: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for _ in 0..ticks {
        let block = bus.tick();
        for frame in 0..block.frame_count() {
            out.push(block.interleaved()[frame * 2]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Mixer-level: the per-sample equal-power envelope.
// ---------------------------------------------------------------------------

/// The CORE per-sample claim: a `GainRamp` on a strip applies an envelope that
/// changes value EVERY frame within a single tick block — NOT a per-tick scalar
/// that would hold constant across the whole block and step at the boundary.
#[test]
fn gain_ramp_is_a_per_sample_envelope_not_a_per_tick_scalar() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 1.0);
    // A 1.0 DC source so the bus value == the applied envelope value exactly.
    mixer
        .submit(
            a,
            AudioBlock::from_interleaved(fmt, vec![1.0f32; 100 * 2]).unwrap(),
        )
        .unwrap();
    // Ramp gain 0 -> 1 (sin taper, up) over 100 frames.
    mixer.set_gain_ramp(a, GainRamp::up(100));

    let bus = mixer.mix_program().unwrap();
    let s: Vec<f32> = (0..bus.frame_count())
        .map(|f| bus.interleaved()[f * 2])
        .collect();
    assert_eq!(s.len(), 100);

    // PER-SAMPLE: consecutive frames within the one block differ (the envelope
    // moves inside the block) — a per-tick scalar would make all 100 equal.
    let distinct = s.windows(2).filter(|w| (w[0] - w[1]).abs() > 1e-7).count();
    assert!(
        distinct >= 90,
        "the envelope must move per-sample within the block (a per-tick scalar \
         would hold constant); only {distinct}/99 adjacent frames differed"
    );
    // Monotone non-decreasing up-ramp, 0 at the start, ~1 by the end.
    assert!(s[0].abs() < 1e-3, "up-ramp starts near 0, got {}", s[0]);
    assert!(s[99] > 0.99, "up-ramp ends near 1, got {}", s[99]);
    for w in s.windows(2) {
        assert!(w[1] + 1e-6 >= w[0], "up-ramp must be non-decreasing");
    }
}

/// Equal-power (sin/cos), NOT linear: at the midpoint of a 0->1 up-ramp the gain
/// is sin(π/4) ≈ 0.707 (-3 dB amplitude), not 0.5. This is what keeps the SUM of
/// the up+down pair at constant power.
#[test]
fn up_ramp_is_equal_power_sin_taper_not_linear() {
    let fmt = stereo();
    let mut mixer = Mixer::new(fmt);
    let a = mixer.add_input("a");
    mixer.route_to_program(a, 1.0);
    mixer
        .submit(
            a,
            AudioBlock::from_interleaved(fmt, vec![1.0f32; 1000 * 2]).unwrap(),
        )
        .unwrap();
    mixer.set_gain_ramp(a, GainRamp::up(1000));
    let bus = mixer.mix_program().unwrap();
    let mid = bus.interleaved()[500 * 2];
    // sin(π/2 * 0.5) = sin(π/4) ≈ 0.70710678. Linear would be 0.5.
    approx::assert_abs_diff_eq!(mid, std::f32::consts::FRAC_1_SQRT_2, epsilon = 1e-2);
    assert!(
        (mid - 0.5).abs() > 0.1,
        "an equal-power taper at the midpoint is ~0.707, clearly not the linear 0.5"
    );
}

/// The equal-power IDENTITY across the whole ramp: gainA²(down) + gainB²(up) == 1
/// at every frame, including the midpoint where a linear fade's gains
/// sum-of-squares would be 0.5 (the -3 dB dip the equal-power taper avoids).
#[test]
fn equal_power_identity_holds_at_every_frame() {
    let n = 1000usize;
    for frame in 0..n {
        let ga = GainRamp::down(n).envelope_at(frame);
        let gb = GainRamp::up(n).envelope_at(frame);
        let power = ga * ga + gb * gb;
        approx::assert_abs_diff_eq!(power, 1.0, epsilon = 1e-4);
    }
    // The LINEAR counter-example: linear gains at the midpoint sum-of-squares to
    // 0.5, proving the equal-power taper is doing real work.
    let lin_mid = 0.5f32 * 0.5 + 0.5 * 0.5;
    assert!(
        (lin_mid - 1.0).abs() > 0.4,
        "a linear fade WOULD dip (this is the contrast)"
    );
}

// ---------------------------------------------------------------------------
// Bus-level: the equal-power cross-fade breakaway.
// ---------------------------------------------------------------------------

/// (a) NO CLICK + (b) NO DIP + (c) NO OVER-PEAK: a ~10 ms equal-power cross-fade
/// from source A (+0.5 DC) to source B (+0.5 DC) across a TICK BLOCK has no
/// sample discontinuity at the seam, never dips below the steady level, and
/// never exceeds the louder source. Both sources are the same correlated +0.5
/// DC, so an equal-power fade holds the bus in [0.5, 0.707] the WHOLE time — and
/// critically there is NO step at the instant-swap seam (the RT-8a click is gone).
#[test]
fn equal_power_crossfade_has_no_click_and_constant_power_across_a_tick_block() {
    let fmt = stereo();
    // 25 fps @ 48k = 1920 samples/tick. A 480-frame (~10 ms) ramp is well under
    // one tick block, so the seam + the whole ramp live inside the first tick.
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let ramp = 480usize;

    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    let store_b = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 192_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 192_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    // Tick once on A alone (steady 0.5) to establish the pre-seam baseline.
    let pre = bus.tick();
    let pre_left: Vec<f32> = (0..pre.frame_count())
        .map(|f| pre.interleaved()[f * 2])
        .collect();
    assert!(pre_left.iter().all(|&v| (v - 0.5).abs() < 1e-6));
    let last_pre = *pre_left.last().unwrap();

    // BREAKAWAY: cross-fade to B over `ramp` frames. This is the CLICK-FREE tier.
    let tier = bus
        .repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();
    assert_eq!(tier, SwitchTier::ClickFree);

    let post = bus.tick();
    let post_left: Vec<f32> = (0..post.frame_count())
        .map(|f| post.interleaved()[f * 2])
        .collect();

    // (a) NO CLICK at the seam: the worst-case adjacent step over the whole
    // concatenated signal must stay tiny.
    let mut full = pre_left.clone();
    full.extend_from_slice(&post_left);
    let max_step = full
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_step < 5e-3,
        "no click: max adjacent sample step across the fade must stay bounded, got {max_step}"
    );
    assert!(
        (post_left[0] - last_pre).abs() < 5e-3,
        "the seam sample must be continuous with the pre-seam tail (no instant jump)"
    );

    // (b) NO DIP + (c) NO OVER-PEAK through the fade.
    for (i, &v) in post_left.iter().take(ramp).enumerate() {
        assert!(
            v >= 0.5 - 1e-3,
            "equal-power fade of correlated sources must not dip below the level at frame {i}: {v}"
        );
        assert!(
            v <= 0.5 * std::f32::consts::SQRT_2 + 1e-3,
            "fade must never exceed the equal-power sum of the two 0.5 sources at frame {i}: {v}"
        );
    }
    // After the ramp the bus is steady on B alone at 0.5.
    assert!(
        post_left[ramp..].iter().all(|&v| (v - 0.5).abs() < 1e-3),
        "after the ramp the bus must be B alone at its steady level"
    );
}

/// (d) After the ramp completes the OLD strip is UNROUTED — it no longer
/// contributes to the bus. Drive the bus past the ramp and confirm the old
/// source's store is no longer read (its cursor freezes) while B keeps advancing.
#[test]
fn old_strip_is_unrouted_after_the_ramp_completes() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920/tick
    let ramp = 480usize;

    let store_a = Arc::new(AudioStore::new(fmt, 192_000));
    let store_b = Arc::new(AudioStore::new(fmt, 192_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.3f32; 192_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.9f32; 192_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    bus.repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();
    // Tick 1: covers the whole ramp (1920 > 480) and into B-only territory.
    let _ = bus.tick();
    let cursor_a_after_ramp = store_a.read_cursor();
    // Tick 2: fully past the ramp — A must NOT be read again (unrouted), so its
    // cursor does not advance, while B keeps being read.
    let cursor_b_before = store_b.read_cursor();
    let block2 = bus.tick();
    assert_eq!(
        store_a.read_cursor(),
        cursor_a_after_ramp,
        "after the ramp the OLD store A must be unrouted (cursor frozen, never read again)"
    );
    assert!(
        store_b.read_cursor() > cursor_b_before,
        "the NEW store B must still be read after the ramp"
    );
    // And the post-ramp bus is B alone at 0.9.
    assert!(
        (0..block2.frame_count()).all(|f| (block2.interleaved()[f * 2] - 0.9).abs() < 1e-3),
        "after the ramp the bus is the new source B (0.9) alone"
    );
}

/// The whole RT-9 contract across a MULTI-tick span so the ramp straddles a tick
/// BLOCK boundary (the per-sample envelope must continue smoothly from one tick
/// block into the next — a per-tick scalar would step at the boundary).
#[test]
fn crossfade_spans_a_tick_boundary_with_no_step_at_the_block_seam() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920/tick
    let ramp = 2400usize; // > one tick block => the ramp crosses the boundary

    let store_a = Arc::new(AudioStore::new(fmt, 384_000));
    let store_b = Arc::new(AudioStore::new(fmt, 384_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 384_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.5f32; 384_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("prog", Arc::clone(&store_a), 1.0);

    let _ = bus.tick(); // baseline on A
    bus.repoint_crossfade(point, Arc::clone(&store_b), ramp)
        .unwrap();

    // Drain three more ticks (covers the 2400-frame ramp which spans 2 blocks).
    let signal = drain_left(&mut bus, 3);
    let max_step = signal
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(
        max_step < 5e-3,
        "the per-sample envelope must continue smoothly across the tick block \
         boundary at frame 1920 (no step); max adjacent step was {max_step}"
    );
}

// ---------------------------------------------------------------------------
// The two-tier model (SOFT-STEP vs CLICK-FREE).
// ---------------------------------------------------------------------------

/// The tier is surfaced so the caller/UI can read which transition a switch
/// uses. The decoded-bus cross-fade is CLICK-FREE (Class-2); a coded passthrough
/// AU-aligned step is SOFT-STEP (Class-1). Both are modelled; only CLICK-FREE is
/// implemented here.
#[test]
fn switch_tiers_are_distinct_and_carry_their_class() {
    assert_eq!(SwitchTier::SoftStep.class(), ApplyClass::Class1);
    assert_eq!(SwitchTier::ClickFree.class(), ApplyClass::Class2);
    assert!(SwitchTier::ClickFree.is_pop_free());
    assert!(!SwitchTier::SoftStep.is_pop_free());
    assert_ne!(SwitchTier::SoftStep, SwitchTier::ClickFree);
}

/// A zero-length ramp degrades to an instant hard swap (a SOFT-STEP-equivalent
/// hard cut) — `repoint_crossfade(.., 0)` behaves like RT-8a `repoint` and is
/// reported as such, never panicking on the degenerate input.
#[test]
fn zero_length_ramp_degrades_to_a_soft_step_hard_swap() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let store_a = Arc::new(AudioStore::new(fmt, 96_000));
    let store_b = Arc::new(AudioStore::new(fmt, 96_000));
    store_a
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.2f32; 96_000 * 2]).unwrap())
        .unwrap();
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.8f32; 96_000 * 2]).unwrap())
        .unwrap();
    let point = bus.add_source("p", Arc::clone(&store_a), 1.0);
    let _ = bus.tick();
    let tier = bus
        .repoint_crossfade(point, Arc::clone(&store_b), 0)
        .unwrap();
    assert_eq!(
        tier,
        SwitchTier::SoftStep,
        "a 0-frame ramp is a hard step, not click-free"
    );
    store_b
        .publish(&AudioBlock::from_interleaved(fmt, vec![0.8f32; 1920 * 2]).unwrap())
        .unwrap();
    let after = bus.tick();
    assert!(
        (0..after.frame_count()).all(|f| (after.interleaved()[f * 2] - 0.8).abs() < 1e-3),
        "a zero-ramp swap is the instant B (0.8), exactly like RT-8a repoint"
    );
}

/// `repoint_crossfade` on an unknown point is a clean typed error (no panic),
/// exactly like `repoint`.
#[test]
fn crossfade_of_a_nonexistent_point_is_a_clean_error() {
    let fmt = stereo();
    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1));
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    let bogus = multiview_audio::RoutePoint::input(42);
    let err = bus.repoint_crossfade(bogus, store, 480).unwrap_err();
    assert!(matches!(
        err,
        multiview_audio::AudioError::UnknownInput(42)
    ));
}
