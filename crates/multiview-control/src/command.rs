//! The engine **command bus** (ADR-W008): control's inbound channel to the
//! engine.
//!
//! Multiview's engine (`multiview-engine`) does not yet expose an inbound command
//! intake (hot-reconfiguration is a later milestone), so the command-bus
//! contract is defined here. It is the *one* direction in which the control
//! plane talks **to** the engine, and it is built so it can **never**
//! back-pressure the engine (invariant #10):
//!
//! * It is a **bounded** `tokio::sync::mpsc` channel. Control is the producer;
//!   the engine side holds the drainable [`CommandReceiver`].
//! * Control submits with [`CommandSender::try_submit`], which **never blocks
//!   or awaits**: when the queue is full it returns [`SubmitError::Full`]
//!   immediately (the HTTP layer maps that to `503` /
//!   [`ControlError::EngineBusy`](crate::error::ControlError::EngineBusy)). A
//!   slow or stopped engine drain therefore sheds control load rather than
//!   blocking control, and a flood of control commands can never grow memory
//!   without bound or stall the engine.
//! * The engine drains **at its leisure** with [`CommandReceiver::try_drain`]
//!   (non-blocking, e.g. once per tick) or [`CommandReceiver::recv`] (awaited on
//!   its own task) — its choice, never forced by control.
//!
//! Every submitted command carries an [`OperationId`]: long-running commands
//! return `202 Accepted` + that id immediately, and the engine reports the
//! eventual outcome on the realtime event stream correlated by the same id
//! (ADR-W008). The HTTP response never waits for the engine to apply the change.
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A correlation id for an asynchronously-applied control command.
///
/// Returned to the client in a `202 Accepted` response and echoed by the engine
/// on the realtime stream (`corr`) when the command's outcome is known.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Mint a fresh, random operation id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    /// Wrap an existing id string (e.g. one parsed from a request).
    #[must_use]
    pub fn from_string(id: String) -> Self {
        Self(id)
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A control-plane command destined for the engine.
///
/// These are the management mutations that must be applied on the data plane
/// (at a frame boundary, per the Class-1/Class-2 model, invariant #11). The
/// control plane validates and enqueues them; the engine applies them when it
/// drains. Each is correlated by an [`OperationId`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Command {
    /// Start program output.
    Start {
        /// Correlation id for the async outcome.
        op: OperationId,
    },
    /// Stop program output.
    Stop {
        /// Correlation id for the async outcome.
        op: OperationId,
    },
    /// Swap the source bound to a tile (make-before-break).
    SwapSource {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The tile/cell id whose binding changes.
        tile: String,
        /// The new source/input id to bind.
        source: String,
    },
    /// Apply a new layout to the running multiview.
    ApplyLayout {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The layout id to make active.
        layout: String,
    },
    /// Arm (stage) a salvo so it is ready for an atomic take. Arming never
    /// changes program output; it only stages the recall (broadcast-multiviewer
    /// brief §8).
    ArmSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to stage.
        salvo: String,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Take (atomically apply) a salvo. If `salvo` is `None` the engine takes the
    /// currently-armed salvo; otherwise it takes (arm-then-take) the named salvo.
    TakeSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to take, or `None` to take the armed salvo.
        salvo: Option<String>,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Cancel a previously-armed salvo before it is taken.
    CancelSalvo {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The salvo id to cancel, or `None` to cancel the armed salvo.
        salvo: Option<String>,
        /// The output head this recall targets, if scoped to one head.
        head: Option<String>,
    },
    /// Force (or clear) a manual tally override on a tile/element, taking
    /// precedence over the arbitrated bus state until released. A `color` of
    /// [`None`] clears the override and returns the element to arbitration.
    SetTallyOverride {
        /// Correlation id for the async outcome.
        op: OperationId,
        /// The tally target the override applies to.
        target: multiview_events::TallyTarget,
        /// The colour to force, or `None` to clear the override.
        color: Option<multiview_core::tally::TallyColor>,
    },
}

