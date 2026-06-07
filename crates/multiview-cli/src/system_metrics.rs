//! The off-hot-path **system-metrics poller** (ADR-0017): a small task that
//! samples whole-system CPU + host memory + per-GPU load at ~1.3 Hz and
//! **pushes** a [`multiview_events::SystemMetrics`] onto the engine's outbound
//! event stream, so the management UI's footer lights up with live data.
//!
//! Per the realtime-api contract these metrics are **pushed** over the realtime
//! stream, never polled by the UI (a REST poll of a per-second value is the
//! wrong shape). The task publishes through the engine's drop-oldest
//! [`EnginePublisher`](multiview_engine::EnginePublisher) event stream, whose
//! `publish` never awaits or blocks a slow subscriber (invariant #10): a stalled
//! UI simply skips samples; it can never back-pressure this task and this task
//! can never back-pressure the engine.
//!
//! The sampling → assembly step is the pure, sync, dependency-light
//! [`assemble_metrics`] function (no async, no sockets, no GPU), so the full
//! `DeviceLoad` → `GpuMetrics` mapping is unit-testable on any host. The live
//! task ([`spawn`]) wires it to the std-only [`CpuSampler`](multiview_telemetry::CpuSampler)
//! (Linux `/proc/stat`), a pure `/proc/meminfo` reader, and an injected
//! [`LoadSource`](multiview_hal::LoadSource) (the NVML poller when the `cuda`
//! feature is on, else the always-compiled
//! [`NullLoadPoller`](multiview_hal::NullLoadPoller)).

use std::sync::Arc;
use std::time::Duration;

use multiview_control::EngineStateSnapshot;
use multiview_engine::{EnginePublisher, StopSignal};
use multiview_events::{Event, GpuMetrics, GpuVendor, SystemMetrics};
use multiview_hal::{DeviceLoad, LoadSource, Vendor};

/// The system-metrics sampling period: 750 ms ≈ 1.3 Hz — inside the ADR-0017
/// ~1-4 Hz envelope and matching the "conflated, a couple of samples a second"
/// cadence the footer expects. A const so the cadence is named once and the
/// published `sampled_hz` is derived from it.
pub const SAMPLE_PERIOD: Duration = Duration::from_millis(750);

/// The effective wire cadence reported in [`SystemMetrics::sampled_hz`], derived
/// from [`SAMPLE_PERIOD`] (rounded to the nearest whole Hz). 750 ms → 1 Hz.
#[must_use]
pub fn sampled_hz() -> u32 {
    let millis = SAMPLE_PERIOD.as_millis();
    if millis == 0 {
        return 0;
    }
    // round(1000 / millis); millis is a small positive value here.
    let hz = (1000 + millis / 2) / millis;
    u32::try_from(hz).unwrap_or(u32::MAX)
}

/// Map a hal [`Vendor`] to the wire [`GpuVendor`] the footer renders.
///
/// Both enums are `#[non_exhaustive]`; an unclassified future hal vendor maps to
/// [`GpuVendor::Other`] rather than guessing or panicking.
#[must_use]
pub fn map_vendor(vendor: Vendor) -> GpuVendor {
    match vendor {
        Vendor::Nvidia => GpuVendor::Nvidia,
        Vendor::Intel => GpuVendor::Intel,
        Vendor::Amd => GpuVendor::Amd,
        Vendor::Apple => GpuVendor::Apple,
        _ => GpuVendor::Other,
    }
}

/// Map one live [`DeviceLoad`] snapshot to the wire [`GpuMetrics`] sample.
///
/// The optional per-engine signals (`encoder_util`, `decoder_util`,
/// `encoder_sessions`) carry straight through as `Option` — an unknown stays
/// `None` (the "n/a, never a false zero" contract). The wire's **non-optional**
/// fields (`compute_util`, `mem_used_bytes`, `mem_total_bytes`) have no `None`
/// representation, so an unknown collapses to `0` there; the compute term
/// prefers the dedicated compute-queue fraction over the 3D-engine one
/// ([`DeviceLoad::effective_compute_frac`]). `encoder_session_ceiling` is not in
/// a `DeviceLoad` (it is a per-system discovery, not a per-device sample), so it
/// is honestly `None` here.
#[must_use]
pub fn map_gpu(load: &DeviceLoad) -> GpuMetrics {
    GpuMetrics {
        id: load.device_id.stable_id().to_owned(),
        vendor: map_vendor(load.device_id.vendor()),
        name: None,
        compute_util: load.effective_compute_frac().unwrap_or(0.0),
        mem_used_bytes: load.vram_used_bytes.unwrap_or(0),
        mem_total_bytes: load.vram_total_bytes.unwrap_or(0),
        encoder_util: load.enc_util_frac,
        decoder_util: load.dec_util_frac,
        encoder_sessions: load.nvenc_session_count,
        encoder_session_ceiling: None,
    }
}

