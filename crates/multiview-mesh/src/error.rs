//! The mesh error taxonomy (per-crate `Error` enum via `thiserror`, conventions
//! §9).

/// Why a mesh operation failed. `#[non_exhaustive]` so new variants add without
/// breaking callers (the wire surfaces are versioned resources).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MeshError {
    /// An announce payload could not be encoded/decoded for the wire (garbage or
    /// truncated bytes). A typed error, never a panic
    /// (bad-inputs-are-the-purpose).
    #[error("malformed announce payload: {0}")]
    MalformedPayload(String),

    /// An announce signature did not verify against the presented originator key
    /// (a spoofed or tampered announcement, or a malformed signature).
    #[error("announce signature verification failed")]
    BadSignature,

    /// The TXT-record encoding of an mDNS announcement was missing the payload
    /// key, or the value was not decodable (live mDNS path only).
    #[error("mdns announcement carried no decodable payload")]
    NoPayload,

    /// The live mDNS transport failed to start or service a request (socket bind,
    /// daemon error). Best-effort: the caller logs + carries on (invariant #10 —
    /// a mesh failure never stalls anything).
    #[error("mdns transport error: {0}")]
    Transport(String),
}
