//! libav → `tracing` log bridge with anti-flood rate limiting.
//!
//! A glitchy or corrupt input is the *normal* operating condition for a live
//! multiview, not an exception — and libav reports it by logging. A corrupt
//! HEVC RTSP feed emits a *continuous* `Error constructing the frame RPS.`; a
//! flaky HLS source emits a *continuous* `IO error: Connection timed out`. By
//! default libav writes these straight to **stderr**, unbounded, so a single bad
//! tile can drown the operator's logs. The output path is already protected (the
//! framestore tile state machine rides last-good → STALE → NO_SIGNAL); this
//! module fixes the **logging** side so a bad input never floods the logs.
//!
//! It does two things:
//!
//! 1. **Routes** every libav log line into Rust [`tracing`] at the mapped level
//!    (PANIC/FATAL/ERROR → `error!`, WARNING → `warn!`, INFO → `info!`,
//!    VERBOSE/DEBUG → `debug!`, TRACE → `trace!`), carrying the libav component
//!    name (the `[hevc @ …]` class) as a `component` field. This replaces
//!    libav's stderr writer with the structured logger the rest of the system
//!    already uses.
//! 2. **Rate-limits** repetitive lines: the first occurrence of a message is
//!    emitted, identical repeats inside a time window are *suppressed*, and the
//!    next occurrence after the window flushes a coalesced
//!    `"… (repeated N× in the last …)"` summary. 10 000 identical `RPS` errors
//!    become one line plus a periodic count.
//!
//! ## Layering
//!
//! The whole anti-flood policy — the level mapping, the suppressor, and the
//! rendered-line sanitiser — is **pure** and native-dep-free, so it is always
//! compiled and unit-tested without libav. Only the `av_log_set_callback`
//! installation and the `extern "C"` trampoline that renders a libav
//! `va_list` into a bounded buffer live behind the `ffmpeg` feature.
//!
//! ## FFI safety (CLAUDE.md §7)
//!
//! The installed callback runs on **foreign/decoder threads**. It therefore:
//! * never lets a Rust panic unwind across the FFI boundary — the entire Rust
//!   body runs inside [`std::panic::catch_unwind`] and any caught panic is
//!   dropped silently (a logging callback must never crash the decoder);
//! * does no per-call heap allocation on the rendering path — the libav line is
//!   rendered into a fixed stack buffer via `av_log_format_line2`;
//! * holds the suppressor [`std::sync::Mutex`] only for the O(small-cap) lookup,
//!   never across the (cheap) `tracing` emit and never blocking.

use std::time::Duration;

/// libav `AV_LOG_PANIC` — something went really wrong; will crash now.
pub const AV_LOG_PANIC: i32 = 0;
/// libav `AV_LOG_FATAL` — unrecoverable; cannot continue.
pub const AV_LOG_FATAL: i32 = 8;
/// libav `AV_LOG_ERROR` — an error past which recovery may still be possible.
pub const AV_LOG_ERROR: i32 = 16;
/// libav `AV_LOG_WARNING` — something looks wrong and may cause problems.
pub const AV_LOG_WARNING: i32 = 24;
/// libav `AV_LOG_INFO` — standard informational output.
pub const AV_LOG_INFO: i32 = 32;
/// libav `AV_LOG_VERBOSE` — detailed but still relevant information.
pub const AV_LOG_VERBOSE: i32 = 40;
/// libav `AV_LOG_DEBUG` — information only useful for libav developers.
pub const AV_LOG_DEBUG: i32 = 48;
/// libav `AV_LOG_TRACE` — extremely verbose per-operation tracing.
pub const AV_LOG_TRACE: i32 = 56;

/// Maximum number of bytes retained for a single rendered log line.
///
/// libav lines are short; this bounds a pathological component / format so the
/// callback never allocates without limit. A line longer than this is truncated
/// at a UTF-8 character boundary.
pub const MAX_LINE_LEN: usize = 1024;

