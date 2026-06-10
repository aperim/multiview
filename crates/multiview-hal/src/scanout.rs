//! KMS scanout discovery: which GPU owns which display connector.
//!
//! The local display sink ([ADR-0044](../../../docs/decisions/ADR-0044.md),
//! [display-out.md §3](../../../docs/research/display-out.md)) is the first
//! **GPU-resident raw-frame consumer** in the product, and KMS scanout requires
//! the framebuffer to live on the **connector-owning GPU**. This module gives
//! the HAL the inventory it needs to express that: per DRM **card node**, the
//! display **connectors** it owns and the stable [`crate::load::DeviceId`] of
//! the GPU behind it. Selection ([`crate::select`]) and the engine's placement
//! controller turn that mapping into a hard placement constraint so composite is
//! never migrated/split off the scanout GPU — which would force the per-frame
//! GPU→host→GPU copy [ADR-0018](../../../docs/decisions/ADR-0018.md) forbids.
//!
//! ## Pure model + feature-gated discovery (the GPU-less CI contract)
//!
//! Exactly like the rest of `multiview-hal`, the **types and the pure mapping
//! logic are always compiled and unit-tested**; only the *real* DRM enumeration
//! (reading `/sys/class/drm` / the `drm` ioctls) is gated behind the
//! off-by-default `display-kms` feature. The vendor seam is the [`ScanoutProbe`]
//! trait: a pure, injectable `enumerate() -> ScanoutInventory`. The real
//! [`DrmScanoutProbe`] implements it on hardware; tests inject a double. On a
//! GPU-less host (or the default feature-off build) the real probe returns an
//! **empty** inventory — never a panic, never a native call — and the empty
//! inventory cleanly maps every connector to `None`.

use crate::load::DeviceId;

/// A KMS connector name (e.g. `"DP-1"`, `"HDMI-A-1"`).
///
/// This is the operator-facing handle a display [`crate::select::PipelineDemand`]
/// references; it is the kernel's connector name, stable for a given card +
/// physical port across reboots.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnectorId(String);

impl ConnectorId {
    /// Construct a connector id from its KMS name.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The connector name (e.g. `"DP-1"`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

/// The hotplug/connection state of a connector, as the kernel reports it.
///
/// `Unknown` is a real kernel state (`DRM_MODE_UNKNOWNCONNECTION`), distinct from
/// disconnected — some connectors cannot reliably detect a sink. Only
/// [`ConnectionStatus::Connected`] outputs are scanout targets; the others are
/// still inventoried (so a re-plug is a connection-state change, not a fresh
/// device lookup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionStatus {
    /// A sink is attached and detected (`DRM_MODE_CONNECTED`).
    Connected,
    /// No sink is attached (`DRM_MODE_DISCONNECTED`).
    Disconnected,
    /// The connector cannot reliably report a sink (`DRM_MODE_UNKNOWNCONNECTION`).
    Unknown,
}

impl ConnectionStatus {
    /// Whether this connector currently has a sink attached (a scanout target).
    #[must_use]
    pub const fn is_connected(self) -> bool {
        matches!(self, ConnectionStatus::Connected)
    }
}

/// A single display connector exposed by a card node.
///
/// Carries its KMS name, its [`ConnectionStatus`], and whether the kernel has a
/// usable EDID for it — the brief's §6 mode policy distinguishes EDID-bearing
/// heads (preferred-mode + ELD audio) from EDID-less heads (CVT-RB forced mode,
/// no audio), so the presence flag is part of the inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    /// The connector's KMS name.
    pub id: ConnectorId,
    /// Whether a sink is currently attached.
    pub status: ConnectionStatus,
    /// Whether the kernel exposes a non-empty EDID for this connector.
    pub has_edid: bool,
}

impl Connector {
    /// Construct a connector descriptor.
    #[must_use]
    pub fn new(id: ConnectorId, status: ConnectionStatus, has_edid: bool) -> Self {
        Self {
            id,
            status,
            has_edid,
        }
    }
}

/// A DRM **card node** (`/dev/dri/cardN`) and the GPU behind it.
///
/// The card node is the display side of a GPU; its connectors are the physical
/// outputs. [`Self::device_id`] is the stable identity of the GPU that owns this
/// card — the same [`DeviceId`] the placement key uses, so a connector resolves
/// directly to the device any composite feeding it must be co-located with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardNode {
    /// The DRM card-node name (e.g. `"card0"`).
    card_name: String,
    /// The stable identity of the GPU that owns this card node.
    device_id: DeviceId,
    /// The connectors this card owns.
    connectors: Vec<Connector>,
}

