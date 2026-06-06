//! Audio routing configuration (config-as-code).
//!
//! This module owns the **declarative** audio-routing schema (ADR-R005 §4.1):
//! a program-bus mix (which inputs contribute, at what gain, muted or not),
//! clean discrete per-input tracks (each routed to a named output track), and
//! the per-output audio selection (which tracks/mode each sink carries). It is
//! the config-as-code half — the mix/encode/mux **runtime** that consumes these
//! routes lives in `multiview-audio`'s `Mixer` and the engine (AUD-3/AUD-4) and
//! is not modelled here. The per-output *capability matrix* (TS = N tracks, HLS
//! = select-one, RTMP = endpoint-gated, NDI = channel-map) lives in
//! `multiview-audio`'s `capability` module, referencing the audio channel-layout
//! types; this crate validates only the document's internal reference
//! consistency.
//!
//! All unions are **internally tagged** by `kind`/`mode` (`#[serde(tag = …)]`),
//! never `untagged` (ADR-0010): the only encoding robust across the
//! self-describing JSON wire form and non-self-describing TOML.
//!
//! Levels are carried as **dB** (`f32`) — a gain is a level, not a rate — while
//! the working **sample rate** is an exact integer in Hz (never a float fps-style
//! rational; a rate is a whole number of samples per second, invariant #3's
//! "never float" applied to the audio clock).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The channel layout requested for one routed input, internally tagged by
/// `kind` (mirrors `multiview_audio::ChannelLayout`; kept independent here so the
/// pure config schema does not depend on the audio crate).
///
/// Only the layouts the program bus and discrete-track model need are
/// enumerated; the audio crate's capability matrix maps these onto a concrete
/// transport (e.g. >2-channel AAC-over-NDI is capped — validated there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioChannels {
    /// Single channel.
    Mono,
    /// Two channels: L, R.
    Stereo,
    /// Six channels: L, R, C, LFE, Ls, Rs (the BS.1770 5.1 ordering).
    FivePointOne,
}

impl AudioChannels {
    /// Number of channels in this layout.
    #[must_use]
    pub const fn channel_count(self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::FivePointOne => 6,
        }
    }
}

/// One per-input audio route (ADR-R005 §4.1).
///
/// An input fans out to (a) a clean **discrete track** (named by
/// [`target_track`](AudioRoute::target_track)) carried where the output supports
/// it, and (b) the mixed **program bus** when
/// [`include_in_program_bus`](AudioRoute::include_in_program_bus) is set, scaled
/// by [`gain_db`](AudioRoute::gain_db). A [`mute`](AudioRoute::mute)d route
/// contributes silence to the bus while keeping its discrete track wired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioRoute {
    /// The managed source id (`sources[].id`) this route takes audio from.
    pub input_id: String,
    /// The channel layout requested for this input.
    pub channels: AudioChannels,
    /// The named discrete output track this input is routed to (if any). `None`
    /// ⇒ this input contributes to the program bus only (no clean track).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_track: Option<String>,
    /// ISO-639 language tag advertised for the discrete track (e.g. `"eng"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Human-friendly track title (e.g. `"Camera A"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether this input contributes to the mixed program bus.
    #[serde(default)]
    pub include_in_program_bus: bool,
    /// Program-bus contribution gain in dB (a level, not a rate; `0.0` ⇒ unity).
    #[serde(default)]
    pub gain_db: f32,
    /// Whether this input is muted (contributes silence to the program bus; its
    /// discrete track, if any, is still declared).
    #[serde(default)]
    pub mute: bool,
}