/// Assemble a [`SystemMetrics`] wire sample from already-sampled inputs — the
/// **pure** core of the poller (no async, no I/O, no GPU).
///
/// * `cpu` is the whole-system busy fraction (`0.0..=1.0`).
/// * `mem` is the host `(used, total)` byte pair when known, else `None` (both
///   `mem_*` fields then stay absent — honest unknown).
/// * `loads` are the live per-GPU snapshots; an empty slice yields an empty
///   `gpus` list (a GPU-free host), never a fabricated device.
/// * `fps` is the aggregate program output rate when running, else `None` (never
///   fabricated).
/// * `hz` is the effective wire cadence to stamp.
#[must_use]
pub fn assemble_metrics(
    cpu: f32,
    mem: Option<(u64, u64)>,
    loads: &[DeviceLoad],
    fps: Option<f32>,
    hz: u32,
) -> SystemMetrics {
    let (mem_used_bytes, mem_total_bytes) = match mem {
        Some((used, total)) => (Some(used), Some(total)),
        None => (None, None),
    };
    SystemMetrics {
        cpu_util: cpu.clamp(0.0, 1.0),
        mem_used_bytes,
        mem_total_bytes,
        gpus: loads.iter().map(map_gpu).collect(),
        program_fps: fps,
        sampled_hz: hz,
    }
}

/// Read the host's `(used, total)` memory in bytes from Linux `/proc/meminfo`,
/// pure std (no `sysinfo`/native dep).
///
/// `used = MemTotal - MemAvailable` (the kernel's own availability estimate,
/// matching what a user sees as "in use"). Returns `None` cleanly when the file
/// is absent/unreadable (non-Linux) or either field is missing — unknown, never
/// a fabricated value and never a panic.
#[must_use]
pub fn read_proc_meminfo() -> Option<(u64, u64)> {
    let contents = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&contents)
}

/// Parse a `/proc/meminfo` body into a host `(used, total)` byte pair.
///
/// The file reports `MemTotal:` and `MemAvailable:` as kibibyte counts
/// (`<label>:\t<value> kB`). `used = total - available`, saturating at `0`.
/// Returns `None` if either line is absent or unparsable.
#[must_use]
fn parse_meminfo(body: &str) -> Option<(u64, u64)> {
    let total_kib = find_meminfo_kib(body, "MemTotal:")?;
    let avail_kib = find_meminfo_kib(body, "MemAvailable:")?;
    let total = total_kib.saturating_mul(1024);
    let avail = avail_kib.saturating_mul(1024);
    let used = total.saturating_sub(avail);
    Some((used, total))
}

/// Find a `/proc/meminfo` `<label> <value> kB` line and return its kibibyte
/// value. `None` if the label is absent or the value does not parse.
#[must_use]
fn find_meminfo_kib(body: &str, label: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Select the per-GPU [`LoadSource`] for this build + host.
///
/// With the `cuda` feature on, this tries the runtime-loaded NVML poller and
/// uses it when an NVIDIA driver/device is present; on a host with no NVIDIA
/// device (CI, this container) NVML init fails gracefully and it falls back to
/// the always-compiled [`NullLoadPoller`](multiview_hal::NullLoadPoller). Without
/// the feature it is always the null source. Either way the returned source is a
/// working `Box<dyn LoadSource>` that polls without blocking the engine.
#[must_use]
pub fn default_load_source() -> Box<dyn LoadSource + Send + Sync> {
    #[cfg(feature = "cuda")]
    {
        // ~4 Hz per-pass bound (the metrics task polls it far slower, at
        // SAMPLE_PERIOD); the interval only caps the cost of one NVML pass.
        let interval = multiview_hal::PollInterval::from_millis(250);
        if let Some(poller) = multiview_hal::NvmlLoadPoller::try_init(interval) {
            tracing::info!("system-metrics: NVML per-GPU load source active");
            return Box::new(poller);
        }
        tracing::debug!("system-metrics: NVML unavailable; using the no-GPU load source");
    }
    Box::new(multiview_hal::NullLoadPoller::new())
}

/// Spawn the off-hot-path system-metrics task on the current Tokio runtime.
///
/// Every [`SAMPLE_PERIOD`] the task samples the whole-system CPU
/// ([`CpuSampler`](multiview_telemetry::CpuSampler)), host memory
/// ([`read_proc_meminfo`]), and the injected per-GPU [`LoadSource`], assembles a
/// [`SystemMetrics`] via [`assemble_metrics`], and publishes
/// [`Event::SystemMetrics`] onto the engine's outbound event stream. The publish
/// is a single non-blocking drop-oldest broadcast send — it can neither block on
/// nor be back-pressured by any subscriber (invariant #10).
///
/// `fps` is the aggregate program output rate when the run exposes one, else
/// `None` (never fabricated). The task selects on `stop`: it returns promptly
/// (within one `SAMPLE_PERIOD`) once the run's [`StopSignal`] is raised, so it
/// tears down cleanly with the rest of the run. The returned [`JoinHandle`] lets
/// the caller `abort`/await it; dropping it detaches the (self-stopping) task.
#[must_use]
pub fn spawn(
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    load_source: Box<dyn LoadSource + Send + Sync>,
    stop: StopSignal,
    fps: Option<f32>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run(publisher.as_ref(), load_source.as_ref(), &stop, fps).await;
    })
}

