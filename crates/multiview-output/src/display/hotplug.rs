//! Connector hotplug detection (DEV-B5 / ADR-0045): the deployment-level
//! wiring that turns kernel `drm` uevents (or, where they cannot be
//! received, periodic `force_probe` polling) into display-sink re-probe
//! requests.
//!
//! ## Design (display-out §10)
//!
//! * **Kernel netlink group, never udevd.** Kernel kobject uevents are
//!   delivered to any netns owned by the initial user namespace, so a normal
//!   **rootful** container receives them with no host udev plumbing; the
//!   udevd-processed stream (its own multicast group, `libudev`-magic
//!   framing) genuinely does not reach a container and is deliberately not
//!   used. Messages carrying the `libudev` magic are rejected by the parser
//!   as defense in depth.
//! * **Rootless fallback = polling.** A user-ns-owned netns gets no kernel
//!   uevents; [`select_mode`] then degrades to [`HotplugMode::Polling`] —
//!   periodic re-probe requests at the configured cadence (2–5 s class; the
//!   kernel itself polls non-HPD connectors at 10 s).
//! * **The monitor only sets flags.** [`HotplugMonitor`] never touches the
//!   DRM device: it parses, debounces, and sets each sink's [`ReprobeFlag`].
//!   The flip-loop thread — the device's owner — notices its flag between
//!   flips and performs the actual probe (and, on a disconnect→reconnect
//!   transition, the TEST_ONLY re-validate + one re-light modeset). Nothing
//!   here can stall the engine: the engine does not participate at all
//!   (invariants #1 + #10).
//!
//! Everything in this file is pure Rust, always compiled, and CI-tested over
//! scripted seams; the real `NETLINK_KOBJECT_UEVENT` socket lives in
//! [`super::kms`] behind the `display-kms` feature.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One parsed kernel uevent: the `action@devpath` header plus the
/// `KEY=VALUE` properties.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Uevent {
    /// The header action (`add`/`remove`/`change`/…).
    pub action: String,
    /// The header device path.
    pub devpath: String,
    /// The `SUBSYSTEM` property, when present.
    pub subsystem: Option<String>,
    /// Whether the `HOTPLUG=1` property is present (drm connector-status
    /// changes carry it; other drm uevents do not).
    pub hotplug: bool,
}

impl Uevent {
    /// Whether this event is a display connector-status change: subsystem
    /// `drm`, action `change`, and the `HOTPLUG=1` marker.
    #[must_use]
    pub fn is_display_hotplug(&self) -> bool {
        self.subsystem.as_deref() == Some("drm") && self.action == "change" && self.hotplug
    }
}

/// The `libudev` magic prefixing udevd's processed-stream messages — never
/// the kernel's. Rejected outright (the design listens to the kernel group
/// only).
const UDEV_MAGIC: &[u8] = b"libudev\0";

/// Parse one kernel uevent datagram (`action@devpath\0KEY=VALUE\0…`).
/// Returns [`None`] for udevd-framed messages and anything malformed.
#[must_use]
pub fn parse_uevent(datagram: &[u8]) -> Option<Uevent> {
    if datagram.starts_with(UDEV_MAGIC) {
        return None;
    }
    let mut parts = datagram.split(|b| *b == 0);
    let header = std::str::from_utf8(parts.next()?).ok()?;
    let (action, devpath) = header.split_once('@')?;
    if action.is_empty() || devpath.is_empty() {
        return None;
    }
    let mut properties: HashMap<&str, &str> = HashMap::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(part) else {
            continue;
        };
        if let Some((key, value)) = text.split_once('=') {
            properties.insert(key, value);
        }
    }
    Some(Uevent {
        action: action.to_owned(),
        devpath: devpath.to_owned(),
        subsystem: properties.get("SUBSYSTEM").map(|s| (*s).to_owned()),
        hotplug: properties.get("HOTPLUG").copied() == Some("1"),
    })
}

/// A suppression-window debounce over injected nanosecond timestamps: the
/// first event fires immediately; further events inside `window` are
/// swallowed (a replug burst is one re-probe; re-probing is idempotent, so a
/// burst spanning the boundary firing twice is harmless).
#[derive(Debug)]
pub struct ReprobeDebounce {
    /// The suppression window in nanoseconds.
    window_ns: u128,
    /// When the last fire happened, if any.
    last_fire_ns: Option<u64>,
}

impl ReprobeDebounce {
    /// A debounce with the given suppression window.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window_ns: window.as_nanos(),
            last_fire_ns: None,
        }
    }

    /// Observe an event at `now_ns`; `true` means "fire the re-probe now".
    pub fn observe(&mut self, now_ns: u64) -> bool {
        if let Some(last) = self.last_fire_ns {
            if u128::from(now_ns.saturating_sub(last)) < self.window_ns {
                return false;
            }
        }
        self.last_fire_ns = Some(now_ns);
        true
    }
}

/// What the monitor needs from a uevent socket, so the loop is CI-testable
/// over a scripted source; the real `NETLINK_KOBJECT_UEVENT` socket
/// implements this in [`super::kms`] (feature `display-kms`).
pub trait UeventSource: Send {
    /// Wait up to `timeout` for one datagram. `Ok(None)` = nothing arrived
    /// (the normal idle case).
    ///
    /// # Errors
    ///
    /// A human-readable reason on an unrecoverable socket failure; the
    /// monitor logs it and keeps going (transient failures must not kill
    /// hotplug for the rest of the run).
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String>;
}

/// A shared re-probe request bit: the monitor (or any caller) sets it; the
/// owning flip loop takes it between flips.
#[derive(Debug, Clone, Default)]
pub struct ReprobeFlag {
    /// The shared request bit.
    requested: Arc<AtomicBool>,
}

