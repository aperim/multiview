//! Dependency-free systemd readiness/watchdog notification (DEV-B5 /
//! ADR-0045).
//!
//! The sd_notify protocol is a single `SOCK_DGRAM` `AF_UNIX` datagram of
//! newline-separated `KEY=VALUE` assignments sent to the socket named by the
//! `NOTIFY_SOCKET` environment variable (a filesystem path, or — with a
//! leading `@` — a Linux abstract-namespace name). That is the whole
//! protocol, so this module implements it directly over
//! [`std::os::unix::net::UnixDatagram`] instead of pulling a crate.
//!
//! Everything here is **best-effort and non-blocking by construction**: the
//! datagram socket is set non-blocking, send failures are logged at debug and
//! dropped, and an absent `NOTIFY_SOCKET` yields an inert [`Notifier`] —
//! correct for every non-systemd deployment (containers, dev shells). The
//! node's first-frame `READY=1` runs on the output-clock loop's frame
//! boundary, which this design keeps safe (invariants #1 + #10: one
//! non-blocking syscall, no waiting, no error propagation into the engine).
//!
//! The watchdog is **liveness-gated**: [`WatchdogGate`] approves a
//! `WATCHDOG=1` ping only when the output tick counter advanced since the
//! previous check. A stalled output clock therefore withholds pings and
//! systemd restarts the node — the watchdog enforces invariant #1 instead of
//! merely proving the process exists.

use std::ffi::OsStr;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::Duration;

/// One `sd_notify` state assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotifyState<'a> {
    /// `READY=1` — startup finished (for the node: the first output frame).
    Ready,
    /// `STOPPING=1` — clean shutdown began.
    Stopping,
    /// `WATCHDOG=1` — the liveness ping (see [`WatchdogGate`]).
    Watchdog,
    /// `STATUS=<text>` — a one-line human-readable status.
    Status(&'a str),
}

