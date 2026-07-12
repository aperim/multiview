//! System capability reporting for `GET /api/v1/system/capabilities` (ADR-W030).
//!
//! The honest **default-build** capability surface: which codec/compositor
//! backends this binary can use, the SA-0 compositor classification, the
//! effective build-profile **licence** (a compliance surface — ADR-0012), and
//! the mandatory NDI attribution. This module holds the wire DTO only; it is
//! deliberately built from **primitives and local enums — no `multiview-hal`
//! types** — so `multiview-control` keeps zero dependency on the HAL. The
//! `multiview` binary maps `hal::probe()` + compiled Cargo features + the
//! resolved compositor `AdapterReport` onto this DTO at startup and installs it
//! with [`crate::AppState::with_capabilities`] (a static snapshot — invariant
//! #10, never an engine channel).
//!
//! [ADR-M014](../../../docs/decisions/ADR-M014.md) §2 extends this additively
//! with the **#180-A** host + per-device *static* telemetry: [`HostInfo`]
//! (OS/arch/CPU/RAM, cgroup-v2 limits, PSI/thermal *availability*), the
//! per-device static identity slice ([`DeviceCapability`] — vendor / stable id /
//! PCI bus id / total VRAM), the [`DetectionInfo`] per-layer probe status, and
//! the [`SystemCapabilities::observed_at`] provenance stamp. Still deferred to
//! **#180-B** (GPU-runner-gated): the deep vendor-caps fields (device model,
//! driver version, engine topology, NVENC session ceiling, per-codec
//! profiles) — added to [`DeviceCapability`] there, never modelled-empty here.
//! Every field is one the running binary honestly fills; unknown is
//! first-class, never a fabricated zero (rule 6 / rule 27).

use serde::Serialize;

/// The pipeline stage a backend serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PipelineStage {
    /// Decode (demux/decode of an input elementary stream).
    Decode,
    /// Composite (scale + place + colour-convert + blend the canvas).
    Composite,
    /// Encode (encode the composited canvas for a rendition).
    Encode,
}

/// The concrete kind of a codec/compositor backend. Serialized to match the
/// Cargo feature names (`cuda`, `vaapi`, `qsv`, `videotoolbox`, `wgpu`, `metal`,
/// `software`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum BackendKind {
    /// Pure-CPU / SIMD software path (always available; the universal fallback).
    Software,
    /// NVIDIA CUDA (NVDEC/NVENC + custom CUDA compositor).
    Cuda,
    /// Apple `VideoToolbox`.
    #[serde(rename = "videotoolbox")]
    VideoToolbox,
    /// Linux VA-API (Intel/AMD).
    Vaapi,
    /// Intel Quick Sync via oneVPL.
    Qsv,
    /// Portable wgpu compositor backend.
    Wgpu,
    /// Apple Metal compositor backend.
    Metal,
}

/// A backend's maximum supported resolution (as probed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BackendResolution {
    /// Maximum frame width in pixels.
    pub width: u32,
    /// Maximum frame height in pixels.
    pub height: u32,
}

/// The availability + probed capability of one `(kind, stage)` backend.
///
/// `available` is the honest single signal: the backend is compiled **and** a
/// usable device is present. The compiled-but-device-absent distinction
/// (`compiled_in`) needs HAL-side feature introspection and is deferred to the
/// SA-1+ deep probe (task #180).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BackendCapability {
    /// The backend kind.
    pub kind: BackendKind,
    /// The pipeline stage this entry describes.
    pub stage: PipelineStage,
    /// Whether the backend is compiled in **and** a usable device is present.
    pub available: bool,
    /// The probed maximum resolution — present only when `available`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_resolution: Option<BackendResolution>,
    /// Whether the backend can resize during decode — present only on an
    /// available decode-stage backend (meaningless elsewhere).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_resize: Option<bool>,
}

/// The SA-0 composite-usability classification of the resolved wgpu adapter
/// (ADR-0035): the acceleration tier the compositor actually runs on. `None`
/// means no adapter resolved — the CPU composite path only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CompositorClass {
    /// A real hardware GPU adapter is present.
    Gpu,
    /// An adapter is present but its GPU-ness is low-confidence.
    LowConfidenceGpu,
    /// A software rasterizer adapter (e.g. llvmpipe).
    Software,
    /// No adapter resolved — CPU composite path only.
    None,
}

