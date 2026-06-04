//! HA cluster **transport** — feature `cluster`, compile-verified only.
//!
//! Exchanging heartbeats with peers and shipping replication snapshots/deltas
//! over a real network requires a live multi-node cluster and sockets, neither of
//! which exist in this environment. This binding therefore lives behind the
//! off-by-default `cluster` feature and is compile-verified only; its correctness
//! rests on the pure, fully-tested model in [`super`] (the heartbeat/failover
//! state machine) and [`super::repl`] (the replication model).
//!
//! ## Isolation is preserved (invariants #1 + #10)
//!
//! Crucially, even with `cluster` enabled the transport never paces or
//! back-pressures the active node's output clock. The runner *samples* inbound
//! heartbeats and feeds them to the pure [`HaStateMachine`]
//! / [`Cluster`]; it *publishes* this node's own heartbeats on a
//! best-effort, drop-oldest path. The engine's `out_pts = f(tick)` is never
//! derived from, nor gated on, any peer or socket — a partitioned, flapping, or
//! slow cluster link changes only when a standby *decides* to promote, never how
//! or when the active emits frames.
use super::repl::{EngineSnapshot, ReplicaApplier, ReplicationDelta};
use super::{Cluster, FailoverDecision, HaStateMachine, Heartbeat, NodeId};
use crate::error::Result;
use multiview_core::time::MediaTime;

/// A best-effort, non-blocking transport over which a node exchanges heartbeats
/// and replication messages with its peers.
///
/// Implementations bind real sockets in a `cluster` build. The contract is that
/// **no method back-pressures the caller**: sends are best-effort drop-oldest and
/// receives are non-blocking polls. The engine's output path never awaits any of
/// these methods (invariants #1 + #10).
#[allow(async_fn_in_trait)]
// reason: like the `Actor` trait, this transport is consumed only inside the
// engine's HA runner; we do not need `Send`-bound futures via `trait-variant`.
pub trait ClusterTransport {
    /// Publish this node's heartbeat to all peers. Best-effort: must never block
    /// or back-pressure; a full/slow link drops, it does not stall the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) if the transport could not accept
    /// the heartbeat for publication (a permanent link/encoding fault — not a
    /// transient drop, which is silent).
    fn publish_heartbeat(&self, hb: Heartbeat) -> Result<()>;

    /// Poll for the next inbound peer heartbeat without blocking. Returns `None`
    /// when none is currently queued.
    fn poll_heartbeat(&mut self) -> Option<Heartbeat>;

    /// Publish a full replication snapshot to standbys (best-effort).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) on a permanent transport fault
    /// while submitting the snapshot.
    fn publish_snapshot(&self, snapshot: &EngineSnapshot) -> Result<()>;

    /// Publish an incremental replication delta to standbys (best-effort).
    ///
    /// # Errors
    ///
    /// Returns an [`Error`](crate::error::Error) on a permanent transport fault
    /// while submitting the delta.
    fn publish_delta(&self, delta: &ReplicationDelta) -> Result<()>;

    /// Poll for the next inbound replication message (snapshot or delta) without
    /// blocking.
    fn poll_replication(&mut self) -> Option<ReplicationMessage>;
}

/// An inbound replication message: a full baseline or an incremental delta.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicationMessage {
    /// A full baseline snapshot.
    Snapshot(EngineSnapshot),
    /// An incremental delta.
    Delta(ReplicationDelta),
}

/// Drives one node's HA participation over a [`ClusterTransport`]: feeds inbound
/// heartbeats into the local [`HaStateMachine`] + [`Cluster`] view, applies
/// inbound replication to the [`ReplicaApplier`], and (on the active) publishes
/// this node's own heartbeats.
///
/// This is the seam a `cluster`-enabled deployment wires to its sockets. It is a
/// pure orchestrator over the tested model — every *decision* it makes is the
/// model's; the runner only moves bytes (best-effort, never blocking the engine).
#[derive(Debug)]
pub struct HaRunner<T: ClusterTransport> {
    transport: T,
    machine: HaStateMachine,
    cluster: Cluster,
    replica: ReplicaApplier,
}

impl<T: ClusterTransport> HaRunner<T> {
    /// Build a runner for `machine`'s node, watching `cluster`, over `transport`.
    #[must_use]
    pub fn new(transport: T, machine: HaStateMachine, cluster: Cluster) -> Self {
        Self {
            transport,
            machine,
            cluster,
            replica: ReplicaApplier::new(),
        }
    }

    /// This node's id.
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        self.machine.node().id()
    }

    /// The local HA state machine (for metrics / role inspection).
    #[must_use]
    pub const fn machine(&self) -> &HaStateMachine {
        &self.machine
    }

    /// The local replica (for promotion: the state to resume from).
    #[must_use]
    pub const fn replica(&self) -> &ReplicaApplier {
        &self.replica
    }

    /// Drain all currently-queued inbound heartbeats into the model, returning
    /// `true` if doing so promoted this node. Non-blocking: stops when the
    /// transport has nothing queued.
    pub fn pump_heartbeats(&mut self, now: MediaTime) -> bool {
        while let Some(hb) = self.transport.poll_heartbeat() {
            self.cluster.observe(hb);
            self.machine.observe_heartbeat(hb);
        }
        self.machine.tick(now)
    }

    /// Drain all currently-queued inbound replication messages into the local
    /// replica. Errors from the pure applier (gaps / non-monotonic) are surfaced
    /// for the caller to react to (e.g. request a fresh snapshot); they never
    /// mutate the replica.
    pub fn pump_replication(&mut self) {
        while let Some(msg) = self.transport.poll_replication() {
            let _result = match msg {
                ReplicationMessage::Snapshot(snap) => self.replica.install_snapshot(snap),
                ReplicationMessage::Delta(delta) => self.replica.apply_delta(delta),
            };
        }
    }

    /// Publish this node's heartbeat for `now` (best-effort, never blocking).
    ///
    /// # Errors
    ///
    /// Propagates a transport-level publish error.
    pub fn beat(&self, now: MediaTime) -> Result<()> {
        self.transport.publish_heartbeat(Heartbeat {
            from: self.machine.node().id(),
            priority: self.machine.node().priority(),
            role: self.machine.role(),
            epoch: self.machine.epoch(),
            drives_output: self.machine.drives_output(),
            sent_at: now,
        })
    }

    /// The cluster-wide failover decision for this node at `now`.
    #[must_use]
    pub fn decision(&self, now: MediaTime) -> FailoverDecision {
        self.cluster.evaluate(self.machine.node().id(), now)
    }
}
