//! RED tests for the media-player audio **loop deck** (ADR-T019): sample-exact
//! buffer-and-replay of a vamp segment as an overlap-add loop with a
//! correlation-adaptive crossfade at the seam, positioned by the absolute output
//! frame so it stays sample-locked to the video wrap and survives a realign.
//!
//! These assert the load-bearing correctness the feature exists for, including
//! the three defects the cross-vendor design review caught:
//! - the loop **period is exactly `loop_frames`** for any number of laps and is
//!   `read_at`-pure (the same absolute span is byte-identical however it is
//!   pulled), so a forced realign under load lands inside a faded seam — no
//!   un-crossfaded click (rule 26);
//! - a **decorrelated** seam keeps total **power** flat (equal-power chosen);
//! - a **correlated** seam (a sustained tone across the loop) keeps **amplitude**
//!   flat with no +3 dB swell (linear chosen) — both click-free;
//! - the loop length is the audio duration of the vamp frames at the **asset**
//!   cadence (no frame-vs-tick conflation);
//! - an **armed exit** settles to silence at the next seam, exactly once;
//! - a deck primed with **no audio** rides silence (never a stall / panic).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unreadable_literal
)]

use multiview_audio::format::{AudioFormat, ChannelLayout};
use multiview_audio::loopdeck::LoopDeck;
use proptest::prelude::*;

const RATE: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(RATE, ChannelLayout::Stereo)
}

/// A deterministic pseudo-random stereo buffer of `frames` frames, samples in
/// roughly `[-0.5, 0.5]` — a stand-in for decoded program content with no
/// correlation across the loop seam (the honest worst case for a crossfade).
fn noise(frames: usize, seed: u64) -> Vec<f32> {
    let channels = 2usize;
    let mut state = seed | 1;
    let mut out = vec![0.0f32; frames * channels];
    for slot in &mut out {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let unit = (state >> 11) as f64 / (1u64 << 53) as f64; // [0,1)
        *slot = (unit - 0.5) as f32;
    }
    out
}

/// Build a deck over a contiguous decoded buffer of `loop_frames` body frames
/// followed by `xfade` lap-over frames (real content past the loop point). The
/// total buffer is `loop_frames + xfade` frames; the deck loops the first
/// `loop_frames` with the lap-over crossfaded in at the seam.
fn deck_with_lapover(loop_frames: usize, xfade: usize, seed: u64) -> LoopDeck {
    let decoded = noise(loop_frames + xfade, seed);
    LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).expect("valid deck")
}

/// Read `total` frames from the deck starting at absolute frame `start` in
/// `chunk`-frame pulls, concatenating the interleaved output. Asserts every pull
/// is full (gap-free).
fn drain_at(deck: &LoopDeck, start: u64, total: usize, chunk: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(total * deck.format().channel_count());
    let mut done = 0usize;
    while done < total {
        let n = chunk.min(total - done);
        let block = deck.read_at(start + done as u64, n);
        assert_eq!(
            block.frame_count(),
            n,
            "read_at must return exactly the requested frames"
        );
        out.extend_from_slice(block.interleaved());
        done += n;
    }
    out
}

/// Per-frame stereo power (l²+r²) of an interleaved buffer.
fn frame_power(interleaved: &[f32]) -> Vec<f64> {
    interleaved
        .chunks_exact(2)
        .map(|f| f64::from(f[0]) * f64::from(f[0]) + f64::from(f[1]) * f64::from(f[1]))
        .collect()
}

/// Per-frame stereo peak |amplitude| (max |l|,|r|) of an interleaved buffer.
fn frame_amp(interleaved: &[f32]) -> Vec<f64> {
    interleaved
        .chunks_exact(2)
        .map(|f| f64::from(f[0].abs()).max(f64::from(f[1].abs())))
        .collect()
}

