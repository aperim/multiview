//! Live per-device load model + the vendor probe seam (ADR-0017).
//!
//! Where [`crate::capability`]/[`crate::cost`]/[`crate::planner`] model what a
//! device *can* do and what *fits*, this module models what a device is *doing
//! right now*: a [`DeviceLoad`] snapshot per GPU (compute-busy, VRAM used/total,
//! per-engine encoder/decoder utilisation, NVENC concurrent-session count). It
//! is the fourth seam in `mosaic-hal`, beside the presence-detection
//! [`crate::probe`], and it mirrors that module's discipline exactly:
//!
//! - The **pure load model** ([`DeviceId`], [`DeviceLoad`], [`Vendor`]) always
//!   compiles, with no native deps.
//! - Every field a vendor cannot report is an [`Option`] — "unknown" is a
//!   first-class state, never a fabricated zero
//!   ([gpu-monitoring §2.5](../../../docs/research/gpu-monitoring-and-scheduling.md)).
//!   The selection policy in [`crate::select`] drops an unknown term and
//!   redistributes its weight; it never invents a metric.
//! - The vendor seam is the injectable [`LoadProbe`] trait (mirroring
//!   [`crate::probe::DeviceProbe`]). A [`LoadPoller`] wraps a probe with the
//!   bounded off-hot-path polling contract.
//! - Real vendor probes are feature-gated behind the existing `cuda` / `vaapi`
//!   / `qsv` features. The NVIDIA path (NVML via the runtime-loaded
//!   `nvml-wrapper`) initialises-or-returns-[`LoadSample::Unavailable`]
//!   gracefully on a host with no NVIDIA device — never a panic.
//!
//! See [gpu-monitoring-and-scheduling](../../../docs/research/gpu-monitoring-and-scheduling.md)
//! and ADR-0017.

/// The vendor family of a device, used to label telemetry and to know which
/// per-engine signals to expect.
///
/// Distinct from [`crate::Stage`] (the pipeline stage) and from
/// [`mosaic_core::traits::BackendKind`] (the backend implementation): a
/// [`Vendor`] is the *physical silicon family* a [`DeviceLoad`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Vendor {
    /// NVIDIA (NVML; per-engine enc/dec util + NVENC session ceiling).
    Nvidia,
    /// Intel (i915 PMU / DRM fdinfo; per-engine enc/dec util).
    Intel,
    /// AMD (amdgpu sysfs + DRM fdinfo; enc/dec merged from VCN4).
    Amd,
    /// Apple (no public per-engine util; unified memory + thermal only).
    Apple,
}

impl Vendor {
    /// A short, stable, lower-case label for telemetry (the `vendor` gauge
    /// label).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Vendor::Nvidia => "nvidia",
            Vendor::Intel => "intel",
            Vendor::Amd => "amd",
            Vendor::Apple => "apple",
        }
    }
}

/// A **stable** device identity — the placement + pin key.
///
/// Per [gpu-monitoring §2.1](../../../docs/research/gpu-monitoring-and-scheduling.md)
/// this is the vendor's stable handle (NVML UUID, PCI bus id, DRM render-node
/// plus PCI, or Metal registryID), **never** the enumeration index, which is
/// unstable across reboots and `CUDA_VISIBLE_DEVICES` reorderings. An operator
/// pin ([`crate::select`]) binds to this.
///
/// The `index` is retained only as a deterministic, never-load-bearing
/// tie-breaker for selection (lowest index wins an exact score tie); identity
/// and equality are defined by `(vendor, stable_id)`.
#[derive(Debug, Clone)]
pub struct DeviceId {
    vendor: Vendor,
    stable_id: String,
    index: u32,
}

impl DeviceId {
    /// Construct a device identity from its vendor, stable id, and enumeration
    /// index.
    ///
    /// `stable_id` must be the vendor's stable handle (UUID / PCI bus id /
    /// registryID). `index` is only ever a deterministic tie-breaker.
    #[must_use]
    pub fn new(vendor: Vendor, stable_id: impl Into<String>, index: u32) -> Self {
        Self {
            vendor,
            stable_id: stable_id.into(),
            index,
        }
    }

    /// The device's vendor family.
    #[must_use]
    pub const fn vendor(&self) -> Vendor {
        self.vendor
    }

