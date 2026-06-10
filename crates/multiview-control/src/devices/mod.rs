//! The Devices domain (ADR-M008/M009/RT007): the managed-device registry, the
//! device runtime state machine, the latest-wins status registry, and the
//! conflating broadcaster that publishes `device.status`/lifecycle events onto
//! [`Topic::Devices`](multiview_events::Topic::Devices).
//!
//! A managed device is operator-adopted hardware (encoder/decoder appliances,
//! display nodes, cast targets). It is **never** itself a Source or Output: it
//! projects source-candidates, output-targets, and display heads, and binding a
//! projection creates an ordinary managed Source/Output carrying a `device_ref`
//! (ADR-M009). Durability is config-as-code — the registry stores
//! ([`crate::resource_store::DeviceKind`] / `SyncGroupKind`) reuse the generic
//! versioned `{id,name,body}` store with `ETag`/`If-Match` → `412`, seeded from
//! [`multiview_config::Device`] / `SyncGroup` exactly as sources/outputs are.
//! Runtime status is a separate read-only projection, never persisted/exported.
//!
//! ## Isolation (invariant #10)
//!
//! Every device event originates from the control-plane [`DeviceBroadcaster`],
//! publishing into the engine's drop-oldest broadcast and a latest-wins status
//! registry. The engine never produces, forwards, or awaits a device event, and
//! every queue is bounded drop-oldest / latest-wins, so nothing in this domain
//! can back-pressure the engine — the same proof shape as the alarms and tally
//! producers.
//!
//! The DEV-A4 [`zowietek`] driver is the first real driver actor: a
//! control-plane poller that drives the state machine, status registry, and
//! broadcaster from a live (feature-gated) device, and mirrors its enumerated
//! facets into the [`DeviceDriverRegistry`] the projection endpoints read. With
//! a driver adopted, `GET /devices/{id}/source-candidates` and `/output-targets`
//! return the driver's real enumerated candidates; without one they stay
//! honestly empty. The discovery driver actor is DEV-A5.

pub mod broadcaster;
pub mod driver_registry;
pub mod projection;
pub mod registry;
pub mod state_machine;
pub mod zowietek;

pub use broadcaster::DeviceBroadcaster;
pub use driver_registry::DeviceDriverRegistry;
pub use projection::{OutputTarget, SourceCandidate};
pub use registry::DeviceStatusRegistry;
pub use state_machine::{DeviceLifecycle, LifecycleEvent};
pub use zowietek::poller::{PollerConfig, PollerControl, PollerHandle, PollerStep, ZowietekPoller};
#[cfg(feature = "zowietek")]
pub use zowietek::runtime::ReqwestPollerFactory;
pub use zowietek::runtime::{
    DevicePollerFactory, DevicePollerRegistry, NoPollerFactory, PollerWiring,
};
pub use zowietek::{ModeConvergence, WorkMode, ZowietekDriver};
