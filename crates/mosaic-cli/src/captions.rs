//! Native in-pipeline **HLS WebVTT caption ingest** (features `ffmpeg` +
//! `overlay`).
//!
//! libav opens an HLS URL as a single program (the chosen video variant); it does
//! **not** surface the master playlist's separate `TYPE=SUBTITLES` rendition as a
//! decodable stream. So when a source's [`CaptionSelector`] resolves to an
//! HLS/`WebVTT` subtitle rendition, this module:
//!
//! 1. fetches the **master** playlist text (the source URL) via libav's I/O,
//! 2. parses it with [`mosaic_input::hls::MasterPlaylist`],
//! 3. [`picks`](mosaic_input::hls::MasterPlaylist::pick_subtitle) the subtitle
//!    rendition for the requested language (or the default),
//! 4. resolves the rendition's (usually relative) `URI` against the master's base
//!    directory ([`resolve_rendition_uri`]), and
//! 5. spawns a **second isolated demux** of that rendition `m3u8` on its own
//!    ingest thread that decodes each `WebVTT` cue with
//!    [`mosaic_ffmpeg::CaptionDecoder`] and publishes it into a per-source
//!    [`CaptionCueStore`].
//!
//! The reader is **best-effort**: any fetch/parse/decode error logs and yields no
//! cues (the tile simply shows none). It only ever *writes* the lock-free cue
//! store — it can neither pace nor stall the output clock (invariant #1) nor
//! back-pressure the engine (invariant #10), exactly like the video ingest
//! threads. The off-hot-path overlay baker *samples* the store at each output
//! tick (`active_at(pts)`), mirroring frame-store latch-on-tick.
//!
//! ## Timing (invariant #3)
//!
//! libav applies the rendition's `X-TIMESTAMP-MAP` itself, so the `WebVTT`
//! packets arrive already rebased onto a 0-based media timeline (the first cue's
//! packet PTS is `1.0s`, matching the bipbop video's source-relative timeline).
//! The reader anchors each cue's window with [`mosaic_ffmpeg::CaptionDecoder`]
//! (packet PTS rebased to ns through the stream time-base) and publishes it on
//! the same source-relative timeline the video frames use, so a cue burns in at
//! the same media instant the output clock samples it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ffmpeg_next as ffmpeg;

use mosaic_config::schema::CaptionSelector;
use mosaic_core::time::MediaTime;
use mosaic_ffmpeg::caption::CaptionCue;
use mosaic_ffmpeg::{CaptionDecoder, CaptionSource};
use mosaic_input::caption_store::CaptionCueStore;
use mosaic_input::hls::MasterPlaylist;

/// A per-source caption cue store: `CaptionCueStore` instantiated for the unified
/// [`CaptionCue`] model. The reader thread writes it; the overlay baker samples it.
pub type CueStore = CaptionCueStore<CaptionCue>;

/// A resolved per-source caption reader plan: which rendition `m3u8` to demux and
/// the store its decoded cues are published into.
pub struct CaptionPlan {
    /// The source id this reader serves (the tile its cues burn into).
    pub id: String,
    /// The absolute rendition media-playlist URL to demux for `WebVTT` cues.
    pub rendition_url: String,
    /// The store the decoded cues are published into (shared with the baker).
    pub store: Arc<CueStore>,
}

/// Resolve a rendition's (possibly relative) `URI` against the master playlist's
/// base directory, `url`-crate-free.
///
/// Rules (RFC 8216 / RFC 3986 §5, the subset HLS uses in practice):
/// * An absolute URL (`scheme://…`) is returned unchanged.
/// * A root-relative path (`/a/b.m3u8`) replaces the master's path, keeping the
///   master's `scheme://authority`.
/// * Otherwise the relative reference is joined onto the master's **base
///   directory** (the master URL with its last path segment — the filename —
///   stripped), with any query/fragment on the master dropped.
///
/// A master URL with no recognizable `scheme://authority` (e.g. a bare local
/// path) falls back to a simple directory join so a `file:`-less local master
/// still resolves.
#[must_use]
pub fn resolve_rendition_uri(master_url: &str, rendition_uri: &str) -> String {
    // An already-absolute rendition URI (has a scheme) is used verbatim.
    if has_scheme(rendition_uri) {
        return rendition_uri.to_owned();
    }

    // Strip any query/fragment off the master before deriving its directory.
    let master_base = master_url.split(['?', '#']).next().unwrap_or(master_url);

    // Split the master into `scheme://authority` and the path that follows.
    let (origin, path) = split_origin(master_base);

    // A root-relative rendition path replaces the master's path entirely.
    if let Some(abs_path) = rendition_uri.strip_prefix('/') {
        return match origin {
            Some(origin) => format!("{origin}/{abs_path}"),
            // No origin (a bare local master): treat as a filesystem-absolute path.
            None => format!("/{abs_path}"),
        };
    }

    // Directory of the master path: everything up to and including the last '/'.
    let dir = match path.rfind('/') {
        Some(idx) => path.get(..=idx).unwrap_or(""),
        None => "",
    };
    let joined = format!("{dir}{rendition_uri}");
    match origin {
        Some(origin) => format!("{origin}{joined}"),
        None => joined,
    }
}

