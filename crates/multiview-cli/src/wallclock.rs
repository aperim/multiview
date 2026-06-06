//! The **real wall-clock time-of-day source** for the on-screen clock overlay
//! (feature `overlay`).
//!
//! The on-screen clock is a **display** concern: it must show the operator the
//! *actual current time of day*, read live from the operating-system clock, and
//! it must **not** influence output pacing in any way. The protected output core
//! keeps its own fixed-cadence monotonic tick clock (`out_pts = f(tick)`,
//! invariant #1); this module only feeds the *displayed* clock face a real
//! instant sampled at draw time.
//!
//! ## How the OS clock is read (no NTP reimplementation)
//!
//! [`SystemWallClock::unix_seconds`] reads [`std::time::SystemTime::now`] and
//! takes its whole-second offset from [`std::time::UNIX_EPOCH`]. On every
//! platform Multiview targets (Linux + macOS) that *is* `CLOCK_REALTIME` — the
//! wall-clock the host disciplines via NTP/PTP. We **read** that clock; we never
//! reimplement NTP. The host in deployment runs stratum-1 NTP, so the displayed
//! time is the disciplined time-of-day. This is also the **anti-drift** property:
//! because the displayed time is re-sampled from the live OS clock at every bake
//! (rather than derived from the monotonic output-tick counter), it can never
//! drift away from real wall-time over a long run — it tracks the NTP-disciplined
//! OS clock exactly.
//!
//! ## Reference status (honest about what we can detect)
//!
//! The overlay model ([`multiview_overlay::clock::TimeRef`]) carries a
//! [`RefSource`] + [`RefStatus`] so the clock can show an accessible
//! text-and-glyph reference badge (never colour alone). This source reports
//! [`RefSource::System`] (label `SYS`): *the host's system clock, which the
//! deployment disciplines via NTP*. For the MVP the status is **configured /
//! assumed** rather than measured: it defaults to [`RefStatus::Locked`] on the
//! basis that the deployment host is NTP-disciplined. True kernel lock-state
//! detection (Linux `adjtimex` / macOS `ntp_adjtime`) is platform-specific and
//! needs `unsafe` FFI — which `multiview-cli` forbids ([`forbid(unsafe_code)`]) — so
//! it is deferred to a tracked follow-up. Operators who know their host is *not*
//! disciplined can construct the source with [`RefStatus::Freerun`] to be honest
//! about it.
//!
//! ## Injectable seam (testability + anti-drift proof)
//!
//! The [`WallClock`] trait is the injectable seam: production wires
//! [`SystemWallClock`] (the real OS clock); tests wire a fake whose returned
//! instant they control, so they can assert the displayed time-of-day tracks the
//! *injected* clock (advancing it by `N` seconds advances the displayed time by
//! `N` seconds) regardless of the output-tick index — the anti-drift contract.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use multiview_overlay::clock::{RefSource, RefStatus, TimeRef, WallTime};

/// The injectable source of the **current** wall-clock instant for the displayed
/// time-of-day, plus the timing-reference descriptor to surface on the clock.
///
/// Production uses [`SystemWallClock`] (reads the OS `CLOCK_REALTIME`); tests use
/// a fake whose instant they control. Implementors must be `Send + Sync` so a
/// [`WallClockSource`] can be shared across the off-hot-path bake.
pub trait WallClock: Send + Sync {
    /// The current time as whole Unix seconds (UTC) — i.e. `CLOCK_REALTIME`
    /// truncated to whole seconds, which is all a clock face needs.
    fn unix_seconds(&self) -> i64;

    /// The timing-reference descriptor to display alongside the clock (source +
    /// lock status). See the module docs for what the status means for the MVP.
    fn reference(&self) -> TimeRef;
}

/// The **real** wall-clock: reads the operating-system `CLOCK_REALTIME` via
/// [`std::time::SystemTime`]. Cross-platform and `unsafe`-free.
///
/// Carries the [`RefStatus`] to *report* for the system reference. It defaults to
/// [`RefStatus::Locked`] (`SystemWallClock::default`), documenting the assumption
/// that the deployment host disciplines its clock via NTP; construct with
/// [`SystemWallClock::with_status`] to report a different status (e.g.
/// [`RefStatus::Freerun`] on a host known not to be disciplined).
#[derive(Debug, Clone, Copy)]
pub struct SystemWallClock {
    /// The reference status to report (see the type docs). The source is always
    /// [`RefSource::System`] — this clock reads the host's system clock.
    status: RefStatus,
}

