//! Video-codec identity + encoder selection (LGPL-clean by default).
//!
//! Callers of the encode path think in terms of a *logical* codec — "give me
//! H.264" — not a concrete libav encoder name. [`VideoCodec`] is that logical
//! identity; [`candidate_encoders`] maps it to the **ordered preference list**
//! of concrete libav encoder short-names that this build is *allowed* to use,
//! and `select_encoder` (the `ffmpeg` feature) picks the first one that is
//! actually present in the linked `FFmpeg`, falling back gracefully.
//!
//! ## Feature gating is the licence/availability boundary
//!
//! The candidate list is pure compile-time logic; which names appear is decided
//! by the Cargo features, never at run time:
//!
//! * **Default `ffmpeg`** — only LGPL software encoders already in `FFmpeg`
//!   (`mpeg2video`, `ffv1`, `mjpeg`). H.264/H.265 have **no** allowed encoder, so
//!   selection returns the LGPL fallback (or `None` for H.264/H.265): the
//!   default build can never silently reach a GPL or proprietary encoder.
//! * **`gpl-codecs`** — adds the GPL software encoders `libx264` (H.264) and
//!   `libx265` (H.265). Enabling this feature makes the whole build GPL; it is
//!   never pulled in by `ffmpeg` alone.
//! * **`cuda`** — adds the NVENC hardware encoders `h264_nvenc` / `hevc_nvenc`,
//!   listed **ahead** of any software encoder so a GPU box prefers hardware. On a
//!   box with the feature compiled but **no usable GPU**, the NVENC name is
//!   absent from the linked `FFmpeg`'s registry (or fails to open), so
//!   `select_encoder` skips it and falls through to the next candidate —
//!   graceful degradation, never a crash.
//!
//! The ordering encodes the preference policy: hardware (NVENC, `cuda`) →
//! GPL software (`gpl-codecs`) → LGPL software (always). Selection walks the
//! list and returns the first encoder the build can actually open.

/// A logical video codec the output pipeline can target.
///
/// This is the *family* a caller requests; the concrete libav encoder (software
/// vs. NVENC, x264 vs. a future encoder) is chosen by `select_encoder` from
/// the compiled features and what the linked `FFmpeg` provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VideoCodec {
    /// H.264 / AVC. Encodable only with `gpl-codecs` (`libx264`) and/or `cuda`
    /// (`h264_nvenc`); the LGPL-clean default build cannot encode it.
    H264,
    /// H.265 / HEVC. Encodable only with `gpl-codecs` (`libx265`) and/or `cuda`
    /// (`hevc_nvenc`); the LGPL-clean default build cannot encode it.
    H265,
    /// MPEG-2 video — an LGPL software encoder always present in `FFmpeg`. The
    /// deny-clean default encode/test path.
    Mpeg2Video,
    /// FFV1 — lossless LGPL software encoder (archival / golden-frame).
    Ffv1,
    /// Motion-JPEG — LGPL software encoder (intra-only, preview-friendly).
    Mjpeg,
}

impl VideoCodec {
    /// The libav short-name of this codec's LGPL software encoder, if it has one
    /// in the default (`ffmpeg`-only) build.
    ///
    /// H.264/H.265 have no LGPL software encoder, so they return [`None`] — the
    /// default build genuinely cannot encode them, and selection reflects that
    /// rather than silently substituting a different codec.
    #[must_use]
    pub const fn lgpl_software_encoder(self) -> Option<&'static str> {
        match self {
            Self::H264 | Self::H265 => None,
            Self::Mpeg2Video => Some("mpeg2video"),
            Self::Ffv1 => Some("ffv1"),
            Self::Mjpeg => Some("mjpeg"),
        }
    }

    /// This codec's GPL software encoder short-name, present only when the
    /// `gpl-codecs` feature is enabled (otherwise [`None`]).
    #[must_use]
    pub const fn gpl_software_encoder(self) -> Option<&'static str> {
        #[cfg(feature = "gpl-codecs")]
        {
            match self {
                Self::H264 => Some("libx264"),
                Self::H265 => Some("libx265"),
                Self::Mpeg2Video | Self::Ffv1 | Self::Mjpeg => None,
            }
        }
        #[cfg(not(feature = "gpl-codecs"))]
        {
            let _ = self;
            None
        }
    }

    /// This codec's NVENC hardware encoder short-name, present only when the
    /// `cuda` feature is enabled (otherwise [`None`]). Presence in the list does
    /// **not** guarantee a usable GPU — `select_encoder` verifies that.
    #[must_use]
    pub const fn nvenc_encoder(self) -> Option<&'static str> {
        #[cfg(feature = "cuda")]
        {
            match self {
                Self::H264 => Some("h264_nvenc"),
                Self::H265 => Some("hevc_nvenc"),
                Self::Mpeg2Video | Self::Ffv1 | Self::Mjpeg => None,
            }
        }
        #[cfg(not(feature = "cuda"))]
        {
            let _ = self;
            None
        }
    }
}