impl Command {
    /// The operation id correlating this command's eventual outcome.
    #[must_use]
    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::Start { op }
            | Self::Stop { op }
            | Self::SwapSource { op, .. }
            | Self::ApplyLayout { op, .. }
            | Self::ArmSalvo { op, .. }
            | Self::TakeSalvo { op, .. }
            | Self::CancelSalvo { op, .. }
            | Self::SetTallyOverride { op, .. } => op,
        }
    }

    /// A stable machine-readable label for the command kind.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Stop { .. } => "stop",
            Self::SwapSource { .. } => "swap_source",
            Self::ApplyLayout { .. } => "apply_layout",
            Self::ArmSalvo { .. } => "arm_salvo",
            Self::TakeSalvo { .. } => "take_salvo",
            Self::CancelSalvo { .. } => "cancel_salvo",
            Self::SetTallyOverride { .. } => "set_tally_override",
        }
    }
}

/// Why a non-blocking command submission failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitError {
    /// The bounded queue is full. The submission is shed (never blocks the
    /// engine); the caller should retry later. Maps to `503`.
    #[error("command bus is at capacity")]
    Full,
    /// The engine side (the receiver) has been dropped; the engine is gone.
    #[error("command bus receiver is closed")]
    Closed,
}

/// The control-side producer half of the command bus.
///
/// Cloneable so every request handler shares one bus. Submission is **always
/// non-blocking** ([`CommandSender::try_submit`]); there is deliberately no
/// awaiting/back-pressuring send, so control can never make the engine wait and
/// the engine can never make control wait.
#[derive(Debug, Clone)]
pub struct CommandSender {
    tx: tokio::sync::mpsc::Sender<Command>,
    capacity: usize,
}

/// The engine-side drainable consumer half of the command bus.
///
/// The engine owns this and drains it on its own schedule — never forced by
/// control. Draining is non-blocking ([`CommandReceiver::try_drain`]) or an
/// awaited [`CommandReceiver::recv`] on a dedicated task; either way control
/// cannot block the engine's drain and the engine's drain cannot block control.
#[derive(Debug)]
pub struct CommandReceiver {
    rx: tokio::sync::mpsc::Receiver<Command>,
}

/// Create a bounded command bus with room for `capacity` queued commands,
/// returning the control-side sender and the engine-side receiver.
///
/// A `capacity` of `0` is promoted to `1` (`tokio::sync::mpsc` requires a
/// positive buffer).
#[must_use]
pub fn command_bus(capacity: usize) -> (CommandSender, CommandReceiver) {
    let capacity = capacity.max(1);
    let (tx, rx) = tokio::sync::mpsc::channel(capacity);
    (CommandSender { tx, capacity }, CommandReceiver { rx })
}

impl CommandSender {
    /// Submit a command **without blocking or awaiting**.
    ///
    /// Returns the command's [`OperationId`] on success. On a full queue returns
    /// [`SubmitError::Full`] (shed the load — the engine is never blocked); if
    /// the engine side has gone away returns [`SubmitError::Closed`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::Full`] when the bounded queue is saturated, or
    /// [`SubmitError::Closed`] when the receiver was dropped.
    pub fn try_submit(&self, command: Command) -> Result<OperationId, SubmitError> {
        let op = command.operation_id().clone();
        match self.tx.try_send(command) {
            Ok(()) => Ok(op),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(SubmitError::Full),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(SubmitError::Closed),
        }
    }

    /// The bounded queue depth (the maximum number of un-drained commands).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl CommandReceiver {
    /// Drain every currently-queued command without awaiting.
    ///
    /// Returns the commands in FIFO order. The engine calls this on its own
    /// cadence (e.g. once per tick). It never blocks waiting for more.
    #[must_use]
    pub fn try_drain(&mut self) -> Vec<Command> {
        let mut drained = Vec::new();
        while let Ok(command) = self.rx.try_recv() {
            drained.push(command);
        }
        drained
    }

    /// Await the next command (for an engine that drains on a dedicated task).
    ///
    /// Returns [`None`] once every [`CommandSender`] has been dropped.
    pub async fn recv(&mut self) -> Option<Command> {
        self.rx.recv().await
    }
}
