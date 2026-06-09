# ADR-T011: HLS rendition isolation — discard unrouted subtitle streams in the main demuxer; the isolated WebVTT reader is the sole WebVTT path

- **Status:** Accepted
- **Area:** Streaming/Timing (ingest)
- **Date:** 2026-06-09
- **Source brief:** [streaming-gotchas.md](../research/streaming-gotchas.md) §§3–4
- **Relates to:** [ADR-T004](ADR-T004.md) (HLS ingest pacing), [ADR-T002](ADR-T002.md)
  (last-good-frame + tile state machine), invariants #1 (output-clock), #2
  (last-good-frame), #10 (isolation)

> Numbered T011: T009 (per-tile ring publish) and T010 (Dante audio) were already
> assigned in this branch, so the next free Streaming/Timing slot is used.

## Context

An HLS source whose **master** playlist carries a WebVTT subtitle rendition
(`EXT-X-MEDIA:TYPE=SUBTITLES`, e.g. ABC News AU
`https://c.mjh.nz/abc-news.m3u8`) can kill the whole video input. libav's HLS
demuxer folds a selected rendition into the **one shared `AVFormatContext`** it
opens for the video. When that rendition is surfaced and its `.vtt` segment is
corrupt / 404 / token-expired, libav either aborts `avformat_open_input`
("Error when loading first segment", `hls.c`) or makes `av_read_frame` return
that rendition's error for the **entire context**. That error escaped the main
read loop in `multiview-cli`'s `open_and_stream`
(`Err(other) => return Err(...)`), killing the ingest thread and blacking out the
tile.

Whether libav even surfaces the WebVTT rendition is **version- and
strictness-gated**. In FFmpeg 7.1 `hls.c` `new_rendition`:

```c
/* TODO: handle subtitles (each segment has to parsed separately) */
if (c->ctx->strict_std_compliance > FF_COMPLIANCE_EXPERIMENTAL)
    if (type == AVMEDIA_TYPE_SUBTITLE) {
        av_log(c->ctx, AV_LOG_WARNING, "Can't support the subtitle(uri: %s)\n", info->uri);
        return NULL;   // rendition dropped pre-probe
    }
```

So a **normal/strict** open drops the rendition before probing it (safe), while
`strict <= experimental` (or a different libav build) **adds** the WebVTT stream
to the shared context — at which point the broken `.vtt` is fatal to the video.

