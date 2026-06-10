//! The real ALSA PCM + ELD readers (feature `display-kms`, DEV-B4 /
//! display-out §5).
//!
//! This is the **hardware-only** leg of the display-audio sink: everything that
//! talks to libasound or reads `/proc` lives here, behind the off-by-default
//! `display-kms` feature, so CI (which has neither) exercises the pure
//! servo/ELD/xrun/FIFO/sink-seam over the mocks in [`super`]. The leg is
//! validated on the t630 / Raspberry Pi node targets after merge.
//!
//! - [`ProcEldSource`] implements [`EldSource`](super::EldSource) by reading
//!   the x86 HDA driver's **textual** dump at `/proc/asound/cardN/eld#C.P` and
//!   handing it to the pure [`parse_proc_eld_text`](super::parse_proc_eld_text).
//!   A missing/empty/`eld_valid 0` file (EDID-less head, or the pipe not lit)
//!   yields [`None`] — no audio path — never an error that stops the run.
//! - [`CtlEldSource`] implements the same seam for **ASoC/hdmi-codec** cards
//!   (the Pi's vc4-hdmi), which expose no proc file: the ELD is a binary
//!   `"ELD"` byte control element on the PCM iface, parsed by the pure
//!   [`parse_eld`](super::parse_eld).
//! - [`AlsaPcmSink`] implements [`AlsaSink`](super::AlsaSink) over the SAFE
//!   `alsa` crate (libasound is dynamically linked, LGPL-2.1, never vendored).
//!   It opens the **`hdmi:CARD=…,DEV=…`** PCM (the `hdmi:` config layer sets
//!   the IEC958/AES channel-status bits and, on the Pi, applies the vc4-hdmi
//!   alsa-lib card config that a raw `hw:` device does not); a raw-`hw:`
//!   fallback covers cards without an `hdmi:` config entry. Writes go through
//!   `snd_pcm_writei`; an underrun/suspend is mapped onto
//!   [`PcmOutcome`](super::PcmOutcome) for the recovery machine.
//! - [`discover_for_connector`] applies the pure
//!   [`discover`](super::discover) policy to the live system: vc4 card-per-port
//!   first, then the HDA `eld#D.P` scan (the entry ordinal doubling as the
//!   `hdmi:` `DEV=` index — the same pin-ordering assumption alsa-lib makes).
//!
//! This module writes **no `unsafe`** — the `alsa` crate owns all FFI — so
//! `multiview-output` stays `forbid(unsafe_code)`.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use alsa::ctl::{Ctl, ElemId, ElemIface, ElemType, ElemValue};
use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};

use super::discover::{pick_eld_entry, vc4_card_candidates};
use super::eld::{parse_eld, parse_proc_eld_text, EldCapability};
use super::sink::{AlsaSink, EldSource, PcmParams};
use super::xrun::PcmOutcome;

/// Reads a connector's ELD from the x86 HDA textual proc dump
/// (`/proc/asound/cardN/eld#C.P`).
///
/// The kernel publishes the parsed EDID audio block at that path while the
/// pipe is lit. The reader re-reads on each [`read_capability`] call so a
/// hotplug that lights (or drops) the audio path is picked up.
///
/// [`read_capability`]: super::EldSource::read_capability
#[derive(Debug, Clone)]
pub struct ProcEldSource {
    path: PathBuf,
}

impl ProcEldSource {
    /// Build a reader from an explicit ELD proc path (as found by
    /// [`discover_for_connector`], or operator diagnostics).
    #[must_use]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl EldSource for ProcEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        let text = fs::read_to_string(&self.path).ok()?;
        parse_proc_eld_text(&text)
    }
}