impl SystemWallClock {
    /// A system clock reporting `status` for its reference.
    #[must_use]
    pub const fn with_status(status: RefStatus) -> Self {
        Self { status }
    }
}

impl Default for SystemWallClock {
    /// Defaults to [`RefStatus::Locked`]: the deployment host is assumed
    /// NTP-disciplined (see the module docs — true kernel lock detection is a
    /// deferred follow-up).
    fn default() -> Self {
        Self {
            status: RefStatus::Locked,
        }
    }
}

impl WallClock for SystemWallClock {
    /// Read `CLOCK_REALTIME` (whole seconds since the Unix epoch).
    ///
    /// A clock somehow set before 1970 (`now < UNIX_EPOCH`) yields a negative
    /// second count via the error arm — [`WallTime`] is happy with a negative
    /// Unix-seconds value, and `with_offset` resolves it with a Euclidean
    /// remainder, so the face still reads a valid time-of-day.
    fn unix_seconds(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_secs())
                .unwrap_or(i64::MAX)
                .saturating_neg(),
        }
    }

    fn reference(&self) -> TimeRef {
        TimeRef::new(RefSource::System, self.status)
    }
}

/// A shareable handle to a [`WallClock`]: the injectable seam the overlay baker
/// holds. Wraps the chosen clock in an [`Arc`] so it can be cloned cheaply into
/// the off-hot-path bake without copying the underlying source.
///
/// [`WallClockSource::now`] samples the live instant **at the moment of the
/// call** (the anti-drift property — the displayed time-of-day is sampled fresh,
/// never derived from the output-tick counter).
#[derive(Clone)]
pub struct WallClockSource {
    clock: Arc<dyn WallClock>,
}

impl WallClockSource {
    /// Wrap `clock` as a shareable source.
    #[must_use]
    pub fn new(clock: Arc<dyn WallClock>) -> Self {
        Self { clock }
    }

    /// A source backed by the real OS clock ([`SystemWallClock`]) reporting the
    /// default (NTP-disciplined ⇒ [`RefStatus::Locked`]) reference status.
    #[must_use]
    pub fn system() -> Self {
        Self::new(Arc::new(SystemWallClock::default()))
    }

    /// Sample the **current** wall-clock instant as a [`WallTime`] (whole Unix
    /// seconds). Called at draw time so the displayed time-of-day tracks the live
    /// OS clock (anti-drift), independent of the output-tick counter.
    #[must_use]
    pub fn now(&self) -> WallTime {
        WallTime::from_unix_seconds(self.clock.unix_seconds())
    }

    /// The timing-reference descriptor to display alongside the clock.
    #[must_use]
    pub fn reference(&self) -> TimeRef {
        self.clock.reference()
    }
}