    /// The stable vendor handle (UUID / PCI bus id / registryID).
    #[must_use]
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    /// The enumeration index — a deterministic tie-breaker only, never an
    /// identity.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }
}

impl PartialEq for DeviceId {
    /// Identity is `(vendor, stable_id)`; the enumeration index is deliberately
    /// excluded so a reordering across reboots does not change identity.
    fn eq(&self, other: &Self) -> bool {
        self.vendor == other.vendor && self.stable_id == other.stable_id
    }
}

impl Eq for DeviceId {}

impl core::hash::Hash for DeviceId {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.vendor.hash(state);
        self.stable_id.hash(state);
    }
}

/// A live per-device load snapshot (ADR-0017's `DeviceLoad`).
///
/// Produced by a [`LoadProbe`] (or injected in tests), consumed by the pure
/// selection policy in [`crate::select`] and mirrored into per-GPU Prometheus
/// gauges in `mosaic-telemetry`.
///
/// Every load field is an [`Option`] because **availability is
/// vendor-asymmetric** (the honest matrix,
/// [gpu-monitoring §1](../../../docs/research/gpu-monitoring-and-scheduling.md)):
/// per-engine encoder/decoder utilisation is clean on NVIDIA + Intel,
/// per-process-only/merged on AMD, and absent on Apple. `None` means **unknown**
/// — the selector drops it from the score and redistributes weight; it is never
/// fabricated as `0.0`, and the telemetry layer does not register a gauge for
/// it (so dashboards show "n/a", not a false zero).
///
/// Fractions are normalised `0.0..=1.0` busy fractions; bytes are absolute.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DeviceLoad {
    /// The device this load describes (stable identity).
    pub device_id: DeviceId,
    /// GPU core / compute busy fraction (`0.0..=1.0`), where exposed. This is
    /// the SM/3D-engine busy fraction (compositor pressure), **not** the memory
    /// controller (the verified NVML `.memory` trap is avoided —
    /// [gpu-monitoring §1](../../../docs/research/gpu-monitoring-and-scheduling.md)).
    pub gpu_busy_frac: Option<f32>,
    /// VRAM bytes currently in use across all consumers, where exposed.
    pub vram_used_bytes: Option<u64>,
    /// Total VRAM bytes, where exposed. With `vram_used_bytes` this is the
    /// authoritative VRAM-pressure pair (`nvmlDeviceGetMemoryInfo`), never the
    /// memory-controller busy %.
    pub vram_total_bytes: Option<u64>,
    /// Encoder-ASIC busy fraction (`0.0..=1.0`), where the vendor meters the
    /// encode engine on its own counter. On AMD VCN4+ this is the *combined*
    /// media figure; on Apple it is always `None`.
    pub enc_util_frac: Option<f32>,
    /// Decoder-ASIC busy fraction (`0.0..=1.0`), where metered per-engine. On
    /// AMD VCN4+ this is the *combined* media figure; on Apple always `None`.
    pub dec_util_frac: Option<f32>,
    /// Concurrent NVENC encode-session count for this device, where the vendor
    /// exposes it (NVIDIA only). Feeds the per-system session-ceiling gate in
    /// [`crate::select`].
    pub nvenc_session_count: Option<u32>,
    /// Compute-engine busy fraction (`0.0..=1.0`) as distinct from
    /// `gpu_busy_frac` on vendors that separate a compute queue from the 3D
    /// queue; `None` collapses to `gpu_busy_frac` for the compositor-pressure
    /// term.
    pub compute_busy_frac: Option<f32>,
}

impl DeviceLoad {
    /// Construct an all-unknown load snapshot for `device_id` — every signal
    /// `None`.
    ///
    /// This is the honest starting point a blind vendor (Apple) or a
    /// probe-before-first-sample yields: the selector falls back to the cost
    /// model, and no telemetry gauge is registered for an unknown field.
    #[must_use]
    pub const fn unknown(device_id: DeviceId) -> Self {
        Self {
            device_id,
            gpu_busy_frac: None,
            vram_used_bytes: None,
            vram_total_bytes: None,
            enc_util_frac: None,
            dec_util_frac: None,
            nvenc_session_count: None,
            compute_busy_frac: None,
        }
    }