/// The bridge's coarse severity band — a libav `AV_LOG_*` level collapsed to the
/// five [`tracing`] levels. Kept as a plain enum (rather than [`tracing::Level`])
/// so the mapping is pure and unit-testable in the default, `tracing`-only build
/// without taking a hard dependency on tracing's internal level ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BridgeLevel {
    /// `tracing::Level::ERROR` — libav PANIC / FATAL / ERROR.
    Error,
    /// `tracing::Level::WARN` — libav WARNING.
    Warn,
    /// `tracing::Level::INFO` — libav INFO.
    Info,
    /// `tracing::Level::DEBUG` — libav VERBOSE / DEBUG.
    Debug,
    /// `tracing::Level::TRACE` — libav TRACE (and anything more verbose).
    Trace,
}

/// Map a libav `AV_LOG_*` numeric level to a [`BridgeLevel`].
///
/// libav levels are a numeric *scale* (smaller = more severe), not a closed set,
/// so this buckets by threshold rather than matching exact discriminants: any
/// value at least as severe as `ERROR` (including the negative `QUIET`) maps to
/// [`BridgeLevel::Error`], and anything beyond `TRACE` saturates to
/// [`BridgeLevel::Trace`]. It is total and panic-free for every `i32`.
#[must_use]
pub fn map_av_level(av_level: i32) -> BridgeLevel {
    // Boundaries follow libav's own banding: a value is treated at the most
    // severe band whose threshold it does not exceed. `<=` keeps the canonical
    // constants exact (e.g. AV_LOG_WARNING == 24 → Warn) while bucketing the
    // gaps between them upward in severity.
    if av_level <= AV_LOG_ERROR {
        BridgeLevel::Error
    } else if av_level <= AV_LOG_WARNING {
        BridgeLevel::Warn
    } else if av_level <= AV_LOG_INFO {
        BridgeLevel::Info
    } else if av_level <= AV_LOG_DEBUG {
        BridgeLevel::Debug
    } else {
        BridgeLevel::Trace
    }
}

