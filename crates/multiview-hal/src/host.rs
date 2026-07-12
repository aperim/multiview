//! Host-machine capability probe (#180-A, [ADR-M014](../../../docs/decisions/ADR-M014.md) §3).
//!
//! A pure, GPU-free, dependency-free probe of the host: OS / arch / CPU / RAM
//! via `std` + `/proc`, cgroup-v2 resource limits, PSI *availability*, and
//! thermal-sensor *presence*. Every field is **"unknown is first-class"**: a
//! missing `/proc` file (a non-Linux host, a restricted container) yields
//! `None` / [`ProbeStatus::Unsupported`], never a fabricated value. The
//! `multiview` binary maps this onto the `multiview_control::system` wire DTO;
//! this module stays **serde-free** (the HAL planner-layer convention — the
//! control plane owns the wire type and keeps zero dependency on this crate,
//! the #263 / [ADR-W030](../../../docs/decisions/ADR-W030.md) boundary).
//!
//! Only *static / semi-static* host facts belong here (invariant #10): PSI and
//! thermal **values** are live gauges that ride the telemetry stream, so this
//! probe reports their *availability*, never a reading.

use std::path::Path;

/// The outcome of one host-probe layer — **"ran" ≠ "succeeded"**.
///
/// Distinguishes *not probed* from *probed-and-present* from
/// *probed-and-confirmed-absent* from *probed-and-errored*, so a consumer never
/// confuses "we did not look" with "it is not here" (rule 27).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProbeStatus {
    /// The probe was not attempted (e.g. a layer that was never reached).
    #[default]
    NotAttempted,
    /// The probe ran and the capability is present.
    Succeeded,
    /// The probe ran; the capability is **confirmed absent** / unsupported on
    /// this host (a kernel without the feature, or a non-Linux host).
    Unsupported,
    /// The probe ran and failed (an I/O or parse error).
    Failed,
}

/// cgroup-v2 resource limits for **this process's** leaf cgroup (`cpu.max` /
/// `memory.max`).
///
/// These are the values at the process's own cgroup node (resolved via the
/// `0::<path>` line of `/proc/self/cgroup`), **not** a computed effective
/// minimum across the hierarchy. `None` on a limit distinguishes *unlimited*
/// (`max`) from *unprobed* only in combination with [`Self::probe`]: when
/// `probe` is [`ProbeStatus::Succeeded`], a `None` limit means the file read
/// `max` (no limit); otherwise the value was not obtained.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CgroupLimits {
    /// Whether the cgroup-v2 unified hierarchy was found and read for this
    /// process.
    pub probe: ProbeStatus,
    /// `cpu.max` quota in microseconds; `None` = unlimited (`max`) or unprobed
    /// (see [`Self::probe`]). Moves together with [`Self::cpu_max_period_us`].
    pub cpu_max_quota_us: Option<u64>,
    /// `cpu.max` period in microseconds; present iff [`Self::cpu_max_quota_us`]
    /// is (a real bandwidth limit).
    pub cpu_max_period_us: Option<u64>,
    /// `memory.max` in bytes; `None` = unlimited (`max`) or unprobed (see
    /// [`Self::probe`]).
    pub memory_max_bytes: Option<u64>,
}

