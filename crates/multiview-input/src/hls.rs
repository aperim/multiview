//! Minimal HLS **master-playlist** parser: discover subtitle (`WebVTT`) renditions
//! (`#EXT-X-MEDIA:TYPE=SUBTITLES`) and their variant linkage.
//!
//! libav opens an HLS URL as a single program (the chosen variant); it does
//! **not** surface the master playlist's separate `SUBTITLES` rendition as a
//! decodable stream. To decode native HLS captions Multiview must read the MASTER
//! itself, find the subtitle rendition's media-playlist URI, and demux that as a
//! second isolated source (its cues are then sampled at the output tick like any
//! other input — never pacing the engine, invariants #1/#10).
//!
//! This is the pure, dependency-free parser (no I/O, no `url` crate): a bounded,
//! panic-free, line-oriented scanner over the subset of RFC 8216 tags Multiview
//! needs (`#EXT-X-MEDIA` and `#EXT-X-STREAM-INF`). The HTTP fetch reuses the
//! existing ingest.

use multiview_core::stream::{
    Bcp47, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
};
use thiserror::Error;

/// Upper bound on lines scanned, so a pathological input cannot make the parser
/// loop unboundedly. A real master playlist is a few dozen lines.
const MAX_LINES: usize = 100_000;

/// Errors raised while parsing an HLS master playlist.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HlsError {
    /// The text did not begin with the mandatory `#EXTM3U` tag, so it is not an
    /// HLS playlist.
    #[error("not an HLS playlist (missing #EXTM3U)")]
    NotAPlaylist,

    /// The playlist exceeded the bounded line budget ([`MAX_LINES`]).
    #[error("playlist exceeds the {0}-line scan budget")]
    TooLarge(usize),
}

/// The media type of an `#EXT-X-MEDIA` rendition (RFC 8216 §4.3.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MediaType {
    /// `TYPE=AUDIO`.
    Audio,
    /// `TYPE=VIDEO`.
    Video,
    /// `TYPE=SUBTITLES` — a separate `WebVTT` media playlist.
    Subtitles,
    /// `TYPE=CLOSED-CAPTIONS` — in-band CEA-608/708 (no separate `URI`).
    ClosedCaptions,
    /// A `TYPE` value this parser does not model.
    Other,
}

impl MediaType {
    /// Classify an `#EXT-X-MEDIA` `TYPE` attribute value (case-insensitive).
    fn from_attr(value: &str) -> Self {
        if value.eq_ignore_ascii_case("AUDIO") {
            Self::Audio
        } else if value.eq_ignore_ascii_case("VIDEO") {
            Self::Video
        } else if value.eq_ignore_ascii_case("SUBTITLES") {
            Self::Subtitles
        } else if value.eq_ignore_ascii_case("CLOSED-CAPTIONS") {
            Self::ClosedCaptions
        } else {
            Self::Other
        }
    }
}

/// One `#EXT-X-MEDIA` rendition (an alternative audio/subtitle/CC track).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MediaRendition {
    /// The rendition's media type.
    pub media_type: MediaType,
    /// `GROUP-ID` — the rendition group a variant references.
    pub group_id: String,
    /// `NAME` — a human-readable label.
    pub name: String,
    /// `LANGUAGE` (RFC 5646 tag), if declared.
    pub language: Option<String>,
    /// `DEFAULT=YES`.
    pub default: bool,
    /// `AUTOSELECT=YES`.
    pub autoselect: bool,
    /// `FORCED=YES`.
    pub forced: bool,
    /// `URI` of the rendition's media playlist (absent for in-band
    /// `CLOSED-CAPTIONS`).
    pub uri: Option<String>,
}

/// One `#EXT-X-STREAM-INF` variant stream and the rendition groups it links.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VariantStream {
    /// The variant's media-playlist URI (the line following the tag).
    pub uri: String,
    /// `SUBTITLES="group"` — the subtitle rendition group this variant uses.
    pub subtitles_group: Option<String>,
}

/// A parsed HLS master playlist: its renditions and variant streams.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct MasterPlaylist {
    /// All `#EXT-X-MEDIA` renditions, in document order.
    pub renditions: Vec<MediaRendition>,
    /// All `#EXT-X-STREAM-INF` variant streams, in document order.
    pub variants: Vec<VariantStream>,
}