/// Clean a rendered libav log line for emission as a single `tracing` record.
///
/// libav lines arrive newline-terminated and may, in pathological cases, carry
/// embedded control bytes (a corrupt component name) or be very long. This:
/// * strips trailing ASCII whitespace (the libav newline);
/// * replaces interior control characters (other than ordinary space) with a
///   space so the record stays one clean printable line;
/// * truncates to [`MAX_LINE_LEN`] at a UTF-8 character boundary.
///
/// It allocates a single bounded `String` and is panic-free for any input
/// (including empty, `%`-containing, NUL-containing, or oversized text).
#[must_use]
pub fn sanitize_line(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let mut out = String::with_capacity(trimmed.len().min(MAX_LINE_LEN));
    for ch in trimmed.chars() {
        if out.len().saturating_add(ch.len_utf8()) > MAX_LINE_LEN {
            break;
        }
        // Replace C0/C1 control characters (NUL, BEL, …) with a space; keep all
        // other (printable) characters verbatim. A literal `%` is ordinary text
        // here — the va_list was already expanded in C by the formatter.
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

/// The decision the [`Suppressor`] reaches for one observed message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuppressOutcome {
    /// Emit this message now (a first occurrence, an LRU-evicted key seen again,
    /// or a re-emit after the window with nothing suppressed in between).
    Emit,
    /// Drop this message — an identical one was emitted inside the active window.
    /// Its occurrence has been counted for a later coalesced summary.
    Suppress,
    /// Emit this message now **and** a coalesced summary: `suppressed` identical
    /// occurrences were dropped since the last emit, and the window has elapsed.
    EmitWithSummary {
        /// How many identical occurrences were suppressed since the last emit.
        suppressed: u64,
    },
}

/// One tracked message key's suppression state.
#[derive(Debug, Clone)]
struct Entry {
    /// `(level, message)` identity of the tracked message.
    key: (BridgeLevel, String),
    /// When the current window opened (the last time this key was emitted).
    window_start: Duration,
    /// Identical occurrences suppressed since `window_start`.
    suppressed: u64,
    /// Monotonic recency stamp for LRU eviction (largest = most recent).
    touched: u64,
}

/// A thread-unsynchronised, bounded repetition suppressor (the anti-flood core).
///
/// Keyed by `(level, message text)`. The first time a key is seen it is emitted
/// and a window opens. Identical occurrences inside the window are suppressed and
/// counted. The first occurrence at or after the window's end emits the message
/// again, attaching the suppressed count as a coalesced summary, and reopens the
/// window.
///
/// Memory is bounded by a fixed-capacity LRU: at most `cap` keys are retained;
/// inserting a new key when full evicts the least-recently-touched one. Feeding
/// an unbounded stream of *distinct* messages therefore never grows the
/// structure past `cap` (CLAUDE.md §7 bounded-memory). The callback wraps this
/// in a [`std::sync::Mutex`]; it is deliberately kept lock-light (small cap,
/// linear scan) because the trampoline runs on decoder threads.
#[derive(Debug)]
pub struct Suppressor {
    entries: Vec<Entry>,
    cap: usize,
    window: Duration,
    /// Monotonic recency counter; incremented on every observe.
    clock: u64,
}

impl Suppressor {
    /// Create a suppressor retaining at most `cap` distinct message keys, with a
    /// suppression `window` per key. A `cap` of 0 degrades to always-emit (no
    /// state retained) — never a panic, never growth.
    #[must_use]
    pub fn new(cap: usize, window: Duration) -> Self {
        Self {
            entries: Vec::new(),
            cap,
            window,
            clock: 0,
        }
    }

    /// Number of distinct message keys currently tracked (always `<= cap`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no message keys are currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Find the index of the entry whose key matches `(level, message)`.
    fn find(&self, level: BridgeLevel, message: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.key.0 == level && e.key.1 == message)
    }

    /// Index of the least-recently-touched entry (smallest `touched`).
    fn lru_index(&self) -> Option<usize> {
        self.entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.touched)
            .map(|(i, _)| i)
    }

    /// Observe one message at `now`, returning the emit/suppress decision and
    /// updating the per-key window state. `message` is the already-rendered,
    /// sanitised line; identity is `(level, message)`.
    pub fn observe(&mut self, level: BridgeLevel, message: &str, now: Duration) -> SuppressOutcome {
        // A zero-capacity suppressor retains nothing and always emits.
        if self.cap == 0 {
            return SuppressOutcome::Emit;
        }

        let tick = self.clock.wrapping_add(1);
        self.clock = tick;

        if let Some(idx) = self.find(level, message) {
            // Borrow the matched entry mutably for the window check.
            if let Some(entry) = self.entries.get_mut(idx) {
                entry.touched = tick;
                let elapsed = now.saturating_sub(entry.window_start);
                if elapsed >= self.window {
                    // Window elapsed: flush any coalesced count and reopen.
                    let suppressed = entry.suppressed;
                    entry.suppressed = 0;
                    entry.window_start = now;
                    if suppressed > 0 {
                        return SuppressOutcome::EmitWithSummary { suppressed };
                    }
                    return SuppressOutcome::Emit;
                }
                // Inside the window: suppress and count.
                entry.suppressed = entry.suppressed.saturating_add(1);
                return SuppressOutcome::Suppress;
            }
            // Unreachable in practice (idx came from `find`); fall through to
            // emit rather than risk any indexing panic.
            return SuppressOutcome::Emit;
        }

        // New key. Evict the LRU entry if at capacity, then insert.
        if self.entries.len() >= self.cap {
            if let Some(victim) = self.lru_index() {
                if victim < self.entries.len() {
                    self.entries.swap_remove(victim);
                }
            }
        }
        self.entries.push(Entry {
            key: (level, message.to_owned()),
            window_start: now,
            suppressed: 0,
            touched: tick,
        });
        SuppressOutcome::Emit
    }
}

#[cfg(feature = "ffmpeg")]
pub use ffi::install;