    /// Fraction of VRAM in use (`used / total`), if both bytes are known and
    /// `total > 0`.
    ///
    /// This is the **primary**, highest-weighted selection signal (a hard OOM
    /// wall) and the one signal trustworthy on every vendor.
    #[must_use]
    pub fn vram_used_frac(&self) -> Option<f32> {
        match (self.vram_used_bytes, self.vram_total_bytes) {
            (Some(used), Some(total)) if total > 0 => {
                // u32-domain fractions: widen both to f64 losslessly, divide,
                // then clamp to [0,1] and narrow to f32. No `as` casts.
                let frac = bytes_ratio(used, total);
                Some(frac)
            }
            _ => None,
        }
    }

    /// Free VRAM in bytes (`total - used`), if both are known. Saturates at `0`
    /// if a transient sample reports `used > total`.
    #[must_use]
    pub fn vram_free_bytes(&self) -> Option<u64> {
        match (self.vram_used_bytes, self.vram_total_bytes) {
            (Some(used), Some(total)) => Some(total.saturating_sub(used)),
            _ => None,
        }
    }

    /// The compositor-pressure busy fraction: `compute_busy_frac` if present,
    /// else `gpu_busy_frac`. `None` if neither is exposed.
    #[must_use]
    pub fn effective_compute_frac(&self) -> Option<f32> {
        self.compute_busy_frac.or(self.gpu_busy_frac)
    }
}

/// `used / total` as an `f32` fraction clamped to `0.0..=1.0`, computed via
/// lossless `u64 -> f64` widening (no `as` casts).
///
/// `total` is assumed `> 0` by the caller. Both byte counts are well below
/// `2^53` for real hardware, so the widening is exact.
fn bytes_ratio(used: u64, total: u64) -> f32 {
    let used = u64_to_f64(used);
    let total = u64_to_f64(total);
    let frac = (used / total).clamp(0.0, 1.0);
    f64_to_f32_saturating(frac)
}

/// Lossless `u64 -> f64` widening for byte counts (`< 2^53`), avoiding `as`.
fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value).map_or_else(
        |_| {
            let high = u32::try_from((value >> 32) & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            let low = u32::try_from(value & 0xFFFF_FFFF).map_or(f64::INFINITY, f64::from);
            high * 4_294_967_296.0 + low
        },
        f64::from,
    )
}

/// Narrow an `f64` in `0.0..=1.0` to `f32` without an `as` cast.
///
/// The value is already clamped to the unit interval by the caller, so we map
/// it onto an exact integer grid (`2^24` ticks — `f32`'s integer-exactness
/// bound), recover that grid count through `TryFrom`/`f32::from` (no `as`), and
/// divide back. The result carries full f32 precision for a unit fraction.
fn f64_to_f32_saturating(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    // 0 < scaled < 2^24 and integer-valued (`.round()`), so the f64 -> u32
    // conversion is lossless and in range via the string-free integer path.
    let ticks = u32::try_from(f64_trunc_to_u64(scaled)).unwrap_or(0);
    f32_from_u32(ticks) / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` to its integer part as a `u64`,
/// avoiding an `as` cast.
///
/// Implemented by reading the IEEE-754 fields: this is exact for any value
/// representable as an integer below `2^53` (our domain is `< 2^24`).
fn f64_trunc_to_u64(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    // Subnormals and values < 1.0 truncate to 0 (already handled above for the
    // <= 0 case; a positive value < 1.0 has exponent < 1023).
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
                                                        // value = significand * 2^(exponent - 52)
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24` (the unit-fraction grid). Avoids an
/// `as` cast by composing two `u16` halves, each lossless via `f32::from`.
fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

/// The outcome of one probe sample for a single device.
///
/// Mirrors [`crate::probe::ProbeOutcome`]: a probe either returns a fresh
/// [`DeviceLoad`] or reports that the device/vendor is *unavailable* (feature
/// off, no device, or the vendor library failed to initialise) — cleanly, never
/// a panic.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum LoadSample {
    /// A fresh per-device load snapshot.
    Ready(DeviceLoad),
    /// No usable device / vendor library for this probe on this host.
    Unavailable {
        /// Static, human-readable reason (e.g. `"NVML library not present"`).
        reason: &'static str,
    },
}