impl MasterPlaylist {
    /// Parse a master playlist's text.
    ///
    /// # Errors
    /// - [`HlsError::NotAPlaylist`] if the first non-empty line is not `#EXTM3U`.
    /// - [`HlsError::TooLarge`] if the text exceeds [`MAX_LINES`].
    pub fn parse(text: &str) -> Result<Self, HlsError> {
        let mut lines = text.lines();
        // The first non-empty line must be the #EXTM3U tag.
        let first = lines
            .by_ref()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if first != "#EXTM3U" {
            return Err(HlsError::NotAPlaylist);
        }

        let mut renditions = Vec::new();
        let mut variants = Vec::new();
        let mut pending_subtitles_group: Option<Option<String>> = None;
        let mut scanned = 1usize;

        for raw in lines {
            scanned = scanned.saturating_add(1);
            if scanned > MAX_LINES {
                return Err(HlsError::TooLarge(MAX_LINES));
            }
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
                renditions.push(parse_media(attrs));
                pending_subtitles_group = None;
            } else if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
                // The URI is on the following non-comment line; stash the group.
                pending_subtitles_group = Some(attr_value(attrs, "SUBTITLES"));
            } else if line.starts_with('#') {
                // Any other tag — ignore, but do not consume a pending variant.
            } else if let Some(group) = pending_subtitles_group.take() {
                variants.push(VariantStream {
                    uri: line.to_owned(),
                    subtitles_group: group,
                });
            }
            // A bare URI with no pending STREAM-INF is ignored (not a variant).
        }

        Ok(Self {
            renditions,
            variants,
        })
    }

    /// Iterate the `TYPE=SUBTITLES` renditions.
    pub fn subtitle_renditions(&self) -> impl Iterator<Item = &MediaRendition> {
        self.renditions
            .iter()
            .filter(|r| r.media_type == MediaType::Subtitles)
    }

    /// Choose a subtitle rendition to decode.
    ///
    /// Preference order: an exact `language` match (case-insensitive) when a
    /// language is requested, then `DEFAULT=YES`, then `AUTOSELECT=YES`, then the
    /// first subtitle rendition. Returns `None` if the master carries no subtitle
    /// rendition with a `URI`.
    #[must_use]
    pub fn pick_subtitle(&self, language: Option<&str>) -> Option<&MediaRendition> {
        let with_uri = || self.subtitle_renditions().filter(|r| r.uri.is_some());
        if let Some(lang) = language {
            if let Some(found) = with_uri().find(|r| {
                r.language
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(lang))
            }) {
                return Some(found);
            }
        }
        with_uri()
            .find(|r| r.default)
            .or_else(|| with_uri().find(|r| r.autoselect))
            .or_else(|| with_uri().next())
    }

    /// Fold the master playlist's AUDIO + SUBTITLES alternate renditions into a
    /// typed [`StreamInventory`] (RT-2, ADR-0034 §3).
    ///
    /// libav opens an HLS URL as a single program and does **not** surface the
    /// master's separate AUDIO / SUBTITLES renditions as decodable streams, so
    /// these would otherwise be invisible to the router. This fold-in makes each
    /// one an addressable [`StreamDescriptor`]:
    ///
    /// * keyed by a **hard** [`StableStreamId::from_hls`]`(group_id, name)` (RFC
    ///   8216 keys a rendition by group + name; these survive a rendition
    ///   reorder). HLS requires a non-empty `NAME`, but real playlists omit it —
    ///   a **deterministic, non-empty** name is synthesised from `group_id` + the
    ///   rendition's ordinal within its `(media_type, group_id)` so two name-less
    ///   renditions in one group get **distinct, stable** ids rather than
    ///   colliding on the empty name;
    /// * carrying the rendition's validated [`Bcp47`] language, `DEFAULT` flag,
    ///   and (for subtitles) the `FORCED` flag.
    ///
    /// `CLOSED-CAPTIONS` renditions (no separate media playlist) and `VIDEO`
    /// renditions (the variant the libav path already decodes) are not folded in
    /// here. The owning input id is left unset (the ingest binder fills it).
    #[must_use]
    pub fn stream_inventory(&self) -> StreamInventory {
        // Per-(media_type, group_id) ordinal so a synthesised name is stable and
        // unique within its group; iterate in document order so the ordinal is
        // deterministic across a re-parse.
        let mut ordinals: std::collections::HashMap<(MediaType, String), u32> =
            std::collections::HashMap::new();
        let mut streams = Vec::new();
        for rendition in &self.renditions {
            let kind = match rendition.media_type {
                MediaType::Audio => StreamKind::Audio,
                MediaType::Subtitles => StreamKind::Subtitle,
                // VIDEO renditions are the libav-decoded variant; CLOSED-CAPTIONS
                // and any other type are not separate routable renditions here.
                MediaType::Video | MediaType::ClosedCaptions | MediaType::Other => continue,
            };
            let key = (rendition.media_type, rendition.group_id.clone());
            let ordinal = ordinals.entry(key).or_insert(0);
            let this_ordinal = *ordinal;
            *ordinal = ordinal.saturating_add(1);
            streams.push(rendition_descriptor(rendition, kind, this_ordinal));
        }
        StreamInventory::from_streams(streams)
    }
}