/// The system-metrics task body: sample → assemble → publish on a fixed cadence
/// until `stop` is raised. Factored out of [`spawn`] so it is driveable directly
/// in a test with an injected publisher + load source.
pub(crate) async fn run(
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    load_source: &(dyn LoadSource + Send + Sync),
    stop: &StopSignal,
    fps: Option<f32>,
) {
    let mut cpu = multiview_telemetry::CpuSampler::new();
    let hz = sampled_hz();
    let mut ticker = tokio::time::interval(SAMPLE_PERIOD);
    // The first `tick()` resolves immediately; that primes the CPU sampler's
    // baseline (its first `sample()` returns None) without publishing a fake
    // value, and every subsequent tick is a real delta sample.
    loop {
        if stop.is_stopped() {
            return;
        }
        ticker.tick().await;
        if stop.is_stopped() {
            return;
        }
        // CPU busy fraction is a delta between two `/proc/stat` snapshots; the
        // first call primes the baseline (None) and is reported as 0.0 until a
        // real interval has elapsed — never a fabricated busy value.
        let cpu_util = cpu.sample().map_or(0.0, frac_to_f32);
        let mem = read_proc_meminfo();
        let loads = load_source.poll();
        let metrics = assemble_metrics(cpu_util, mem, &loads, fps, hz);
        // Non-blocking, drop-oldest: never awaits a subscriber (invariant #10).
        publisher.publish_event(Event::SystemMetrics(metrics));
    }
}

/// Narrow a `0.0..=1.0` `f64` CPU fraction to `f32` without an `as` cast.
///
/// The CPU sampler already clamps to the unit interval; we map it onto the
/// `2^24` integer grid (f32's integer-exactness bound), recover that count via
/// `TryFrom`/`f32::from`, and divide back — full f32 precision for a unit
/// fraction, no lossy `as`. Mirrors the same `as`-free narrowing the hal load
/// model uses for its busy fractions.
#[must_use]
fn frac_to_f32(value: f64) -> f32 {
    const SCALE: f64 = 16_777_216.0; // 2^24, exact in both f64 and f32.
    let scaled = (value.clamp(0.0, 1.0) * SCALE).round();
    if scaled <= 0.0 {
        return 0.0;
    }
    if scaled >= SCALE {
        return 1.0;
    }
    // 0 < scaled < 2^24 and integer-valued (`.round()`), so the grid count fits a
    // u32 exactly; recover it through the integer path and divide back.
    let ticks = u32::try_from(f64_trunc_to_u32_grid(scaled)).unwrap_or(0);
    f32_from_u32(ticks) / 16_777_216.0_f32
}

/// Truncate a finite, non-negative `f64` that is already `< 2^24` and
/// integer-valued to a `u64`, reading the IEEE-754 fields (no `as` cast). Exact
/// for any integer below `2^53`; our domain is `< 2^24`.
#[must_use]
fn f64_trunc_to_u32_grid(value: f64) -> u64 {
    let truncated = value.trunc();
    if truncated <= 0.0 {
        return 0;
    }
    let bits = truncated.to_bits();
    let exponent_biased = (bits >> 52) & 0x7FF;
    let mantissa = bits & 0x000F_FFFF_FFFF_FFFF;
    let Some(exponent) = exponent_biased.checked_sub(1023) else {
        return 0;
    };
    let significand = mantissa | 0x0010_0000_0000_0000; // implicit leading 1
    if exponent >= 52 {
        significand << (exponent - 52)
    } else {
        significand >> (52 - exponent)
    }
}