/// Whether `s` begins with a URL scheme (`scheme://`), so it is already absolute.
fn has_scheme(s: &str) -> bool {
    match s.find("://") {
        Some(idx) => {
            let scheme = s.get(..idx).unwrap_or("");
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        }
        None => false,
    }
}

/// Split a URL into its `scheme://authority` origin and the path that follows.
///
/// Returns `(Some(origin), path)` for an `scheme://authority/path` URL (origin
/// has **no** trailing slash, path **starts** with the leading slash, or is empty
/// when the URL is just an origin); `(None, whole)` when there is no
/// `scheme://authority` (a bare path).
fn split_origin(url: &str) -> (Option<&str>, &str) {
    let Some(scheme_end) = url.find("://") else {
        return (None, url);
    };
    let after = scheme_end.saturating_add(3);
    let authority_start = url.get(after..).unwrap_or("");
    // The path begins at the first '/' after the authority.
    match authority_start.find('/') {
        Some(rel) => {
            let split = after.saturating_add(rel);
            (url.get(..split), url.get(split..).unwrap_or(""))
        }
        // Origin only, no path (e.g. `https://host`).
        None => (Some(url), ""),
    }
}

/// Whether a [`mosaic_config::Source`]'s selector resolves to the **native
/// in-container DVB-sub (bitmap) caption** path: a `Ts`/`File` source with an
/// `Auto` or `Track` selector. The DVB-sub stream lives in the SAME container as
/// the video, so this path decodes it on the source's own ingest thread (no
/// second demux) and publishes bitmap cues into the per-source store.
///
/// `TeletextPage` is explicitly **not** this path (it is the teletext decoder),
/// and `Off`/`EmbeddedCc`/`Sidecar` are other paths. HLS sources take the
/// separate WebVTT-rendition path ([`webvtt_language`]).
#[must_use]
pub fn dvbsub_selector(kind: &mosaic_config::SourceKind, selector: &CaptionSelector) -> bool {
    use mosaic_config::SourceKind;
    // Only an in-container TS/file source carries a muxed DVB-sub stream the
    // video-ingest thread can decode as a sibling of the video packets.
    if !matches!(kind, SourceKind::Ts { .. } | SourceKind::File { .. }) {
        return false;
    }
    matches!(
        selector,
        CaptionSelector::Auto | CaptionSelector::Track { .. }
    )
}

/// Publish each decoded **bitmap** cue into the store using its own `[start,
/// end)` window — the DVB-sub sibling of [`publish_cues`], but with **no
/// `CuePacer`**: these cues are decoded inside the source's already-PTS-paced
/// video ingest loop, so the bitmap cue is published at the same media instant
/// its packet arrives (no separate wall-clock pacing needed). The store is the
/// lock-free hand-off the off-hot-path baker samples per tick (#1/#10).
pub fn publish_bitmap_cues(store: &CueStore, cues: Vec<CaptionCue>) {
    for cue in cues {
        let (start, end) = (cue.start(), cue.end());
        store.publish(start, end, cue);
    }
}

/// Whether a [`mosaic_config::Source`] should attempt native HLS `WebVTT` caption
/// ingest for the given selector, and the language to prefer.
///
/// Returns `Some(language)` (the language may itself be `None` for "default")
/// when the selector resolves to the HLS/`WebVTT` rendition path:
/// * [`CaptionSelector::Auto`] on any HLS source,
/// * [`CaptionSelector::Track`] (treated as a language tag) on any HLS source.
///
/// Other selectors (`Off`, `TeletextPage`, `EmbeddedCc`, `Sidecar`) are **not**
/// this module's path and return `None` (Phase 2/3 or the sidecar path).
#[must_use]
pub fn webvtt_language(
    kind: &mosaic_config::SourceKind,
    selector: &CaptionSelector,
) -> Option<Option<String>> {
    use mosaic_config::SourceKind;
    // Only an HLS source carries a master playlist with a SUBTITLES rendition.
    if !matches!(kind, SourceKind::Hls { .. }) {
        return None;
    }
    match selector {
        CaptionSelector::Auto => Some(None),
        CaptionSelector::Track { id } => Some(Some(id.clone())),
        // Off / teletext / embedded-cc / sidecar are not the HLS-rendition path.
        _ => None,
    }
}