/// Reads a connector's ELD from the ASoC/hdmi-codec **binary** `"ELD"` byte
/// control element (the Pi vc4-hdmi shape — those cards expose no proc dump).
///
/// The element lives on the card's PCM iface, device `pcm_device`; while the
/// pipe is dark the codec publishes an all-zero/empty blob, which the pure
/// [`parse_eld`](super::parse_eld) maps to [`None`] (no audio path).
#[derive(Debug, Clone)]
pub struct CtlEldSource {
    /// The ALSA ctl name (`hw:vc4hdmi0`).
    ctl: String,
    /// The PCM device the ELD element is attached to (0 on vc4).
    pcm_device: u32,
}

impl CtlEldSource {
    /// Build a reader for `card_id`'s ELD element on PCM device `pcm_device`.
    #[must_use]
    pub fn new(card_id: &str, pcm_device: u32) -> Self {
        Self {
            ctl: format!("hw:{card_id}"),
            pcm_device,
        }
    }

    /// One read attempt; [`None`] for any failure (no card, no element, dark
    /// pipe) — the sink just keeps polling.
    fn read_once(&self) -> Option<EldCapability> {
        let ctl = Ctl::new(&self.ctl, false).ok()?;
        let name = CString::new("ELD").ok()?;
        let mut id = ElemId::new(ElemIface::PCM);
        id.set_device(self.pcm_device);
        id.set_name(&name);
        let mut value = ElemValue::new(ElemType::Bytes).ok()?;
        value.set_id(&id);
        ctl.elem_read(&mut value).ok()?;
        parse_eld(value.get_bytes()?)
    }
}

impl EldSource for CtlEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        self.read_once()
    }
}

/// A discovered ELD source: proc-text (x86 HDA) or binary ctl element
/// (ASoC/vc4), behind one [`EldSource`] face for the sink loop.
#[derive(Debug, Clone)]
pub enum DiscoveredEldSource {
    /// The x86 HDA textual proc dump.
    Proc(ProcEldSource),
    /// The ASoC/hdmi-codec binary control element.
    Ctl(CtlEldSource),
}

impl EldSource for DiscoveredEldSource {
    fn read_capability(&mut self) -> Option<EldCapability> {
        match self {
            Self::Proc(src) => src.read_capability(),
            Self::Ctl(src) => src.read_capability(),
        }
    }
}

/// The ALSA endpoints serving one KMS connector: where to watch the ELD and
/// which PCM to play into.
#[derive(Debug)]
pub struct DiscoveredDisplayAudio {
    /// The ELD gate for the connector.
    pub eld: DiscoveredEldSource,
    /// The PCM sink (`hdmi:CARD=…,DEV=…` with a raw-`hw:` fallback).
    pub pcm: AlsaPcmSink,
    /// The ALSA card id the pair lives on (diagnostics).
    pub card_id: String,
}

/// Find the ALSA card/device/ELD serving `connector`, applying the pure
/// [`discover`](super::discover) policy to the live `/proc/asound` tree:
///
/// 1. **vc4 (Pi)**: a card id mapped from the connector index
///    ([`vc4_card_candidates`]) → that card's `"ELD"` ctl element + its
///    `hdmi:CARD=…,DEV=0` PCM.
/// 2. **x86 HDA**: scan every card's `eld#D.P` proc entries and pick per
///    [`pick_eld_entry`] (first lit, else first — so a dark pipe keeps being
///    polled); the entry ordinal is used as the `hdmi:` `DEV=` index.
///
/// [`None`] when no candidate exists at all (no ALSA cards / no HDMI pins) —
/// the caller logs and runs the head video-only. Heuristic corners (several
/// simultaneously-lit pins on one card) are logged at the call site via
/// `card_id`; the mapping is hardware-validated on the t630/Pi legs.
#[must_use]
pub fn discover_for_connector(connector: &str) -> Option<DiscoveredDisplayAudio> {
    discover_in(Path::new("/proc/asound"), connector)
}