/// The kind of graphics device backing the compositor adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CompositorDeviceType {
    /// A discrete GPU.
    DiscreteGpu,
    /// An integrated GPU.
    IntegratedGpu,
    /// A virtual/paravirtual GPU.
    VirtualGpu,
    /// A CPU (software rasterizer).
    Cpu,
    /// Some other/unknown device type.
    Other,
}

/// The compositor's resolved acceleration tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CompositorCapability {
    /// The composite-usability class.
    pub class: CompositorClass,
    /// The device type — present only when an adapter resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_type: Option<CompositorDeviceType>,
    /// The adapter's driver string — present only when an adapter resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
}

/// The effective **build-profile** licence — the codec-linking licence of the
/// compiled artifact (ADR-0012 / AGENTS.md §G). Distinct from the project's
/// source-available licence and from NDI's redistribution restriction (carried
/// separately by [`BuildInfo::ndi`] / [`NdiAttribution`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[non_exhaustive]
pub enum EffectiveLicense {
    /// The default build: `FFmpeg` linked LGPL, no x264/x265, redistributable.
    #[serde(rename = "LGPL-clean")]
    LgplClean,
    /// `gpl-codecs` is compiled (x264/x265) → the whole product is GPL.
    #[serde(rename = "GPL")]
    Gpl,
}

/// The build-profile compliance surface: effective licence, redistributability,
/// the compiled feature set, and whether NDI is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct BuildInfo {
    /// The effective codec-linking licence of this artifact.
    pub effective_license: EffectiveLicense,
    /// Whether the artifact is redistributable. `true` for every shippable
    /// Multiview build: the default is LGPL-clean, `gpl-codecs` is GPL
    /// (redistributable under copyleft), and NDI is runtime-loaded (never
    /// vendored/linked) — there is no non-redistributable `--enable-nonfree`
    /// closure (no `nonfree` Cargo feature exists).
    pub redistributable: bool,
    /// The compiled, capability/licence-relevant Cargo features.
    pub features: Vec<String>,
    /// Whether the `ndi` feature is compiled (the proprietary, runtime-loaded
    /// SDK path). When `true`, [`SystemCapabilities::ndi_attribution`] is set.
    pub ndi: bool,
}

impl BuildInfo {
    /// Resolve the compliance surface from the compiled licence-escalating
    /// features. The caller (the `multiview` binary) passes its own
    /// `cfg!(feature = "gpl-codecs")` / `cfg!(feature = "ndi")` and the compiled
    /// feature list.
    ///
    /// Pinned to AGENTS.md §G / ADR-0012: `gpl-codecs` ⇒ the whole product is
    /// GPL; otherwise LGPL-clean. Every shippable build is redistributable.
    #[must_use]
    pub fn resolve(gpl_codecs: bool, ndi: bool, features: Vec<String>) -> Self {
        // `gpl-codecs` (x264/x265) relicenses the whole product GPL; otherwise
        // the default LGPL-clean profile stands (AGENTS.md §G / ADR-0012). Every
        // shippable build is redistributable — there is no `--enable-nonfree`
        // Cargo feature, and NDI is runtime-loaded (never linked/vendored).
        let effective_license = if gpl_codecs {
            EffectiveLicense::Gpl
        } else {
            EffectiveLicense::LgplClean
        };
        Self {
            effective_license,
            redistributable: true,
            features,
            ndi,
        }
    }
}

/// The mandatory NDI attribution (AGENTS.md §G) — present iff the `ndi` feature
/// is compiled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NdiAttribution {
    /// The required trademark line.
    pub trademark: String,
    /// A link to the NDI site.
    pub url: String,
}

impl NdiAttribution {
    /// The mandatory NDI® attribution carried whenever the `ndi` feature is on.
    #[must_use]
    pub fn required() -> Self {
        Self {
            trademark: "NDI® is a registered trademark of Vizrt NDI AB".to_owned(),
            url: "https://ndi.video".to_owned(),
        }
    }
}

