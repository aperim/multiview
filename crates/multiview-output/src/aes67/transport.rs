//! The AES67 / ST 2110-30 UDP **send transport** — feature `aes67`.
//!
//! The thin, off-hot-path tokio `UdpSocket` layer that carries the
//! [`Aes67Sender`](super::sender::Aes67Sender)'s continuous marker=0 RTP packets
//! onto the wire. It owns only the socket and the destination; the packetization
//! (the pure [`super::packet`] / [`super::sender`]) is what carries the
//! correctness load and is tested with no NIC.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! [`Aes67UdpSender::serve`] runs on its **own** timer (never the output-clock
//! loop), draining one packet per `ptime` tick and `send_to`-ing it. It samples
//! the sender's bounded drop-oldest FIFO — a stalled socket can only drop the
//! oldest buffered frames, never back-pressure the engine. `send_to` never runs
//! on the output-clock loop.
//!
//! IPv6-first (ADR-0042): bind a dual-stack or IPv6 local address and send to a
//! bracketed IPv6 group for real multicast; loopback validation uses `[::1]` /
//! `127.0.0.1`. Multicast group join + TTL/hop-limit 255 for a real deployment
//! is wired by the engine at bind time (the send socket needs no group
//! membership); this transport carries the datagrams.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::UdpSocket;

use crate::error::{Error, Result};

use super::sender::Aes67Sender;

/// A bound UDP send socket for one AES67 / ST 2110-30 flow.
#[derive(Debug)]
pub struct Aes67UdpSender {
    socket: UdpSocket,
    dest: SocketAddr,
}

impl Aes67UdpSender {
    /// Bind a send socket to `local` and target `dest` (the multicast group +
    /// port, or a unicast peer for loopback validation).
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if the socket cannot be bound.
    pub async fn bind(local: SocketAddr, dest: SocketAddr) -> Result<Self> {
        let socket = UdpSocket::bind(local)
            .await
            .map_err(|e| Error::Output(format!("aes67 bind {local}: {e}")))?;
        Ok(Self { socket, dest })
    }

    /// The destination this sender targets.
    #[must_use]
    pub const fn dest(&self) -> SocketAddr {
        self.dest
    }

    /// Set the IPv4 multicast TTL for a real multicast deployment (ADR-0033 §5 /
    /// ADR-0041 §7 use TTL 255). No-op for a unicast loopback destination.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if the socket option cannot be set.
    pub fn set_multicast_ttl_v4(&self, ttl: u32) -> Result<()> {
        self.socket
            .set_multicast_ttl_v4(ttl)
            .map_err(|e| Error::Output(format!("aes67 set multicast ttl: {e}")))
    }

    /// Send one already-built RTP packet to the destination.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] on a socket send failure.
    pub async fn send_packet(&self, packet: &[u8]) -> Result<()> {
        self.socket
            .send_to(packet, self.dest)
            .await
            .map(|_sent| ())
            .map_err(|e| Error::Output(format!("aes67 send_to {}: {e}", self.dest)))
    }

    /// Drive `sender` on an independent `ptime` timer, sending one continuous
    /// marker=0 packet per tick until `stop` resolves or the socket faults.
    ///
    /// This is the off-hot-path send loop: it samples the sender's bounded
    /// drop-oldest FIFO and never paces the engine (invariants #1 / #10). The
    /// engine wires the program bus into `sender` (via
    /// [`Aes67Sender::push`](super::sender::Aes67Sender::push)); this loop only
    /// drains and transmits.
    ///
    /// # Errors
    ///
    /// [`Error::Output`] if a `send_to` fails (the supervisor reconnects).
    pub async fn serve<S>(&self, sender: &mut Aes67Sender, ptime: Duration, stop: S) -> Result<()>
    where
        S: std::future::Future<Output = ()>,
    {
        tokio::pin!(stop);
        let mut ticker = tokio::time::interval(ptime.max(Duration::from_micros(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let packet = sender.next_packet();
                    self.send_packet(&packet).await?;
                }
                () = &mut stop => return Ok(()),
            }
        }
    }
}
