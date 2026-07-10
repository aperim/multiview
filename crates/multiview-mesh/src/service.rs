//! The live **mDNS announce + browse** service (ADR-0051 §2, brief §9.1) — the
//! `mdns` feature only.
//!
//! Wraps the maintained, deny-clean [`mdns_sd`] daemon (RFC 6762/6763) as a
//! [`MeshTransport`]: it **announces** this machine's signed
//! [`AnnouncePayload`](crate::announce::AnnouncePayload) as a Conspect service
//! type and **browses** for neighbours, draining their announcements for the
//! pure logic to decode + fold into the [`PeerTable`](crate::peer::PeerTable).
//!
//! ## IPv6-first (ADR-0042, hard rule)
//!
//! mDNS is link-local multicast on `ff02::fb` (IPv6 primary) with IPv4
//! `224.0.0.251` as legacy interop — `mdns_sd` joins both groups on every
//! multicast-capable interface, IPv6 included, so the announce is IPv6-first by
//! construction. The service binds dual-stack via the daemon's per-interface
//! join; there is no IPv4-only path.
//!
//! ## Isolation — best-effort, never blocks (invariant #10)
//!
//! Every operation is best-effort: a daemon error is a typed
//! [`MeshError::Transport`] the caller logs and carries on from. [`poll_received`]
//! is **non-blocking** (it drains the browse channel with `try_recv`, never
//! `recv`), so a poll that finds nothing returns immediately and the announce
//! task never blocks on the network. The service holds no engine handle.
//!
//! ## TXT chunking
//!
//! The signed payload (JSON, all-ASCII) can exceed a single 255-byte mDNS TXT
//! string, so it is split into byte-chunks across numbered properties
//! (`c` = chunk count, `p0`, `p1`, …) and reassembled on receipt. JSON of the
//! payload is pure ASCII (hex/integer arrays + field names), so byte-chunking
//! never splits a multi-byte char.

use std::collections::HashSet;
use std::sync::Mutex;

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::announce::AnnouncePayload;
use crate::error::MeshError;
use crate::transport::{
    reassemble_txt, MeshTransport, ReceivedAnnouncement, CHUNK_BYTES, CHUNK_COUNT_KEY,
};

/// The Conspect mDNS service type (RFC 6763). The leading `_conspect-mesh`
/// service name + `_udp` (mesh announcements are connectionless) under the mDNS
/// `.local.` domain.
pub const SERVICE_TYPE: &str = "_conspect-mesh._udp.local.";

/// The default mDNS announce port advertised in the SRV record. The payload
/// rides the TXT record; the port is informational (the mesh carries lease
/// artefacts over a separate, operator-confirmed channel — O1). A fixed,
/// registered-range default; overridable by the caller.
pub const DEFAULT_PORT: u16 = 5354;

/// The live mDNS announce + browse transport.
///
/// Owns the [`ServiceDaemon`] and the browse receiver. The announce instance
/// name is the machine's stable hex peer id (so re-announcing updates the same
/// record). The browse receiver is drained non-blocking on each
/// [`MeshTransport::poll_received`].
pub struct MdnsService {
    daemon: ServiceDaemon,
    /// The browse-event receiver (drained non-blocking).
    browse: Receiver<ServiceEvent>,
    /// This machine's announce instance name (its hex peer id) + advertised host.
    instance: String,
    host: String,
    port: u16,
    /// The fullnames currently registered, so a re-announce unregisters the prior
    /// record first (idempotent update). Guarded by a `Mutex` — touched only off
    /// the engine, control-plane only.
    registered: Mutex<HashSet<String>>,
}