impl CardNode {
    /// Construct a card node from its DRM name, owning GPU, and connectors.
    #[must_use]
    pub fn new(
        card_name: impl Into<String>,
        device_id: DeviceId,
        connectors: Vec<Connector>,
    ) -> Self {
        Self {
            card_name: card_name.into(),
            device_id,
            connectors,
        }
    }

    /// The DRM card-node name (e.g. `"card0"`).
    #[must_use]
    pub fn card_name(&self) -> &str {
        &self.card_name
    }

    /// The stable identity of the GPU that owns this card node.
    #[must_use]
    pub const fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// The connectors this card owns.
    #[must_use]
    pub fn connectors(&self) -> &[Connector] {
        &self.connectors
    }

    /// Whether this card owns a connector with the given id.
    #[must_use]
    fn owns(&self, connector: &ConnectorId) -> bool {
        self.connectors.iter().any(|c| &c.id == connector)
    }
}

/// The discovered scanout inventory: every DRM card node and its connectors.
///
/// This is what a [`ScanoutProbe`] produces. Its core query,
/// [`Self::owning_device`], answers the governing scanout-affinity question —
/// *which GPU owns the connector this display sink targets?* — by membership,
/// independent of hotplug state. Construction is a plain value; the empty
/// inventory (no DRM cards / feature-off CI) cleanly owns nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanoutInventory {
    cards: Vec<CardNode>,
}

impl ScanoutInventory {
    /// Construct an inventory from a set of card nodes.
    #[must_use]
    pub fn new(cards: Vec<CardNode>) -> Self {
        Self { cards }
    }

    /// The card nodes in this inventory.
    #[must_use]
    pub fn cards(&self) -> &[CardNode] {
        &self.cards
    }

    /// The stable [`DeviceId`] of the GPU that owns `connector`, if any card
    /// node lists it.
    ///
    /// Returns `None` for an unlisted connector — never a fabricated owner.
    /// Membership is by card ownership, so a momentarily-disconnected connector
    /// still resolves to its card's GPU (a re-plug needs no fresh lookup).
    #[must_use]
    pub fn owning_device(&self, connector: &ConnectorId) -> Option<&DeviceId> {
        self.cards
            .iter()
            .find(|card| card.owns(connector))
            .map(CardNode::device_id)
    }

    /// The connector ids that currently have a sink attached
    /// ([`ConnectionStatus::Connected`]) — the live scanout targets.
    pub fn connected_connectors(&self) -> impl Iterator<Item = &ConnectorId> {
        self.cards.iter().flat_map(|card| {
            card.connectors
                .iter()
                .filter(|c| c.status.is_connected())
                .map(|c| &c.id)
        })
    }

    /// The scanout-locality set for a display sink that drives `connectors`: the
    /// deduplicated owning GPUs of those connectors.
    ///
    /// This is exactly the constraint a display
    /// [`crate::select::PipelineDemand`] declares — the composite feeding the
    /// sink may legally live only on a GPU in this set. Connectors with no owner
    /// (unlisted) contribute nothing; two connectors on one GPU collapse to a
    /// single locality device. Order follows first appearance, so the result is
    /// deterministic.
    #[must_use]
    pub fn locality_for(&self, connectors: &[ConnectorId]) -> Vec<DeviceId> {
        let mut out: Vec<DeviceId> = Vec::new();
        for connector in connectors {
            if let Some(device) = self.owning_device(connector) {
                if !out.contains(device) {
                    out.push(device.clone());
                }
            }
        }
        out
    }
}

/// The vendor seam for scanout discovery: enumerate the host's DRM card nodes
/// and connectors.
///
/// Implemented for real by [`DrmScanoutProbe`] (behind `display-kms`) and by
/// test doubles. Keeping it a trait makes the connector → owning-GPU mapping
/// unit-testable without DRM or hardware — the GPU-less CI contract.
pub trait ScanoutProbe {
    /// Enumerate the host's DRM card nodes and their connectors.
    fn enumerate(&self) -> ScanoutInventory;
}