/// [`discover_for_connector`] over an explicit proc root (separated for
/// diagnostics and so the scan logic is exercisable against a fixture tree).
fn discover_in(proc_asound: &Path, connector: &str) -> Option<DiscoveredDisplayAudio> {
    // --- vc4: one ALSA card per HDMI port, ELD via the ctl element. ---
    for card_id in vc4_card_candidates(connector) {
        if proc_asound.join(&card_id).is_dir() {
            return Some(DiscoveredDisplayAudio {
                eld: DiscoveredEldSource::Ctl(CtlEldSource::new(&card_id, 0)),
                pcm: AlsaPcmSink::for_connector(&card_id, 0),
                card_id,
            });
        }
    }

    // --- HDA: scan card*/eld#* and pick the first lit (else first) entry. ---
    let mut cards: Vec<PathBuf> = fs::read_dir(proc_asound)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card") && n.len() > 4)
        })
        .collect();
    cards.sort();
    for card in cards {
        let mut elds: Vec<PathBuf> = fs::read_dir(&card)
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("eld#"))
            })
            .collect();
        if elds.is_empty() {
            continue;
        }
        elds.sort();
        let parsed: Vec<Option<EldCapability>> = elds
            .iter()
            .map(|p| {
                fs::read_to_string(p)
                    .ok()
                    .and_then(|t| parse_proc_eld_text(&t))
            })
            .collect();
        let Some(index) = pick_eld_entry(&parsed) else {
            continue;
        };
        let eld_path = elds.get(index)?.clone();
        // The card id (`/proc/asound/cardN/id`) names the `hdmi:CARD=` PCM; the
        // chosen entry ordinal is the `DEV=` index (alsa-lib's pin ordering).
        let card_id = fs::read_to_string(card.join("id")).ok()?.trim().to_owned();
        let dev = u32::try_from(index).unwrap_or(0);
        return Some(DiscoveredDisplayAudio {
            eld: DiscoveredEldSource::Proc(ProcEldSource::from_path(eld_path)),
            pcm: AlsaPcmSink::for_connector(&card_id, dev),
            card_id,
        });
    }
    None
}

/// A libasound PCM playback sink for an HDMI/DP connector.
///
/// Prefers the `hdmi:CARD=…,DEV=…` ALSA PCM (the config layer that sets IEC958
/// channel-status and the vc4-hdmi card setup); a raw `hw:CARD=…,DEV=…` is the
/// fallback for cards without an `hdmi:` config entry.
#[derive(Debug)]
pub struct AlsaPcmSink {
    device: String,
    fallback_hw: Option<String>,
    pcm: Option<PCM>,
    channels: usize,
}

impl AlsaPcmSink {
    /// Build a sink for the `hdmi:` PCM of `card_name`/`dev`, with a `hw:`
    /// fallback. `card_name` is the ALSA card id (e.g. `vc4hdmi0` on the Pi or
    /// `PCH` on x86 HDA).
    #[must_use]
    pub fn for_connector(card_name: &str, dev: u32) -> Self {
        Self {
            device: format!("hdmi:CARD={card_name},DEV={dev}"),
            fallback_hw: Some(format!("hw:CARD={card_name},DEV={dev}")),
            pcm: None,
            channels: 2,
        }
    }