Crucially, Multiview **already** ingests HLS WebVTT via a **separate isolated
reader** (`multiview-cli`'s `captions.rs::read_captions`): its own
`AVFormatContext` on its own thread (spawned by the `IngestSupervisor`), opened
with `allowed_extensions=ALL`. The main video demuxer therefore needs **nothing**
from the WebVTT rendition; stopping it from fetching the rendition loses no
stream.

## Decision

The **main video demuxer EXCLUDES and DISCARDS unrouted subtitle renditions**;
the **isolated WebVTT reader is the sole WebVTT path**. Four parts:

1. **Primary, version-robust guard (`multiview-ffmpeg`).** A new safe free
   function `discard_unrouted_subtitles(input, keep)` iterates the opened input's
   streams and, for every stream whose `medium() == Subtitle` whose index `!=
   keep`, sets `discard = AVDISCARD_ALL` (the one raw field write libav exposes —
   ffmpeg-next 8.1 `StreamMut` has no `set_discard`; bounded `// SAFETY:` in the
   crate's `unsafe = deny` island). `open_and_stream` calls it **before the first
   `read_packet`** (libav's HLS `recheck_discard_flags` fires one-shot on the
   first packet), passing `keep = Some(dvbsub.stream_index)` (the routed MPEG-TS
   DVB-sub stream) or `None` (HLS → all subtitles discarded).
2. **Open-time hardening (`multiview-cli` `ingest_open_options`).** For an HLS
   master (`.m3u8`, incl. a resolved YouTube master), set `strict=normal` (so
   libav drops the SUBTITLES rendition pre-probe per the gate above),
   `seg_max_retry=8` (a transient segment fetch retries), and a sane
   `protocol_whitelist=file,http,https,tcp,tls,crypto,data` (HTTPS + AES `crypto`
   open). **`allowed_extensions` is never widened to admit `.vtt`** in the main
   demuxer.
3. **Main-source recovery (unchanged).** `ingest_loop` already
   supervised-reconnects a live source on `Err` (capped-exponential + per-source
   jitter), so the main read loop keeps returning `Err` → reconnect.
4. **Isolated WebVTT reader recovery.** `caption_loop` is now a supervised
   reconnect loop mirroring `ingest_loop`: a **live** rendition (HLS) backs off
   and retries on EOF/error (a transient `.vtt` 404 / token-expiry never
   permanently kills captions); a finite VOD rendition plays out once. The video
   reconnect helpers (`reconnect_backoff` / `JitterRng` / `next_reconnect_attempt`
   / `sleep_interruptible`) are reused (visibility bumped to `pub(crate)`).

## Policy invariants

- **The discard keys strictly on `medium() == Subtitle`.** Audio renditions
  (also folded into a shared HLS context) and video are **never** touched — an
  HLS audio rendition keeps flowing.
- **A routed in-container DVB-sub stream is kept** (`keep`): the MPEG-TS DVB-sub
  caption route decodes that stream as a sibling of the video packets, so it must
  not be discarded.
- **Discarding an unrouted stream loses nothing.** Every stream Multiview
  *consumes* has a route (video → decoder; DVB-sub → the kept subtitle route;
  CEA-608/708 → A53 side-data on the video frames) **or** its own isolated
  demuxer (HLS WebVTT → `read_captions`). An unrouted subtitle rendition is, by
  construction, served by the isolated reader or not consumed at all.
- **Isolation/output-clock preserved.** The discard + recovery touch only the
  ingest threads, which write the lock-free last-good stores and never pace or
  stall the output clock (invariants #1/#10); a broken rendition rides
  WARN + RECOVER while the tile holds last-good (#2).

## Consequences

- An HLS source with a broken/expired WebVTT rendition keeps its **video tile
  alive**, and the isolated reader keeps retrying the captions — the operator's
  hard "ingest all streams, a broken one recovers, never kills the others"
  requirement.
- Version-robust: even if a future libav default or a leaked `strict` setting
  surfaces the WebVTT rendition into the shared context, the discard removes it
  before the first read.
- One extra stream-iteration per (re)open (negligible); no per-packet cost.

## Empirical note (libav 7.1.4 / libavformat 61)

Verified offline against the installed FFmpeg 7.1.4. With the **default**
`strict` (normal), the HLS demuxer drops the WebVTT rendition at parse
("Can't support the subtitle"), so the broken-`.vtt` fixture does **not** kill
the video through the default open path on this build. Forcing
`strict=experimental` + `allowed_extensions=ALL` reproduces the dangerous
shared-context shape (the WebVTT subtitle stream is surfaced alongside the
video); `discard_unrouted_subtitles` then removes it while leaving audio/video
untouched (proven by `crates/multiview-ffmpeg/tests/discard_subtitles.rs`). The
fix is therefore version-robust **defense-in-depth**: (a) hardens the open so the
rendition is dropped pre-probe, and (b) discards it unconditionally if any build
surfaces it anyway.

## Alternatives rejected

- **Widen `allowed_extensions` to accept `.vtt` in the main demuxer.** This
  re-introduces the shared-context fragility — the corrupt segment is then fetched
  by the one video context, and its failure is fatal to the video. The isolated
  reader (which sets `allowed_extensions=ALL` on its **own** context) is the only
  place that path belongs.
- **Swallow the read error in the main loop** (`Err(other) => continue`). This
  blinds the supervisor to a genuinely dead source and breaks the live-source
  reconnect/last-good contract (invariant #2). The error must keep propagating to
  the reconnect bracket; the fix is to stop the broken rendition from being read
  at all.