impl MdnsService {
    /// Start the mDNS daemon, begin browsing the Conspect service type, and
    /// prepare to announce under `instance` (the machine's hex peer id) on `host`
    /// (a `.local.` hostname) and `port`.
    ///
    /// # Errors
    /// [`MeshError::Transport`] if the daemon could not start or the browse could
    /// not begin (e.g. no multicast-capable interface). Best-effort: the caller
    /// logs + continues; discovery simply yields no peers.
    pub fn start(instance: &str, host: &str, port: u16) -> Result<Self, MeshError> {
        let daemon = ServiceDaemon::new().map_err(|e| MeshError::Transport(e.to_string()))?;
        let browse = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| MeshError::Transport(e.to_string()))?;
        Ok(Self {
            daemon,
            browse,
            instance: instance.to_owned(),
            host: host.to_owned(),
            port,
            registered: Mutex::new(HashSet::new()),
        })
    }

    /// Split the wire bytes into the numbered TXT properties (`c` + `p0`…`pN`).
    fn chunk_properties(wire: &[u8]) -> Vec<(String, String)> {
        let chunks: Vec<&[u8]> = wire.chunks(CHUNK_BYTES).collect();
        let mut props: Vec<(String, String)> = Vec::with_capacity(chunks.len() + 1);
        props.push((CHUNK_COUNT_KEY.to_owned(), chunks.len().to_string()));
        for (i, chunk) in chunks.iter().enumerate() {
            // The chunk is ASCII JSON bytes; `from_utf8` cannot fail here, but the
            // guardrails forbid `unwrap`, so a non-UTF8 chunk (impossible) is
            // skipped rather than panicking.
            if let Ok(text) = std::str::from_utf8(chunk) {
                props.push((format!("p{i}"), text.to_owned()));
            }
        }
        props
    }

    /// Reassemble the wire bytes from a resolved service's TXT properties, or
    /// `None` if the chunking is absent/inconsistent (a foreign or malformed
    /// announcement is ignored, never panicked on).
    fn reassemble(resolved: &mdns_sd::ResolvedService) -> Option<Vec<u8>> {
        reassemble_txt(|key| resolved.get_property_val_str(key))
    }
}

impl MeshTransport for MdnsService {
    fn announce(&self, wire: &[u8]) -> Result<(), MeshError> {
        let props = Self::chunk_properties(wire);
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &self.instance,
            &self.host,
            (),
            self.port,
            &props[..],
        )
        .map_err(|e| MeshError::Transport(e.to_string()))?
        // Let the library track interface address changes (IPv6 + IPv4) so the
        // announce follows the host's live addresses (dual-stack, IPv6-first).
        .enable_addr_auto();

        let fullname = info.get_fullname().to_owned();
        // Idempotent update: unregister a prior record for this instance first.
        if let Ok(mut registered) = self.registered.lock() {
            if registered.contains(&fullname) {
                let _ = self.daemon.unregister(&fullname);
            }
            registered.insert(fullname);
        }
        self.daemon
            .register(info)
            .map_err(|e| MeshError::Transport(e.to_string()))
    }

    fn poll_received(&self) -> Result<Vec<ReceivedAnnouncement>, MeshError> {
        let mut out = Vec::new();
        // Non-blocking drain: `try_recv` returns immediately when empty, so the
        // poll never blocks on the network (invariant #10).
        while let Ok(event) = self.browse.try_recv() {
            if let ServiceEvent::ServiceResolved(resolved) = event {
                // Skip our own announcement (same instance fullname).
                if resolved.get_fullname().starts_with(&self.instance) {
                    continue;
                }
                if let Some(wire) = Self::reassemble(&resolved) {
                    out.push(ReceivedAnnouncement::new(wire));
                }
            }
        }
        Ok(out)
    }
}

impl Drop for MdnsService {
    fn drop(&mut self) {
        // Best-effort shutdown of the daemon thread; errors are ignored (the
        // process is tearing down). Never panics in Drop.
        let _ = self.daemon.shutdown();
    }
}

/// Decode a reassembled received announcement into an untrusted payload.
///
/// Signature verification happens at the lease-install layer against the pinned
/// server key; this transport helper performs structural decoding only.
///
/// # Errors
/// [`MeshError::MalformedPayload`] if the bytes do not decode.
pub fn decode_received(received: &ReceivedAnnouncement) -> Result<AnnouncePayload, MeshError> {
    received.decode()
}