/// A static snapshot of the host machine's capacity-relevant facts
/// ([ADR-M014](../../../docs/decisions/ADR-M014.md) §3, #180-A).
///
/// Cross-platform where `std` allows ([`Self::os`] / [`Self::arch`] /
/// [`Self::available_parallelism`]); the `/proc`- and `/sys`-sourced fields are
/// `None` / [`ProbeStatus::Unsupported`] off Linux (honest, never fabricated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    /// The target OS (`std::env::consts::OS`, e.g. `linux`, `macos`).
    pub os: String,
    /// The target architecture (`std::env::consts::ARCH`, e.g. `x86_64`,
    /// `aarch64`).
    pub arch: String,
    /// Logical CPU count as reported by `/proc/cpuinfo`; `None` off Linux or
    /// when unreadable.
    pub cpu_cores: Option<u32>,
    /// The scheduler's available parallelism (`std::thread::available_parallelism`,
    /// which honours cgroup CPU affinity/quota where the platform exposes it).
    pub available_parallelism: Option<u32>,
    /// Total physical RAM in bytes (`/proc/meminfo` `MemTotal`); `None` off
    /// Linux or when unreadable.
    pub total_ram_bytes: Option<u64>,
    /// This process's cgroup-v2 CPU / memory limits.
    pub cgroup: CgroupLimits,
    /// Whether Linux PSI (`/proc/pressure`) is **available** (never a reading —
    /// PSI values are live telemetry).
    pub psi: ProbeStatus,
    /// The names of the host's thermal zones (`/sys/class/thermal/thermal_zone*/type`).
    /// `None` = the sysfs thermal tree was **not probed** (absent / non-Linux);
    /// `Some([])` = probed, **none present** — never conflating absence with a
    /// probe failure.
    pub thermal_sensors: Option<Vec<String>>,
}

/// Probe the host machine's static capability facts from the real `/proc`,
/// `/sys`, and cgroup-v2 roots.
///
/// Off-hot-path, allocation-light, and infallible: an unreadable or absent
/// source yields an honest *unknown*, never a panic.
#[must_use]
pub fn probe() -> HostInfo {
    probe_at(
        Path::new("/proc"),
        Path::new("/sys"),
        Path::new("/sys/fs/cgroup"),
    )
}

// ---- probe helpers (root-injectable for fixture tests) ----

/// Read a file to a `String`, mapping any I/O error to `None` (absent /
/// unreadable is a first-class *unknown*).
fn read_to_string_opt(path: &Path) -> Option<String> {
    let _ = path;
    None // red skeleton — the green commit fills this
}

/// Parse `MemTotal` (kB) from `/proc/meminfo` content into bytes.
fn parse_meminfo_memtotal(content: &str) -> Option<u64> {
    let _ = content;
    None // red skeleton — the green commit fills this
}

/// Count logical CPUs from `/proc/cpuinfo` content (`processor` lines).
fn parse_cpuinfo_cores(content: &str) -> Option<u32> {
    let _ = content;
    None // red skeleton — the green commit fills this
}

/// Parse cgroup-v2 `cpu.max` content (`"<quota|max> <period>"`) into
/// `Some((quota_us, period_us))` for a real limit, or `None` for `max`
/// (unlimited).
fn parse_cpu_max(content: &str) -> Option<(u64, u64)> {
    let _ = content;
    None // red skeleton — the green commit fills this
}

/// Parse cgroup-v2 `memory.max` content into bytes, or `None` for `max`
/// (unlimited).
fn parse_memory_max(content: &str) -> Option<u64> {
    let _ = content;
    None // red skeleton — the green commit fills this
}

/// Extract this process's cgroup-v2 unified path (the `0::<path>` line) from
/// `/proc/self/cgroup` content.
fn parse_self_cgroup_v2_path(content: &str) -> Option<String> {
    let _ = content;
    None // red skeleton — the green commit fills this
}

/// Resolve + read this process's cgroup-v2 `cpu.max` / `memory.max`.
fn probe_cgroup(proc_root: &Path, cgroup_root: &Path) -> CgroupLimits {
    let _ = (proc_root, cgroup_root);
    CgroupLimits::default() // red skeleton — the green commit fills this
}

/// Report whether Linux PSI is available (`/proc/pressure/cpu` presence).
fn probe_psi(proc_root: &Path) -> ProbeStatus {
    let _ = proc_root;
    ProbeStatus::NotAttempted // red skeleton — the green commit fills this
}

/// Enumerate the host's thermal-zone type names (`/sys/class/thermal`).
fn probe_thermal(sys_root: &Path) -> Option<Vec<String>> {
    let _ = sys_root;
    None // red skeleton — the green commit fills this
}

