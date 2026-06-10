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
//! The state machine, the status registry, and the broadcaster are real and
//! complete. mDNS/DNS-SD **discovery** ([`discovery`]) is real and complete too
//! (DEV-A5): a bounded, TTL-expiring, **untrusted** inventory of services found
//! on the LAN, requiring explicit confirm-adopt (ADR-0041) — discovery never
//! creates a device. The remaining device driver actors (live HTTP probe /
//! mode-switch round-trips) are DEV-A4; the projection endpoints return the
//! honestly-declared candidate shape (empty until a driver enumerates), never
//! fabricated live telemetry.

pub mod broadcaster;
pub mod discovery;
pub mod projection;
pub mod registry;
pub mod state_machine;

pub use broadcaster::DeviceBroadcaster;
pub use discovery::{
    DiscoveredEndpoint, DiscoveredService, DiscoveryBrowser, DiscoveryDriverKind,
    DiscoveryInventory, NullBrowser, RawDiscoveredService, StaticBrowser,
};
pub use projection::{OutputTarget, SourceCandidate};
pub use registry::DeviceStatusRegistry;
pub use state_machine::{DeviceLifecycle, LifecycleEvent};
