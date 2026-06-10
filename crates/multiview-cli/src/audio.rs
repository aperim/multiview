//! Per-source **runtime audio ingest** — the audio peer of the video decode
//! thread (AUD-2).
//!
//! The pure decode + resample logic lives in [`multiview_audio`] (the
//! [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) over an
//! [`AudioFileDecoder`](multiview_audio::AudioFileDecoder), itself a thin wrapper
//! over the [`multiview_ffmpeg`] safe seam): it opens the container on the worker
//! thread, decodes + resamples to the canonical 48 kHz / stereo / `f32` format,
//! and publishes blocks into a lock-free [`AudioStore`]. This module is the
//! **cli-side supervision**: it maps a configured source to a decodable location
//! and drives the decode loop under the SAME supervised-reconnect bracket the
//! video [`ingest_loop`](crate::pipeline) uses — a live source whose audio drops
//! or EOFs reconnects with capped-exponential, jittered backoff; a finite file
//! plays once and then rides silence (the store silence-fills past EOF, so the
//! sampled track is gap-free — ADR-R005 §4.1).
//!
//! ## It samples, it never paces (invariants #1/#2/#10)
//! The decode thread only ever **writes** the lock-free [`AudioStore`] it shares
//! by `Arc` with the [`ProgramBus`](multiview_audio::ProgramBus). The output
//! clock *samples* that store per tick (a wait-free read that silence-fills any
//! un-written span), so a slow, fast, wedged, or never-ending audio source can
//! neither pace nor stall the output clock (invariant #1) and cannot
//! back-pressure the engine (invariant #10) — exactly like the video tiles.
//!
//! ## Bad inputs are the purpose
//! An open failure, a no-audio source, or a mid-stream decode error logs and
//! ends *this* connection — never the run. A live source then reconnects; a
//! finite/audio-free source rides silence. No decode fault ever crashes or
//! stalls the program (bulletproof continuous output is the product).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use multiview_audio::store::{audio_decode_loop, AudioStore};
use multiview_audio::{AudioFormat, ChannelLayout};

use crate::pipeline::{next_reconnect_attempt, reconnect_backoff, sleep_interruptible, JitterRng};

/// The bounded depth (in frames) of each per-source [`AudioStore`].
///
/// Two seconds at the canonical 48 kHz is ample headroom for the decode thread
/// to run ahead of the output clock's per-tick pull without the ring ever
/// growing: anything beyond this is dropped oldest-first (the data-plane "queues
/// drop, never grow" rule). Mirrors the `96_000` the program-bus unit tests use.
const STORE_CAPACITY_FRAMES: usize = 96_000;

/// The canonical program audio format every per-source store is built at: 48 kHz
/// stereo `f32`. Every source's decode resamples to this BEFORE publishing, so
/// the [`ProgramBus`](multiview_audio::ProgramBus) mixes a single uniform format
/// (streaming-gotchas §5: "per-input resample to 48k fltp BEFORE mixing").
#[must_use]
pub(crate) fn canonical_format() -> AudioFormat {
    AudioFormat::new(AudioFormat::CANONICAL_RATE, ChannelLayout::Stereo)
}

/// Build an empty per-source [`AudioStore`] at the [`canonical_format`], shared
/// (`Arc`) between this source's decode thread (writer) and the program bus
/// (reader). Returned to the pipeline so it can both route the store onto the
/// bus AND hand a clone to the decode thread.
#[must_use]
pub(crate) fn new_store() -> Arc<AudioStore> {
    Arc::new(AudioStore::new(canonical_format(), STORE_CAPACITY_FRAMES))
}

/// Everything one source needs to decode its audio on its own thread: where the
/// audio lives (a libav-openable path or URL) and whether it is a live
/// (never-ending) source.
///
/// Built only for sources whose media libav can open for audio — a local file or
/// a network URL (rtsp/hls/ts/srt/rtmp). Synthetic sources carry no audio; NDI
/// audio is a separate host-memory path (not wired here). A source with no audio
/// stream simply has its decode loop end at open time and rides silence.
pub(crate) struct AudioIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable audio location (a file path or a network URL string).
    pub(crate) location: String,
    /// Whether this is a live (continuous, never-EOF) source: a live source's
    /// audio is reopened on EOF/error (a transient HLS/RTSP audio drop
    /// reconnects); a finite file plays its audio once and then rides silence.
    pub(crate) live: bool,
}

/// Drive a source's audio decode under the supervised-reconnect bracket: open +
/// decode the source's best audio stream into `store` (resampling to the
/// canonical 48 kHz stereo format), reconnecting a *live* source on EOF/error
/// with capped-exponential, jittered backoff until `stop` is raised.
///
/// This is the audio twin of the video
/// [`ingest_loop`](crate::pipeline): the inner per-connection decode is the pure
/// [`audio_decode_loop`](multiview_audio::store::audio_decode_loop) (which opens
/// the `!Send` libav decoder on this worker thread, publishes blocks, and returns
/// on stop/EOF/error); this function wraps it in the reconnect supervision a live
/// transport needs. It only ever WRITES the lock-free store, so it can neither
/// pace nor stall the output clock (invariant #1) nor back-pressure the engine
/// (invariant #10); a wedged libav network call is bounded by the supervisor's
/// detach-on-grace teardown, exactly as for video.
pub(crate) fn audio_ingest_loop(plan: &AudioIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    let mut attempt: u32 = 0;
    let mut jitter = JitterRng::seeded(&plan.id);
    let location = std::path::Path::new(&plan.location);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        // One connection's worth of decode. `audio_decode_loop` opens the decoder
        // on THIS thread (libav contexts are not `Send`), publishes every decoded
        // block, and returns on stop / EOF / open-or-decode error — logging the
        // fault itself. A no-audio source returns immediately (open finds no audio
        // stream); its tile then simply rides silence.
        audio_decode_loop(location, ChannelLayout::Stereo, store, stop);
        let ran_for = started.elapsed();
        if !plan.live || stop.load(Ordering::Acquire) {
            // A finite source has played its audio out (the store silence-fills
            // past EOF forever); a stop was requested. Either way, this ends.
            return;
        }
        // Live source: escalate from THIS connection's health, then wait the
        // resulting backoff (checking `stop` in slices so teardown stays prompt)
        // and reconnect — the same policy the video ingest uses.
        attempt = next_reconnect_attempt(attempt, ran_for);
        let nap = reconnect_backoff(attempt, jitter.next_unit());
        tracing::debug!(
            source = %plan.id,
            attempt,
            ?nap,
            "reconnecting live source audio after backoff"
        );
        sleep_interruptible(nap, stop);
    }
}