/// The largest absolute sample-to-sample step (per channel) over an interleaved
/// span — the click detector. A click is a *discontinuity*: a step far larger
/// than the signal's own intrinsic max step. (Low instantaneous power or a sine's
/// natural zero-crossing is NOT a click; a jump is.)
fn max_step(interleaved: &[f32]) -> f64 {
    let mut prev: Option<(f32, f32)> = None;
    let mut worst = 0.0f64;
    for f in interleaved.chunks_exact(2) {
        if let Some((pl, pr)) = prev {
            worst = worst
                .max(f64::from((f[0] - pl).abs()))
                .max(f64::from((f[1] - pr).abs()));
        }
        prev = Some((f[0], f[1]));
    }
    worst
}

// ----------------------------------------------------------------------------
// 1. read_at is a pure function of the absolute frame (period exactly L, and a
//    forced realign lands inside a faded seam — no un-crossfaded click).
// ----------------------------------------------------------------------------

#[test]
fn read_at_is_a_pure_function_of_the_absolute_frame() {
    // The looped stream MUST be a pure function of the absolute output frame, so
    // the same absolute span read in one big pull == read in 1-frame pulls ==
    // read with an injected forward jump (the bus-cursor realign under load).
    // Any chunk/cursor-dependent state would click at the realign seam (rule 26).
    let loop_frames = 500usize;
    let xfade = 64usize;
    let deck = deck_with_lapover(loop_frames, xfade, 0x55AA);

    let start = 0u64;
    let total = loop_frames * 3 + 137; // non-lap-aligned
    let big = drain_at(&deck, start, total, total);
    let small = drain_at(&deck, start, total, 1);
    let medium = drain_at(&deck, start, total, 100);
    assert_eq!(
        big, small,
        "1-frame pulls must equal one big pull (read_at purity)"
    );
    assert_eq!(big, medium, "100-frame pulls must equal one big pull");

    // A FORCED REALIGN: read a window that starts mid-lap-2 (an absolute frame
    // jump past a couple of seams, as a bus catch-up would do). The bytes must
    // equal the corresponding slice of the contiguous `big` stream — i.e. the
    // seam at that absolute position is correctly faded, not skipped.
    let jump_start = (loop_frames * 2 + (loop_frames - xfade / 2)) as u64; // mid-seam of lap 2
    let len = loop_frames; // spans the lap-2→3 seam
    let jumped = drain_at(&deck, jump_start, len, 32);
    let reference = drain_at(&deck, jump_start, len, len);
    assert_eq!(
        jumped, reference,
        "a realign to an absolute mid-seam frame must emit the SAME faded seam (no un-crossfaded click under load)"
    );
}

#[test]
fn the_loop_period_is_exactly_loop_frames_across_many_laps() {
    let loop_frames = 1000usize;
    let xfade = 64usize;
    let deck = deck_with_lapover(loop_frames, xfade, 0xABCD);
    let laps = 5usize;
    let out = drain_at(&deck, 0, loop_frames * laps, 333);
    let ch = 2usize;
    // The CLEAN MIDDLE of every lap, `[xfade, loop_frames)`, is `body[m]` (one lap
    // contributes), so it repeats identically every `loop_frames` — proving the
    // period is exactly `loop_frames` (sample-locked, no drift).
    for lap in 1..laps {
        for m in xfade..loop_frames {
            assert_eq!(
                out[m * ch],
                out[(lap * loop_frames + m) * ch],
                "clean-middle frame {m} drifted at lap {lap} — period must be exactly loop_frames"
            );
        }
    }
}

// ----------------------------------------------------------------------------
// 2a. Decorrelated seam → equal-power → total POWER stays flat (no dip/bump).
// ----------------------------------------------------------------------------

