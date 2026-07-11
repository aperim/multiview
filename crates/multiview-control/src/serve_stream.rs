//! Shutdown-aware, guard-owning transport wrapper for the control-plane serve loop
//! (SEC-14 #126 R2 / [ADR-W031](../../../docs/decisions/ADR-W031.md)).
//!
//! [`TrackedStream`] wraps an accepted connection stream (a `TcpStream`, or a
//! `TlsStream` **after** its handshake) so that:
//!
//! * it **owns** the accept-level
//!   [`ConnectionGuard`](crate::limits::ConnectionGuard), so the population-cap slot
//!   (global + per-IP) is held for the connection's *real* life — including after an
//!   HTTP/1 **Upgrade**. hyper moves the wrapped IO into its `Upgraded`, so the guard
//!   rides into the (detached) WebSocket task and releases only when that socket
//!   finally closes. Without this the guard would drop the instant `serve_connection`
//!   returns — which is at the upgrade handshake, not the WebSocket's end — so
//!   sequential WebSocket upgrades would silently bypass the per-IP population cap
//!   (the F1 escape).
//!
//! * it is **shutdown-aware**: once the serve loop's shutdown `watch` flips (or its
//!   sender drops), reads return EOF and writes fail. A live upgraded WebSocket is a
//!   detached task the drain [`JoinSet`](tokio::task::JoinSet) does not track, so it
//!   cannot be aborted at the ceiling; instead it observes the shutdown *through its
//!   own transport* and drains cooperatively (the [`run_ws_session`] loop selects on
//!   `socket.recv()`, which then returns end-of-stream). serve() does **not**
//!   synchronously await that detached task — the drain is prompt-and-cooperative, not
//!   awaited.
//!
//! The shutdown `watch` waker is registered so a **parked** read/write wakes the
//! instant shutdown is signalled, not only on the next inbound byte — the future owns
//! its receiver (moved in), so its registration persists across polls (a fresh
//! `changed()` per poll would deregister on drop and miss the signal).
//!
//! [`run_ws_session`]: crate::realtime

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;

use crate::limits::ConnectionGuard;

/// The error a shutdown-draining [`TrackedStream`] fails writes with, so a connection
/// (or upgraded socket) blocked on a write unblocks and tears down at drain rather than
/// waiting on a peer that will never read again.
fn shutdown_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "control-plane serve loop is shutting down",
    )
}

/// A connection-stream wrapper that owns the accept-level admission guard (so it rides
/// an HTTP/1 upgrade into the detached WebSocket task) and cooperatively drains on the
/// serve loop's shutdown signal. See the module docs.
pub(crate) struct TrackedStream<S> {
    /// The wrapped transport (a `TcpStream`, or a `TlsStream` post-handshake).
    inner: S,
    /// The accept-level admission guard, held for this stream's whole life — including
    /// after an upgrade, since hyper moves this wrapper into its `Upgraded`. `None` when
    /// no population cap is configured. Released by `Drop` (RAII).
    _guard: Option<ConnectionGuard>,
    /// Resolves when the serve loop signals shutdown (the `watch` value flips to `true`,
    /// or its sender drops). Owns its [`watch::Receiver`] (moved in), so its waker
    /// registration persists across polls — a parked read/write wakes the instant
    /// shutdown fires, not merely on the next inbound byte.
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Latches once `shutdown` has resolved: a completed future must not be polled
    /// again, and every subsequent read is EOF / write is an error.
    draining: bool,
}

impl<S> TrackedStream<S> {
    /// Wrap `inner`, taking ownership of the admission `guard` and arming shutdown
    /// awareness on `shutdown`.
    pub(crate) fn new(
        inner: S,
        guard: Option<ConnectionGuard>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        // Own the receiver inside the future so its waker registration persists across
        // polls. Resolves on the flip to `true` OR the sender dropping (serve() gone,
        // which `wait_for` surfaces as `Err` — either way the connection must drain).
        let mut shutdown = shutdown;
        let shutdown = Box::pin(async move {
            let _ = shutdown.wait_for(|signalled| *signalled).await;
        });
        Self {
            inner,
            _guard: guard,
            shutdown,
            draining: false,
        }
    }

    /// Poll the shutdown signal, latching `draining` once it fires, and report whether
    /// the serve loop is now draining. Registers `cx`'s waker so a caller parked on the
    /// inner transport wakes the instant shutdown is signalled. Never polls the
    /// completed future twice (the `draining` latch guards it).
    fn poll_draining(&mut self, cx: &mut Context<'_>) -> bool {
        if self.draining {
            return true;
        }
        if self.shutdown.as_mut().poll(cx).is_ready() {
            self.draining = true;
        }
        self.draining
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TrackedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // On shutdown, signal EOF (`Ready(Ok(()))` without filling `buf`): hyper — or,
        // post-upgrade, the WebSocket reading through this transport — sees
        // end-of-stream and drains. `poll_draining` registers the waker, so a read
        // otherwise parked on `inner` still wakes the instant shutdown fires.
        if this.poll_draining(cx) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TrackedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.poll_draining(cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.poll_draining(cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.poll_draining(cx) {
            return Poll::Ready(Err(shutdown_error()));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Closing the write half is how the peer is told we are done — always let it
        // proceed on the inner transport rather than synthesising an error.
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}