/// Build a [`CaptionPlan`] for `source` if its selector resolves to an HLS
/// `WebVTT` rendition: fetch + parse the master, pick the subtitle rendition for the
/// requested language, and resolve its rendition URL.
///
/// Best-effort: a fetch/parse failure or a master with no usable subtitle
/// rendition logs and returns `None` (the source simply shows no captions — it
/// must never fail the pipeline build of a live source).
#[must_use]
pub fn caption_plan_for(source: &mosaic_config::Source) -> Option<CaptionPlan> {
    let selector = source.captions.as_ref()?;
    let language = webvtt_language(&source.kind, selector)?;
    let master_url = hls_url(&source.kind)?;

    let text = match fetch_text(master_url) {
        Ok(text) => text,
        Err(reason) => {
            tracing::warn!(source = %source.id, %reason, "could not fetch HLS master for captions");
            return None;
        }
    };
    let master = match MasterPlaylist::parse(&text) {
        Ok(master) => master,
        Err(err) => {
            tracing::warn!(source = %source.id, error = %err, "HLS master parse failed; no captions");
            return None;
        }
    };
    let rendition = master.pick_subtitle(language.as_deref())?;
    let uri = rendition.uri.as_deref()?;
    let rendition_url = resolve_rendition_uri(master_url, uri);
    tracing::info!(
        source = %source.id,
        language = ?rendition.language,
        %rendition_url,
        "native HLS WebVTT caption rendition resolved"
    );
    Some(CaptionPlan {
        id: source.id.clone(),
        rendition_url,
        store: Arc::new(CueStore::new()),
    })
}

/// The HLS master URL for a source kind, if it is an HLS source.
fn hls_url(kind: &mosaic_config::SourceKind) -> Option<&str> {
    match kind {
        mosaic_config::SourceKind::Hls { url } => Some(url.as_str()),
        _ => None,
    }
}

