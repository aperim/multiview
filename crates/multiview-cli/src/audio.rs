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
use std::time::{Duration, Instant};

use multiview_audio::store::{audio_decode_loop, AudioStore};
use multiview_audio::{AudioFormat, ChannelLayout, ToneGenerator};
use multiview_core::time::Rational;

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

/// Everything a synthetic source needs to publish its **line-up tone** on its own
/// thread (AUD-5): the source id (the program-bus route key + diagnostics) and
/// the output cadence the tone is paced to.
///
/// Built only for the `bars` synthetic source — the SMPTE/EBU colour-bars card's
/// companion is a 1 kHz reference tone; `solid` and `clock` synthetic sources
/// carry no audio and ride silence on the bus.
pub(crate) struct ToneIngestPlan {
    /// The source id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The output cadence the tone is paced to (so the store stays exactly one
    /// per-tick budget ahead of the program bus's pull, gap-free).
    pub(crate) cadence: Rational,
}

/// Target buffered lead (frames) the tone keeps ahead of the bus's read cursor:
/// half a second at the canonical 48 kHz. Comfortably inside the store's
/// [`STORE_CAPACITY_FRAMES`] (2 s) so a top-up never evicts un-read tone, and
/// large enough that a scheduling hiccup on this thread never lets the bus's
/// per-tick pull outrun the published tone (which would read silence — a gap).
const TONE_LEAD_FRAMES: i64 = 24_000;

/// Refill the store whenever the lead drops below this (half the target): a
/// hysteresis band so the thread does a few bounded `sin` bursts rather than a
/// tight spin, then sleeps.
const TONE_REFILL_THRESHOLD_FRAMES: i64 = TONE_LEAD_FRAMES / 2;

/// Publish the synthetic **1 kHz line-up tone** into `store` until `stop` is
/// raised — the audio peer of [`crate::synth::generator_loop`] for the `bars`
/// source (AUD-5).
///
/// The tone is generated by [`multiview_audio::ToneGenerator`] (a pure,
/// phase-continuous sine carrying integer phase, so it never drifts — invariant
/// #3/#6). The loop keeps the store filled to a fixed [`TONE_LEAD_FRAMES`] lead
/// **ahead of the bus's own read cursor** ([`AudioStore::read_cursor`]): it tops
/// the buffer up in one phase-continuous burst whenever the lead falls below the
/// refill threshold, then sleeps. Pacing against the reader's cursor (not
/// wall-clock) makes the tone gap-free regardless of how the bake consumer is
/// paced — the bus's per-tick pull always lands inside published tone, never on
/// an un-written (silence) span.
///
/// If the reader cursor *jumps past* the write head (a `DropOnOverload` catch-up,
/// where the bus's tick-driven [`SampleClock`](multiview_audio::cadence::SampleClock)
/// skips a large span) the loop **seeks the generator forward** to the cursor and
/// resumes from there — it never tries to back-fill the skipped (already-evicted,
/// never-read) span, so every burst stays bounded by [`TONE_LEAD_FRAMES`]
/// (bounded copy cost, no spin). The lead is bounded well inside the store
/// capacity, so the ring never evicts tone the reader has not consumed and never
/// grows (the data-plane "queues drop, never grow" rule).
///
/// It only ever **writes** the lock-free store, so it can neither pace nor stall
/// the output clock (invariant #1) nor back-pressure the engine (invariant #10);
/// a refill is a cheap, bounded per-frame `sin` burst off the hot path. `stop` is
/// polled each wakeup so teardown is prompt.
pub(crate) fn tone_publish_loop(plan: &ToneIngestPlan, store: &AudioStore, stop: &AtomicBool) {
    let format = store.format();
    let mut generator = ToneGenerator::line_up(format);
    // Absolute count of frames published so far (the writer's write head; the
    // store's `read_cursor` lives in the same absolute coordinate space).
    let mut published: i64 = 0;
    let poll = refill_poll_interval(plan.cadence);
    while !stop.load(Ordering::Acquire) {
        let read_cursor = store.read_cursor();
        // If the reader has overrun the write head (a large tick-driven catch-up),
        // skip the generator forward to the cursor rather than back-filling the
        // never-read span — keeps every burst bounded by the lead.
        if read_cursor > published {
            generator.seek_to_frame(u64::try_from(read_cursor).unwrap_or(0));
            published = read_cursor;
        }
        let lead = published.saturating_sub(read_cursor);
        if lead < TONE_REFILL_THRESHOLD_FRAMES {
            // Top up to the target lead in one phase-continuous burst (bounded by
            // `TONE_LEAD_FRAMES` since the overrun case above already realigned).
            let want = TONE_LEAD_FRAMES.saturating_sub(lead).max(0);
            let frames = usize::try_from(want).unwrap_or(0);
            if frames > 0 {
                match generator.next_block(frames) {
                    Ok(block) => {
                        if let Err(error) = store.publish(&block) {
                            // The generator and store share the canonical format, so
                            // a mismatch is a programming error: log and stop rather
                            // than busy-loop. The source then rides silence.
                            tracing::error!(%error, source = %plan.id, "tone publish rejected; stopping");
                            return;
                        }
                        published = published.saturating_add(want);
                    }
                    Err(error) => {
                        tracing::error!(%error, source = %plan.id, "tone generation failed; stopping");
                        return;
                    }
                }
            }
        }
        sleep_interruptible(poll, stop);
    }
}