/// Exact `u32 -> f32` for values `<= 2^24` (the unit-fraction grid), composing
/// two `u16` halves each lossless via `f32::from` (no `as` cast).
#[must_use]
fn f32_from_u32(value: u32) -> f32 {
    let high = u16::try_from((value >> 16) & 0xFFFF).map_or(f32::INFINITY, f32::from);
    let low = u16::try_from(value & 0xFFFF).map_or(f32::INFINITY, f32::from);
    high * 65_536.0_f32 + low
}

#[cfg(test)]
mod tests {
    // Exact-equality asserts here compare against values that are exactly
    // representable in f32 (0.0, 0.5, 1.0, the clamp bounds, and the mapped
    // fractions), so a strict `==` is the precise contract, not a tolerance.
    #![allow(clippy::float_cmp)]
    use super::*;
    use multiview_hal::DeviceId;

    fn device(vendor: Vendor, id: &str) -> DeviceId {
        DeviceId::new(vendor, id, 0)
    }

    fn nvidia_load() -> DeviceLoad {
        let mut load = DeviceLoad::unknown(device(Vendor::Nvidia, "GPU-uuid-nv"));
        load.gpu_busy_frac = Some(0.5);
        load.vram_used_bytes = Some(4_000_000_000);
        load.vram_total_bytes = Some(12_000_000_000);
        load.enc_util_frac = Some(0.25);
        load.dec_util_frac = Some(0.10);
        load.nvenc_session_count = Some(3);
        load
    }

    #[test]
    fn maps_a_full_nvidia_device_including_optional_nvenc_fields() {
        let metrics = assemble_metrics(0.42, Some((8, 16)), &[nvidia_load()], Some(59.94), 1);
        assert_eq!(metrics.cpu_util, 0.42_f32);
        assert_eq!(metrics.mem_used_bytes, Some(8));
        assert_eq!(metrics.mem_total_bytes, Some(16));
        assert_eq!(metrics.program_fps, Some(59.94_f32));
        assert_eq!(metrics.sampled_hz, 1);

        assert_eq!(metrics.gpus.len(), 1);
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(gpu.id, "GPU-uuid-nv");
        assert_eq!(gpu.vendor, GpuVendor::Nvidia);
        assert_eq!(gpu.compute_util, 0.5_f32);
        assert_eq!(gpu.mem_used_bytes, 4_000_000_000);
        assert_eq!(gpu.mem_total_bytes, 12_000_000_000);
        // Optional per-engine signals carry through.
        assert_eq!(gpu.encoder_util, Some(0.25_f32));
        assert_eq!(gpu.decoder_util, Some(0.10_f32));
        assert_eq!(gpu.encoder_sessions, Some(3));
        // A DeviceLoad carries no per-device ceiling -> honest None.
        assert_eq!(gpu.encoder_session_ceiling, None);
    }

    #[test]
    fn gpu_free_host_yields_empty_gpus_and_no_fabricated_fps() {
        let metrics = assemble_metrics(0.0, None, &[], None, 1);
        assert!(metrics.gpus.is_empty(), "no devices => empty gpus");
        assert_eq!(metrics.mem_used_bytes, None, "unknown mem stays absent");
        assert_eq!(metrics.mem_total_bytes, None);
        assert_eq!(metrics.program_fps, None, "fps must not be fabricated");
    }

    #[test]
    fn unknown_per_engine_signals_stay_none_never_false_zero() {
        // An Apple device exposes no per-engine util and no sessions: those wire
        // fields must be absent (None), not a misleading 0.0.
        let load = DeviceLoad::unknown(device(Vendor::Apple, "apple-0"));
        let metrics = assemble_metrics(0.1, None, &[load], None, 1);
        let gpu = metrics.gpus.first().expect("one gpu");
        assert_eq!(gpu.vendor, GpuVendor::Apple);
        assert_eq!(gpu.encoder_util, None);
        assert_eq!(gpu.decoder_util, None);
        assert_eq!(gpu.encoder_sessions, None);
        // The non-optional compute/mem fields have no None representation; an
        // unknown collapses to 0 there (the only representable default).
        assert_eq!(gpu.compute_util, 0.0_f32);
        assert_eq!(gpu.mem_used_bytes, 0);
        assert_eq!(gpu.mem_total_bytes, 0);
    }