/// The shared resolver that turns one AUDIO/SUBTITLES [`MediaRendition`] into a
/// [`StreamDescriptor`] — used for both rendition families so audio and subtitle
/// fold-in stay consistent (RT-2 §3).
///
/// `ordinal` is the rendition's position within its `(media_type, group_id)`,
/// used only to synthesise a non-empty `NAME` when the playlist omits one.
fn rendition_descriptor(
    rendition: &MediaRendition,
    kind: StreamKind,
    ordinal: u32,
) -> StreamDescriptor {
    let name = synthesised_name(&rendition.group_id, &rendition.name, ordinal);
    let id = StableStreamId::from_hls(kind, &rendition.group_id, &name);
    let language = rendition
        .language
        .as_deref()
        .and_then(|l| Bcp47::parse(l).ok());
    let detail = match kind {
        StreamKind::Subtitle => StreamDetail::Subtitle {
            forced: rendition.forced,
        },
        // An HLS master rendition carries no channel layout / sample rate (that
        // needs decoding the media playlist's segments); the audio detail is the
        // zero-valued shape, refined by a later decode probe.
        _ => StreamDetail::Audio {
            channels: 0,
            sample_rate: 0,
        },
    };
    // The rendition's NAME is the natural title; for a synthesised name there is
    // no operator-facing title, so leave it unset.
    let title = if rendition.name.trim().is_empty() {
        None
    } else {
        Some(rendition.name.clone())
    };
    StreamDescriptor::new(id, kind, "hls", detail)
        .with_language(language)
        .with_title(title)
        .with_default(rendition.default)
}

/// Synthesise a **non-empty, deterministic** rendition name.
///
/// RFC 8216 requires `#EXT-X-MEDIA:NAME` to be present and unique within its
/// group, but real playlists omit it. An empty name would make two renditions in
/// one group collide on the same `(group_id, "")` [`StableStreamId`]. When `name`
/// is empty (or whitespace-only) this returns a stable `"{group_id}#{ordinal}"`
/// so the ids are distinct and survive a re-parse (the ordinal is the rendition's
/// deterministic document-order position within its group).
fn synthesised_name(group_id: &str, name: &str, ordinal: u32) -> String {
    if name.trim().is_empty() {
        format!("{group_id}#{ordinal}")
    } else {
        name.to_owned()
    }
}

/// Build a [`MediaRendition`] from an `#EXT-X-MEDIA` attribute list.
fn parse_media(attrs: &str) -> MediaRendition {
    let media_type = attr_value(attrs, "TYPE")
        .as_deref()
        .map_or(MediaType::Other, MediaType::from_attr);
    MediaRendition {
        media_type,
        group_id: attr_value(attrs, "GROUP-ID").unwrap_or_default(),
        name: attr_value(attrs, "NAME").unwrap_or_default(),
        language: attr_value(attrs, "LANGUAGE"),
        default: attr_flag(attrs, "DEFAULT"),
        autoselect: attr_flag(attrs, "AUTOSELECT"),
        forced: attr_flag(attrs, "FORCED"),
        uri: attr_value(attrs, "URI"),
    }
}