impl ReprobeFlag {
    /// A fresh, un-requested flag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a re-probe (idempotent).
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    /// Whether a re-probe is currently requested (does not consume it).
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Consume a pending request, returning whether one was pending.
    #[must_use]
    pub fn take(&self) -> bool {
        self.requested.swap(false, Ordering::AcqRel)
    }
}

/// How hotplug is detected for a run.
///
/// Deliberately **not** `#[non_exhaustive]`: this is a closed two-mode union
/// (event-driven or polling), and a hypothetical third mode must be a
/// compile error for every consumer until it is handled.
#[derive(Debug)]
pub enum HotplugMode<S: UeventSource> {
    /// Event-driven: the kernel netlink uevent group (bare metal and rootful
    /// containers).
    Netlink(S),
    /// Periodic `force_probe` polling at the given cadence (rootless
    /// containers — no kernel uevent delivery).
    Polling(Duration),
}

/// Choose the hotplug mode: a working netlink source wins; otherwise degrade
/// to polling at `poll`, returning the netlink failure reason so the caller
/// can log the degradation once.
#[must_use]
pub fn select_mode<S: UeventSource>(
    netlink: Result<S, String>,
    poll: Duration,
) -> (HotplugMode<S>, Option<String>) {
    match netlink {
        Ok(source) => (HotplugMode::Netlink(source), None),
        Err(reason) => (HotplugMode::Polling(poll), Some(reason)),
    }
}

/// How long the netlink loop waits per `recv` before re-checking the stop
/// flag (also bounds stop latency).
const NETLINK_RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// How long the polling loop sleeps per slice while waiting out its cadence
/// (bounds stop latency under multi-second cadences).
const POLL_SLICE: Duration = Duration::from_millis(100);

/// The hotplug monitor thread: consumes the [`HotplugMode`], debounces, and
/// fans re-probe requests to every registered sink flag. Stopping (or
/// dropping) the handle stops and joins the thread.
#[derive(Debug)]
pub struct HotplugMonitor {
    /// The thread's stop request.
    stop: Arc<AtomicBool>,
    /// The running thread (taken on stop/drop).
    thread: Option<JoinHandle<()>>,
}

impl HotplugMonitor {
    /// Start the monitor over `mode`, fanning debounced re-probe requests to
    /// `flags` (one per running display sink). `debounce` is the burst
    /// suppression window (kernel replug bursts emit several uevents within
    /// milliseconds).
    #[must_use]
    pub fn start<S: UeventSource + 'static>(
        mode: HotplugMode<S>,
        flags: Vec<ReprobeFlag>,
        debounce: Duration,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let spawned = std::thread::Builder::new()
            .name("display-hotplug".to_owned())
            .spawn(move || match mode {
                HotplugMode::Netlink(source) => {
                    netlink_loop(source, &flags, debounce, &thread_stop);
                }
                HotplugMode::Polling(interval) => {
                    polling_loop(interval, &flags, &thread_stop);
                }
            });
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(e) => {
                // Hotplug is an aid, not a dependency: the run continues with
                // startup-probed connectors only.
                tracing::warn!(error = %e, "could not spawn the display hotplug monitor");
                None
            }
        };
        Self { stop, thread }
    }

    /// Stop the monitor and join its thread.
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("display hotplug monitor thread panicked during the run");
            }
        }
    }
}

impl Drop for HotplugMonitor {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The event-driven loop: bounded receive → parse → drm-hotplug filter →
/// debounce → fan to every sink flag.
fn netlink_loop<S: UeventSource>(
    mut source: S,
    flags: &[ReprobeFlag],
    debounce: Duration,
    stop: &AtomicBool,
) {
    let mut gate = ReprobeDebounce::new(debounce);
    let epoch = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let datagram = match source.recv_timeout(NETLINK_RECV_TIMEOUT) {
            Ok(Some(datagram)) => datagram,
            Ok(None) => continue,
            Err(reason) => {
                tracing::warn!(reason, "display hotplug uevent receive failed");
                // A receive error can mean the kernel DROPPED datagrams (an
                // ENOBUFS queue overrun) — possibly the one drm hotplug
                // uevent — and event mode has no periodic re-probe, so a
                // swallowed event would otherwise be lost for the rest of
                // the run. Treat the error as "connector state may have
                // changed unobserved": request a recovery re-probe on every
                // sink, through the same debounce gate (a persistent error
                // storm collapses to one request per window) and the same
                // idempotent flag the relight path consumes.
                let now_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
                if gate.observe(now_ns) {
                    for flag in flags {
                        flag.request();
                    }
                }
                // Keep going: a transient socket failure must not end hotplug
                // for the rest of the run. The bounded wait prevents a hot
                // error loop from spinning.
                std::thread::sleep(NETLINK_RECV_TIMEOUT);
                continue;
            }
        };
        let Some(event) = parse_uevent(&datagram) else {
            continue;
        };
        if !event.is_display_hotplug() {
            continue;
        }
        let now_ns = u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if !gate.observe(now_ns) {
            continue;
        }
        tracing::info!(
            devpath = %event.devpath,
            "drm hotplug uevent: requesting a connector re-probe on every display sink"
        );
        for flag in flags {
            flag.request();
        }
    }
}

/// The polling fallback loop: every `interval`, request a re-probe on every
/// sink (the flip loop's probe IS the `force_probe` — a probe from userspace
/// forces connector detection).
fn polling_loop(interval: Duration, flags: &[ReprobeFlag], stop: &AtomicBool) {
    let mut next = Instant::now() + interval;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(POLL_SLICE.min(interval));
        if Instant::now() < next {
            continue;
        }
        next += interval;
        for flag in flags {
            flag.request();
        }
    }
}