/// The real DRM scanout-discovery probe.
///
/// Implements [`ScanoutProbe`] by enumerating `/sys/class/drm` card nodes, their
/// connectors (connection state + EDID presence), and the PCI/stable handle that
/// pairs each card node with its render-node GPU [`DeviceId`]. The enumeration is
/// read-only and confined to the off-by-default `display-kms` feature; with the
/// feature off (the default, GPU-less CI build) it returns an **empty**
/// inventory without touching the filesystem, so every connector cleanly maps to
/// `None`.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct DrmScanoutProbe;

impl DrmScanoutProbe {
    /// Construct the real DRM scanout probe.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ScanoutProbe for DrmScanoutProbe {
    fn enumerate(&self) -> ScanoutInventory {
        enumerate_drm_cards()
    }
}

// ----------------------------------------------------------------------------
// Real DRM enumeration — feature-gated.
//
// The feature-off arm (the default, GPU-less CI build) returns an empty
// inventory without any I/O. The feature-on arm reads `/sys/class/drm` with std
// only (no native link): card nodes, their connectors (status + EDID presence),
// and the PCI handle that pairs each card with its GPU identity.
// ----------------------------------------------------------------------------

#[cfg(not(feature = "display-kms"))]
fn enumerate_drm_cards() -> ScanoutInventory {
    // No DRM enumeration without the feature: an empty inventory owns nothing,
    // which is exactly right for a GPU-less / headless host.
    ScanoutInventory::default()
}

#[cfg(feature = "display-kms")]
fn enumerate_drm_cards() -> ScanoutInventory {
    use std::path::Path;

    // `/sys/class/drm` lists `cardN` and `cardN-<CONNECTOR>` entries. We group
    // each card's connectors and pair the card with its GPU identity via the PCI
    // bus id under `cardN/device`.
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return ScanoutInventory::default();
    };

    // Collect the bare `cardN` directories first (the card nodes), skipping
    // `cardN-...` connector aliases and render nodes.
    let mut card_names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let is_card = name.starts_with("card")
            && !name.contains('-')
            && name
                .strip_prefix("card")
                .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        if is_card {
            card_names.push(name);
        }
    }
    card_names.sort();

    let mut cards: Vec<CardNode> = Vec::new();
    for (index, card_name) in card_names.iter().enumerate() {
        let base = Path::new("/sys/class/drm").join(card_name);

        // Stable identity: the PCI bus id behind `cardN/device` (a symlink into
        // `/sys/devices/pci…/0000:..`). The PCI bus address is the stable handle
        // that also identifies the render-node GPU (gpu-monitoring §2.1).
        let stable_id = std::fs::read_link(base.join("device"))
            .ok()
            .and_then(|target| {
                target
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| card_name.clone());
        // The discovery layer does not link a vendor SDK; the precise vendor
        // family is refined by the render-node probe. Tag by driver where cheap,
        // else leave it as the conservative default the placement key tolerates.
        let vendor = read_card_vendor(&base);
        let device_id = DeviceId::new(vendor, stable_id, u32::try_from(index).unwrap_or(u32::MAX));

        let connectors = read_card_connectors(&base, card_name);
        cards.push(CardNode::new(card_name.clone(), device_id, connectors));
    }

    ScanoutInventory::new(cards)
}

/// Read a card's connectors from its `/sys/class/drm/cardN-*` aliases.
#[cfg(feature = "display-kms")]
fn read_card_connectors(_base: &std::path::Path, card_name: &str) -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let prefix = format!("{card_name}-");
    let mut connectors: Vec<Connector> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(conn_name) = name.strip_prefix(&prefix) else {
            continue;
        };
        let conn_dir = entry.path();
        let status = match std::fs::read_to_string(conn_dir.join("status")) {
            Ok(s) => match s.trim() {
                "connected" => ConnectionStatus::Connected,
                "disconnected" => ConnectionStatus::Disconnected,
                _ => ConnectionStatus::Unknown,
            },
            Err(_) => ConnectionStatus::Unknown,
        };
        let has_edid = std::fs::metadata(conn_dir.join("edid")).is_ok_and(|m| m.len() > 0);
        connectors.push(Connector::new(
            ConnectorId::new(conn_name.to_owned()),
            status,
            has_edid,
        ));
    }
    connectors.sort_by(|a, b| a.id.name().cmp(b.id.name()));
    connectors
}