impl LoadSample {
    /// The [`DeviceLoad`] if this sample is [`LoadSample::Ready`], else `None`.
    #[must_use]
    pub fn load(&self) -> Option<&DeviceLoad> {
        match self {
            LoadSample::Ready(load) => Some(load),
            LoadSample::Unavailable { .. } => None,
        }
    }

    /// Whether this sample carries a usable load snapshot.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, LoadSample::Ready(_))
    }
}

/// The vendor seam for live-load probing.
///
/// A `LoadProbe` enumerates the devices it can see and samples each one's
/// current load. It mirrors the injectable [`crate::probe::DeviceProbe`]: real
/// feature-gated impls (NVML, i915, amdgpu) implement it, and tests inject a
/// double. Implementations must be **non-blocking-on-the-engine** and bounded:
/// the [`LoadPoller`] runs them off the hot path (ADR-0017 §2).
///
/// Probing must never panic. A vendor library that fails to initialise, or a
/// host with no device, yields [`LoadSample::Unavailable`] (and, for `sample`,
/// an empty/`Unavailable` result) — exactly the graceful-absence contract.
pub trait LoadProbe {
    /// The stable identities of the devices this probe can currently see.
    ///
    /// Returns an empty vector cleanly when the vendor library is absent or no
    /// device is present.
    fn devices(&self) -> Vec<DeviceId>;

    /// Sample the current load of one device.
    ///
    /// Returns [`LoadSample::Unavailable`] (never a panic) when the device is
    /// gone or the vendor library cannot answer.
    fn sample(&self, device: &DeviceId) -> LoadSample;

    /// Sample every visible device in one pass.
    ///
    /// The default walks [`LoadProbe::devices`] and [`LoadProbe::sample`]; a
    /// vendor impl may override for a cheaper single-pass query. Only
    /// [`LoadSample::Ready`] samples are returned.
    fn sample_all(&self) -> Vec<DeviceLoad> {
        self.devices()
            .iter()
            .filter_map(|d| match self.sample(d) {
                LoadSample::Ready(load) => Some(load),
                LoadSample::Unavailable { .. } => None,
            })
            .collect()
    }
}

/// A bounded, off-hot-path poller around a [`LoadProbe`].
///
/// This is the runtime abstraction ADR-0017 §2 describes: the engine owns a
/// dedicated blocking thread that drives `poll()` at ~1-4 Hz and publishes the
/// resulting `Vec<DeviceLoad>` into a wait-free snapshot (`arc_swap`, owned by
/// `mosaic-engine`) plus the telemetry gauges. The poller itself holds only the
/// probe and the configured cadence — it does **no** I/O, spawns **no** thread,
/// and never blocks the engine; the engine decides when to call `poll`. Keeping
/// it this thin makes it pure and unit-testable with an injected probe, and
/// keeps invariants #1/#10 the caller's structural guarantee.
#[derive(Debug, Clone)]
pub struct LoadPoller<P: LoadProbe> {
    probe: P,
    interval: PollInterval,
}

impl<P: LoadProbe> LoadPoller<P> {
    /// Wrap a probe with a polling cadence.
    #[must_use]
    pub const fn new(probe: P, interval: PollInterval) -> Self {
        Self { probe, interval }
    }

    /// The configured poll cadence.
    #[must_use]
    pub const fn interval(&self) -> PollInterval {
        self.interval
    }

    /// Borrow the underlying probe.
    #[must_use]
    pub const fn probe(&self) -> &P {
        &self.probe
    }

    /// Take one sample pass over every visible device.
    ///
    /// This is what the engine's blocking poll thread calls each tick. Returns
    /// the fresh loads to publish into the wait-free snapshot + the gauges.
    #[must_use]
    pub fn poll(&self) -> Vec<DeviceLoad> {
        self.probe.sample_all()
    }
}

/// A bounded poll cadence, in hertz, clamped to the ADR-0017 ~1-4 Hz envelope.
///
/// A newtype so the cadence cannot be set to an unbounded value that would turn
/// the probe into a hot-path cost (the fdinfo walk on tiny boxes; §5 risk 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollInterval {
    millis: u32,
}

impl PollInterval {
    /// Minimum poll period (250 ms = 4 Hz, the fast end of the envelope).
    pub const MIN_MILLIS: u32 = 250;
    /// Maximum poll period (1000 ms = 1 Hz, the slow end of the envelope).
    pub const MAX_MILLIS: u32 = 1000;