/// The refill poll interval: ~10 output ticks (clamped to `[5 ms, 200 ms]`). Short
/// enough that the bus never drains the buffered lead between polls, long enough
/// that the thread is mostly asleep. Derived from `cadence` so it scales with the
/// output rate.
#[must_use]
fn refill_poll_interval(cadence: Rational) -> Duration {
    let num = u64::try_from(cadence.num).unwrap_or(1).max(1);
    let den = u64::try_from(cadence.den).unwrap_or(1).max(1);
    // One tick = den/num seconds; ten ticks in nanos = 10 * den * 1e9 / num.
    let nanos = den.saturating_mul(10_000_000_000) / num;
    Duration::from_nanos(nanos.clamp(5_000_000, 200_000_000))
}

/// Everything a **media-player channel** needs to loop its embedded audio on its
/// own thread (ADR-T019 / ADR-0097 §8): where the asset audio lives, the
/// integer-frame vamp window + cadence that defines the loop body (the same
/// [`PlayoutGeometry`](crate::player::PlayoutGeometry) the video uses, so the
/// audio wraps on the same instant), and the shared transport
/// [`mailbox`](crate::player::TransportMailbox) the video thread also drains.
pub(crate) struct PlayerAudioPlan {
    /// The player channel id (for diagnostics + the program-bus route key).
    pub(crate) id: String,
    /// The libav-openable asset audio location (a file path).
    pub(crate) location: String,
    /// The vamp-segment start (asset source frames).
    pub(crate) vamp_in_frames: u64,
    /// The vamp-segment end, exclusive (asset source frames).
    pub(crate) vamp_out_frames: u64,
    /// The asset cadence — the frame↔sample mapping the loop body is sliced by
    /// (so the loop length is the audio duration of `vamp_len` asset frames, the
    /// same sample count at any output cadence — ADR-T019 §3).
    pub(crate) cadence: Rational,
    /// Whether the channel loops (vamps) on its first play, mirroring the video
    /// [`PlayerHandle::loop_on_start`](crate::player::PlayerHandle). A non-looping
    /// player plays its audio once then rides silence (the deck settles).
    pub(crate) loop_on_start: bool,
    /// The output cadence (the bus tick rate) the fill loop paces its top-ups to.
    pub(crate) output_cadence: Rational,
    /// The shared transport mailbox (the SAME `Arc` the video player thread
    /// drains): transport verbs reach the audio loop on the same boundary.
    pub(crate) mailbox: Arc<crate::player::TransportMailbox>,
}