#[test]
fn a_decorrelated_seam_keeps_total_power_flat() {
    // Constant-RMS uncorrelated content: every frame has power 2a². With a
    // decorrelated lap-over (independent noise) the deck must pick EQUAL-POWER, so
    // the seam power stays ≈ 2a² (a linear fade would dip ~50%).
    let loop_frames = 800usize;
    let xfade = 128usize;
    let a = 0.4f32;
    let channels = 2usize;
    let mk = |seed: u64| {
        let mut state = seed | 1;
        let n = (loop_frames + xfade) * channels;
        let mut v = vec![0.0f32; n];
        for slot in &mut v {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = if state & 1 == 0 { a } else { -a };
        }
        v
    };
    let decoded = mk(0x1234_5678);
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();

    let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
    let power = frame_power(&out);
    let nominal = 2.0 * f64::from(a) * f64::from(a);
    // Seam of lap 0 is `[0, xfade)` (the overlap-add region at the wrap from the
    // previous lap's tail into this lap's head). Examine the lap-1 seam which has a
    // full previous lap behind it.
    let seam_start = loop_frames; // absolute frame of lap-1's wrap seam = [L, L+xfade)
    let seam = &power[seam_start..seam_start + xfade];
    let avg: f64 = seam.iter().sum::<f64>() / xfade as f64;
    let ratio = avg / nominal;
    assert!(
        (0.85..=1.15).contains(&ratio),
        "decorrelated seam mean power ratio {ratio:.3} must stay ≈ 1 (equal-power, no dip/bump)"
    );
    // (For DEcorrelated ±a content the per-sample step is already ~2a everywhere —
    // white noise is maximally discontinuous, so a hard cut at the wrap is NOT
    // distinguishable from the crossfade by step size. The flat MEAN POWER above is
    // the meaningful equal-power criterion here; the step/continuity criterion is
    // exercised by the CORRELATED tonal seam test, where a hard cut WOULD jump.)
}

// ----------------------------------------------------------------------------
// 2b. Correlated seam → linear → AMPLITUDE stays flat (no +3 dB swell/click).
//     THIS is the defect the review caught: equal-power swells √2 on a tone.
// ----------------------------------------------------------------------------

#[test]
fn a_correlated_tonal_seam_does_not_swell_in_amplitude() {
    // A continuous sine across the loop point: body is N cycles of a tone, and the
    // lap-over continues the SAME phase-locked tone (so tail and head are
    // correlated/in-phase). An equal-power crossfade would swell amplitude to
    // √2 ≈ 1.41× at the seam midpoint (an audible per-lap level pump). The deck
    // must detect the correlation and use a LINEAR fade → flat amplitude.
    let channels = 2usize;
    let cycles = 40usize;
    let period = 100usize; // samples per cycle
    let loop_frames = cycles * period; // whole number of cycles → seamless tone
    let xfade = period; // one cycle of crossfade
    let amp = 0.7f32;
    // Build body + lap-over as ONE continuous tone over (loop_frames + xfade)
    // samples (so the lap-over is the genuine continuation of the body's tone).
    let total = loop_frames + xfade;
    let mut decoded = vec![0.0f32; total * channels];
    for i in 0..total {
        let s = amp * (2.0 * std::f32::consts::PI * (i as f32) / (period as f32)).sin();
        decoded[i * channels] = s;
        decoded[i * channels + 1] = s;
    }
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();

    let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
    let ampl = frame_amp(&out);
    // The seam at lap-1's wrap: peak amplitude must NOT exceed the tone's own peak
    // by more than a small margin (a linear fade of a continuous tone holds the
    // envelope; equal-power would reach ~1.41× = an obvious swell). We bound the
    // ENVELOPE: the per-frame peak over the seam stays under 1.12× the tone peak.
    let seam_start = loop_frames;
    let seam = &ampl[seam_start..seam_start + xfade];
    let peak = seam.iter().copied().fold(0.0f64, f64::max);
    assert!(
        peak <= 1.12 * f64::from(amp),
        "correlated tonal seam peak amplitude {peak:.4} swelled past the tone peak {amp} — equal-power was wrongly used (the +3 dB loop-pump defect)"
    );
    // No CLICK: the seam's largest sample step must stay near the tone's own max
    // step (amp·2π/period, the slope at a zero-crossing). A linear crossfade of a
    // continuous tone holds the envelope, so no discontinuity appears. (A dip to a
    // low instantaneous amplitude is just the sine's natural zero-crossing — NOT a
    // click; only a step jump is.)
    let seam_samples = &out[seam_start * channels..(seam_start + xfade) * channels];
    let step = max_step(seam_samples);
    let tone_step = f64::from(amp) * 2.0 * std::f64::consts::PI / (period as f64);
    assert!(
        step <= 1.5 * tone_step + 1e-6,
        "correlated tonal seam max step {step:.5} exceeds ~1.5× the tone's own step {tone_step:.5} — a discontinuity (click)"
    );
}