    /// Construct a poll interval from a period in milliseconds, clamped to the
    /// `MIN_MILLIS..=MAX_MILLIS` envelope.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        let clamped = if millis < Self::MIN_MILLIS {
            Self::MIN_MILLIS
        } else if millis > Self::MAX_MILLIS {
            Self::MAX_MILLIS
        } else {
            millis
        };
        Self { millis: clamped }
    }

    /// The (clamped) poll period in milliseconds.
    #[must_use]
    pub const fn as_millis(self) -> u32 {
        self.millis
    }
}

impl Default for PollInterval {
    /// 2 Hz (500 ms) — the middle of the ADR-0017 envelope.
    fn default() -> Self {
        Self::from_millis(500)
    }
}

// ----------------------------------------------------------------------------
// Feature-gated vendor probes.
//
// Each probe has a feature-off arm (absent from the build) and a feature-on arm
// that performs the real query. The feature-on arm MUST report Unavailable
// cleanly when the vendor library fails to initialise or no device is present
// (CI / this box) — never a panic. The NVIDIA path uses the runtime-loaded
// `nvml-wrapper` (libloading) so there is no build-time native dep.
// ----------------------------------------------------------------------------

#[cfg(feature = "cuda")]
pub use self::nvml::NvmlLoadProbe;

#[cfg(feature = "cuda")]
mod nvml {
    use super::{DeviceId, DeviceLoad, LoadProbe, LoadSample, Vendor};

    /// NVIDIA live-load probe via NVML (`nvml-wrapper`, runtime-loaded through
    /// `libloading`).
    ///
    /// Construction is fallible-but-graceful: [`NvmlLoadProbe::try_init`]
    /// returns `None` when NVML is not present (no driver, no device, library
    /// not loadable) — exactly the no-GPU fallback. Once initialised, every
    /// query that the device cannot answer maps to an unknown field (`None`) or
    /// [`LoadSample::Unavailable`]; nothing here panics.
    pub struct NvmlLoadProbe {
        nvml: nvml_wrapper::Nvml,
    }

    impl core::fmt::Debug for NvmlLoadProbe {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("NvmlLoadProbe").finish_non_exhaustive()
        }
    }

    impl NvmlLoadProbe {
        /// Try to initialise NVML, returning `None` if it is unavailable.
        ///
        /// On a host with no NVIDIA driver/device (this box, CI) `Nvml::init`
        /// returns an error which we map to `None` — initialise-or-unavailable,
        /// never a panic (ADR-0017 §2 design rule 1).
        #[must_use]
        pub fn try_init() -> Option<Self> {
            match nvml_wrapper::Nvml::init() {
                Ok(nvml) => Some(Self { nvml }),
                Err(error) => {
                    tracing::debug!(%error, "NVML unavailable; GPU load probe disabled");
                    None
                }
            }
        }
    }

    impl LoadProbe for NvmlLoadProbe {
        fn devices(&self) -> Vec<DeviceId> {
            let Ok(count) = self.nvml.device_count() else {
                return Vec::new();
            };
            (0..count)
                .filter_map(|index| {
                    let device = self.nvml.device_by_index(index).ok()?;
                    // Stable identity: the NVML UUID, never the enumeration
                    // index (ADR-0017 §2.1).
                    let uuid = device.uuid().ok()?;
                    Some(DeviceId::new(Vendor::Nvidia, uuid, index))
                })
                .collect()
        }

        fn sample(&self, device: &DeviceId) -> LoadSample {
            let Ok(handle) = self.nvml.device_by_index(device.index()) else {
                return LoadSample::Unavailable {
                    reason: "NVML device handle unavailable",
                };
            };
            // Confirm identity still matches (handles reorderings): if the UUID
            // changed under us, report Unavailable rather than mislabel.
            if handle.uuid().map_or(true, |u| u != device.stable_id()) {
                return LoadSample::Unavailable {
                    reason: "NVML device identity changed",
                };
            }

            // Each query is independently optional: the authoritative VRAM
            // pressure comes from MemoryInfo (bytes), NOT UtilizationRates.memory
            // (the verified memory-controller-busy trap).
            let mem = handle.memory_info().ok();
            let util = handle.utilization_rates().ok();
            let enc = handle.encoder_utilization().ok();
            let dec = handle.decoder_utilization().ok();
            let sessions = handle.encoder_stats().ok().map(|stats| stats.session_count);

            LoadSample::Ready(DeviceLoad {
                device_id: device.clone(),
                gpu_busy_frac: util.map(|u| percent_to_frac(u.gpu)),
                vram_used_bytes: mem.as_ref().map(|m| m.used),
                vram_total_bytes: mem.as_ref().map(|m| m.total),
                enc_util_frac: enc.map(|u| percent_to_frac(u.utilization)),
                dec_util_frac: dec.map(|u| percent_to_frac(u.utilization)),
                nvenc_session_count: sessions,
                // NVML does not separate a compute queue; the compositor-pressure
                // term falls back to gpu_busy_frac via effective_compute_frac.
                compute_busy_frac: None,
            })
        }
    }

    /// Convert an NVML 0-100 integer percentage to a clamped `0.0..=1.0` f32.
    fn percent_to_frac(percent: u32) -> f32 {
        let clamped = percent.min(100);
        let hundredths = u16::try_from(clamped).map_or(100.0_f32, f32::from);
        hundredths / 100.0_f32
    }
}

