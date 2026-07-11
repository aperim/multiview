//! The SAP **socket transport** — the supervised UDP listener + announcer
//! (RFC 2974 §3/§5; ADR-0041 §3/§5). **Feature `st2110`** (a `tokio` `UdpSocket`,
//! no FFI — the crate stays `unsafe_code = forbid`, LGPL-clean).
//!
//! [`SapListener`] binds an IPv6-first UDP socket, (on a real network) joins the
//! SAP group set, and runs a receive loop that parses each datagram and folds
//! announcements into the wait-free [`SapSessionTable`]. A malformed/rejected
//! datagram is skipped — the loop never dies on bad input; a spoofed inbound
//! deletion never withdraws a tracked session (the table ignores `T=1`,
//! ADR-0041 §8). [`SapAnnouncer`] builds and sends the `T=0`/`T=1` packets on an
//! **independent timer** ([`AnnounceSchedule`]).
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Everything here is control/discovery plane: the receive loop folds into a
//! **bounded, drop-oldest, wait-free** table (no unbounded queue anywhere), the
//! announce timer is independent of the per-tick output loop, and nothing can
//! block or back-pressure the engine. The multicast group join is the live
//! deployment path (this devcontainer's loopback is not multicast-capable), so
//! the tests exercise the sockets over unicast loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket;

use crate::error::{Error, Result};

use super::announce::{announcement, deletion, AnnounceSchedule};
use super::groups::{receive_group_set, SAP_TTL};
use super::packet::SapPacket;
use super::session::SapSessionTable;

/// The largest SAP datagram accepted (the max UDP payload). The packet parser
/// separately caps the SDP body, so a large datagram cannot force an unbounded
/// allocation.
pub const MAX_SAP_DATAGRAM: usize = 65_536;

/// A supervised SAP receive listener: a bound UDP socket folding discovered
/// announcements into a wait-free, bounded session table.
#[derive(Debug)]
pub struct SapListener {
    socket: Arc<UdpSocket>,
    table: Arc<SapSessionTable>,
    started: Instant,
}