/// Encode states as the `sd_notify` wire form: newline-joined `KEY=VALUE`
/// lines. Newlines inside a [`NotifyState::Status`] text are replaced with
/// spaces — a literal newline would inject a forged protocol line.
#[must_use]
pub fn encode_states(states: &[NotifyState<'_>]) -> Vec<u8> {
    let mut lines: Vec<String> = Vec::with_capacity(states.len());
    for state in states {
        match state {
            NotifyState::Ready => lines.push("READY=1".to_owned()),
            NotifyState::Stopping => lines.push("STOPPING=1".to_owned()),
            NotifyState::Watchdog => lines.push("WATCHDOG=1".to_owned()),
            NotifyState::Status(text) => {
                let clean = text.replace(['\n', '\r'], " ");
                lines.push(format!("STATUS={clean}"));
            }
        }
    }
    lines.join("\n").into_bytes()
}

/// Resolve the watchdog ping interval from `WATCHDOG_USEC` / `WATCHDOG_PID`
/// values: half the budget (systemd's own guidance), `None` when the budget
/// is absent/zero/garbage or the pid filter names another process.
#[must_use]
pub fn watchdog_interval_from(
    usec: Option<&str>,
    pid: Option<&str>,
    my_pid: u32,
) -> Option<Duration> {
    if let Some(pid_text) = pid {
        let for_pid: u32 = pid_text.trim().parse().ok()?;
        if for_pid != my_pid {
            return None;
        }
    }
    let budget_usec: u64 = usec?.trim().parse().ok()?;
    if budget_usec == 0 {
        return None;
    }
    Some(Duration::from_micros(budget_usec / 2))
}

/// Resolve the watchdog ping interval from this process's environment
/// (`WATCHDOG_USEC` / `WATCHDOG_PID`).
#[must_use]
pub fn watchdog_interval_from_env() -> Option<Duration> {
    let usec = std::env::var("WATCHDOG_USEC").ok();
    let pid = std::env::var("WATCHDOG_PID").ok();
    watchdog_interval_from(usec.as_deref(), pid.as_deref(), std::process::id())
}

/// The tick-gated watchdog liveness decision: ping exactly when the output
/// tick counter advanced since the previous check. Before the first tick no
/// ping is sent — startup is the unit's start-timeout's job (`Type=notify`),
/// not the watchdog's.
#[derive(Debug, Default)]
pub struct WatchdogGate {
    /// The tick count observed at the previous check.
    last: u64,
}

impl WatchdogGate {
    /// A fresh gate (no ticks observed yet).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to send `WATCHDOG=1` now, given the current output tick count.
    pub fn should_ping(&mut self, ticks_now: u64) -> bool {
        let advanced = ticks_now > self.last;
        self.last = ticks_now;
        advanced
    }
}

/// The resolved notification socket (kept open for the process lifetime).
#[derive(Debug)]
struct NotifyTarget {
    /// The unbound, non-blocking sender socket.
    socket: UnixDatagram,
    /// Where datagrams go (path or abstract).
    addr: std::os::unix::net::SocketAddr,
}

/// A cloneable, best-effort `sd_notify` sender. Inert (every call a no-op) when
/// no `NOTIFY_SOCKET` was given — the non-systemd case.
#[derive(Debug, Clone, Default)]
pub struct Notifier {
    /// The shared target; `None` = inert.
    target: Option<Arc<NotifyTarget>>,
}

impl Notifier {
    /// Build from this process's `NOTIFY_SOCKET` environment variable.
    #[must_use]
    pub fn from_env() -> Self {
        let addr = std::env::var_os("NOTIFY_SOCKET");
        Self::from_address(addr.as_deref())
    }

    /// Build from an explicit `NOTIFY_SOCKET`-shaped address: a filesystem
    /// path, or `@name` for the Linux abstract namespace. `None` (or an
    /// unusable address, logged at debug) yields an inert notifier.
    #[must_use]
    pub fn from_address(address: Option<&OsStr>) -> Self {
        let Some(address) = address else {
            return Self { target: None };
        };
        match resolve_target(address) {
            Ok(target) => Self {
                target: Some(Arc::new(target)),
            },
            Err(reason) => {
                tracing::debug!(reason, "sd_notify socket unusable; notifications disabled");
                Self { target: None }
            }
        }
    }

    /// Whether notifications actually go anywhere.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.target.is_some()
    }

    /// Send one datagram carrying `states`. Best-effort and non-blocking: a
    /// full receiver queue or a vanished socket is logged at debug and
    /// dropped — never an error, never a wait (safe on the frame boundary).
    pub fn notify(&self, states: &[NotifyState<'_>]) {
        let Some(target) = &self.target else {
            return;
        };
        let payload = encode_states(states);
        if let Err(e) = target.socket.send_to_addr(&payload, &target.addr) {
            tracing::debug!(error = %e, "sd_notify send failed (dropped; best-effort)");
        }
    }
}

/// Open the unbound non-blocking sender socket and resolve `address` into a
/// datagram destination (path, or `@`-prefixed Linux abstract name).
fn resolve_target(address: &OsStr) -> Result<NotifyTarget, String> {
    let socket =
        UnixDatagram::unbound().map_err(|e| format!("opening the datagram socket: {e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("setting non-blocking: {e}"))?;

    let bytes = address.as_encoded_bytes();
    let addr = if let Some(name) = bytes.strip_prefix(b"@") {
        abstract_addr(name)?
    } else {
        let path = std::path::Path::new(address);
        std::os::unix::net::SocketAddr::from_pathname(path)
            .map_err(|e| format!("path socket address {}: {e}", path.display()))?
    };
    Ok(NotifyTarget { socket, addr })
}

/// Build a Linux abstract-namespace socket address.
#[cfg(target_os = "linux")]
fn abstract_addr(name: &[u8]) -> Result<std::os::unix::net::SocketAddr, String> {
    use std::os::linux::net::SocketAddrExt as _;
    std::os::unix::net::SocketAddr::from_abstract_name(name)
        .map_err(|e| format!("abstract socket address: {e}"))
}

/// Abstract-namespace sockets are Linux-only; elsewhere an `@`-address is
/// unusable (and systemd does not exist there anyway).
#[cfg(not(target_os = "linux"))]
fn abstract_addr(_name: &[u8]) -> Result<std::os::unix::net::SocketAddr, String> {
    Err("abstract-namespace NOTIFY_SOCKET is Linux-only".to_owned())
}
