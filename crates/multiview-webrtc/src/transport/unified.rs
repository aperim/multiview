//! The single-socket [`UnifiedEndpoint`] (ADR-0048 §4) — **feature `native`**.
//!
//! ADR-0048 mandates **one** process-wide dual-stack UDP socket adopted by **all**
//! WebRTC roles: WHIP ingest publishers, WHEP preview viewers, WHEP output
//! viewers, and the outbound `whip_push` client. Before this, the cli bound the
//! `webrtc.udp_port` once per role (preview + WHIP + WHEP-serve + each whip_push),
//! so the 2nd/3rd `bind` hit `EADDRINUSE` and silently degraded those roles to
//! "unavailable" — with preview + WHIP + WHEP-serve in one config, ingest and
//! output-serve were dead (box-validation defect B).
//!
//! [`UnifiedEndpoint`] fixes that: it binds the **one** socket, owns the **one**
//! [`TurnRelayDriver`] (so every role shares the relay allocations), and runs a
//! **single** driver task that demultiplexes every inbound datagram to the right
//! role's session by str0m's ufrag/peer demux (`Rtc::accepts()`) — exactly the
//! sans-IO single-socket pattern. Outbound datagrams are routed relay-aware (a
//! str0m `Transmit` whose source is an allocated relay is framed as a TURN Send
//! indication — defect C). No `SO_REUSEPORT`, one socket, one driver, many
//! sessions.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! The driver never `.await`s a peer (UDP send is non-blocking); per-role media
//! crosses only bounded drop-oldest rings (ingest `RtpRing`, egress `SampleFeed` /
//! `EgressFeed`). A wedged or saturated endpoint loses *preview/ingest/viewer
//! media*, never an output tick. Registration crosses bounded command channels.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::EndpointConfig;
use crate::egress::EgressFeed;
use crate::error::{Result, WebRtcError};
use crate::transport::whep_serve::{ServeLane, WhepServeHandle};
use crate::transport::whip_endpoint::{IngestLane, WhipEndpoint, WhipHandle};
use crate::transport::whip_push::{PushLane, WhipPushSpec};
use crate::transport::{WebRtcEndpoint, WhepServeEndpoint};
use crate::turn::TurnRelayDriver;

/// The maximum UDP datagram the unified recv loop reads.
const RECV_BUFFER: usize = 2048;

/// How often the driver wakes to advance timers, drain feeds, GC, and pump TURN
/// when otherwise idle. The tightest role cadence (WHEP egress) wins so program
/// AUs are packetized promptly.
const DRIVER_TICK: Duration = Duration::from_millis(10);

/// The single-socket WebRTC endpoint: ONE bound dual-stack UDP socket and ONE
/// TURN relay driver, shared by every WebRTC role (ADR-0048 §4).
///
/// Build with [`UnifiedEndpoint::bind`], register the roles your config needs
/// (preview egress, WHIP ingest, WHEP-serve outputs, `whip_push` outputs) via the
/// returned [`UnifiedBuilder`], then run the one driver task with
/// [`UnifiedEndpoint::run`].
pub struct UnifiedEndpoint {
    endpoint: WebRtcEndpoint,
    /// The single TURN relay driver shared by every role (one allocation set).
    config: EndpointConfig,
    /// The WHIP ingest lane (registered when the config has `webrtc` sources).
    ingest: Option<IngestLane>,
    /// The WHEP output-serve lane (registered when the config has `webrtc` outputs).
    serve: Option<ServeLane>,
    /// The native WHEP preview egress (registered when preview is wired).
    preview: Option<Arc<crate::whep_egress::WhepEgress>>,
    /// The `whip_push` client lanes (one per configured `whip_push` output).
    push: Vec<PushLane>,
}

impl std::fmt::Debug for UnifiedEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedEndpoint")
            .field("endpoint", &self.endpoint)
            .field("has_ingest", &self.ingest.is_some())
            .field("has_serve", &self.serve.is_some())
            .field("has_preview", &self.preview.is_some())
            .field("push_lanes", &self.push.len())
            .finish_non_exhaustive()
    }
}