/// The ordered preference list of concrete libav encoder short-names this build
/// is allowed to use for `codec`, best first.
///
/// Order is fixed policy: NVENC hardware (`cuda`) → GPL software (`gpl-codecs`)
/// → LGPL software (always). Names absent from the compiled features are
/// omitted, so the default build's list contains only LGPL software encoders
/// (and is empty for H.264/H.265). The result is pure compile-time data; no
/// libav call happens here.
#[must_use]
pub fn candidate_encoders(codec: VideoCodec) -> Vec<&'static str> {
    let mut candidates = Vec::with_capacity(3);
    if let Some(name) = codec.nvenc_encoder() {
        candidates.push(name);
    }
    if let Some(name) = codec.gpl_software_encoder() {
        candidates.push(name);
    }
    if let Some(name) = codec.lgpl_software_encoder() {
        candidates.push(name);
    }
    candidates
}

/// Whether this build *can* encode `codec` at all (its candidate list, before
/// the run-time availability check, is non-empty).
///
/// Pure: a quick compile-time capability probe for callers that want to reject a
/// requested codec early (e.g. config validation) without touching libav.
#[must_use]
pub fn can_encode(codec: VideoCodec) -> bool {
    !candidate_encoders(codec).is_empty()
}

/// Pick the best concrete libav encoder for `codec` that is actually present in
/// the linked `FFmpeg` build, walking [`candidate_encoders`] best-first and
/// returning the first that resolves.
///
/// This is where the `cuda` run-time gate lives: an NVENC name is a candidate
/// when the feature is compiled, but if the linked `FFmpeg` has no such encoder
/// (no NVENC support / no GPU), it is skipped and selection falls through to the
/// next candidate — graceful degradation. Returns [`None`] only when **no**
/// candidate resolves (e.g. requesting H.264 on the LGPL-clean default build).
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn select_encoder(codec: VideoCodec) -> Option<&'static str> {
    candidate_encoders(codec)
        .into_iter()
        .find(|name| ffmpeg_next::encoder::find_by_name(name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lgpl_codecs_always_have_a_software_encoder() {
        // The deny-clean default codecs always resolve to an LGPL software
        // encoder name regardless of feature flags.
        assert_eq!(
            VideoCodec::Mpeg2Video.lgpl_software_encoder(),
            Some("mpeg2video")
        );
        assert_eq!(VideoCodec::Ffv1.lgpl_software_encoder(), Some("ffv1"));
        assert_eq!(VideoCodec::Mjpeg.lgpl_software_encoder(), Some("mjpeg"));
    }

    #[test]
    fn h264_has_no_lgpl_software_encoder() {
        // H.264/H.265 must never have an LGPL fallback: the default build
        // genuinely cannot encode them rather than silently substituting.
        assert_eq!(VideoCodec::H264.lgpl_software_encoder(), None);
        assert_eq!(VideoCodec::H265.lgpl_software_encoder(), None);
    }

    #[test]
    fn lgpl_candidate_list_is_software_only_and_ordered() {
        // mpeg2video's candidate list ends with the LGPL software encoder and
        // never contains a GPL/NVENC name unless that feature is compiled.
        let list = candidate_encoders(VideoCodec::Mpeg2Video);
        assert_eq!(
            list.last(),
            Some(&"mpeg2video"),
            "LGPL software encoder must be the final fallback"
        );
        assert!(
            !list.contains(&"libx264") && !list.contains(&"libx265"),
            "an LGPL codec must never list a GPL encoder"
        );
    }

    #[cfg(not(feature = "gpl-codecs"))]
    #[test]
    fn default_build_cannot_encode_h264_or_h265() {
        // Without gpl-codecs (and without cuda) the H.264/H.265 candidate list
        // is empty: the LGPL-clean build cannot reach a GPL/proprietary encoder.
        #[cfg(not(feature = "cuda"))]
        {
            assert!(candidate_encoders(VideoCodec::H264).is_empty());
            assert!(candidate_encoders(VideoCodec::H265).is_empty());
            assert!(!can_encode(VideoCodec::H264));
            assert!(!can_encode(VideoCodec::H265));
        }
        // gpl_software_encoder is gated off regardless of cuda.
        assert_eq!(VideoCodec::H264.gpl_software_encoder(), None);
        assert_eq!(VideoCodec::H265.gpl_software_encoder(), None);
    }

    #[cfg(feature = "gpl-codecs")]
    #[test]
    fn gpl_build_lists_x264_x265_ahead_of_nothing_lgpl() {
        // With gpl-codecs, H.264 -> libx264 (and after any NVENC candidate, when
        // cuda is also on). The GPL software encoder is present and is the only
        // software option for H.264.
        assert_eq!(VideoCodec::H264.gpl_software_encoder(), Some("libx264"));
        assert_eq!(VideoCodec::H265.gpl_software_encoder(), Some("libx265"));
        let list = candidate_encoders(VideoCodec::H264);
        assert!(list.contains(&"libx264"));
        assert!(can_encode(VideoCodec::H264));
    }

    #[cfg(feature = "cuda")]
    #[test]
    fn cuda_build_prefers_nvenc_first() {
        // With cuda, the NVENC encoder is the first candidate (hardware
        // preferred), ahead of any software encoder.
        assert_eq!(VideoCodec::H264.nvenc_encoder(), Some("h264_nvenc"));
        assert_eq!(VideoCodec::H265.nvenc_encoder(), Some("hevc_nvenc"));
        let list = candidate_encoders(VideoCodec::H264);
        assert_eq!(
            list.first(),
            Some(&"h264_nvenc"),
            "hardware NVENC must be preferred over software"
        );
    }
}