/// Return the (quote-stripped) value of `key` in an attribute list, if present.
fn attr_value(list: &str, key: &str) -> Option<String> {
    split_attrs(list).into_iter().find_map(|attr| {
        let (k, v) = split_kv(attr)?;
        if k.eq_ignore_ascii_case(key) {
            Some(v.to_owned())
        } else {
            None
        }
    })
}

/// Return whether an enumerated attribute equals `YES` (case-insensitive).
fn attr_flag(list: &str, key: &str) -> bool {
    attr_value(list, key).is_some_and(|v| v.eq_ignore_ascii_case("YES"))
}

/// Split a comma-separated attribute list, honouring quoted values that may
/// themselves contain commas (e.g. `CODECS="avc1.4d401f,mp4a.40.2"`).
fn split_attrs(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0usize;
    for (i, c) in list.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                if let Some(seg) = list.get(start..i) {
                    out.push(seg);
                }
                // ',' is one byte, so `i + 1` is a valid char boundary.
                start = i.saturating_add(1);
            }
            _ => {}
        }
    }
    if let Some(seg) = list.get(start..) {
        out.push(seg);
    }
    out
}

/// Split one `KEY=VALUE` attribute, trimming whitespace and surrounding quotes.
fn split_kv(attr: &str) -> Option<(&str, &str)> {
    let (key, value) = attr.trim().split_once('=')?;
    let value = value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value);
    Some((key.trim(), unquoted))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative Apple-style master playlist with audio + a subtitles
    /// group (the structure verified live against the bipbop master).
    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/eng/prog_index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles/eng/prog_index.m3u8\"\n\
        #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Espanol\",LANGUAGE=\"es\",DEFAULT=NO,AUTOSELECT=YES,FORCED=NO,URI=\"subtitles/spa/prog_index.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\"\n\
        video/variant_2000.m3u8\n";

    #[test]
    fn parses_subtitle_renditions_and_resolves_english_uri() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        assert_eq!(master.subtitle_renditions().count(), 2);
        let en = master.pick_subtitle(Some("en")).expect("english subs");
        assert_eq!(en.uri.as_deref(), Some("subtitles/eng/prog_index.m3u8"));
        assert_eq!(en.language.as_deref(), Some("en"));
        assert_eq!(en.media_type, MediaType::Subtitles);
        let es = master.pick_subtitle(Some("es")).expect("spanish subs");
        assert_eq!(es.uri.as_deref(), Some("subtitles/spa/prog_index.m3u8"));
    }

    #[test]
    fn quoted_codecs_comma_does_not_split_the_attribute_list() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        let variant = master.variants.first().expect("one variant");
        assert_eq!(variant.uri, "video/variant_2000.m3u8");
        assert_eq!(variant.subtitles_group.as_deref(), Some("subs"));
    }

    #[test]
    fn pick_falls_back_to_default_then_first_without_language() {
        let master = MasterPlaylist::parse(MASTER).expect("parse");
        // No language requested -> DEFAULT=YES (the English track) wins.
        let pick = master.pick_subtitle(None).expect("a default");
        assert_eq!(pick.language.as_deref(), Some("en"));
    }

    #[test]
    fn master_without_subtitles_yields_none() {
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000\n\
            low.m3u8\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        assert_eq!(master.subtitle_renditions().count(), 0);
        assert!(master.pick_subtitle(Some("en")).is_none());
        assert_eq!(master.variants.len(), 1);
        assert!(master.variants[0].subtitles_group.is_none());
    }

    #[test]
    fn non_playlist_text_is_rejected() {
        assert_eq!(
            MasterPlaylist::parse("not a playlist\n"),
            Err(HlsError::NotAPlaylist)
        );
        assert_eq!(MasterPlaylist::parse(""), Err(HlsError::NotAPlaylist));
    }

    #[test]
    fn a_closed_captions_rendition_has_no_uri() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",INSTREAM-ID=\"CC1\"\n";
        let master = MasterPlaylist::parse(text).expect("parse");
        let cc = master.renditions.first().expect("one rendition");
        assert_eq!(cc.media_type, MediaType::ClosedCaptions);
        assert!(cc.uri.is_none());
        // CLOSED-CAPTIONS is not a separate subtitle media playlist.
        assert_eq!(master.subtitle_renditions().count(), 0);
    }
}