/// Decode a whole asset's audio to canonical 48 kHz stereo `f32`, accumulating up
/// to `max_frames` frames (the [`LoopDeck`](multiview_audio::LoopDeck) cap — a
/// truncated/over-cap asset is bounded here, never an unbounded read). Returns the
/// interleaved buffer, or an empty `Vec` on open/decode failure or a no-audio
/// source (the player then rides silence — never an error, never a stall).
#[cfg(feature = "ffmpeg")]
#[must_use]
fn decode_clip_to_48k(path: &std::path::Path, max_frames: usize) -> Vec<f32> {
    use multiview_audio::AudioFileDecoder;

    let channels = canonical_format().channel_count();
    // `multiview_audio::AudioFileDecoder` resamples to 48 kHz / this layout / f32.
    let mut decoder = match AudioFileDecoder::open(path, ChannelLayout::Stereo) {
        Ok(d) => d,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "media-player audio: open failed; riding silence");
            return Vec::new();
        }
    };
    let cap_samples = max_frames.saturating_mul(channels);
    let mut out: Vec<f32> = Vec::new();
    loop {
        match decoder.next_block() {
            Ok(Some(block)) => {
                out.extend_from_slice(block.interleaved());
                if out.len() >= cap_samples {
                    out.truncate(cap_samples);
                    tracing::warn!(
                        player_path = %path.display(),
                        cap_seconds = multiview_audio::loopdeck::MAX_LOOP_SECONDS,
                        "media-player audio: asset exceeds the loop cap; truncating the loop body"
                    );
                    break;
                }
            }
            Ok(None) => break, // EOS — the whole clip is decoded.
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "media-player audio: decode error; using what decoded so far");
                break;
            }
        }
    }
    out
}