/// Fetch the text of a (small) playlist URL.
///
/// For an `http(s)` master this shells out to the `curl` CLI — the same
/// shell-out-to-a-system-tool pattern the pipeline already uses to stage `test`
/// clips with the `ffmpeg` CLI — keeping the caption fetch dependency-free (no
/// HTTP crate, no new `cargo deny` surface). A bare local path is read from disk.
///
/// Bounded: a fetched body over [`MAX_PLAYLIST_BYTES`] is rejected so a
/// misbehaving server cannot grow memory unboundedly. A real master/rendition
/// playlist is a few KB.
fn fetch_text(url: &str) -> Result<String, String> {
    if has_scheme(url) {
        let output = std::process::Command::new("curl")
            .args([
                "-fsSL",
                "--max-time",
                "30",
                "--max-filesize",
                &MAX_PLAYLIST_BYTES.to_string(),
                url,
            ])
            .output()
            .map_err(|e| format!("spawning curl: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "curl failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        if output.stdout.len() > MAX_PLAYLIST_BYTES {
            return Err("playlist exceeds the byte budget".to_owned());
        }
        return String::from_utf8(output.stdout).map_err(|e| e.to_string());
    }
    // A bare local path (no scheme): read it from disk.
    std::fs::read_to_string(url).map_err(|e| e.to_string())
}

/// Upper bound on a fetched playlist's size (a real master/rendition is a few KB).
const MAX_PLAYLIST_BYTES: usize = 8 * 1024 * 1024;

/// The per-source caption reader loop, run on a dedicated ingest thread.
///
/// Opens the resolved rendition `m3u8`, finds its subtitle stream, builds a
/// `WebVTT` [`CaptionDecoder`], pumps packets, and publishes each decoded cue
/// into the per-source [`CueStore`]. Returns when `stop` is raised or the
/// rendition demux ends (a VOD rendition has an end). Any error is logged and the
/// loop ends (best-effort — no cues, never a stall).
///
/// The loop only ever *writes* the lock-free store, so it can neither pace nor
/// stall the output clock (invariant #1) nor back-pressure the engine (#10). A
/// VOD rendition (bipbop) plays out once; its decoded cues remain in the bounded
/// store for the baker to sample — we do not hammer-reconnect a finished VOD.
pub fn caption_loop(plan: &CaptionPlan, stop: &AtomicBool) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    if let Err(reason) = read_captions(plan, stop) {
        tracing::warn!(source = %plan.id, %reason, "caption rendition ended/errored");
    }
}

/// Open `plan.rendition_url`, decode its `WebVTT` subtitle stream, and publish
/// each cue into `plan.store`. Returns `Ok(())` at clean EOF.
///
/// libav's HLS demuxer is strict about segment extensions; the rendition's
/// `.webvtt` segments mismatch the default allow-list, so `extension_picky` is
/// disabled for this isolated demux (it never touches the program path). No FFI,
/// no `unsafe`: only `ffmpeg-next`'s safe `Input`/`Parameters` value API bridges
/// into `mosaic-ffmpeg`'s safe `CaptionDecoder`.
fn read_captions(plan: &CaptionPlan, stop: &AtomicBool) -> Result<(), String> {
    mosaic_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    // The HLS demuxer rejects `.webvtt` segments under its default extension
    // allow-list; relax it for THIS isolated rendition demux only. Cap the
    // stream-probe so `avformat_find_stream_info` does NOT read the entire VOD
    // rendition (all 60 segments) up front — a tiny probe is enough to identify
    // the single WebVTT subtitle stream, and decoding then streams the cues out
    // promptly rather than after a multi-second whole-playlist read-ahead.
    let mut opts = ffmpeg::Dictionary::new();
    opts.set("extension_picky", "0");
    opts.set("allowed_extensions", "ALL");
    opts.set("probesize", "131072");
    opts.set("analyzeduration", "0");

    let mut input = ffmpeg::format::input_with_dictionary(&plan.rendition_url.as_str(), opts)
        .map_err(|e| e.to_string())?;

    let (stream_index, params, time_base) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Subtitle)
            .ok_or_else(|| "rendition has no subtitle stream".to_owned())?;
        (
            stream.index(),
            stream.parameters(),
            mosaic_ffmpeg::from_ff_rational(stream.time_base()),
        )
    };

    let mut decoder = CaptionDecoder::from_parameters(CaptionSource::WebVtt, params, time_base)
        .map_err(|e| e.to_string())?;

    // Pace each decoded cue's publish to wall-clock by its start PTS (invariant
    // #4 — a VOD-as-live rendition is paced by PTS, never slurped). Without this
    // the unpaced reader would drain the whole VOD into the bounded drop-oldest
    // store in seconds, evicting the early cues before the output clock samples
    // them; pacing keeps the store window aligned with output media time so the
    // baker's per-tick `active_at(pts)` lands on the right cue. The pacer only
    // ever *delays the writer*; it never blocks the reader of the store, so it
    // cannot pace or stall the output clock (#1/#10).
    let mut pacer = CuePacer::new();

    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() != stream_index {
                    continue;
                }
                // A decode error on one packet is logged and skipped — captions
                // are intermittent and a malformed cue must never stall ingest.
                match decoder.decode(&packet) {
                    Ok(cues) => publish_cues(plan, &mut pacer, cues, stop),
                    Err(e) => {
                        tracing::debug!(source = %plan.id, error = %e, "caption packet decode error");
                    }
                }
            }
            Err(ffmpeg::Error::Eof) => return Ok(()),
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// Publish each decoded cue into the store using its own `[start, end)` window,
/// paced to wall-clock by the cue's start PTS so the bounded store window tracks
/// output media time.
fn publish_cues(
    plan: &CaptionPlan,
    pacer: &mut CuePacer,
    cues: Vec<CaptionCue>,
    stop: &AtomicBool,
) {
    for cue in cues {
        let (start, end) = (cue.start(), cue.end());
        pacer.wait_for(start, stop);
        if stop.load(Ordering::Acquire) {
            return;
        }
        plan.store.publish(start, end, cue);
    }
}

/// A wall-clock pacer keyed on a cue's start PTS (invariant #4): the first cue
/// anchors `base_instant = now` to `base_pts = start`; each later cue is held
/// until `now - base_instant >= start - base_pts`. A backwards PTS re-anchors so
/// a discontinuity never stalls the reader for long.
struct CuePacer {
    anchor: Option<(std::time::Instant, MediaTime)>,
}

impl CuePacer {
    fn new() -> Self {
        Self { anchor: None }
    }