/// A builder that wires the WebRTC roles onto the one shared socket before the
/// driver task is spawned. Each `with_*` call returns the role's negotiation
/// handle (or the shared preview egress) so the cli can connect it to the control
/// plane / pipeline, exactly as the per-role endpoints did — but all on one socket.
pub struct UnifiedBuilder {
    endpoint: UnifiedEndpoint,
    host_candidates: Vec<SocketAddr>,
}

impl std::fmt::Debug for UnifiedBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedBuilder")
            .field("host_candidates", &self.host_candidates)
            .finish_non_exhaustive()
    }
}

impl UnifiedEndpoint {
    /// Bind the single dual-stack socket and start a [`UnifiedBuilder`].
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] / [`WebRtcError::Config`] if the bind fails.
    pub fn bind(config: EndpointConfig) -> Result<UnifiedBuilder> {
        let endpoint = WebRtcEndpoint::bind(config.clone())?;
        let host_candidates = endpoint.host_candidates()?;
        Ok(UnifiedBuilder {
            endpoint: Self {
                endpoint,
                config,
                ingest: None,
                serve: None,
                preview: None,
                push: Vec::new(),
            },
            host_candidates,
        })
    }

    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn host_candidates(&self) -> Result<Vec<SocketAddr>> {
        self.endpoint.host_candidates()
    }
}

impl UnifiedBuilder {
    /// The gathered host candidate addresses (IPv6-first), shared by every role.
    #[must_use]
    pub fn host_candidates(&self) -> &[SocketAddr] {
        &self.host_candidates
    }

    /// The local address the single shared socket is bound to (the relay
    /// candidate's `raddr` base; the bound host candidate the preview egress uses).
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if the local address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.endpoint.local_addr()
    }

    /// Register the WHIP **ingest** lane on the shared socket, returning the
    /// [`WhipHandle`] the control plane negotiates publishers against.
    #[must_use]
    pub fn with_ingest(mut self) -> (Self, WhipHandle) {
        let (handle, commands, shared) =
            WhipHandle::build(self.endpoint.config.clone(), self.host_candidates.clone());
        self.endpoint.ingest = Some(IngestLane {
            commands,
            shared,
            sessions: Vec::new(),
        });
        (self, handle)
    }

    /// Register the WHEP **output-serve** lane on the shared socket, returning the
    /// [`WhepServeHandle`] the control plane negotiates viewers against.
    #[must_use]
    pub fn with_serve(mut self) -> (Self, WhepServeHandle) {
        let (handle, commands, shared) =
            WhepServeHandle::build(self.endpoint.config.clone(), self.host_candidates.clone());
        self.endpoint.serve = Some(ServeLane {
            commands,
            shared,
            viewers: Vec::new(),
        });
        (self, handle)
    }

    /// Register the native **WHEP preview egress** on the shared socket. The
    /// returned [`WhepEgress`](crate::whep_egress::WhepEgress) is the transport the
    /// cli's `CliWhepProvider` negotiates preview viewers against; the driver pumps
    /// its egress + fans inbound datagrams to it.
    #[must_use]
    pub fn with_preview(mut self, preview: Arc<crate::whep_egress::WhepEgress>) -> Self {
        self.endpoint.preview = Some(preview);
        self
    }

    /// Register one `whip_push` **output** on the shared socket: the program egress
    /// `feed` it publishes, whether it carries audio, and the HTTP `signaller` that
    /// `POST`s the offer to the remote WHIP origin.
    #[must_use]
    pub fn with_push(
        mut self,
        feed: EgressFeed,
        audio: bool,
        signaller: Box<dyn crate::transport::whip_push::WhipSignaller>,
    ) -> Self {
        self.endpoint.push.push(PushLane::new(
            WhipPushSpec {
                feed,
                audio,
                host_candidates: self.host_candidates.clone(),
            },
            signaller,
        ));
        self
    }

    /// Finish wiring and return the [`UnifiedEndpoint`] ready to [`run`](UnifiedEndpoint::run).
    #[must_use]
    pub fn build(self) -> UnifiedEndpoint {
        self.endpoint
    }
}