/// The outcome of one capability-probe layer or per-device probe — **"ran" ≠
/// "succeeded"**.
///
/// Distinguishes *not attempted* from *succeeded* from *unsupported* (probed,
/// confirmed absent) from *failed* (probed, errored), so an empty result is
/// never mistaken for a probe that did not run (rule 27). #180-A fills the
/// host-global [`DetectionInfo`] and host [`HostInfo::psi`] / cgroup status; the
/// per-device deep-probe `caps` status is the #180-B slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProbeStatus {
    /// The probe was not attempted (a layer that was never reached).
    #[default]
    NotAttempted,
    /// The probe ran and the capability is present.
    Succeeded,
    /// The probe ran; the capability is **confirmed absent** / unsupported here.
    Unsupported,
    /// The probe ran and failed (an I/O or query error).
    Failed,
}

/// A detected accelerator's **static** capability slice.
///
/// #180-A surfaces only the verified `DeviceLoad`/`DeviceId` static fields
/// (`multiview-hal` `load.rs`): vendor, stable id, PCI bus id, total VRAM. The
/// deep vendor-caps fields — device model, driver version, engine topology,
/// NVENC session ceiling, per-codec profiles — are the **#180-B** slice and are
/// added here *with their real vendor-query impls* (GPU-runner-gated, rule 26),
/// never modelled-empty now (rule 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeviceCapability {
    /// The accelerator vendor label (`nvidia` / `intel` / `amd` / `apple`).
    pub vendor: String,
    /// The stable device id (the vendor's stable handle — an NVML UUID or PCI
    /// slot), the placement + cross-probe correlation key.
    pub id: String,
    /// The device's PCI bus id in canonical form, where the probe knows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pci_bus_id: Option<String>,
    /// Total VRAM in bytes, where the vendor exposes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vram_total_bytes: Option<u64>,
}

/// cgroup-v2 resource limits for the process's leaf cgroup (`cpu.max` /
/// `memory.max`).
///
/// [`Self::probe`] disambiguates *unlimited* (`Succeeded` + a `None` limit, the
/// file read `max`) from *unprobed* (`Unsupported` / `Failed` /
/// `NotAttempted`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CgroupLimits {
    /// Whether the cgroup-v2 hierarchy was found and read for this process.
    pub probe: ProbeStatus,
    /// `cpu.max` quota in microseconds; `None` = unlimited or unprobed (see
    /// [`Self::probe`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_max_quota_us: Option<u64>,
    /// `cpu.max` period in microseconds; present iff a real quota is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_max_period_us: Option<u64>,
    /// `memory.max` in bytes; `None` = unlimited or unprobed (see
    /// [`Self::probe`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
}

/// The host machine's static capacity-relevant facts (#180-A).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct HostInfo {
    /// The target OS (`linux`, `macos`, …).
    pub os: String,
    /// The target architecture (`x86_64`, `aarch64`, …).
    pub arch: String,
    /// Logical CPU count, where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    /// Scheduler-available parallelism, where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_parallelism: Option<u32>,
    /// Total physical RAM in bytes, where known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ram_bytes: Option<u64>,
    /// This process's cgroup-v2 CPU / memory limits.
    pub cgroup: CgroupLimits,
    /// Whether Linux PSI is **available** (never a reading — PSI values are
    /// live telemetry, excluded from this static snapshot; invariant #10).
    pub psi: ProbeStatus,
    /// The host's thermal-zone names. `None` = the sysfs thermal tree was **not
    /// probed** (absent / non-Linux); `Some([])` = probed, **none present** —
    /// never conflating absence with a probe failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thermal_sensors: Option<Vec<String>>,
}

/// Host-global capability-detection status per layer (#180-A).
///
/// L2 (per-device vendor caps) is **not** global — it rides each
/// [`DeviceCapability`] in the #180-B slice, because L2 can *succeed* on one GPU
/// and *fail* on another. Only the host-global L1 (FFmpeg-backed codec probe)
/// and L3 (environment/heuristic probe) layers are reported here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DetectionInfo {
    /// The L1 FFmpeg-backed codec-probe status.
    pub l1_ffmpeg: ProbeStatus,
    /// The L3 environment/heuristic probe status.
    pub l3_probe: ProbeStatus,
}