/// Drive a media-player channel's **looping embedded audio** (ADR-T019): prime a
/// [`LoopDeck`](multiview_audio::LoopDeck) once from the asset's `[vamp_in,
/// vamp_out + W)` window, then keep `store` filled a bounded lead **ahead of the
/// bus's read cursor** with the looped (crossfaded-at-the-seam) stream, draining
/// the shared transport [`mailbox`](crate::player::TransportMailbox) between
/// top-ups so `vamp`/`arm-exit`/`stop` land on the same boundary as the video.
///
/// It only ever **writes** the lock-free `store`, so it can neither pace nor stall
/// the output clock (inv #1) nor back-pressure the engine (inv #10) — exactly like
/// the tone loop it mirrors. A no-audio / failed / over-cap prime yields an empty
/// deck → the channel rides **silence** (the store silence-fills), never an error.
///
/// Unlike [`audio_ingest_loop`], a player channel's audio is **buffer-and-replay**,
/// not a streamed decode: the loop seam is sample-exact and click-free
/// (ADR-T019), which a per-lap container seek (no audio IDR) could not be.
#[cfg(feature = "ffmpeg")]
pub(crate) fn player_audio_loop(plan: &PlayerAudioPlan, store: &AudioStore, stop: &AtomicBool) {
    use multiview_audio::loopdeck::{LoopDeck, MAX_LOOP_SECONDS, SAMPLE_RATE};

    // Prime: decode the whole asset (bounded by the cap + a crossfade window of
    // lap-over headroom), then slice the vamp window into a deck. Any failure → an
    // empty (silent) deck.
    let cap_frames = MAX_LOOP_SECONDS.saturating_mul(SAMPLE_RATE);
    let max_frames = usize::try_from(cap_frames)
        .unwrap_or(usize::MAX)
        .saturating_add(multiview_audio::loopdeck::DEFAULT_CROSSFADE_FRAMES);
    let clip = decode_clip_to_48k(std::path::Path::new(&plan.location), max_frames);
    let mut deck = LoopDeck::from_clip_window(
        canonical_format(),
        &clip,
        plan.vamp_in_frames,
        plan.vamp_out_frames,
        plan.cadence,
        0,
    )
    .unwrap_or_else(|_| LoopDeck::empty(canonical_format()));
    drop(clip); // the deck owns its bounded body+seam; release the full clip.

    // A non-looping player plays its audio once then settles; a looping/vamping
    // player vamps. (Mirrors the video `loop_on_start`.)
    if !plan.loop_on_start {
        // One-shot: arm the exit immediately so the deck plays one lap then
        // settles to silence (a non-looping play-through; the video tile holds its
        // last frame separately).
        deck.arm_exit();
    }

    // The absolute write head (next frame to publish); the bus's `read_cursor`
    // lives in the same absolute coordinate space.
    let mut published: i64 = 0;
    let poll = refill_poll_interval(plan.output_cadence);
    while !stop.load(Ordering::Acquire) {
        // Drain transport verbs (the SAME mailbox the video thread drains) and
        // apply them to the deck — so vamp/arm-exit/stop land on the same boundary
        // as the video. O(pending), a quick mutex `mem::take`, never blocking.
        for verb in plan.mailbox.drain() {
            apply_audio_verb(&mut deck, &verb);
        }

        let read_cursor = store.read_cursor();
        // If the bus's tick-driven cursor overran the write head (a DropOnOverload
        // catch-up), realign the deck + write head to the cursor rather than
        // back-filling the never-read span — keeps every top-up bounded and,
        // because the deck is positioned by the ABSOLUTE frame, the realign still
        // lands inside a correctly-faded seam (no un-crossfaded click — ADR-T019).
        if read_cursor > published {
            deck.seek_to(u64::try_from(read_cursor).unwrap_or(0));
            published = read_cursor;
        }
        let lead = published.saturating_sub(read_cursor);
        if lead < PLAYER_AUDIO_REFILL_THRESHOLD_FRAMES {
            let want = PLAYER_AUDIO_LEAD_FRAMES.saturating_sub(lead).max(0);
            let frames = usize::try_from(want).unwrap_or(0);
            if frames > 0 {
                let block = deck.read(frames);
                if let Err(error) = store.publish(&block) {
                    tracing::error!(%error, player = %plan.id, "media-player audio publish rejected; stopping");
                    return;
                }
                published = published.saturating_add(want);
            }
        }
        sleep_interruptible(poll, stop);
    }
}

/// Map a transport verb onto the [`LoopDeck`](multiview_audio::LoopDeck) (the
/// audio half of the video player's verb handling). Targeted verbs (load/cue/seek)
/// re-cue the video; for the MVP single-asset audio deck they are treated as a
/// fresh `vamp` (re-cue to the head) — the deck plays one bound asset, exactly as
/// the video executor honours them by re-seeking.
#[cfg(feature = "ffmpeg")]
fn apply_audio_verb(deck: &mut multiview_audio::LoopDeck, verb: &crate::player::TransportVerb) {
    use crate::player::TransportVerb;
    match verb {
        TransportVerb::Vamp | TransportVerb::Play => deck.vamp(),
        TransportVerb::Pause => deck.pause(),
        TransportVerb::Stop => deck.stop(),
        TransportVerb::ArmExit | TransportVerb::TakeExit => deck.arm_exit(),
        TransportVerb::CancelExit => deck.cancel_exit(),
        // load/cue/seek: the MVP plays one bound asset; re-cue to the head.
        TransportVerb::Load { .. } | TransportVerb::Cue { .. } | TransportVerb::Seek { .. } => {
            deck.stop();
            deck.vamp();
        }
    }
}

/// Target buffered lead (frames) the player audio keeps ahead of the bus's read
/// cursor: half a second at 48 kHz (mirrors the tone loop's [`TONE_LEAD_FRAMES`]).
#[cfg(feature = "ffmpeg")]
const PLAYER_AUDIO_LEAD_FRAMES: i64 = 24_000;