impl UnifiedEndpoint {
    /// Run the single driver loop until `stop` is raised: ONE socket, ONE TURN
    /// driver, every role's sessions demuxed by str0m on each inbound datagram and
    /// pumped on each tick (ADR-0048 §4/§7). The live socket loop is hardware-gated
    /// for full media flow; the structure (demux, relay framing, isolation) is
    /// proven offline.
    ///
    /// # Errors
    ///
    /// [`WebRtcError::Socket`] if converting the bound socket to async fails.
    #[allow(
        clippy::too_many_lines,
        reason = "one select! loop owning the single socket per ADR-0048 §4 — \
                  splitting the arms would re-introduce per-role drivers"
    )]
    pub async fn run(self, stop: Arc<AtomicBool>) -> Result<()> {
        let bind_addr = self.endpoint.config().bind_addr();
        let local_addr = self.endpoint.local_addr()?;
        let mut turn = TurnRelayDriver::from_config(&self.config, Instant::now());

        let std_socket = self.endpoint.into_socket();
        std_socket
            .set_nonblocking(true)
            .map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;
        let socket =
            tokio::net::UdpSocket::from_std(std_socket).map_err(|source| WebRtcError::Socket {
                addr: bind_addr,
                source,
            })?;

        let mut ingest = self.ingest;
        let mut serve = self.serve;
        let preview = self.preview;
        let mut push = self.push;

        let mut buf = vec![0u8; RECV_BUFFER];
        let mut tick = tokio::time::interval(DRIVER_TICK);

        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }

            // Recv is the only awaited source besides the tick; commands are
            // drained non-blocking each pass so one closed channel never wedges the
            // others (each role lane is independent on the one socket).
            tokio::select! {
                recv = socket.recv_from(&mut buf) => {
                    let now = Instant::now();
                    if let Ok((len, src)) = recv {
                        if let Some(payload) = buf.get(..len) {
                            Self::route_inbound(
                                &mut turn,
                                ingest.as_mut(),
                                serve.as_mut(),
                                preview.as_ref(),
                                &mut push,
                                src,
                                local_addr,
                                payload,
                                now,
                            );
                        }
                    }
                    Self::pump_all(
                        &socket, &mut turn,
                        ingest.as_mut(), serve.as_mut(), preview.as_ref(), &mut push,
                        local_addr, now,
                    ).await;
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    // Drain pending register/release commands for each lane.
                    Self::drain_commands(ingest.as_mut(), serve.as_mut());
                    // Advance each role's timers / feeds.
                    if let Some(lane) = ingest.as_mut() {
                        WhipEndpoint::tick(&mut lane.sessions, now);
                    }
                    if let Some(lane) = serve.as_mut() {
                        WhepServeEndpoint::tick(&mut lane.viewers, &lane.shared, now);
                    }
                    Self::pump_all(
                        &socket, &mut turn,
                        ingest.as_mut(), serve.as_mut(), preview.as_ref(), &mut push,
                        local_addr, now,
                    ).await;
                    // GC dead sessions per lane.
                    if let Some(lane) = ingest.as_mut() {
                        WhipEndpoint::reap(&mut lane.sessions, &lane.shared);
                    }
                    if let Some(lane) = serve.as_mut() {
                        WhepServeEndpoint::reap(&mut lane.viewers, &lane.shared);
                    }
                }
            }
        }
    }

    /// Drain (non-blocking) every pending register/release command for the lanes,
    /// so a registration is admitted promptly without an awaited per-lane arm.
    fn drain_commands(ingest: Option<&mut IngestLane>, serve: Option<&mut ServeLane>) {
        if let Some(lane) = ingest {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhipEndpoint::apply_command(&mut lane.sessions, Some(cmd));
            }
        }
        if let Some(lane) = serve {
            while let Ok(cmd) = lane.commands.try_recv() {
                WhepServeEndpoint::apply_command(&mut lane.viewers, Some(cmd));
            }
        }
    }

    /// Route one inbound datagram to the role whose session accepts it. The relay
    /// classification (TURN control / relayed Data / media) happens once; relayed
    /// media is fanned to every role exactly like direct media (str0m's per-session
    /// ufrag/peer demux silently ignores a datagram not addressed to a session).
    #[allow(clippy::too_many_arguments, reason = "the single demux fans one datagram to every role lane")]
    fn route_inbound(
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        src: SocketAddr,
        local_addr: SocketAddr,
        payload: &[u8],
        now: Instant,
    ) {
        // Classify once: a relayed Data indication decapsulates to media arriving
        // on the relay addr; a TURN control reply feeds the relay driver; anything
        // else is media from `src` on the local socket addr (defect C).
        let (route_src, route_dst, bytes): (SocketAddr, SocketAddr, Vec<u8>) =
            match crate::transport::relay_io::classify_inbound(turn, src, payload, now) {
                crate::transport::relay_io::Inbound::Relayed { peer, relay, payload } => {
                    (peer, relay, payload)
                }
                crate::transport::relay_io::Inbound::TurnControl => return,
                crate::transport::relay_io::Inbound::Media => (src, local_addr, payload.to_vec()),
            };

        // Fan the (decapsulated) datagram to every registered role; str0m's demux
        // means only the owning session consumes it.
        if let Some(lane) = ingest {
            WhipEndpoint::route_datagram(&mut lane.sessions, route_src, route_dst, &bytes, now);
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::route_datagram(&mut lane.viewers, route_src, route_dst, &bytes, now);
        }
        if let Some(preview) = preview {
            let _ = preview.handle_datagram_broadcast(route_src, route_dst, &bytes, now);
        }
        for lane in push.iter_mut() {
            lane.handle_inbound(route_src, route_dst, &bytes, now);
        }
    }

    /// Pump every role's outbound + the shared TURN driver onto the one socket.
    #[allow(clippy::too_many_arguments, reason = "the single loop drives every role on one socket")]
    async fn pump_all(
        socket: &tokio::net::UdpSocket,
        turn: &mut TurnRelayDriver,
        ingest: Option<&mut IngestLane>,
        serve: Option<&mut ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        push: &mut [PushLane],
        local_addr: SocketAddr,
        now: Instant,
    ) {
        // Drive the shared TURN driver first so a fresh relay is published before
        // the roles gather candidates for the next negotiation.
        while let Some((destination, payload)) = turn.poll_transmit(now) {
            let _ = socket.send_to(&payload, destination).await;
        }
        let new_relays = turn.take_new_relays();
        if !new_relays.is_empty() {
            Self::publish_relays(&new_relays, ingest.as_deref(), serve.as_deref(), preview, local_addr);
        }

        if let Some(lane) = ingest {
            WhipEndpoint::pump_outbound(socket, &mut lane.sessions, turn, now).await;
        }
        if let Some(lane) = serve {
            WhepServeEndpoint::pump_outbound(socket, &mut lane.viewers, turn, now).await;
        }
        if let Some(preview) = preview {
            if let Ok(out) = preview.drive_all(now) {
                for (source, dst, payload) in out {
                    crate::transport::relay_io::send_routed(
                        socket, turn, source, dst, &payload, now,
                    )
                    .await;
                }
            }
        }
        for lane in push.iter_mut() {
            lane.step(socket, turn, local_addr, now).await;
        }
    }

    /// Publish freshly-learned relays to each role's relay-candidate sink so the
    /// next negotiation offers them (WHIP/WHEP-serve via `learned_relays`, preview
    /// via `WhepEgress::learn_relay`).
    fn publish_relays(
        relays: &[SocketAddr],
        ingest: Option<&IngestLane>,
        serve: Option<&ServeLane>,
        preview: Option<&Arc<crate::whep_egress::WhepEgress>>,
        local_addr: SocketAddr,
    ) {
        if let Some(lane) = ingest {
            lane.shared.push_relays(relays);
        }
        if let Some(lane) = serve {
            lane.shared.push_relays(relays);
        }
        if let Some(preview) = preview {
            for relay in relays {
                preview.learn_relay(*relay, local_addr);
            }
        }
    }
}
