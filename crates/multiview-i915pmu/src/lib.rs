//! Minimal safe wrapper over the Linux i915 PMU per-engine busy-ns counter.
//!
//! RED stub: signatures only; the real encoding/diff and the `perf_event_open`
//! FFI land in the follow-up green commit. The tests fail against this stub.

/// i915 engine class for the Render/3D engine (RCS).
pub const ENGINE_CLASS_RENDER: u8 = 0;
/// i915 engine class for the Video engine (VCS).
pub const ENGINE_CLASS_VIDEO: u8 = 2;
/// i915 engine class for the Video Enhance engine (VECS).
pub const ENGINE_CLASS_VIDEO_ENHANCE: u8 = 3;

/// Encode an i915 PMU `config` for the busy-ns counter of one engine — STUB.
#[must_use]
pub const fn engine_busy_config(_class: u8, _instance: u8) -> u64 {
    0
}

/// Derive an engine busy fraction from two busy-ns reads — STUB.
#[must_use]
pub fn busy_fraction(_earlier_ns: u64, _later_ns: u64, _interval_ns: u64) -> Option<f32> {
    None
}

/// Read the dynamic i915 PMU `type` from a sysfs `type` file — STUB.
#[cfg(target_os = "linux")]
#[must_use]
pub fn read_pmu_type(_type_path: &std::path::Path) -> Option<u32> {
    None
}

/// An open i915 PMU per-engine busy-ns counter — STUB.
pub struct I915PmuCounter {
    _private: (),
}

impl I915PmuCounter {
    /// Open a busy-ns counter — STUB (always unavailable).
    #[must_use]
    pub fn open(_pmu_type: u32, _config: u64) -> Option<Self> {
        None
    }

    /// Read the running busy-ns — STUB.
    #[must_use]
    pub fn read_busy_ns(&self) -> Option<u64> {
        None
    }
}