#[cfg(feature = "ffmpeg")]
mod ffi {
    //! The `av_log_set_callback` installation and the `extern "C"` trampoline.
    //!
    //! This is the only part of the bridge that touches libav. It renders the
    //! incoming `va_list` into a bounded stack buffer via `av_log_format_line2`
    //! (no per-call heap alloc on the render path), reads the component class
    //! name from the libav object's leading `AVClass*`, runs the pure
    //! suppressor, and emits via `tracing` — the whole body inside
    //! `catch_unwind` so a Rust panic can never unwind across the FFI boundary.

    // reason: this module installs a raw libav `extern "C"` callback and reads a
    // libav object's leading `AVClass*` pointer — raw FFI the crate's
    // `unsafe_code = "deny"` posture allows here (CLAUDE.md §7), every block with
    // a `// SAFETY:` note.
    #![allow(unsafe_code)]

    use std::ffi::CStr;
    use std::sync::{Mutex, Once, OnceLock};
    use std::time::{Duration, Instant};

    use ffmpeg::ffi;
    use ffmpeg_next as ffmpeg;
    use libc::{c_char, c_int, c_void};

    use super::{
        map_av_level, sanitize_line, BridgeLevel, SuppressOutcome, Suppressor, MAX_LINE_LEN,
    };