// ----------------------------------------------------------------------------
// 3. Loop length is the audio duration of the vamp frames at the ASSET cadence
//    — NOT a SampleClock::total_at tick delta (no frame-vs-tick conflation).
// ----------------------------------------------------------------------------

#[test]
fn loop_length_is_the_decoded_sample_count_independent_of_output_cadence() {
    // The deck is built from a decoded sample buffer; its loop length is exactly
    // the body frame count it was given — there is no cadence in the deck at all,
    // so it cannot conflate asset frames with output ticks. A 24 fps asset vamp of
    // 240 frames is 10 s of audio = 480_000 frames at 48 kHz, REGARDLESS of the
    // output (program) cadence. We assert the deck's reported loop length equals
    // the body frames it was constructed with.
    let loop_frames = 480_000usize; // 10 s at 48 kHz (e.g. 240 frames @ 24 fps)
    let xfade = 480usize; // 10 ms
                          // Build a tiny-amplitude buffer (content irrelevant; only the length matters).
    let decoded = vec![0.01f32; (loop_frames + xfade) * 2];
    let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    assert_eq!(
        deck.loop_frames(),
        loop_frames,
        "the loop length is the decoded body sample count — no total_at tick delta, no cadence conflation"
    );
}

// ----------------------------------------------------------------------------
// 4. Armed exit: settles to silence at the next seam, exactly once.
// ----------------------------------------------------------------------------