/// The `GET /api/v1/system/capabilities` response (ADR-W030, extended
/// additively by [ADR-M014](../../../docs/decisions/ADR-M014.md) §2): the honest
/// default-build capability + licence surface, plus the #180-A host + per-device
/// static telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SystemCapabilities {
    /// Per-`(kind, stage)` backend availability.
    pub backends: Vec<BackendCapability>,
    /// The compositor's resolved acceleration tier.
    pub compositor: CompositorCapability,
    /// The build-profile compliance surface.
    pub build: BuildInfo,
    /// The mandatory NDI attribution — present iff `ndi` is compiled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ndi_attribution: Option<NdiAttribution>,
    /// The startup capture time (RFC 3339) — the **provenance anchor**
    /// (ADR-M014 §1): every field is a fact *as observed at process startup*, a
    /// *known-vs-unknown* claim, never *current-vs-stale*. **Always serialized**
    /// (a required schema field): the §1 rule-27 provenance guarantee depends on
    /// it being present on every response, even the absent-fallback.
    // RED: this `skip_serializing_if` is WRONG — `observed_at` must ALWAYS
    // serialize (it is the provenance anchor). The green commit removes it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub observed_at: String,
    /// Per detected accelerator — the static identity + VRAM slice (#180-A).
    /// Empty on a host with no accelerator (the honest absent-fallback).
    // RED: no `skip_serializing_if` yet — empty should be omitted (additive).
    pub devices: Vec<DeviceCapability>,
    /// The host machine block (#180-A); `None` only if the probe was not run.
    // RED: no `skip_serializing_if` yet — `None` should be omitted.
    pub host: Option<HostInfo>,
    /// Host-global capability-detection status per layer (#180-A).
    pub detection: DetectionInfo,
}