/// Infer a card's [`Vendor`](crate::load::Vendor) from its sysfs driver link,
/// for the discovery-layer [`DeviceId`]. The render-node probe refines this.
#[cfg(feature = "display-kms")]
fn read_card_vendor(base: &std::path::Path) -> crate::load::Vendor {
    use crate::load::Vendor;
    // `cardN/device/driver` symlinks to the kernel driver module (amdgpu, i915,
    // xe, nvidia, …); map the well-known ones, defaulting conservatively.
    let driver = std::fs::read_link(base.join("device").join("driver"))
        .ok()
        .and_then(|t| t.file_name().and_then(|f| f.to_str()).map(str::to_owned))
        .unwrap_or_default();
    match driver.as_str() {
        "i915" | "xe" => Vendor::Intel,
        "nvidia" | "nvidia-drm" => Vendor::Nvidia,
        // `amdgpu`/`radeon` and any unrecognized driver fall here. The vendor
        // family is NOT load-bearing for the owning-device mapping (identity is
        // the stable PCI id), and the render-node probe refines it; so AMD — the
        // common KMS-only thin-client family (display-out.md §12) — is the
        // conservative default for everything not positively Intel/NVIDIA.
        _ => Vendor::Amd,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::load::Vendor;

    fn dev(stable: &str, index: u32) -> DeviceId {
        DeviceId::new(Vendor::Amd, stable, index)
    }

    #[test]
    fn owning_device_maps_by_card_membership() {
        let inv = ScanoutInventory::new(vec![CardNode::new(
            "card0",
            dev("0000:00:01.0", 0),
            vec![Connector::new(
                ConnectorId::new("DP-1"),
                ConnectionStatus::Connected,
                true,
            )],
        )]);
        assert_eq!(
            inv.owning_device(&ConnectorId::new("DP-1")),
            Some(&dev("0000:00:01.0", 0))
        );
        assert_eq!(inv.owning_device(&ConnectorId::new("DP-2")), None);
    }

    #[test]
    fn default_inventory_is_empty_and_owns_nothing() {
        // The feature-off CI build's real probe returns this — it must own
        // nothing, cleanly.
        let inv = ScanoutInventory::default();
        assert!(inv.cards().is_empty());
        assert_eq!(inv.owning_device(&ConnectorId::new("DP-1")), None);
        assert_eq!(inv.connected_connectors().count(), 0);
    }

    #[test]
    fn drm_probe_enumerate_is_well_formed() {
        // With `display-kms` off (the default CI build) the real probe never
        // touches the filesystem and returns an empty inventory; with it on, it
        // reads `/sys/class/drm`. Either way it must never panic and must produce
        // a well-formed inventory — we do not assert empty unconditionally (a
        // `display-kms` runner with a GPU could list real cards), only that every
        // listed card is well-formed.
        let inv = DrmScanoutProbe::new().enumerate();
        for card in inv.cards() {
            assert!(!card.card_name().is_empty());
        }
    }

    #[test]
    fn connection_status_is_connected_only_for_connected() {
        assert!(ConnectionStatus::Connected.is_connected());
        assert!(!ConnectionStatus::Disconnected.is_connected());
        assert!(!ConnectionStatus::Unknown.is_connected());
    }

    /// The governing cross-probe identity test (the MAJOR review finding).
    ///
    /// The placement controller reasons over `DeviceId` `(vendor, stable_id)`
    /// equality, and the candidate/current `DeviceId`s come from the **render-node
    /// load probe** (on NVIDIA: vendor `Nvidia`, stable_id the GPU UUID). The
    /// scanout probe reads `/sys/class/drm` and only knows a card's PCI slot — it
    /// must therefore **reuse the canonical render-node `DeviceId` for the same
    /// physical GPU verbatim**, matched on the PCI bus id, NOT re-derive its own
    /// `(vendor, stable_id)`. If it re-derived, the locality `DeviceId` would
    /// differ from the candidate `DeviceId` for the SAME GPU and
    /// `satisfies_sink_locality` would silently MISS, re-opening the migrate/split
    /// path this slice forbids.
    #[test]
    fn scanout_locality_reuses_the_render_node_device_id_for_the_same_gpu() {
        // The canonical render-node DeviceId, exactly as the NVML load probe
        // builds it: identity is (Nvidia, UUID); the PCI bus id is the non-identity
        // cross-probe matching key (NVML's 8-hex-digit-domain form).
        let render_node =
            DeviceId::new(Vendor::Nvidia, "GPU-9f1e-uuid", 0).with_pci_bus_id("00000000:03:00.0");

        // The same physical GPU as the SCANOUT probe sees it: a DRM card whose only
        // handle is the kernel PCI slot (4-hex-digit-domain form, the `nouveau`
        // driver — a vendor the driver-name heuristic would mislabel as AMD).
        let drm_card = DrmCardDescriptor {
            card_name: "card0".to_owned(),
            pci_slot: Some("0000:03:00.0".to_owned()),
            connectors: vec![Connector::new(
                ConnectorId::new("DP-1"),
                ConnectionStatus::Connected,
                true,
            )],
        };

        // Build the inventory by reconciling the DRM card against the canonical
        // device set — reusing the render-node DeviceId, never re-deriving.
        let inv = ScanoutInventory::from_drm_cards(&[drm_card], &[render_node.clone()]);

        // The scanout-sourced owning device MUST be byte-identical to the
        // render-node DeviceId (same vendor, same stable_id, same pci) — so a
        // HashMap/equality lookup against placement's `current`/candidates hits.
        let owner = inv
            .owning_device(&ConnectorId::new("DP-1"))
            .expect("the DP-1 connector resolves to its reconciled owner");
        assert_eq!(owner.vendor(), Vendor::Nvidia, "vendor reused, not guessed");
        assert_eq!(owner.stable_id(), "GPU-9f1e-uuid", "the UUID stable id is reused");
        assert_eq!(owner, &render_node);

        // The end-to-end consequence: the locality this produces satisfies the
        // sink-locality gate against the render-node candidate for the same GPU.
        let locality = inv.locality_for(&[ConnectorId::new("DP-1")]);
        assert_eq!(locality, vec![render_node.clone()]);
        let demand = crate::select::PipelineDemand::new(
            multiview_core::time::Rational::new(30, 1),
            Vec::new(),
            crate::Resolution::HD1080,
            multiview_core::pixel::PixelFormat::Nv12,
            0,
            true,
        )
        .with_sink_locality(locality);
        assert!(
            demand.satisfies_sink_locality(&render_node),
            "the reconciled locality matches the render-node candidate — no silent miss"
        );
    }

    /// PCI-bus-id matching is domain-width insensitive: NVML's 8-hex-digit domain
    /// and the kernel sysfs 4-hex-digit domain for the SAME slot reconcile.
    #[test]
    fn drm_card_reconciliation_normalizes_pci_domain_width() {
        let render_node =
            DeviceId::new(Vendor::Nvidia, "GPU-uuid", 0).with_pci_bus_id("00000000:0a:00.0");
        let drm_card = DrmCardDescriptor {
            card_name: "card0".to_owned(),
            pci_slot: Some("0000:0A:00.0".to_owned()), // upper-case, 4-digit domain
            connectors: vec![Connector::new(
                ConnectorId::new("HDMI-A-1"),
                ConnectionStatus::Connected,
                false,
            )],
        };
        let inv = ScanoutInventory::from_drm_cards(&[drm_card], &[render_node.clone()]);
        assert_eq!(
            inv.owning_device(&ConnectorId::new("HDMI-A-1")),
            Some(&render_node)
        );
    }

    /// A DRM card with no canonical match (the GPU the render-node probe could not
    /// enumerate, or a PCI slot that matches nothing) is dropped rather than
    /// fabricating a re-derived identity that would never equal any candidate.
    #[test]
    fn drm_card_with_no_render_node_match_is_dropped() {
        let render_node =
            DeviceId::new(Vendor::Nvidia, "GPU-uuid", 0).with_pci_bus_id("0000:03:00.0");
        let orphan = DrmCardDescriptor {
            card_name: "card9".to_owned(),
            pci_slot: Some("0000:99:00.0".to_owned()),
            connectors: vec![Connector::new(
                ConnectorId::new("DP-9"),
                ConnectionStatus::Connected,
                true,
            )],
        };
        let inv = ScanoutInventory::from_drm_cards(&[orphan], &[render_node]);
        assert!(inv.cards().is_empty(), "an unmatched DRM card owns nothing");
        assert_eq!(inv.owning_device(&ConnectorId::new("DP-9")), None);
    }
}