impl std::fmt::Debug for WallClockSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallClockSource")
            .field("reference", &self.clock.reference())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::sync::atomic::{AtomicI64, Ordering};

    use super::*;

    /// A fake wall-clock whose instant the test controls. Backed by an atomic so a
    /// single shared instance can be advanced between samples (the anti-drift
    /// proof advances the same injected clock the baker holds).
    struct FakeClock {
        secs: AtomicI64,
        status: RefStatus,
    }

    impl FakeClock {
        fn new(secs: i64, status: RefStatus) -> Self {
            Self {
                secs: AtomicI64::new(secs),
                status,
            }
        }

        fn advance(&self, by: i64) {
            self.secs.fetch_add(by, Ordering::SeqCst);
        }
    }

    impl WallClock for FakeClock {
        fn unix_seconds(&self) -> i64 {
            self.secs.load(Ordering::SeqCst)
        }

        fn reference(&self) -> TimeRef {
            TimeRef::new(RefSource::System, self.status)
        }
    }

    #[test]
    fn system_clock_reads_a_plausible_recent_instant() {
        // The real OS clock must read a recent, plausible instant — well after
        // 2020-01-01 (1_577_836_800) and before 2100. This proves we are reading
        // CLOCK_REALTIME, not returning a fixed zero.
        let clock = SystemWallClock::default();
        let secs = clock.unix_seconds();
        assert!(
            secs > 1_577_836_800,
            "system clock {secs} is before 2020 — not reading CLOCK_REALTIME?"
        );
        assert!(secs < 4_102_444_800, "system clock {secs} is after 2100");
    }

    #[test]
    fn system_clock_default_reports_sys_locked() {
        // The default real clock reports a SYS reference assumed locked (the host
        // disciplines its clock via NTP).
        let r = SystemWallClock::default().reference();
        assert_eq!(r.source, RefSource::System);
        assert_eq!(r.status, RefStatus::Locked);
        assert_eq!(r.status_text(), "SYS locked");
    }

    #[test]
    fn system_clock_can_report_freerun() {
        // A host known NOT to be disciplined can report freerun honestly.
        let r = SystemWallClock::with_status(RefStatus::Freerun).reference();
        assert_eq!(r.status_text(), "SYS freerun");
    }

    #[test]
    fn source_now_reflects_the_injected_instant() {
        // A fake clock set to a known instant makes `now()` return exactly that
        // WallTime — the injectable seam works end to end.
        let fake = Arc::new(FakeClock::new(1_780_000_000, RefStatus::Locked));
        let src = WallClockSource::new(fake);
        assert_eq!(src.now().unix_seconds(), 1_780_000_000);
        assert_eq!(src.reference().status_text(), "SYS locked");
    }

    #[test]
    fn source_now_tracks_the_advancing_injected_clock() {
        // Advancing the injected clock advances `now()` by the same amount — the
        // source samples the LIVE clock each call, never caches a first reading.
        let fake = Arc::new(FakeClock::new(1_000_000, RefStatus::Locked));
        let dynamic: Arc<dyn WallClock> = fake.clone();
        let src = WallClockSource::new(dynamic);
        let t0 = src.now().unix_seconds();
        fake.advance(37);
        let t1 = src.now().unix_seconds();
        assert_eq!(t1 - t0, 37, "now() must track the advancing injected clock");
    }

    // ENG-3b — the measured system reference: `ref_from_query` reads the kernel
    // NTP discipline (via the engine `sysref` classifier) and maps it onto the
    // overlay badge. Driven here by a FAKE `NtpQuery` so the mapping + the
    // read-unavailable fallback are tested without the live `adjtimex` syscall.
    use multiview_engine::ptp::LockState;
    use multiview_engine::sysref::{
        NtpClockState, NtpQuery, NtpReading, NtpStatusFlags, SystemRefConfig,
    };

    use super::ref_from_query;

    struct FakeNtp(Option<NtpReading>);
    impl NtpQuery for FakeNtp {
        fn read(&mut self) -> Option<NtpReading> {
            self.0
        }
    }

    fn synced_reading(est_error_ns: i64) -> NtpReading {
        NtpReading {
            status: NtpStatusFlags::from_bits(0), // synchronised (no STA_UNSYNC)
            clock_state: NtpClockState::Ok,
            est_error_ns,
            max_error_ns: est_error_ns,
            offset_ns: 0,
        }
    }

    #[test]
    fn measured_ref_reports_sys_locked_when_synced_within_tolerance() {
        // A synchronised kernel clock with a tiny estimated error reads LOCKED on
        // the SYS badge (the default tolerance is 100 us).
        let mut q = FakeNtp(Some(synced_reading(1_000)));
        let r = ref_from_query(&mut q, &SystemRefConfig::default());
        assert_eq!(r.source, RefSource::System);
        assert_eq!(r.status, RefStatus::Locked);
    }

    #[test]
    fn measured_ref_reports_holdover_when_synced_but_over_tolerance() {
        // Synchronised but the estimated error is well past tolerance ⇒ HOLDOVER.
        let mut q = FakeNtp(Some(synced_reading(10_000_000)));
        let r = ref_from_query(&mut q, &SystemRefConfig::default());
        assert_eq!(r.status, RefStatus::Holdover);
    }

    #[test]
    fn measured_ref_reports_freerun_when_unsynchronized() {
        // STA_UNSYNC set ⇒ the clock is free-running, honestly reported.
        let mut reading = synced_reading(0);
        reading.status = NtpStatusFlags::from_bits(NtpStatusFlags::STA_UNSYNC);
        let r = ref_from_query(&mut FakeNtp(Some(reading)), &SystemRefConfig::default());
        assert_eq!(r.status, RefStatus::Freerun);
    }

    #[test]
    fn measured_ref_falls_back_to_assumed_when_read_unavailable() {
        // No reading (denied / non-Linux): honest fallback to the configured
        // assumed state — Locked on a host known to be disciplined, Freerun if not.
        let locked = SystemRefConfig {
            est_error_tolerance_ns: 100_000,
            assumed_when_unavailable: LockState::Locked,
        };
        let freerun = SystemRefConfig {
            est_error_tolerance_ns: 100_000,
            assumed_when_unavailable: LockState::Freerun,
        };
        assert_eq!(
            ref_from_query(&mut FakeNtp(None), &locked).status,
            RefStatus::Locked
        );
        assert_eq!(
            ref_from_query(&mut FakeNtp(None), &freerun).status,
            RefStatus::Freerun
        );
    }
}