    /// Block (in `stop`-checked slices) until wall-clock reaches `pts`'s release
    /// instant. The first call anchors the timeline and returns immediately.
    fn wait_for(&mut self, pts: MediaTime, stop: &AtomicBool) {
        let Some((base_instant, base_pts)) = self.anchor else {
            self.anchor = Some((std::time::Instant::now(), pts));
            return;
        };
        if pts < base_pts {
            self.anchor = Some((std::time::Instant::now(), pts));
            return;
        }
        let delta = pts.saturating_sub(base_pts);
        let target_ns = u64::try_from(delta.as_nanos()).unwrap_or(0);
        let target = base_instant + std::time::Duration::from_nanos(target_ns);
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = std::time::Instant::now();
            if now >= target {
                return;
            }
            let remaining = target.saturating_duration_since(now);
            std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn relative_rendition_uri_joins_against_the_master_directory() {
        // The bipbop case: a relative rendition URI under the master's directory.
        let master =
            "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8";
        let got = resolve_rendition_uri(master, "subtitles/eng/prog_index.m3u8");
        assert_eq!(
            got,
            "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/subtitles/eng/prog_index.m3u8"
        );
    }

    #[test]
    fn root_relative_rendition_uri_replaces_the_master_path() {
        let master = "https://host.example/a/b/c/master.m3u8";
        let got = resolve_rendition_uri(master, "/subs/en/index.m3u8");
        assert_eq!(got, "https://host.example/subs/en/index.m3u8");
    }

    #[test]
    fn absolute_rendition_uri_is_used_verbatim() {
        let master = "https://host.example/a/master.m3u8";
        let got = resolve_rendition_uri(master, "https://cdn.other/subs/index.m3u8");
        assert_eq!(got, "https://cdn.other/subs/index.m3u8");
    }

    #[test]
    fn master_query_and_fragment_are_dropped_before_joining() {
        let master = "https://host.example/a/master.m3u8?token=xyz#frag";
        let got = resolve_rendition_uri(master, "subs/index.m3u8");
        assert_eq!(got, "https://host.example/a/subs/index.m3u8");
    }

    #[test]
    fn bare_local_master_directory_join() {
        // No scheme://authority — a simple directory join still works.
        let master = "/srv/streams/master.m3u8";
        let got = resolve_rendition_uri(master, "subs/index.m3u8");
        assert_eq!(got, "/srv/streams/subs/index.m3u8");
        let abs = resolve_rendition_uri(master, "/abs/index.m3u8");
        assert_eq!(abs, "/abs/index.m3u8");
    }

    #[test]
    fn dvbsub_selector_only_for_ts_or_file_auto_or_track() {
        use mosaic_config::SourceKind;
        let ts = SourceKind::Ts {
            url: "udp://x".to_owned(),
        };
        let file = SourceKind::File {
            path: "/x.ts".to_owned(),
        };
        let hls = SourceKind::Hls {
            url: "https://x/m.m3u8".to_owned(),
        };
        assert!(dvbsub_selector(&ts, &CaptionSelector::Auto));
        assert!(dvbsub_selector(
            &file,
            &CaptionSelector::Track {
                id: "eng".to_owned()
            }
        ));
        // Teletext stays out (it is the teletext decoder, not dvbsub).
        assert!(!dvbsub_selector(
            &ts,
            &CaptionSelector::TeletextPage { page: 801 }
        ));
        assert!(!dvbsub_selector(&file, &CaptionSelector::Off));
        // HLS is the WebVTT-rendition path, not the in-container dvbsub path.
        assert!(!dvbsub_selector(&hls, &CaptionSelector::Auto));
    }

    #[test]
    fn publish_bitmap_cues_inserts_each_cue_at_its_window() {
        use mosaic_ffmpeg::caption::{CaptionCue, CueRect};
        let store = CueStore::new();
        let rgba = vec![0_u8; 2 * 2 * 4];
        let rect = CueRect::new(0, 0, 2, 2);
        let cue = CaptionCue::try_bitmap(
            MediaTime::from_nanos(1_000),
            MediaTime::from_nanos(3_000),
            rgba,
            rect,
        )
        .expect("valid bitmap cue");
        publish_bitmap_cues(&store, vec![cue]);
        assert!(store.active_at(MediaTime::from_nanos(2_000)).is_some());
        assert!(store.active_at(MediaTime::from_nanos(500)).is_none());
    }

    #[test]
    fn webvtt_language_only_for_hls_auto_or_track() {
        use mosaic_config::SourceKind;
        let hls = SourceKind::Hls {
            url: "https://x/master.m3u8".to_owned(),
        };
        assert_eq!(webvtt_language(&hls, &CaptionSelector::Auto), Some(None));
        assert_eq!(
            webvtt_language(
                &hls,
                &CaptionSelector::Track {
                    id: "en".to_owned()
                }
            ),
            Some(Some("en".to_owned()))
        );
        assert_eq!(webvtt_language(&hls, &CaptionSelector::Off), None);
        // A non-HLS source never takes the HLS-rendition path.
        let file = SourceKind::File {
            path: "/x.ts".to_owned(),
        };
        assert_eq!(webvtt_language(&file, &CaptionSelector::Auto), None);
    }
}