impl SapListener {
    /// Bind a SAP listener to `local` (IPv6-first: pass `[::]:9875` for the live
    /// receiver; an ephemeral loopback port for tests).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Ingest(format!("sap bind {local}: {e}")))?;
        Ok(Self {
            socket: Arc::new(socket),
            table: Arc::new(SapSessionTable::new()),
            started: Instant::now(),
        })
    }

    /// Use a caller-provided (shared) session table instead of a fresh one, so
    /// the control plane can read the same inventory this listener writes.
    #[must_use]
    pub fn with_table(mut self, table: Arc<SapSessionTable>) -> Self {
        self.table = table;
        self
    }

    /// A shared handle to the session table this listener folds into (read its
    /// wait-free [`inventory`](SapSessionTable::inventory) from anywhere).
    #[must_use]
    pub fn table(&self) -> Arc<SapSessionTable> {
        Arc::clone(&self.table)
    }

    /// The socket's local address.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket
            .local_addr()
            .map_err(|e| Error::Ingest(format!("sap local_addr: {e}")))
    }

    /// Join the SAP receive group set for this socket's address family (the live
    /// multicast discovery path). A dual-stack deployment runs a v4 and a v6
    /// listener; each joins only its own family's groups (a single socket cannot
    /// join the other family).
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if a membership cannot be joined.
    pub fn join_receive_groups(&self) -> Result<()> {
        let want_v6 = self.local_addr()?.is_ipv6();
        for group in receive_group_set() {
            match group {
                IpAddr::V4(v4) if !want_v6 => {
                    self.socket
                        .join_multicast_v4(v4, Ipv4Addr::UNSPECIFIED)
                        .map_err(|e| Error::Ingest(format!("sap join {v4}: {e}")))?;
                }
                IpAddr::V6(v6) if want_v6 => {
                    self.socket
                        .join_multicast_v6(&v6, 0)
                        .map_err(|e| Error::Ingest(format!("sap join {v6}: {e}")))?;
                }
                _ => {} // the other family — joined by that family's listener
            }
        }
        Ok(())
    }

    /// Receive one datagram and fold it into the table. A malformed or rejected
    /// datagram is skipped so the loop never dies on bad input (rule 26: bad
    /// inputs are the purpose).
    async fn recv_fold_once(&self, buf: &mut [u8]) -> Result<()> {
        let (n, _peer) = self
            .socket
            .recv_from(buf)
            .await
            .map_err(|e| Error::Ingest(format!("sap recv: {e}")))?;
        let now = self.started.elapsed();
        if let Some(bytes) = buf.get(..n) {
            if let Ok(pkt) = SapPacket::parse(bytes) {
                self.table.observe(&pkt, now);
            }
        }
        Ok(())
    }

    /// Run the receive loop until the socket faults (then the supervisor restarts
    /// it), folding announcements into the wait-free table and purging stale
    /// sessions each datagram. Never blocks a consumer or paces the engine
    /// (inv #1/#10).
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` on a clean socket-fault exit; it does not surface the
    /// fault (the supervisor reconnects) but logs it.
    pub async fn run(self) -> Result<()> {
        let mut buf = vec![0u8; MAX_SAP_DATAGRAM];
        loop {
            if let Err(e) = self.recv_fold_once(&mut buf).await {
                tracing::debug!(error = %e, "sap listener socket ended; supervisor restarts");
                return Ok(());
            }
            self.table.purge(self.started.elapsed());
        }
    }
}

/// One session an announcer re-announces: its stable hash, originating source,
/// opaque SDP body, and the destination (the scope-selected SAP group on the SAP
/// port) to send to.
#[derive(Debug, Clone)]
pub struct AnnouncedSession {
    /// The stable non-zero message-id hash (see [`stable_hash`](super::announce::stable_hash)).
    pub hash: NonZeroU16,
    /// The announcement's originating source address.
    pub origin: IpAddr,
    /// The opaque SDP body to announce.
    pub sdp: Vec<u8>,
    /// The destination: the scope-selected SAP group on the SAP port.
    pub dest: SocketAddr,
}

/// A SAP announcer: a bound UDP socket that emits `T=0` announcements on a
/// jittered timer and `T=1` deletions on teardown.
#[derive(Debug)]
pub struct SapAnnouncer {
    socket: Arc<UdpSocket>,
}

impl SapAnnouncer {
    /// Bind an announcer socket to `local`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Ingest(format!("sap announcer bind {local}: {e}")))?;
        // SAP uses TTL/hop-limit 255 (scope is carried by the group address, not
        // the TTL). Best-effort: a v6-only socket has no v4 multicast TTL.
        if let Err(e) = socket.set_multicast_ttl_v4(SAP_TTL) {
            tracing::trace!(error = %e, "sap v4 multicast ttl unset (non-fatal, e.g. v6 socket)");
        }
        Ok(Self {
            socket: Arc::new(socket),
        })
    }

    /// Encode and send one SAP packet to `dest`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the send fails.
    pub async fn send_to(&self, packet: &SapPacket, dest: SocketAddr) -> Result<usize> {
        let bytes = packet.encode();
        self.socket
            .send_to(&bytes, dest)
            .await
            .map_err(|e| Error::Ingest(format!("sap send {dest}: {e}")))
    }

    /// Announce every session once immediately, then re-announce on the jittered
    /// [`AnnounceSchedule`] cadence forever (the supervisor aborts the task on
    /// shutdown and calls [`send_deletions`](Self::send_deletions)). An
    /// independent timer that never paces the output clock (inv #1). A per-send
    /// error is logged, not fatal — the next cycle retries.
    pub async fn run(self, sessions: Vec<AnnouncedSession>, schedule: AnnounceSchedule) {
        let mut rng = seed(&sessions);
        loop {
            for s in &sessions {
                let pkt = announcement(s.hash, s.origin, s.sdp.clone());
                if let Err(e) = self.send_to(&pkt, s.dest).await {
                    tracing::debug!(error = %e, dest = %s.dest, "sap announce send failed");
                }
            }
            rng = xorshift(rng);
            tokio::time::sleep(schedule.next_delay(rng)).await;
        }
    }

    /// Send a courtesy `T=1` deletion for each session (best-effort teardown).
    pub async fn send_deletions(&self, sessions: &[AnnouncedSession]) {
        for s in sessions {
            let pkt = deletion(s.hash, s.origin, s.sdp.clone());
            if let Err(e) = self.send_to(&pkt, s.dest).await {
                tracing::debug!(error = %e, dest = %s.dest, "sap deletion send failed");
            }
        }
    }
}

/// A per-run jitter seed that de-correlates announcers across hosts/sessions:
/// wall-clock nanoseconds mixed with the first session's hash. Non-cryptographic
/// — only spreads the ±1/3 announce jitter. Never zero (xorshift needs non-zero
/// state).
fn seed(sessions: &[AnnouncedSession]) -> u64 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut s = u64::try_from(base & u128::from(u64::MAX)).unwrap_or(0x9E37_79B9_7F4A_7C15);
    if let Some(first) = sessions.first() {
        s ^= u64::from(first.hash.get());
    }
    s | 1
}

/// A tiny non-cryptographic xorshift64 step for the announce jitter sample.
fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}