impl AudioRoute {
    /// Validate this route's own fields in isolation: a non-empty `input_id`, a
    /// finite `gain_db`, and a non-empty `target_track` when one is named.
    ///
    /// Reference resolution (does `input_id` name a declared source? does
    /// `target_track` collide with another route?) is the routing block's
    /// responsibility and is enforced by [`AudioRouting::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty `input_id`, a non-finite
    /// `gain_db`, or an empty `target_track`/`language`/`title`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.input_id.is_empty() {
            return Err(ConfigError::Validation(
                "an audio route has an empty input_id".to_owned(),
            ));
        }
        if !self.gain_db.is_finite() {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: gain_db must be finite (got {})",
                self.input_id, self.gain_db
            )));
        }
        if let Some(track) = &self.target_track {
            if track.is_empty() {
                return Err(ConfigError::Validation(format!(
                    "audio route for {:?}: target_track is empty",
                    self.input_id
                )));
            }
        }
        if matches!(self.language.as_deref(), Some("")) {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: language is empty",
                self.input_id
            )));
        }
        if matches!(self.title.as_deref(), Some("")) {
            return Err(ConfigError::Validation(format!(
                "audio route for {:?}: title is empty",
                self.input_id
            )));
        }
        Ok(())
    }

    /// Whether this route actually contributes signal to the program bus: it is
    /// included and not muted. A muted (or unincluded) route is silent on the
    /// bus regardless of its `gain_db`.
    #[must_use]
    pub const fn contributes_to_program(&self) -> bool {
        self.include_in_program_bus && !self.mute
    }
}

/// The whole-document audio routing block (ADR-R005 §4.1).
///
/// Carries the working sample rate (exact integer Hz), and the set of per-input
/// routes that define the program bus membership and the discrete tracks. The
/// per-output *selection* of these tracks lives on each [`crate::Output`] as an
/// [`OutputAudio`] block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AudioRouting {
    /// The working/program-bus sample rate in Hz (exact integer; ADR-R005's
    /// canonical resample target is 48 000). Never a float.
    pub sample_rate_hz: u32,
    /// The per-input routes.
    #[serde(default)]
    pub routes: Vec<AudioRoute>,
}

impl AudioRouting {
    /// The set of discrete track names this routing declares (the program bus,
    /// `"prog"`, is always available as a selectable track in addition to these).
    #[must_use]
    pub fn declared_tracks(&self) -> Vec<&str> {
        let mut tracks: Vec<&str> = vec![PROGRAM_TRACK];
        for route in &self.routes {
            if let Some(track) = &route.target_track {
                if !tracks.contains(&track.as_str()) {
                    tracks.push(track.as_str());
                }
            }
        }
        tracks
    }