/// Refill the player-audio store whenever the lead drops below this (half the
/// target) — a hysteresis band so the thread does a few bounded reads then sleeps.
#[cfg(feature = "ffmpeg")]
const PLAYER_AUDIO_REFILL_THRESHOLD_FRAMES: i64 = PLAYER_AUDIO_LEAD_FRAMES / 2;

#[cfg(all(test, feature = "ffmpeg"))]
mod player_audio_tests {
    //! Integration tests for the media-player audio loop driver (ADR-T019),
    //! `ffmpeg`-gated: each packages an in-memory tone to a lossless WAV via the
    //! `ffmpeg` CLI (LGPL-clean), exactly as the audio decode-thread tests do.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #![allow(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::float_cmp,
        clippy::indexing_slicing
    )]

    use std::f64::consts::PI;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use multiview_audio::store::AudioStore;
    use multiview_core::time::Rational;

    use super::{canonical_format, player_audio_loop, PlayerAudioPlan};
    use crate::player::{TransportMailbox, TransportVerb};

    const FS: u32 = 48_000;

    fn ffmpeg_cli_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn tone(amp: f64, freq: f64, seconds: f64) -> Vec<f32> {
        let n = (f64::from(FS) * seconds).round() as usize;
        let mut out = Vec::with_capacity(n * 2);
        let w = 2.0 * PI * freq / f64::from(FS);
        for i in 0..n {
            let s = (amp * (w * i as f64).sin()) as f32;
            out.push(s);
            out.push(s);
        }
        out
    }

    fn encode_to_wav(dir: &Path, samples: &[f32]) -> PathBuf {
        let raw = dir.join("clip.f32");
        let wav = dir.join("clip.wav");
        {
            let mut file = std::fs::File::create(&raw).unwrap();
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for &s in samples {
                bytes.extend_from_slice(&s.to_le_bytes());
            }
            file.write_all(&bytes).unwrap();
        }
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ac",
                "2",
                "-i",
            ])
            .arg(&raw)
            .args(["-c:a", "pcm_s16le"])
            .arg(&wav)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg failed to encode the WAV");
        wav
    }

    /// The driver primes from a 1 s tone clip and LOOPS its `[0, 1 s)` vamp window:
    /// after advancing the bus read cursor PAST one clip length, the store still
    /// yields the tone (a one-shot decode would read silence past EOF). The loop
    /// is sample-locked (period == the vamp window's sample count) and the thread
    /// only writes the store — it never paces the (simulated) clock.
    #[test]
    fn player_audio_loops_the_vamp_window_past_one_clip_length() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio loop test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 1.0 s of a 1 kHz tone (48_000 frames). At 48 fps, frames 0..48 = [0,1 s).
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 1.0));

        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let mailbox = Arc::new(TransportMailbox::new());
        let plan = PlayerAudioPlan {
            id: "vt1".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48, // 48 frames @ 48 fps = 1 s = 48_000 samples
            cadence: Rational::new(48, 1),
            loop_on_start: true,
            output_cadence: Rational::new(48, 1),
            mailbox: Arc::clone(&mailbox),
        };

        // Run the driver on its own thread (the data-plane peer of the video
        // player thread).
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });

        // Simulate the bus SAMPLING the store at ~real-time: pull tick-sized blocks
        // (1600 frames ≈ one 30 fps tick), pacing the pulls so the driver (which
        // keeps a 0.5 s lead) stays ahead — exactly how the bake consumer samples a
        // per-source store. Advance well past one clip length (1.5 clips) and keep
        // every block read, so the LAST block is past frame 48_000.
        let mut blocks: Vec<Vec<f32>> = Vec::new();
        let mut advanced = 0usize;
        let target = 72_000usize; // 1.5 × the 48_000-frame clip
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        // Let the driver prime + publish its first lead before the bus starts.
        std::thread::sleep(std::time::Duration::from_millis(250));
        while advanced < target && std::time::Instant::now() < deadline {
            let block = store.read(1600);
            blocks.push(block.interleaved().to_vec());
            advanced += 1600;
            // ~33 ms per 1600-frame tick: pace near real-time so the driver refills.
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");

        assert!(
            advanced >= target,
            "did not advance past one clip length in time (advanced {advanced})"
        );
        // A block well PAST one full clip length (frame ~60_000, deep in lap 2) must
        // carry real tone energy: a one-shot decode would silence-fill there; the
        // LOOP keeps the tone alive (sample-locked, period == the vamp window).
        let late_idx = 60_000 / 1600; // the block covering frame ~60_000
        let late = blocks.get(late_idx).cloned().unwrap_or_default();
        let energy: f64 = late.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
        assert!(
            energy > 1.0,
            "past one clip length the store is silent — the audio did not LOOP (energy {energy:.3})"
        );
    }

    /// A player with NO audio stream (a silent asset) rides silence: the driver
    /// primes an empty deck and the store reads silence forever — never a stall,
    /// never a panic. (Here: a non-existent path → open fails → empty deck.)
    #[test]
    fn a_missing_or_silent_asset_rides_silence() {
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let mailbox = Arc::new(TransportMailbox::new());
        let plan = PlayerAudioPlan {
            id: "silent".to_owned(),
            location: "/nonexistent/no-such-asset.wav".to_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 48,
            cadence: Rational::new(48, 1),
            loop_on_start: true,
            output_cadence: Rational::new(48, 1),
            mailbox,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Let it run briefly, sampling the store — every read is full silence.
        for _ in 0..5 {
            let block = store.read(1600);
            assert_eq!(
                block.frame_count(),
                1600,
                "silent player still returns full blocks"
            );
            assert!(
                block.interleaved().iter().all(|&s| s == 0.0),
                "a missing/silent asset must ride silence"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
    }

    /// An armed exit (or a non-looping player) settles the audio to silence:
    /// `loop_on_start = false` plays one lap then the deck goes silent — the bus
    /// contribution ends cleanly (the video tile holds its last frame separately).
    #[test]
    fn a_non_looping_player_settles_audio_to_silence_after_one_lap() {
        if !ffmpeg_cli_available() {
            eprintln!("ffmpeg CLI unavailable; skipping player-audio settle test");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wav = encode_to_wav(dir.path(), &tone(0.5, 1000.0, 0.25)); // 0.25 s = 12_000 frames
        let store = Arc::new(AudioStore::new(canonical_format(), 96_000));
        let stop = Arc::new(AtomicBool::new(false));
        let mailbox = Arc::new(TransportMailbox::new());
        let plan = PlayerAudioPlan {
            id: "oneshot".to_owned(),
            location: wav.to_string_lossy().into_owned(),
            vamp_in_frames: 0,
            vamp_out_frames: 12, // 12 frames @ 48 fps = 0.25 s = 12_000 samples
            cadence: Rational::new(48, 1),
            loop_on_start: false, // one-shot
            output_cadence: Rational::new(48, 1),
            mailbox,
        };
        let loop_store = Arc::clone(&store);
        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            player_audio_loop(&plan, &loop_store, &loop_stop);
        });
        // Advance ~3 clip lengths; after the single lap + exit tail the store must
        // settle to silence.
        let mut advanced = 0usize;
        let mut tail_block = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while advanced < 36_000 && std::time::Instant::now() < deadline {
            let buffered = store.buffered_frames();
            if buffered == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
            let take = buffered.min(1600);
            tail_block = store.read(take).interleaved().to_vec();
            advanced += take;
        }
        stop.store(true, Ordering::Release);
        handle.join().expect("player audio loop thread panicked");
        let energy: f64 = tail_block
            .iter()
            .map(|&v| f64::from(v) * f64::from(v))
            .sum();
        assert!(
            energy < 1e-3,
            "a non-looping player must settle its audio to silence after one lap (tail energy {energy:.5})"
        );
        let _ = TransportVerb::Vamp; // keep the import meaningful across cfgs
    }
}