impl Default for SystemCapabilities {
    /// A coarse, honest software-only default so the endpoint is always present
    /// and truthful even when the binary wires nothing (tests, non-run paths).
    /// The software path is always available (the universal fallback); detailed
    /// probed caps are filled by the binary's HAL-backed mapping. No adapter is
    /// assumed (CPU composite), and the default profile is LGPL-clean.
    fn default() -> Self {
        let software = |stage| BackendCapability {
            kind: BackendKind::Software,
            stage,
            available: true,
            max_resolution: None,
            decode_resize: None,
        };
        Self {
            backends: vec![
                software(PipelineStage::Decode),
                software(PipelineStage::Composite),
                software(PipelineStage::Encode),
            ],
            compositor: CompositorCapability {
                class: CompositorClass::None,
                device_type: None,
                driver: None,
            },
            build: BuildInfo::resolve(false, false, vec!["software".to_owned()]),
            ndi_attribution: None,
            // The unwired default carries no real snapshot: an empty capture
            // time, no devices, no host. The `multiview` binary always installs
            // a real snapshot (a stamped `observed_at`, the probed host + device
            // slice) via `with_capabilities` — this default only backs the
            // never-run/test paths so the endpoint is always present + truthful.
            observed_at: String::new(),
            devices: Vec::new(),
            host: None,
            detection: DetectionInfo::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_host() -> HostInfo {
        HostInfo {
            os: "linux".to_owned(),
            arch: "x86_64".to_owned(),
            cpu_cores: Some(8),
            available_parallelism: Some(8),
            total_ram_bytes: Some(16_000_000_000),
            cgroup: CgroupLimits {
                probe: ProbeStatus::Succeeded,
                cpu_max_quota_us: Some(50_000),
                cpu_max_period_us: Some(100_000),
                memory_max_bytes: Some(4_000_000_000),
            },
            psi: ProbeStatus::Succeeded,
            thermal_sensors: Some(vec!["x86_pkg_temp".to_owned()]),
        }
    }

    #[test]
    fn w030_fields_serialize_unchanged() {
        // The extension is additive: the existing ADR-W030 keys + values are
        // unchanged, never a rename/removal (backward-compat, not byte-identity).
        let caps = SystemCapabilities::default();
        let v = serde_json::to_value(&caps).unwrap();
        assert!(v.get("backends").is_some());
        assert!(v.get("compositor").is_some());
        assert_eq!(v["build"]["effective_license"], "LGPL-clean");
        assert_eq!(v["build"]["redistributable"], true);
        // `ndi_attribution` keeps its skip-when-None behavior.
        assert!(v.get("ndi_attribution").is_none());
    }

    #[test]
    fn observed_at_is_always_serialized_even_when_empty() {
        // The provenance anchor is never skipped (ADR-M014 §2): even the
        // absent-fallback (empty capture time, no devices, no host) emits the
        // `observed_at` key so a consumer can always read the snapshot's origin.
        let caps = SystemCapabilities::default();
        assert!(caps.observed_at.is_empty());
        let v = serde_json::to_value(&caps).unwrap();
        assert!(
            v.get("observed_at").is_some(),
            "observed_at must always serialize (the provenance anchor)"
        );
    }

    #[test]
    fn empty_devices_and_none_host_are_omitted() {
        // devices/host are additive + optional: omitted when empty/absent so an
        // old client that ignores unknown keys still parses the response.
        let caps = SystemCapabilities::default();
        let v = serde_json::to_value(&caps).unwrap();
        assert!(v.get("devices").is_none(), "empty devices should be omitted");
        assert!(v.get("host").is_none(), "None host should be omitted");
    }

    #[test]
    fn populated_devices_host_and_detection_serialize() {
        let caps = SystemCapabilities {
            observed_at: "2026-07-12T00:00:00Z".to_owned(),
            devices: vec![DeviceCapability {
                vendor: "nvidia".to_owned(),
                id: "GPU-1234".to_owned(),
                pci_bus_id: Some("0000:03:00.0".to_owned()),
                vram_total_bytes: Some(17_000_000_000),
            }],
            host: Some(sample_host()),
            detection: DetectionInfo {
                l1_ffmpeg: ProbeStatus::NotAttempted,
                l3_probe: ProbeStatus::Succeeded,
            },
            ..SystemCapabilities::default()
        };
        let v = serde_json::to_value(&caps).unwrap();
        assert_eq!(v["observed_at"], "2026-07-12T00:00:00Z");
        assert_eq!(v["devices"][0]["vendor"], "nvidia");
        assert_eq!(v["devices"][0]["id"], "GPU-1234");
        assert_eq!(v["devices"][0]["pci_bus_id"], "0000:03:00.0");
        assert_eq!(v["devices"][0]["vram_total_bytes"], 17_000_000_000_u64);
        assert_eq!(v["host"]["os"], "linux");
        assert_eq!(v["host"]["cgroup"]["cpu_max_quota_us"], 50_000);
        // Per-layer probe status serializes snake_case.
        assert_eq!(v["detection"]["l1_ffmpeg"], "not_attempted");
        assert_eq!(v["detection"]["l3_probe"], "succeeded");
    }

    #[test]
    fn device_capability_omits_unknown_optionals() {
        // Unknown is first-class: a device with no PCI/VRAM omits those keys
        // (never a fabricated zero).
        let dev = DeviceCapability {
            vendor: "amd".to_owned(),
            id: "card0".to_owned(),
            pci_bus_id: None,
            vram_total_bytes: None,
        };
        let v = serde_json::to_value(&dev).unwrap();
        assert_eq!(v["vendor"], "amd");
        assert!(v.get("pci_bus_id").is_none());
        assert!(v.get("vram_total_bytes").is_none());
    }

    #[test]
    fn probe_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(ProbeStatus::NotAttempted).unwrap(),
            "not_attempted"
        );
        assert_eq!(
            serde_json::to_value(ProbeStatus::Succeeded).unwrap(),
            "succeeded"
        );
        assert_eq!(
            serde_json::to_value(ProbeStatus::Unsupported).unwrap(),
            "unsupported"
        );
        assert_eq!(serde_json::to_value(ProbeStatus::Failed).unwrap(), "failed");
    }
}