    /// Validate the routing block against the declared `source_ids` and the set
    /// of `selectable_tracks` (the union of program bus + every named track) the
    /// caller wishes to expose.
    ///
    /// Enforces:
    /// - the working sample rate is non-zero;
    /// - each route is internally consistent ([`AudioRoute::validate`]);
    /// - every route's `input_id` resolves to a declared source;
    /// - no two routes share the same `input_id` (a duplicate declaration);
    /// - no two routes claim the same `target_track` (ambiguous discrete wiring);
    /// - the program bus has at least one contributing (included, non-muted)
    ///   member when any route asks to be on the bus — an all-muted bus is a
    ///   silent program an operator almost never intends.
    ///
    /// `selectable_tracks` is accepted so the caller can pre-compute the
    /// document-wide selectable set; the routing's own
    /// [`declared_tracks`](AudioRouting::declared_tracks) is a subset of it.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the first violated invariant.
    pub fn validate(
        &self,
        source_ids: &[&str],
        selectable_tracks: &[&str],
    ) -> Result<(), ConfigError> {
        if self.sample_rate_hz == 0 {
            return Err(ConfigError::Validation(
                "audio.sample_rate_hz must be > 0".to_owned(),
            ));
        }

        let sources: HashSet<&str> = source_ids.iter().copied().collect();
        let selectable: HashSet<&str> = selectable_tracks.iter().copied().collect();

        let mut seen_inputs: HashSet<&str> = HashSet::with_capacity(self.routes.len());
        let mut seen_tracks: HashSet<&str> = HashSet::with_capacity(self.routes.len());
        let mut any_program_request = false;
        let mut any_program_contributor = false;

        for route in &self.routes {
            route.validate()?;

            if !sources.contains(route.input_id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "audio route binds unknown source input_id {:?}",
                    route.input_id
                )));
            }
            if !seen_inputs.insert(route.input_id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate audio route for input_id {:?}",
                    route.input_id
                )));
            }
            if let Some(track) = &route.target_track {
                if track == PROGRAM_TRACK {
                    return Err(ConfigError::Validation(format!(
                        "audio route for {:?}: target_track {PROGRAM_TRACK:?} is reserved for the \
                         mixed program bus",
                        route.input_id
                    )));
                }
                if !seen_tracks.insert(track.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "two audio routes target the same discrete track {track:?}"
                    )));
                }
                // A declared track must be in the selectable set the document
                // exposes (it always is, since `selectable_tracks` is derived to
                // include it; this guards a caller passing a stale set).
                if !selectable.contains(track.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "audio route for {:?}: target_track {track:?} is not a selectable track",
                        route.input_id
                    )));
                }
            }

            if route.include_in_program_bus {
                any_program_request = true;
                if route.contributes_to_program() {
                    any_program_contributor = true;
                }
            }
        }

        if any_program_request && !any_program_contributor {
            return Err(ConfigError::Validation(
                "the program bus has no contributing member (every included input is muted)"
                    .to_owned(),
            ));
        }

        Ok(())
    }
}

/// The reserved name of the mixed **program bus** track, always selectable in
/// addition to the named discrete tracks.
pub const PROGRAM_TRACK: &str = "prog";

/// How an output selects audio, internally tagged by `mode`.
///
/// `program` carries only the mixed program bus; `tracks` carries an explicit
/// list of selectable tracks (program + named discrete). Which of these an
/// output transport can actually deliver is decided by the per-output capability
/// matrix in `multiview-audio` (e.g. an HLS output is select-one; an RTMP output
/// is endpoint-gated) — this schema only declares the operator's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputAudioMode {
    /// Carry only the mixed program bus.
    Program,
    /// Carry an explicit list of selectable tracks.
    Tracks,
}

/// The per-output audio selection block (attached to a [`crate::Output`]).
///
/// In [`OutputAudioMode::Program`] the `tracks` list is ignored (the output
/// carries the program bus). In [`OutputAudioMode::Tracks`] every name in
/// `tracks` must resolve to a selectable track declared by the routing block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputAudio {
    /// How this output selects audio.
    pub mode: OutputAudioMode,
    /// The explicit selectable-track list (used only in
    /// [`OutputAudioMode::Tracks`]).
    #[serde(default)]
    pub tracks: Vec<String>,
}

impl OutputAudio {
    /// Validate this output's audio selection against the document's
    /// `selectable_tracks` (program bus + every named discrete track).
    ///
    /// Enforces that, in [`OutputAudioMode::Tracks`], the list is non-empty,
    /// carries no duplicates, and every name resolves to a selectable track.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the offending output/track.
    pub fn validate(
        &self,
        output_label: &str,
        selectable_tracks: &[&str],
    ) -> Result<(), ConfigError> {
        if self.mode == OutputAudioMode::Program {
            return Ok(());
        }
        if self.tracks.is_empty() {
            return Err(ConfigError::Validation(format!(
                "output {output_label:?}: audio mode is `tracks` but no tracks are selected"
            )));
        }
        let selectable: HashSet<&str> = selectable_tracks.iter().copied().collect();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tracks.len());
        for track in &self.tracks {
            if !seen.insert(track.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output {output_label:?}: selects track {track:?} more than once"
                )));
            }
            if !selectable.contains(track.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "output {output_label:?}: selects unknown audio track {track:?}"
                )));
            }
        }
        Ok(())
    }
}