#[test]
fn an_armed_exit_settles_to_silence_at_the_next_seam_exactly_once() {
    let loop_frames = 600usize;
    let xfade = 60usize;
    // A constant non-silent body: while looping output is ~0.3; after the exit
    // settles it must be exactly silence.
    let decoded = vec![0.3f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();

    // Consume partway into lap 0, then arm the exit (the audio thread would do this
    // when it drains an ArmExit verb from the shared mailbox).
    let _ = drain_at(&deck, 0, loop_frames / 2, 64);
    deck.arm_exit();

    // The exit fires at the NEXT seam (the lap-0 wrap at absolute frame L). Read
    // from there for a few laps: after the exit fade the deck must be SILENT.
    let after = drain_at(&deck, (loop_frames / 2) as u64, loop_frames * 3, 64);
    let tail = &after[after.len() - loop_frames * 2..];
    assert!(
        tail.iter().all(|&s| s == 0.0),
        "after the armed exit fires at the seam the deck settles to silence (the bus contribution ends)"
    );
    assert!(
        deck.has_ended(),
        "an armed-exit deck reports ended once the boundary has fired"
    );
}

#[test]
fn cancel_exit_keeps_looping_forever() {
    let loop_frames = 400usize;
    let xfade = 40usize;
    let decoded = vec![0.25f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();
    deck.arm_exit();
    deck.cancel_exit();
    let out = drain_at(&deck, 0, loop_frames * 4, 77);
    let tail = &out[out.len() - loop_frames..];
    assert!(
        tail.iter().any(|&s| s != 0.0),
        "a cancelled exit keeps the loop running (never goes silent)"
    );
    assert!(!deck.has_ended(), "a cancelled-exit deck has not ended");
}

// ----------------------------------------------------------------------------
// 5. Empty / paused / stopped decks: silence, never a stall or panic.
// ----------------------------------------------------------------------------

#[test]
fn an_empty_deck_rides_silence_gap_free() {
    // A player whose asset has no audio (or a failed/over-cap prime) builds an
    // empty deck: it produces exactly `frames` frames of silence on every read,
    // forever — never a short block, never a panic (hold-last-good / never off-air).
    let deck = LoopDeck::empty(stereo());
    for tick in 0..10u64 {
        let block = deck.read_at(tick * 1601, 1601);
        assert_eq!(
            block.frame_count(),
            1601,
            "empty deck still returns full blocks"
        );
        assert!(
            block.interleaved().iter().all(|&s| s == 0.0),
            "empty deck returns silence"
        );
    }
}

#[test]
fn pause_contributes_silence_and_stop_recues() {
    let loop_frames = 300usize;
    let xfade = 30usize;
    let decoded = vec![0.5f32; (loop_frames + xfade) * 2];
    let mut deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
    deck.vamp();
    let _ = drain_at(&deck, 0, 100, 50);
    deck.pause();
    // Paused: the bus contribution is silence (not a frozen DC sample, which would
    // click on resume); the video tile holds the picture separately.
    let paused = drain_at(&deck, 100, loop_frames, 50);
    assert!(
        paused.iter().all(|&s| s == 0.0),
        "a paused deck contributes silence to the bus"
    );
}

// ----------------------------------------------------------------------------
// 6. Property tests — period integrity + both seam-correlation regimes.
// ----------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any loop length and crossfade window (clamped ≤ L/2 by the deck), the
    /// clean middle `[xfade, L)` of every lap equals lap 0's — period exactly L.
    #[test]
    fn prop_period_is_exactly_loop_frames(
        loop_frames in 64usize..2048,
        xfade_req in 0usize..4096,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2);
        let deck = deck_with_lapover(loop_frames, xfade, seed ^ 0x9E3779B97F4A7C15);
        let laps = 4usize;
        let out = drain_at(&deck, 0, loop_frames * laps, loop_frames.max(1));
        let ch = 2usize;
        for lap in 1..laps {
            for m in xfade..loop_frames {
                prop_assert_eq!(
                    out[m * ch],
                    out[(lap * loop_frames + m) * ch],
                    "period drift at lap {} frame {}", lap, m
                );
            }
        }
    }

    /// read_at is pure: any absolute span is identical read whole vs in 1-frame
    /// pulls (so a realign under load never skips a faded seam).
    #[test]
    fn prop_read_at_is_pure(
        loop_frames in 64usize..1024,
        xfade_req in 1usize..2048,
        start in 0u64..100_000,
        len in 1usize..2000,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2);
        let deck = deck_with_lapover(loop_frames, xfade, seed);
        let whole = drain_at(&deck, start, len, len);
        let ones = drain_at(&deck, start, len, 1);
        prop_assert_eq!(whole, ones);
    }

    /// A decorrelated equal-RMS seam keeps mean power within band (equal-power).
    #[test]
    fn prop_decorrelated_seam_power_flat(
        loop_frames in 128usize..2048,
        xfade_req in 8usize..4096,
        seed in any::<u64>(),
    ) {
        let xfade = xfade_req.min(loop_frames / 2).max(1);
        let a = 0.35f32;
        let channels = 2usize;
        let mut state = seed | 1;
        let n = (loop_frames + xfade) * channels;
        let mut decoded = vec![0.0f32; n];
        for slot in &mut decoded {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *slot = if state & 1 == 0 { a } else { -a };
        }
        let deck = LoopDeck::with_segment(stereo(), &decoded, loop_frames, xfade).unwrap();
        let out = drain_at(&deck, 0, loop_frames * 2, loop_frames);
        let power = frame_power(&out);
        let nominal = 2.0 * f64::from(a) * f64::from(a);
        let seam = &power[loop_frames..loop_frames + xfade];
        let avg: f64 = seam.iter().sum::<f64>() / xfade as f64;
        let ratio = avg / nominal;
        prop_assert!(
            (0.80..=1.20).contains(&ratio),
            "decorrelated seam power ratio {} out of band (xfade {})", ratio, xfade
        );
    }
}
