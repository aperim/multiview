//! The mesh **transport seam** — the trait that isolates the mDNS socket so the
//! announce/browse logic is testable **offline** (ADR-0051 §1, brief §13.2).
//!
//! The pure logic (build a signed [`AnnouncePayload`](crate::announce::AnnouncePayload),
//! verify a received one, fold an observation into the [`PeerTable`](crate::peer::PeerTable))
//! depends only on this trait, never on a concrete socket. The live mDNS-sd
//! implementation lives behind the off-by-default `mdns` feature
//! ([`crate::service`]); an in-memory fake drives the logic in unit tests with no
//! network. This is the documented pattern for live-socket code: the socket layer
//! is a trait, the logic is pure, and the live-network test is `#[ignore]`d +
//! hardware-gated.

use crate::announce::AnnouncePayload;
use crate::error::MeshError;

/// A received mesh announcement from the transport: the raw wire bytes of a
/// peer's TXT-record payload. The logic decodes + verifies it; the transport
/// never interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReceivedAnnouncement {
    /// The raw wire bytes the transport observed (decoded by the logic via
    /// [`AnnouncePayload::from_wire`]).
    pub wire: Vec<u8>,
}

impl ReceivedAnnouncement {
    /// Wrap observed wire bytes.
    #[must_use]
    pub fn new(wire: Vec<u8>) -> Self {
        Self { wire }
    }

    /// Decode the observed bytes into a payload (a typed error on garbage).
    ///
    /// # Errors
    /// [`MeshError::MalformedPayload`] if the bytes are not a well-formed
    /// announce payload.
    pub fn decode(&self) -> Result<AnnouncePayload, MeshError> {
        AnnouncePayload::from_wire(&self.wire)
    }
}

/// The transport seam: publish this machine's announcement, and report observed
/// peer announcements. Implemented by the live mDNS-sd service ([`crate::service`],
/// `mdns` feature) and by an in-memory fake in tests.
///
/// Every method is **best-effort**: a transport failure is a typed error the
/// caller logs and carries on from — the mesh never stalls anything (invariant
/// #10). The trait holds no engine handle.
pub trait MeshTransport {
    /// Publish (or re-publish) this machine's announcement carrying `wire` (the
    /// encoded [`AnnouncePayload`]). Idempotent: re-announcing updates the TXT
    /// record.
    ///
    /// # Errors
    /// [`MeshError::Transport`] if the transport could not publish (e.g. the
    /// socket is down). Best-effort — the caller logs + retries on the next tick.
    fn announce(&self, wire: &[u8]) -> Result<(), MeshError>;

    /// Drain the announcements observed since the last poll (non-blocking). An
    /// empty vec means nothing new. Never blocks on the network — a poll that
    /// finds nothing returns immediately.
    ///
    /// # Errors
    /// [`MeshError::Transport`] if the transport could not be polled.
    fn poll_received(&self) -> Result<Vec<ReceivedAnnouncement>, MeshError>;
}
