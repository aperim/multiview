#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
//! Latch-on-tick sampling: the output clock samples each tile by **output media
//! time**, selecting the frame whose source-relative `media_time` is
//! *nearest-but-not-after* the requested instant (streaming-gotchas §1).
//!
//! This is the regression guard for the "file/VOD tile races ultra-fast then
//! freezes" bug: when the output loop runs slower than real-time, the ingest
//! thread (which decodes ahead) had published many frames into the store, and a
//! `latest-wins` read sampled the newest one — so the tile's content raced far
//! ahead of the output's own media clock. Sampling by media time makes the tile
//! advance 1:1 with output time regardless of how fast frames were produced.

use mosaic_core::time::MediaTime;
use mosaic_framestore::{NoSignalPolicy, TileStore, TileThresholds};

/// Helper: a store that holds frames forever (so selection, not the failure
/// ladder, is what we are asserting) with very generous freshness thresholds.
fn store() -> TileStore<u32> {
    TileStore::new("t", TileThresholds::default(), NoSignalPolicy::HoldForever)
}

#[test]
fn read_at_selects_nearest_but_not_after() {
    let s = store();
    // Three frames stamped on the OUTPUT media timeline (ms): 0, 40, 80.
    s.publish(10_u32, MediaTime::from_nanos(0));
    s.publish(20_u32, MediaTime::from_nanos(40_000_000));
    s.publish(30_u32, MediaTime::from_nanos(80_000_000));

    // At output time 0 we must see frame@0, NOT the latest (frame@80).
    let r0 = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(r0.frame().map(|f| **f), Some(10), "t=0 must latch frame@0");

    // Between 40 and 80 we latch frame@40 (nearest-but-not-after).
    let r1 = s.read_at(MediaTime::from_nanos(60_000_000));
    assert_eq!(r1.frame().map(|f| **f), Some(20), "t=60ms latches frame@40");

    // At/after the newest we latch the newest.
    let r2 = s.read_at(MediaTime::from_nanos(80_000_000));
    assert_eq!(r2.frame().map(|f| **f), Some(30), "t=80ms latches frame@80");
}

#[test]
fn read_at_does_not_race_ahead_when_producer_runs_far_ahead() {
    // The bug scenario in miniature: the producer has published 0..=250 frames
    // (the file decoded ahead), each stamped at i*40ms of OUTPUT media time,
    // BUT the consumer's clock has only reached output time 1.0s (== frame 25).
    let s = store();
    for i in 0..=250_i64 {
        let at = MediaTime::from_nanos(i.saturating_mul(40_000_000));
        let val = u32::try_from(i).unwrap();
        s.publish(val, at);
    }

    // The output clock is at 1.0s. We MUST sample frame 25 (1.0s / 40ms), not
    // the newest frame 250 — that newest-wins read is exactly the race.
    let now = MediaTime::from_nanos(1_000_000_000);
    let r = s.read_at(now);
    assert_eq!(
        r.frame().map(|f| **f),
        Some(25),
        "output@1.0s must latch source frame 25, not race to the newest (250)"
    );
}

#[test]
fn read_at_holds_last_frame_when_output_passes_the_end() {
    // A finite source played out: frames 0..=9 at 40ms steps. Once output time
    // passes the last frame, the tile holds the last-good frame (freeze), it
    // never goes blank and never rewinds.
    let s = store();
    for i in 0..=9_i64 {
        let at = MediaTime::from_nanos(i.saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
    }
    let way_past = MediaTime::from_nanos(10_000_000_000);
    let r = s.read_at(way_past);
    assert_eq!(r.frame().map(|f| **f), Some(9), "holds the last frame");
}

#[test]
fn read_at_before_first_frame_uses_earliest() {
    // If the only frames available are stamped slightly after `now` (e.g. the
    // first frame arrives a touch late, or a static primed frame stamped at a
    // non-zero instant), the tile shows the earliest frame rather than a slate —
    // a tile should not flash NO_SIGNAL just because its first frame's stamp is
    // marginally ahead of the very first tick.
    let s = store();
    s.publish(7_u32, MediaTime::from_nanos(5_000_000));
    let r = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(
        r.frame().map(|f| **f),
        Some(7),
        "earliest frame, not a slate"
    );
}

#[test]
fn is_primed_flips_false_to_true_on_first_publish() {
    // The startup prime-wait uses `is_primed` to tell a tile that has decoded its
    // first frame from one still cold. A fresh store has published nothing, so it
    // is NOT primed; the very first publish flips it primed, forever.
    let s = store();
    assert!(
        !s.is_primed(),
        "a store with no published frame must read NOT primed (cold tile)"
    );
    s.publish(42_u32, MediaTime::from_nanos(0));
    assert!(
        s.is_primed(),
        "the first published frame must mark the tile primed"
    );
    // Newest-wins publishes keep it primed (it never reverts to cold).
    s.publish(43_u32, MediaTime::from_nanos(40_000_000));
    assert!(s.is_primed(), "a primed store stays primed");
}

#[test]
fn ring_is_bounded_drop_oldest() {
    // The ring is bounded: publishing far more than its capacity drops the
    // OLDEST entries (newest wins for memory), so sampling an old instant falls
    // back to the earliest *retained* frame rather than growing without bound.
    let s = store();
    let cap = TileStore::<u32>::RING_CAPACITY;
    let total = cap.saturating_add(50);
    for i in 0..total {
        let at = MediaTime::from_nanos(i64::try_from(i).unwrap().saturating_mul(40_000_000));
        s.publish(u32::try_from(i).unwrap(), at);
    }
    // Sampling t=0 (long since evicted) must not panic and must yield the
    // earliest retained frame, which is `total - cap`.
    let earliest_retained = u32::try_from(total - cap).unwrap();
    let r = s.read_at(MediaTime::from_nanos(0));
    assert_eq!(r.frame().map(|f| **f), Some(earliest_retained));
    // Sampling the newest instant still yields the newest frame.
    let newest_at = MediaTime::from_nanos(i64::try_from(total - 1).unwrap() * 40_000_000);
    let r2 = s.read_at(newest_at);
    assert_eq!(
        r2.frame().map(|f| **f),
        Some(u32::try_from(total - 1).unwrap())
    );
}