#[cfg(any(feature = "vaapi", feature = "qsv"))]
pub use self::linux_sysfs::SysfsLoadProbe;

#[cfg(any(feature = "vaapi", feature = "qsv"))]
mod linux_sysfs {
    use super::{DeviceId, LoadProbe, LoadSample};

    /// Intel/AMD Linux live-load probe (compile-only scaffold).
    ///
    /// The full implementation reads `amdgpu` sysfs `gpu_busy_percent`, the
    /// i915 PMU, and DRM fdinfo per ADR-0017; this scaffold compiles under the
    /// `vaapi`/`qsv` features and reports [`LoadSample::Unavailable`] until that
    /// landing — graceful absence where the sensors are not yet wired, never a
    /// panic and never a fabricated metric.
    #[derive(Debug, Clone, Copy, Default)]
    #[non_exhaustive]
    pub struct SysfsLoadProbe;

    impl SysfsLoadProbe {
        /// Construct the sysfs probe scaffold.
        #[must_use]
        pub const fn new() -> Self {
            Self
        }
    }

    impl LoadProbe for SysfsLoadProbe {
        fn devices(&self) -> Vec<DeviceId> {
            // Scaffold: no devices enumerated until the sysfs/PMU walk lands.
            Vec::new()
        }

        fn sample(&self, _device: &DeviceId) -> LoadSample {
            LoadSample::Unavailable {
                reason: "Linux sysfs/i915 PMU load probe not yet implemented",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]
    use super::*;

    fn nv(id: &str, index: u32) -> DeviceId {
        DeviceId::new(Vendor::Nvidia, id, index)
    }

    #[test]
    fn device_id_identity_ignores_index() {
        // Same vendor + stable id is the SAME device even if the enumeration
        // index reordered across a reboot.
        let a = nv("GPU-uuid-1", 0);
        let b = nv("GPU-uuid-1", 3);
        assert_eq!(a, b);
        // Different stable id is a different device.
        let c = nv("GPU-uuid-2", 0);
        assert_ne!(a, c);
    }

    #[test]
    fn unknown_load_is_all_none() {
        let load = DeviceLoad::unknown(nv("GPU-x", 0));
        assert!(load.gpu_busy_frac.is_none());
        assert!(load.vram_used_frac().is_none());
        assert!(load.vram_free_bytes().is_none());
        assert!(load.effective_compute_frac().is_none());
        assert!(load.nvenc_session_count.is_none());
    }

    #[test]
    fn vram_used_frac_is_clamped_unit_fraction() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(6_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        let frac = load.vram_used_frac().expect("both bytes known");
        assert!((frac - 0.5).abs() < 1e-4, "half used => 0.5, got {frac}");
        assert_eq!(load.vram_free_bytes(), Some(6_000_000_000));
    }

    #[test]
    fn vram_frac_handles_transient_over_total() {
        // A transient sample reporting used > total must clamp, not exceed 1.0,
        // and free saturates at 0 rather than underflowing.
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(13_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        assert_eq!(load.vram_used_frac(), Some(1.0));
        assert_eq!(load.vram_free_bytes(), Some(0));
    }

    #[test]
    fn vram_frac_unknown_when_total_zero() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.vram_used_bytes = Some(0);
        load.vram_total_bytes = Some(0);
        assert!(load.vram_used_frac().is_none(), "zero total => unknown");
    }

    #[test]
    fn effective_compute_prefers_compute_over_gpu_busy() {
        let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
        load.gpu_busy_frac = Some(0.4);
        load.compute_busy_frac = Some(0.7);
        assert_eq!(load.effective_compute_frac(), Some(0.7));
        load.compute_busy_frac = None;
        assert_eq!(load.effective_compute_frac(), Some(0.4));
    }

    #[test]
    fn poll_interval_clamps_to_envelope() {
        assert_eq!(PollInterval::from_millis(10).as_millis(), 250);
        assert_eq!(PollInterval::from_millis(5000).as_millis(), 1000);
        assert_eq!(PollInterval::from_millis(500).as_millis(), 500);
        assert_eq!(PollInterval::default().as_millis(), 500);
    }

    /// A probe double that returns fixed samples — exercises the poller and the
    /// `sample_all` default without hardware.
    struct FakeProbe {
        loads: Vec<DeviceLoad>,
    }

    impl LoadProbe for FakeProbe {
        fn devices(&self) -> Vec<DeviceId> {
            self.loads.iter().map(|l| l.device_id.clone()).collect()
        }
        fn sample(&self, device: &DeviceId) -> LoadSample {
            self.loads
                .iter()
                .find(|l| &l.device_id == device)
                .cloned()
                .map_or(
                    LoadSample::Unavailable {
                        reason: "no such device",
                    },
                    LoadSample::Ready,
                )
        }
    }

    #[test]
    fn poller_samples_every_visible_device() {
        let loads = vec![
            DeviceLoad::unknown(nv("GPU-a", 0)),
            DeviceLoad::unknown(nv("GPU-b", 1)),
        ];
        let poller = LoadPoller::new(FakeProbe { loads }, PollInterval::default());
        let sampled = poller.poll();
        assert_eq!(sampled.len(), 2);
        assert_eq!(sampled[0].device_id, nv("GPU-a", 0));
    }

    #[test]
    fn sample_of_absent_device_is_unavailable() {
        let probe = FakeProbe { loads: Vec::new() };
        let sample = probe.sample(&nv("GPU-missing", 0));
        assert!(!sample.is_ready());
        assert!(sample.load().is_none());
    }

    #[test]
    fn vram_frac_narrowing_is_precise_at_quarters() {
        // Exercise the as-free f64 -> f32 narrowing at exact fractions.
        for (used, total, expect) in [
            (1_u64, 4_u64, 0.25_f32),
            (3, 4, 0.75),
            (1, 1, 1.0),
            (0, 4, 0.0),
        ] {
            let mut load = DeviceLoad::unknown(nv("GPU-x", 0));
            load.vram_used_bytes = Some(used);
            load.vram_total_bytes = Some(total);
            let frac = load.vram_used_frac().expect("known");
            assert!(
                (frac - expect).abs() < 1e-4,
                "used={used} total={total} => {frac}, want {expect}"
            );
        }
    }

    /// Feature-gated NVIDIA probe test (ADR-0017): on a host with **no** NVIDIA
    /// device (this box / CI), `NvmlLoadProbe::try_init` must return `None`
    /// gracefully — NVML initialise-or-unavailable, never a panic. If a real
    /// NVIDIA GPU were present the probe would init and enumerate devices
    /// without panicking either; both arms are non-panicking by contract.
    #[cfg(feature = "cuda")]
    #[test]
    fn nvml_probe_is_graceful_without_a_gpu() {
        match NvmlLoadProbe::try_init() {
            None => {
                // The expected path on a non-NVIDIA host: cleanly unavailable.
            }
            Some(probe) => {
                // A real GPU is present (hardware runner): enumeration and
                // sampling must not panic, and every sample is well-formed.
                for device in probe.devices() {
                    let sample = probe.sample(&device);
                    if let Some(load) = sample.load() {
                        assert_eq!(load.device_id, device);
                    }
                }
            }
        }
    }
}