/// Assemble a [`HostInfo`] from the given filesystem roots (real in [`probe`],
/// a fixture tree in tests).
fn probe_at(proc_root: &Path, sys_root: &Path, cgroup_root: &Path) -> HostInfo {
    HostInfo {
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        cpu_cores: read_to_string_opt(&proc_root.join("cpuinfo"))
            .as_deref()
            .and_then(parse_cpuinfo_cores),
        available_parallelism: std::thread::available_parallelism()
            .ok()
            .and_then(|n| u32::try_from(n.get()).ok()),
        total_ram_bytes: read_to_string_opt(&proc_root.join("meminfo"))
            .as_deref()
            .and_then(parse_meminfo_memtotal),
        cgroup: probe_cgroup(proc_root, cgroup_root),
        psi: probe_psi(proc_root),
        thermal_sensors: probe_thermal(sys_root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Write `content` to `root/rel`, creating parent directories.
    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    // ---- pure parse helpers ----

    #[test]
    fn meminfo_memtotal_kb_to_bytes() {
        let content = "MemTotal:       16384000 kB\nMemFree:  1000 kB\n";
        assert_eq!(parse_meminfo_memtotal(content), Some(16_384_000 * 1024));
    }

    #[test]
    fn meminfo_without_memtotal_is_unknown() {
        assert_eq!(parse_meminfo_memtotal("MemFree: 10 kB\n"), None);
    }

    #[test]
    fn cpuinfo_counts_processor_lines() {
        let content = "processor\t: 0\nmodel\t: x\nprocessor\t: 1\nprocessor\t: 2\n";
        assert_eq!(parse_cpuinfo_cores(content), Some(3));
    }

    #[test]
    fn cpuinfo_with_no_processors_is_unknown() {
        assert_eq!(parse_cpuinfo_cores("model\t: x\n"), None);
    }

    #[test]
    fn cpu_max_limited_yields_quota_and_period() {
        assert_eq!(parse_cpu_max("50000 100000\n"), Some((50_000, 100_000)));
    }

    #[test]
    fn cpu_max_unlimited_is_none() {
        assert_eq!(parse_cpu_max("max 100000\n"), None);
    }

    #[test]
    fn memory_max_limited_is_bytes() {
        assert_eq!(parse_memory_max("2147483648\n"), Some(2_147_483_648));
    }

    #[test]
    fn memory_max_unlimited_is_none() {
        assert_eq!(parse_memory_max("max\n"), None);
    }

    #[test]
    fn self_cgroup_v2_path_is_the_unified_line() {
        let content = "0::/system.slice/multiview.service\n";
        assert_eq!(
            parse_self_cgroup_v2_path(content).as_deref(),
            Some("/system.slice/multiview.service")
        );
    }

    #[test]
    fn self_cgroup_without_v2_line_is_unknown() {
        // A cgroup-v1-only host has controller lines but no `0::` unified line.
        let content = "12:cpu,cpuacct:/foo\n11:memory:/foo\n";
        assert_eq!(parse_self_cgroup_v2_path(content), None);
    }

    // ---- root-injected probe orchestration ----

    #[test]
    fn probe_at_reads_a_full_linux_fixture() {
        let proc = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let cgroup = TempDir::new().unwrap();

        write_file(
            proc.path(),
            "cpuinfo",
            "processor\t: 0\nprocessor\t: 1\nprocessor\t: 2\nprocessor\t: 3\n",
        );
        write_file(proc.path(), "meminfo", "MemTotal:       8000000 kB\n");
        write_file(proc.path(), "pressure/cpu", "some avg10=0.00\n");
        write_file(proc.path(), "self/cgroup", "0::/mv.slice\n");
        write_file(cgroup.path(), "mv.slice/cpu.max", "50000 100000\n");
        write_file(cgroup.path(), "mv.slice/memory.max", "4000000000\n");
        write_file(sys.path(), "class/thermal/thermal_zone0/type", "x86_pkg_temp\n");

        let info = probe_at(proc.path(), sys.path(), cgroup.path());

        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert_eq!(info.cpu_cores, Some(4));
        assert_eq!(info.total_ram_bytes, Some(8_000_000 * 1024));
        assert_eq!(info.cgroup.probe, ProbeStatus::Succeeded);
        assert_eq!(info.cgroup.cpu_max_quota_us, Some(50_000));
        assert_eq!(info.cgroup.cpu_max_period_us, Some(100_000));
        assert_eq!(info.cgroup.memory_max_bytes, Some(4_000_000_000));
        assert_eq!(info.psi, ProbeStatus::Succeeded);
        assert_eq!(info.thermal_sensors, Some(vec!["x86_pkg_temp".to_owned()]));
    }

    #[test]
    fn probe_at_bare_fixture_is_honestly_unknown() {
        // Empty roots — nothing present. Every /proc-sourced field is unknown,
        // and the presence probes report confirmed-absent, never a fabrication.
        let proc = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let cgroup = TempDir::new().unwrap();

        let info = probe_at(proc.path(), sys.path(), cgroup.path());

        assert!(!info.os.is_empty());
        assert!(!info.arch.is_empty());
        assert_eq!(info.cpu_cores, None);
        assert_eq!(info.total_ram_bytes, None);
        // No `/proc/self/cgroup` v2 line ⇒ this host is not cgroup-v2: Unsupported.
        assert_eq!(info.cgroup.probe, ProbeStatus::Unsupported);
        assert_eq!(info.cgroup.cpu_max_quota_us, None);
        assert_eq!(info.cgroup.memory_max_bytes, None);
        // No `/proc/pressure` ⇒ PSI confirmed-absent (not "not attempted").
        assert_eq!(info.psi, ProbeStatus::Unsupported);
        // No `/sys/class/thermal` ⇒ not probed (distinct from probed-but-empty).
        assert_eq!(info.thermal_sensors, None);
    }

    #[test]
    fn thermal_probed_but_empty_is_some_empty_not_none() {
        let proc = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let cgroup = TempDir::new().unwrap();
        // The thermal directory exists but holds no zones.
        fs::create_dir_all(sys.path().join("class/thermal")).unwrap();

        let info = probe_at(proc.path(), sys.path(), cgroup.path());

        assert_eq!(info.thermal_sensors, Some(Vec::new()));
    }

    #[test]
    fn cgroup_unlimited_is_succeeded_with_no_numeric_limit() {
        // A cgroup-v2 process with no bandwidth/memory cap: `max`. This is
        // "probed, no limit" (Succeeded + None), distinct from "unprobed"
        // (Unsupported).
        let proc = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let cgroup = TempDir::new().unwrap();
        write_file(proc.path(), "self/cgroup", "0::/\n");
        write_file(cgroup.path(), "cpu.max", "max 100000\n");
        write_file(cgroup.path(), "memory.max", "max\n");

        let info = probe_at(proc.path(), sys.path(), cgroup.path());

        assert_eq!(info.cgroup.probe, ProbeStatus::Succeeded);
        assert_eq!(info.cgroup.cpu_max_quota_us, None);
        assert_eq!(info.cgroup.cpu_max_period_us, None);
        assert_eq!(info.cgroup.memory_max_bytes, None);
    }

    #[test]
    fn psi_present_when_pressure_cpu_exists() {
        let proc = TempDir::new().unwrap();
        let sys = TempDir::new().unwrap();
        let cgroup = TempDir::new().unwrap();
        write_file(proc.path(), "pressure/cpu", "some avg10=0.00\n");

        let info = probe_at(proc.path(), sys.path(), cgroup.path());

        assert_eq!(info.psi, ProbeStatus::Succeeded);
    }
}