    /// Build a sink for an explicit ALSA device string (diagnostics / override).
    #[must_use]
    pub fn for_device(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            fallback_hw: None,
            pcm: None,
            channels: 2,
        }
    }

    /// Try to open `name` and configure it for `params`. Returns the configured
    /// PCM on success.
    fn try_open(name: &str, params: PcmParams) -> Result<PCM, String> {
        let pcm =
            PCM::new(name, Direction::Playback, false).map_err(|e| format!("open {name}: {e}"))?;
        {
            let hwp = HwParams::any(&pcm).map_err(|e| format!("hw_params {name}: {e}"))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| format!("set_access: {e}"))?;
            hwp.set_format(Format::float())
                .map_err(|e| format!("set_format f32: {e}"))?;
            hwp.set_channels(u32::from(params.channels))
                .map_err(|e| format!("set_channels: {e}"))?;
            hwp.set_rate(params.sample_rate, ValueOr::Nearest)
                .map_err(|e| format!("set_rate: {e}"))?;
            hwp.set_period_size_near(
                alsa::pcm::Frames::from(i32::try_from(params.period_frames).unwrap_or(480)),
                ValueOr::Nearest,
            )
            .map_err(|e| format!("set_period_size: {e}"))?;
            hwp.set_periods(params.periods, ValueOr::Nearest)
                .map_err(|e| format!("set_periods: {e}"))?;
            pcm.hw_params(&hwp)
                .map_err(|e| format!("commit hw_params {name}: {e}"))?;
        }
        pcm.prepare().map_err(|e| format!("prepare {name}: {e}"))?;
        Ok(pcm)
    }

    /// Map a libasound write error onto a [`PcmOutcome`] for the recovery
    /// machine. `EPIPE` is an underrun, `ESTRPIPE` a suspend; anything else is
    /// surfaced as an underrun so the loop attempts a prepare (the conservative
    /// recover).
    fn classify(err: &alsa::Error) -> PcmOutcome {
        let errno = err.errno();
        if errno == EPIPE {
            PcmOutcome::Underrun
        } else if errno == ESTRPIPE {
            PcmOutcome::Suspended
        } else {
            PcmOutcome::Underrun
        }
    }
}

impl AlsaSink for AlsaPcmSink {
    fn open(&mut self, params: PcmParams) -> Result<(), String> {
        self.channels = usize::from(params.channels.max(1));
        match Self::try_open(&self.device, params) {
            Ok(pcm) => {
                self.pcm = Some(pcm);
                Ok(())
            }
            Err(primary) => {
                // The `hdmi:` config may be absent on this card; try the raw
                // `hw:` device before giving up (the sink then stays silent).
                if let Some(hw) = self.fallback_hw.clone() {
                    match Self::try_open(&hw, params) {
                        Ok(pcm) => {
                            tracing::warn!(
                                device = %self.device,
                                fallback = %hw,
                                "hdmi: PCM unavailable; using raw hw: (no IEC958 config layer)"
                            );
                            self.pcm = Some(pcm);
                            return Ok(());
                        }
                        Err(secondary) => {
                            return Err(format!("{primary}; fallback {secondary}"));
                        }
                    }
                }
                Err(primary)
            }
        }
    }

    fn write(&mut self, interleaved: &[f32], _channels: usize) -> PcmOutcome {
        let Some(pcm) = self.pcm.as_ref() else {
            return PcmOutcome::Underrun;
        };
        let Ok(io) = pcm.io_f32() else {
            return PcmOutcome::Underrun;
        };
        match io.writei(interleaved) {
            Ok(frames) => PcmOutcome::Wrote(frames),
            Err(e) => Self::classify(&e),
        }
    }

    fn recover(&mut self) -> PcmOutcome {
        let Some(pcm) = self.pcm.as_ref() else {
            return PcmOutcome::RecoverFailed;
        };
        // A suspended PCM needs resume; an underrun needs prepare. `prepare`
        // covers both for our purposes (resume-then-prepare); a remaining error
        // means the device is gone.
        if pcm.state() == State::Suspended {
            // Best-effort resume; ignore EAGAIN spins by falling through to
            // prepare.
            let _ = pcm.resume();
        }
        match pcm.prepare() {
            Ok(()) => PcmOutcome::Recovered,
            Err(_) => PcmOutcome::RecoverFailed,
        }
    }

    fn close(&mut self) {
        // Dropping the PCM closes it; we do not drain (teardown).
        self.pcm = None;
    }

    fn delay_frames(&mut self) -> Option<i64> {
        // `snd_pcm_delay`: frames delivered but not yet audible — the skew
        // measurement's refinement term. `Frames` is the platform `c_long`
        // (i64 on the 64-bit Linux targets we ship display nodes on).
        let pcm = self.pcm.as_ref()?;
        pcm.delay().ok()
    }
}

/// `EPIPE` errno (an xrun/underrun on a playback PCM).
const EPIPE: i32 = 32;

/// `ESTRPIPE` errno (a suspended PCM stream awaiting resume).
const ESTRPIPE: i32 = 86;
