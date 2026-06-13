//! The shared sans-IO **TURN relay driver** (ADR-0048 §5.1).
//!
//! Both endpoint drivers — the WHIP-ingest endpoint ([`crate::transport`], async
//! tokio over the shared UDP socket) and the WHEP-egress preview driver (in the
//! cli, a sync socket loop) — must run their configured TURN clients over their
//! own socket and harvest the allocated relay address so str0m can offer it as a
//! relay candidate. [`TurnRelayDriver`] wraps one [`TurnClient`] per configured
//! TURN server with the build / pump / feed / harvest steps both endpoints need,
//! so the [`TurnClient`] (the shared machinery) is reused and the driving glue
//! lives in one place — never re-implemented or duplicated per endpoint.
//!
//! It is **pure / sans-IO**: it never owns a socket. The caller pulls outbound
//! datagrams with [`TurnRelayDriver::poll_transmit`] and sends them on its own
//! socket; pushes received datagrams in with [`TurnRelayDriver::feed`]; and reads
//! the newly-learned relays with [`TurnRelayDriver::take_new_relays`]. This keeps
//! it offline-testable against the in-process fake TURN server, and lets the sync
//! (WHEP) and async (WHIP) endpoints drive it the same way.

use std::net::SocketAddr;
use std::time::Instant;

use crate::config::{EndpointConfig, IceServerKind};
use crate::turn::{TurnClient, TurnEvent, TurnOutput};

/// The number of queued transmits drained from one client per
/// [`TurnRelayDriver::poll_transmit`] internal pass — bounded so a misbehaving
/// client can never spin the caller's loop (the client serializes one request at
/// a time, so this ceiling is ample).
const MAX_TRANSMITS_PER_CLIENT: usize = 8;

/// A driven TURN client wrapping the shared sans-IO [`TurnClient`].
struct DrivenClient {
    client: TurnClient,
}

/// Runs the configured TURN clients sans-IO and harvests their relay candidates.
///
/// Build with [`Self::from_config`]; drive with [`Self::poll_transmit`] +
/// [`Self::feed`]; collect learned relays with [`Self::take_new_relays`]. Empty
/// when no TURN server is configured (the common self-hosted / port-forwarded
/// case), in which case every method is a cheap no-op.
pub struct TurnRelayDriver {
    clients: Vec<DrivenClient>,
    /// Relays learned since the last [`Self::take_new_relays`] drain, de-duped
    /// against everything already handed out.
    pending_relays: Vec<SocketAddr>,
    /// Every relay ever harvested (de-dup set; a re-allocation or a relay seen via
    /// both `feed` and `poll_transmit` is published once).
    known_relays: Vec<SocketAddr>,
}

impl std::fmt::Debug for TurnRelayDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnRelayDriver")
            .field("clients", &self.clients.len())
            .field("known_relays", &self.known_relays.len())
            .finish_non_exhaustive()
    }
}

impl TurnRelayDriver {
    /// Build a [`TurnClient`] per configured TURN server (ADR-0048 §5.1). The
    /// per-allocation credential is resolved `now` (ephemeral REST derives a
    /// time-limited username/password from the wall clock; long-term uses the
    /// static pair). STUN servers need no client here (str0m's server-reflexive
    /// candidates are gathered from the bound/advertised addresses). Empty when no
    /// TURN server is configured.
    #[must_use]
    pub fn from_config(config: &EndpointConfig, now: Instant) -> Self {
        // A wall-clock seconds value for the ephemeral-REST expiry derivation. The
        // monotonic `now` is the driver's tick clock; the REST username's expiry
        // is a unix time, so use the system clock here (a credential-derivation
        // detail — not a media-timeline clock, so this is not invariant-#3
        // territory).
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let _ = now;
        let clients = config
            .ice_servers
            .iter()
            .filter(|s| s.kind == IceServerKind::Turn)
            .filter_map(|server| {
                let credential = server.credentials.as_ref()?.resolve(now_unix);
                Some(DrivenClient {
                    client: TurnClient::new(server.addr, credential),
                })
            })
            .collect();
        Self {
            clients,
            pending_relays: Vec::new(),
            known_relays: Vec::new(),
        }
    }

    /// The number of driven TURN clients (one per configured TURN server).
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Whether there are no TURN clients to drive (no TURN server configured).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Pull the next outbound datagram the driver wants sent (to a TURN server),
    /// or `None` when every client is idle/waiting. The caller sends it on its own
    /// socket. Each call drains at most one datagram across the clients; call in a
    /// `while let Some(..)` to flush a tick. Also harvests any relay a client has
    /// reached via a `poll_output`-driven transition.
    #[must_use]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        for driven in &mut self.clients {
            for _ in 0..MAX_TRANSMITS_PER_CLIENT {
                match driven.client.poll_output(now) {
                    TurnOutput::Transmit {
                        destination,
                        payload,
                    } => {
                        Self::harvest(
                            &mut self.pending_relays,
                            &mut self.known_relays,
                            driven.client.relay(),
                        );
                        return Some((destination, payload));
                    }
                    TurnOutput::Timeout(_) | TurnOutput::Idle => break,
                }
            }
            Self::harvest(
                &mut self.pending_relays,
                &mut self.known_relays,
                driven.client.relay(),
            );
        }
        None
    }

    /// Feed a received datagram into the client that owns `src` (a configured TURN
    /// server). Returns `true` if a TURN client consumed it (the datagram was a
    /// TURN reply / relayed-data indication); `false` means it was **not** from a
    /// TURN server and the caller should route it to the media path. A successful
    /// `Allocate` (or a relay reached as a side effect) is harvested.
    pub fn feed(&mut self, src: SocketAddr, payload: &[u8], now: Instant) -> bool {
        for driven in &mut self.clients {
            if driven.client.server_addr() != src {
                continue;
            }
            if let Ok(Some(TurnEvent::Allocated(relay))) = driven.client.handle_input(payload, now)
            {
                Self::push_relay(&mut self.pending_relays, &mut self.known_relays, relay);
            }
            // Also catch a relay reached without an explicit `Allocated` event
            // surfacing here (e.g. it was already learned on a prior pass).
            Self::harvest(
                &mut self.pending_relays,
                &mut self.known_relays,
                driven.client.relay(),
            );
            return true;
        }
        false
    }

    /// Drain and return the relays learned since the last call (each relay is
    /// returned exactly once over the driver's life). The caller registers each as
    /// a str0m relay candidate (WHIP: per future negotiation; WHEP:
    /// `WhepEgress::learn_relay`).
    #[must_use]
    pub fn take_new_relays(&mut self) -> Vec<SocketAddr> {
        std::mem::take(&mut self.pending_relays)
    }

    /// Harvest `relay` (if any) into the pending+known sets, de-duped.
    fn harvest(
        pending: &mut Vec<SocketAddr>,
        known: &mut Vec<SocketAddr>,
        relay: Option<SocketAddr>,
    ) {
        if let Some(relay) = relay {
            Self::push_relay(pending, known, relay);
        }
    }

    /// Record `relay` as newly learned unless it was already published.
    fn push_relay(pending: &mut Vec<SocketAddr>, known: &mut Vec<SocketAddr>, relay: SocketAddr) {
        if known.contains(&relay) {
            return;
        }
        known.push(relay);
        pending.push(relay);
    }
}