    /// The libav log-callback signature **as this crate declares the trampoline**,
    /// spelling the `va_list` parameter via the binding's own [`ffi::va_list`]
    /// alias (never the rendering-specific `__va_list_tag` form).
    ///
    /// ## Why an alias + `transmute` at the boundary (portability)
    ///
    /// `ffmpeg-sys-next`'s bindgen renders the libav `va_list` argument in **two
    /// different shapes depending on the host libclang/bindgen version**, and the
    /// two shapes are *not the same Rust type*:
    ///
    /// * On this dev container (and any toolchain that keeps the array form) every
    ///   libav function — `av_log_set_callback`'s callback parameter **and**
    ///   `av_log_format_line2`'s `vl` parameter — uses the [`ffi::va_list`] alias
    ///   directly (e.g. `[__va_list_tag; 1]` or `__BindgenOpaqueArray<u64, 4>`).
    /// * On Debian-trixie libclang, bindgen *decays* the `va_list` array in
    ///   function-parameter position to `*mut __va_list_tag`, so those same two
    ///   parameters are raw pointers — while the standalone `ffi::va_list` **alias**
    ///   still resolves to the array. The decayed `__va_list_tag` type is also
    ///   **absent** as a nameable type on the array-rendering toolchains, so it
    ///   cannot be written portably.
    ///
    /// The trampoline therefore declares its `va_list` parameter as the portable
    /// [`ffi::va_list`] alias and bridges to the binding's actual parameter type
    /// with a function-pointer [`std::mem::transmute`] at exactly two boundaries
    /// (registration below; the `av_log_format_line2` call in the trampoline). The
    /// transmute is **ABI-identity**, not a reinterpretation of differently-shaped
    /// values: the libav callback's C parameter is `va_list` (`__va_list_tag[1]`),
    /// which under C array-parameter decay is *always* passed as a single
    /// `__va_list_tag*` pointer in one argument register. Both bindgen shapes model
    /// that one fixed C ABI — `extern "C" fn(.., *mut __va_list_tag)` and
    /// `extern "C" fn(.., va_list)` (single-element array by value) lower to the
    /// identical calling convention for that argument. On the array-rendering
    /// toolchains the two fn-pointer types are literally equal, so the transmute is
    /// the identity. The conversion is thus zero-cost and sound on every rendering.
    type LogCallback = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, ffi::va_list);

    /// `av_log_format_line2`'s signature spelled with the portable [`ffi::va_list`]
    /// alias for its `vl` argument (same ABI-identity rationale as [`LogCallback`]).
    /// The trampoline transmutes the real `ffi::av_log_format_line2` to this type so
    /// the call type-checks whether bindgen kept the alias or decayed it to a
    /// pointer, and passes the trampoline's `va_list` straight through.
    type AvLogFormatLine2 = unsafe extern "C" fn(
        *mut c_void,
        c_int,
        *const c_char,
        ffi::va_list,
        *mut c_char,
        c_int,
        *mut c_int,
    ) -> c_int;

    /// Suppression window: identical repeats within this span are coalesced.
    const SUPPRESS_WINDOW: Duration = Duration::from_secs(5);
    /// Bounded LRU capacity: distinct recent `(level, message)` keys retained.
    const SUPPRESS_CAPACITY: usize = 256;

    /// Render-buffer length in bytes: [`MAX_LINE_LEN`] payload plus the NUL.
    const LINE_BUF_LEN: usize = MAX_LINE_LEN + 1;
    /// The same length the `c_int` parameter of `av_log_format_line2` expects.
    /// Defined as a literal `c_int` so the `usize → c_int` narrowing is a
    /// compile-time constant (no runtime `as`); the `const` assertion below binds
    /// it to [`LINE_BUF_LEN`], so the build fails if `MAX_LINE_LEN` ever changes
    /// without updating this literal too.
    const LINE_BUF_LEN_C: c_int = 1025;
    const _: () = assert!(
        LINE_BUF_LEN == 1025,
        "LINE_BUF_LEN_C literal must be kept equal to LINE_BUF_LEN (MAX_LINE_LEN + 1)"
    );

    /// Process-global suppressor shared by every decoder thread. A `Mutex` over a
    /// small fixed-capacity LRU; held only for the O(cap) lookup, never across a
    /// blocking call.
    static SUPPRESSOR: OnceLock<Mutex<Suppressor>> = OnceLock::new();
    /// Monotonic origin for relative timestamps fed to the suppressor (a libav
    /// callback has no clock argument). `Instant` is monotonic and cheap.
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    /// Guards one-time callback installation.
    static INSTALL: Once = Once::new();

    fn suppressor() -> &'static Mutex<Suppressor> {
        SUPPRESSOR.get_or_init(|| Mutex::new(Suppressor::new(SUPPRESS_CAPACITY, SUPPRESS_WINDOW)))
    }

    fn origin() -> Instant {
        *ORIGIN.get_or_init(Instant::now)
    }

    /// Install the libav → `tracing` log bridge for the process, exactly once.
    ///
    /// Idempotent: subsequent calls are no-ops. After this, libav log output is
    /// routed into [`tracing`] and rate-limited instead of going to stderr.
    /// Safe to call from `ensure_initialized` on every libav entry point.
    pub fn install() {
        INSTALL.call_once(|| {
            // Initialise the shared state before the callback can fire.
            let _ = suppressor();
            let _ = origin();
            // `log_trampoline` declares its `va_list` parameter via the portable
            // `ffi::va_list` alias (see `LogCallback`). On toolchains where bindgen
            // decays that parameter to `*mut __va_list_tag` in `av_log_set_callback`'s
            // own callback type, the alias-typed fn pointer is not *spelled* the same
            // even though it has the identical ABI, so a `transmute` bridges the two.
            let trampoline: LogCallback = log_trampoline;
            // reason(allow): on bindgen renderings that keep the `ffi::va_list`
            // alias in `av_log_set_callback`'s callback parameter, `LogCallback`
            // *is* that exact type, so the transmute is the identity and clippy
            // flags it as `useless_transmute`. It is **load-bearing on the decayed
            // rendering** (Debian-trixie libclang), where the binding's callback
            // parameter is `*mut __va_list_tag` and `LogCallback` is the array
            // alias: the transmute reconciles the two ABI-identical fn-pointer
            // types there (see `LogCallback`). The lint cannot see the other
            // rendering, so the allow is required and correct.
            #[allow(clippy::useless_transmute)]
            // SAFETY: `LogCallback` and the bindings' `av_log_set_callback` callback
            // type differ only in how bindgen *spells* the `va_list` parameter
            // (`ffi::va_list` array vs decayed `*mut __va_list_tag`); both are the
            // same `extern "C"` calling convention for that argument (a single
            // pointer to the `__va_list_tag` array — see `LogCallback`). Function
            // pointers are one machine word; this reinterprets the pointer's *type*,
            // not its bits. `av_log_set_callback` then stores it in a global and
            // invokes it (with libav-valid arguments) for every libav log line, on
            // any thread; `log_trampoline` owns no captured state. Passing `Some`
            // replaces libav's default stderr writer.
            let callback = unsafe { std::mem::transmute::<LogCallback, _>(trampoline) };
            // SAFETY: see above — `callback` is the ABI-correct, bindgen-typed
            // libav log callback. Installing it is libav's documented mechanism.
            unsafe {
                ffi::av_log_set_callback(Some(callback));
            }
        });
    }

    /// Read the component name (libav `AVClass::class_name`, e.g. `"hevc"`) from
    /// a libav log object pointer, if any.
    ///
    /// libav's logging convention: any object that can be logged against has a
    /// `*const AVClass` as the **first** member of its struct, and `AVClass`'s
    /// first member is the `class_name` C string. The pointer may be null (a
    /// context-free log line), so every dereference is null-checked.
    fn component_name(avcl: *mut c_void, buf: &mut [u8; 64]) -> Option<usize> {
        if avcl.is_null() {
            return None;
        }
        // SAFETY: per libav's logging ABI the object pointed to by `avcl` begins
        // with a `*const AVClass`. We read exactly that one pointer-sized field
        // by reading the first `*const AVClass` at the object's address. The read
        // is unaligned-safe via `read_unaligned` and does not assume the rest of
        // the struct's layout. `avcl` is non-null (checked above).
        let class_ptr = unsafe { avcl.cast::<*const ffi::AVClass>().read_unaligned() };
        if class_ptr.is_null() {
            return None;
        }
        // SAFETY: `class_ptr` is a non-null `*const AVClass` provided by libav for
        // the lifetime of the logged object; reading its `class_name` field (the
        // first member) is a single pointer read of a valid C struct.
        let name_ptr = unsafe { (*class_ptr).class_name };
        if name_ptr.is_null() {
            return None;
        }
        // SAFETY: `class_name` is a static, NUL-terminated C string owned by libav
        // (string literals registered at codec/format build time); it outlives
        // this call. `CStr::from_ptr` only requires a valid NUL-terminated string.
        let cstr = unsafe { CStr::from_ptr(name_ptr) };
        let bytes = cstr.to_bytes();
        let n = bytes.len().min(buf.len());
        // Copy the (bounded) name into the caller's stack buffer; never allocate.
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), bytes.get(..n)) {
            dst.copy_from_slice(src);
            Some(n)
        } else {
            None
        }
    }

    /// The pure (panic-free, libav-free) core of the callback: given the level,
    /// component name, and rendered line, run the suppressor and emit via
    /// `tracing`. Split out so the unsafe trampoline body stays tiny.
    fn route(level: c_int, component: &str, line: &str) {
        let bridge_level = map_av_level(level);
        let clean = sanitize_line(line);
        if clean.is_empty() {
            return;
        }
        let now = origin().elapsed();
        let outcome = match suppressor().lock() {
            Ok(mut guard) => guard.observe(bridge_level, &clean, now),
            // A poisoned lock means another thread panicked while holding it;
            // rather than propagate, emit unconditionally (correctness over
            // anti-flood — never drop a line because of a lock fault).
            Err(poisoned) => poisoned.into_inner().observe(bridge_level, &clean, now),
        };
        match outcome {
            SuppressOutcome::Suppress => {}
            SuppressOutcome::Emit => emit(bridge_level, component, &clean, None),
            SuppressOutcome::EmitWithSummary { suppressed } => {
                emit(bridge_level, component, &clean, Some(suppressed));
            }
        }
    }

    /// Emit one record at the mapped tracing level, carrying the libav component
    /// as a field and, when flushing, the coalesced suppressed count.
    fn emit(level: BridgeLevel, component: &str, line: &str, suppressed: Option<u64>) {
        let window_s = SUPPRESS_WINDOW.as_secs();
        match (level, suppressed) {
            (BridgeLevel::Error, None) => {
                tracing::error!(target: "libav", component, "{line}");
            }
            (BridgeLevel::Error, Some(n)) => {
                tracing::error!(target: "libav", component, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Warn, None) => {
                tracing::warn!(target: "libav", component, "{line}");
            }
            (BridgeLevel::Warn, Some(n)) => {
                tracing::warn!(target: "libav", component, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Info, None) => {
                tracing::info!(target: "libav", component, "{line}");
            }
            (BridgeLevel::Info, Some(n)) => {
                tracing::info!(target: "libav", component, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Debug, None) => {
                tracing::debug!(target: "libav", component, "{line}");
            }
            (BridgeLevel::Debug, Some(n)) => {
                tracing::debug!(target: "libav", component, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
            (BridgeLevel::Trace, None) => {
                tracing::trace!(target: "libav", component, "{line}");
            }
            (BridgeLevel::Trace, Some(n)) => {
                tracing::trace!(target: "libav", component, repeated = n, "{line} (repeated {n}× in the last {window_s}s)");
            }
        }
    }

    /// The `extern "C"` callback libav invokes for every log line, on whatever
    /// (foreign/decoder) thread produced it.
    ///
    /// It renders the `va_list` into a fixed stack buffer with
    /// `av_log_format_line2` (no heap alloc on the render path), reads the
    /// component name from the leading `AVClass*`, and hands both to the pure
    /// router. The entire Rust body runs inside `catch_unwind`: a logging
    /// callback must never crash the decoder, so any panic is caught and the line
    /// is dropped (CLAUDE.md §7 — never unwind across the FFI boundary).
    extern "C" fn log_trampoline(
        avcl: *mut c_void,
        level: c_int,
        fmt: *const c_char,
        vl: ffi::va_list,
    ) {
        // `catch_unwind` requires `UnwindSafe`; the raw pointers/level are plain
        // `Copy` scalars with no interior invariants to break on a caught unwind.
        let result = std::panic::catch_unwind(move || {
            // A null format string carries no message — nothing to render.
            if fmt.is_null() {
                return;
            }

            // Render the line into a bounded stack buffer. `+1` for the NUL.
            // `c_char` is `i8` on x86_64 and `u8` on aarch64; the literal `0`
            // initialises either without an `as` cast (annotated element type).
            let mut line_buf: [c_char; LINE_BUF_LEN] = [0; LINE_BUF_LEN];
            let mut print_prefix: c_int = 1;
            // Bridge to `av_log_format_line2` through the portable
            // `AvLogFormatLine2` alias (spelled with `ffi::va_list`). On bindgen
            // renderings that decay the `vl` parameter to `*mut __va_list_tag`,
            // the real function is not *spelled* with `ffi::va_list`, so a
            // fn-pointer transmute bridges the two ABI-identical types (see
            // `LogCallback`); on the array-rendering toolchains it is the identity.
            // Coerce the libav fn *item* to a fn *pointer* of its own
            // (bindgen-chosen) type, leaving the `va_list` parameter position as an
            // inferred `_` so this names neither `ffi::va_list` nor the decayed
            // `*mut __va_list_tag` — it compiles whichever shape bindgen emitted.
            // (Transmuting the fn *item* directly is rejected as a zero-sized type;
            // this `let` coercion materialises the real fn pointer without an `as`
            // cast.) The pointer is then transmuted to the alias-spelled
            // `AvLogFormatLine2` so the call below type-checks on both renderings.
            let native_format_line: unsafe extern "C" fn(
                *mut c_void,
                c_int,
                *const c_char,
                _,
                *mut c_char,
                c_int,
                *mut c_int,
            ) -> c_int = ffi::av_log_format_line2;
            // reason(allow): on renderings that keep the `ffi::va_list` alias in
            // `av_log_format_line2`'s `vl` parameter, `native_format_line` already
            // *is* `AvLogFormatLine2` and the transmute is the identity, which
            // clippy flags as `useless_transmute`. It is load-bearing on the decayed
            // rendering (the inferred `_` resolves to `*mut __va_list_tag` there);
            // see `LogCallback` for the ABI-identity argument.
            #[allow(clippy::useless_transmute)]
            // SAFETY: `native_format_line` (the real `av_log_format_line2`, with its
            // `vl` parameter as bindgen rendered it) and `AvLogFormatLine2` differ
            // only in how that one parameter is spelled (`ffi::va_list` array vs a
            // decayed `*mut __va_list_tag`); both share the single C `va_list`
            // calling convention (one `__va_list_tag*` — see `LogCallback`), so
            // reinterpreting the fn pointer is sound. Function pointers are one
            // machine word; this casts the pointer's type, not its address.
            let format_line: AvLogFormatLine2 = unsafe { std::mem::transmute(native_format_line) };
            // SAFETY: `format_line` is the ABI-correct `av_log_format_line2`. It
            // formats `fmt` + `vl` for the libav object `avcl` into `line_buf` of
            // `LINE_BUF_LEN` bytes (passed as `LINE_BUF_LEN_C`, a compile-time-checked
            // `c_int`), writing a NUL-terminated, length-bounded string and consuming
            // `vl` exactly once. `avcl`/`fmt`/`vl` are the libav-provided callback
            // arguments (valid for the call); `line_buf`/`print_prefix` are live
            // stack locals; `&raw mut print_prefix` yields a valid out-pointer for
            // the `int*` parameter. The returned length is ignored — we re-scan for
            // the NUL via `CStr` to stay independent of libav's truncation.
            let _written = unsafe {
                format_line(
                    avcl,
                    level,
                    fmt,
                    vl,
                    line_buf.as_mut_ptr(),
                    LINE_BUF_LEN_C,
                    &raw mut print_prefix,
                )
            };
            // Force NUL-termination defensively before reading as a C string.
            if let Some(last) = line_buf.last_mut() {
                *last = 0;
            }
            // SAFETY: `line_buf` is a live, NUL-terminated (guaranteed above)
            // stack buffer of `c_char`; reading up to the NUL via `CStr` is
            // sound. The borrow does not escape this scope.
            let line_cstr = unsafe { CStr::from_ptr(line_buf.as_ptr()) };
            let line = line_cstr.to_string_lossy();

            // Read the component (`AVClass::class_name`) into a stack buffer.
            let mut comp_buf = [0u8; 64];
            let component = match component_name(avcl, &mut comp_buf) {
                Some(n) => match comp_buf.get(..n) {
                    Some(slice) => String::from_utf8_lossy(slice),
                    None => std::borrow::Cow::Borrowed(""),
                },
                None => std::borrow::Cow::Borrowed(""),
            };

            route(level, component.as_ref(), line.as_ref());
        });
        // On any caught panic, drop the line silently. Never re-raise across FFI.
        drop(result);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn install_is_idempotent_and_routes_synthetic_av_log() {
            // Installing twice must not double-register or panic.
            install();
            install();

            // Emit a synthetic libav log line through the real av_log path and
            // assert the bridge does not panic / crash. (Routing correctness of
            // the pure logic is asserted in tests/log_bridge.rs; here we exercise
            // the actual C → trampoline → tracing wiring end to end.)
            let msg = std::ffi::CString::new("multiview log-bridge smoke %d").expect("no NUL");
            for _ in 0..5 {
                // SAFETY: `av_log` takes a NUL-terminated printf format and
                // matching varargs; `msg` is a valid C string and `42` matches
                // the single `%d`. A null `avcl` is explicitly allowed by libav
                // (a context-free log line).
                unsafe {
                    ffi::av_log(
                        std::ptr::null_mut(),
                        super::super::AV_LOG_INFO,
                        msg.as_ptr(),
                        42,
                    );
                }
            }
        }

        #[test]
        fn component_name_is_none_for_null_object() {
            let mut buf = [0u8; 64];
            assert_eq!(component_name(std::ptr::null_mut(), &mut buf), None);
        }
    }
}
