//! ST 2110 UDP receive transport — **feature `st2110`, compile-verified only**.
//!
//! This is the thin socket binding that drives the pure depacketizers in the
//! parent module. It is gated behind the off-by-default **`st2110`** feature
//! because it needs a real ST 2110 network (multicast groups, a PTP-disciplined
//! NIC); this devcontainer has none, so the code here is **compile-verified
//! only** — its correctness rests entirely on the pure, fully-tested
//! [`super::rtp`] / [`super::v20`] / [`super::v30`] / [`super::v40`] parsers and
//! the [`crate::st2022_7`] reconstructor it feeds.
//!
//! It deliberately does **no** native FFI (uses `tokio`'s safe `UdpSocket`),
//! keeping `multiview-input` `unsafe_code = forbid`, and it never paces the output
//! clock: received datagrams are depacketized and handed to the ingest pipeline
//! (which writes the last-good-frame store), exactly like every other source
//! (invariants #1 / #10).

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use crate::error::{Error, Result};
use crate::st2022_7::{HitlessReconstructor, Path};
use crate::st2110::rtp::RtpPacket;

/// The largest UDP datagram an ST 2110 receiver will buffer. ST 2110-20 uses a
/// ~1500-byte standard MTU (or up to ~9000 jumbo); 9 KiB covers both with margin.
pub const MAX_DATAGRAM: usize = 9216;

/// A single-path ST 2110 RTP receive socket.
///
/// Binds a UDP socket and (optionally) joins a source-specific multicast group,
/// then yields raw RTP packets one datagram at a time. Parsing is delegated to
/// the pure [`RtpPacket::parse`]; this type owns only the socket and a receive
/// buffer.
#[derive(Debug)]
pub struct RtpReceiver {
    socket: UdpSocket,
    buf: Vec<u8>,
}

impl RtpReceiver {
    /// Bind a receive socket to `local`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 bind {local}: {e}")))?;
        Ok(Self {
            socket,
            buf: vec![0u8; MAX_DATAGRAM],
        })
    }

    /// Join an IPv4 multicast `group` on the interface address `interface`.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the membership cannot be joined.
    pub fn join_multicast_v4(
        &self,
        group: std::net::Ipv4Addr,
        interface: std::net::Ipv4Addr,
    ) -> Result<()> {
        self.socket
            .join_multicast_v4(group, interface)
            .map_err(|e| Error::Ingest(format!("st2110 join {group}: {e}")))
    }

    /// Receive one datagram and return the number of payload bytes read into the
    /// internal buffer; the bytes are then available via [`RtpReceiver::buffer`].
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] if the receive fails.
    pub async fn recv(&mut self) -> Result<usize> {
        let n = self
            .socket
            .recv(&mut self.buf)
            .await
            .map_err(|e| Error::Ingest(format!("st2110 recv: {e}")))?;
        Ok(n)
    }

    /// The first `len` bytes of the last received datagram.
    #[must_use]
    pub fn buffer(&self, len: usize) -> &[u8] {
        let end = len.min(self.buf.len());
        self.buf.get(..end).unwrap_or(&[])
    }

    /// Receive one datagram and parse it as an RTP packet, returning the parsed
    /// view borrowed from the internal buffer.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or an RTP parse failure.
    pub async fn recv_rtp(&mut self) -> Result<RtpPacket<'_>> {
        let n = self.recv().await?;
        let end = n.min(self.buf.len());
        let bytes = self.buf.get(..end).unwrap_or(&[]);
        RtpPacket::parse(bytes).map_err(|e| Error::Ingest(format!("st2110 rtp: {e}")))
    }
}

/// A two-path ST 2022-7 receiver: two [`RtpReceiver`]s feeding one
/// [`HitlessReconstructor`] keyed by RTP sequence number.
///
/// `P` is the per-packet payload the caller extracts from each datagram (e.g. an
/// owned copy of the RTP payload). The reconstructor lives in the always-compiled
/// pure module; this struct is only the socket wiring around it.
#[derive(Debug)]
pub struct DualPathReceiver<P> {
    path_a: RtpReceiver,
    path_b: RtpReceiver,
    reconstructor: HitlessReconstructor<P>,
}

impl<P> DualPathReceiver<P> {
    /// Build a dual-path receiver from two bound sockets and a reorder-window
    /// capacity.
    #[must_use]
    pub fn new(path_a: RtpReceiver, path_b: RtpReceiver, window: usize) -> Self {
        Self {
            path_a,
            path_b,
            reconstructor: HitlessReconstructor::new(window),
        }
    }

    /// Receive concurrently from both paths; whichever delivers first is parsed,
    /// its payload extracted by `extract`, pushed into the reconstructor, and the
    /// merged in-order packets returned.
    ///
    /// `extract` maps a parsed [`RtpPacket`] to the caller's payload `P` (e.g. an
    /// owned payload copy). The merge/de-dup/reorder is performed by the pure
    /// reconstructor.
    ///
    /// # Errors
    ///
    /// [`Error::Ingest`] on a socket error or RTP parse failure on the path that
    /// fired.
    pub async fn recv_merged<F>(&mut self, mut extract: F) -> Result<Vec<P>>
    where
        F: FnMut(&RtpPacket<'_>) -> P,
    {
        let (path, packet) = tokio::select! {
            r = self.path_a.recv_rtp() => (Path::A, r?),
            r = self.path_b.recv_rtp() => (Path::B, r?),
        };
        let payload = extract(&packet);
        let _ = self
            .reconstructor
            .push(path, packet.header.sequence, payload);
        Ok(self.reconstructor.drain())
    }

    /// Borrow the underlying reconstructor (for metrics / window inspection).
    #[must_use]
    pub const fn reconstructor(&self) -> &HitlessReconstructor<P> {
        &self.reconstructor
    }
}