    #[test]
    fn each_vendor_maps_to_its_wire_vendor() {
        assert_eq!(map_vendor(Vendor::Nvidia), GpuVendor::Nvidia);
        assert_eq!(map_vendor(Vendor::Intel), GpuVendor::Intel);
        assert_eq!(map_vendor(Vendor::Amd), GpuVendor::Amd);
        assert_eq!(map_vendor(Vendor::Apple), GpuVendor::Apple);
    }

    #[test]
    fn cpu_util_is_clamped_to_unit_interval() {
        let over = assemble_metrics(1.5, None, &[], None, 1);
        assert_eq!(over.cpu_util, 1.0_f32, "over-range CPU clamps to 1.0");
        let under = assemble_metrics(-0.3, None, &[], None, 1);
        assert_eq!(under.cpu_util, 0.0_f32, "under-range CPU clamps to 0.0");
    }

    #[test]
    fn parse_meminfo_computes_used_as_total_minus_available() {
        let body = "MemTotal:       16384 kB\nMemFree:  1000 kB\nMemAvailable:    4096 kB\n";
        let (used, total) = parse_meminfo(body).expect("both fields present");
        assert_eq!(total, 16384 * 1024);
        assert_eq!(used, (16384 - 4096) * 1024);
    }

    #[test]
    fn parse_meminfo_is_none_when_a_field_is_missing() {
        assert!(
            parse_meminfo("MemTotal: 16384 kB\n").is_none(),
            "missing MemAvailable => unknown"
        );
        assert!(parse_meminfo("").is_none());
    }

    #[test]
    fn sampled_hz_rounds_the_period_to_whole_hz() {
        // 750 ms -> round(1000/750) = round(1.33) = 1 Hz.
        assert_eq!(sampled_hz(), 1);
    }

    #[test]
    fn frac_to_f32_is_an_as_free_unit_narrow() {
        assert_eq!(frac_to_f32(0.0), 0.0_f32);
        assert_eq!(frac_to_f32(1.0), 1.0_f32);
        assert_eq!(frac_to_f32(1.5), 1.0_f32, "over-range clamps");
        assert_eq!(frac_to_f32(-0.2), 0.0_f32, "under-range clamps");
        assert!((frac_to_f32(0.5) - 0.5_f32).abs() < 1e-4);
    }

    /// The live task publishes `Event::SystemMetrics` onto the engine event
    /// stream and stops promptly when the `StopSignal` is raised — proving the
    /// publish wiring (not just the pure assembly) and the clean-shutdown
    /// contract, with an injected no-GPU source (no sockets, no real GPU).
    #[tokio::test(start_paused = true)]
    async fn task_publishes_system_metrics_then_stops_on_signal() {
        let publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>> =
            Arc::new(EnginePublisher::new(16));
        let mut sub = publisher.subscribe();
        let stop = StopSignal::new();
        let source: Box<dyn LoadSource + Send + Sync> =
            Box::new(multiview_hal::NullLoadPoller::new());

        let pub_for_task = Arc::clone(&publisher);
        let stop_for_task = stop.clone();
        let handle = tokio::spawn(async move {
            run(pub_for_task.as_ref(), source.as_ref(), &stop_for_task, None).await;
        });

        // Advance virtual time past a couple of sample periods so the interval
        // fires and the task publishes at least one metrics event.
        tokio::time::advance(SAMPLE_PERIOD * 3).await;
        tokio::task::yield_now().await;

        // A SystemMetrics event was pushed onto the stream.
        let mut saw_metrics = false;
        while let Ok(seq_event) = sub.try_recv() {
            if matches!(seq_event.event.as_ref(), Event::SystemMetrics(_)) {
                saw_metrics = true;
            }
        }
        assert!(
            saw_metrics,
            "the task must push at least one Event::SystemMetrics onto the stream"
        );

        // Raising the stop signal terminates the task promptly.
        stop.stop();
        tokio::time::advance(SAMPLE_PERIOD).await;
        let joined = tokio::time::timeout(SAMPLE_PERIOD * 4, handle).await;
        assert!(
            joined.is_ok(),
            "the task must return promptly after the StopSignal is raised"
        );
    }
}
