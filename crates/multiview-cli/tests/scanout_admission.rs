//! DEV-B2 audit: scanout affinity must be wired into **admission** so a
//! display-bound pipeline's compositor is placed on the GPU that owns the
//! display connector — never the merely-least-loaded GPU (ADR-0044 §3). Placing
//! the composite on a different GPU than the one scanning the display out forces
//! the per-frame GPU→host→GPU copy ADR-0018 forbids.
//!
//! These tests prove the wiring the audit found missing (the probe +
//! `with_sink_locality` had ZERO non-test callers): the CLI resolves the
//! display heads' connectors to their owning `DeviceId`(s) through an injected
//! [`ScanoutProbe`] double (GPU-less CI — no DRM), and `select_device` then
//! admits ONLY a connector-owning GPU. The load-bearing case: with GPU-A the
//! least-loaded but the display connector owned by GPU-B, admission must place
//! the composite on GPU-B, and a display-bound pick is never a Migrate/Split
//! (the demand carries the hard affinity).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::pipeline::scanout_localities;
use multiview_core::pixel::PixelFormat;
use multiview_core::time::Rational;
use multiview_core::traits::BackendKind;
use multiview_hal::load::{DeviceId, DeviceLoad, Vendor};
use multiview_hal::scanout::{
    CardNode, ConnectionStatus, Connector, ConnectorId, ScanoutInventory, ScanoutProbe,
};
use multiview_hal::select::{select_device, GpuCandidate, Pins, PipelineDemand, StageCaps};
use multiview_hal::{Capability, CostBudget, PlacementPolicy, Resolution, Stage, TileLoad};
use multiview_output::display::ConnectorSelector;

fn nv(stable: &str, index: u32) -> DeviceId {
    DeviceId::new(Vendor::Nvidia, stable, index)
}

/// A probe double returning a fixed, already-reconciled inventory (the
/// `_devices` set is irrelevant to a pre-built double — the real probe
/// reconciles by PCI bus id, exercised in `multiview-hal`'s own tests).
struct FixedProbe(ScanoutInventory);
impl ScanoutProbe for FixedProbe {
    fn enumerate(&self, _devices: &[DeviceId]) -> ScanoutInventory {
        self.0.clone()
    }
}

/// A two-GPU host: GPU-A (idle, least-loaded) owns no connected display;
/// GPU-B owns the connected `HDMI-A-1` the run drives. The scanout target is
/// therefore GPU-B even though GPU-A is the load-only winner.
fn two_gpu_display_on_b() -> ScanoutInventory {
    let card_a = CardNode::new(
        "card0",
        nv("GPU-A", 0),
        vec![Connector::new(
            ConnectorId::new("DP-1"),
            ConnectionStatus::Disconnected,
            false,
        )],
    );
    let card_b = CardNode::new(
        "card1",
        nv("GPU-B", 1),
        vec![Connector::new(
            ConnectorId::new("HDMI-A-1"),
            ConnectionStatus::Connected,
            true,
        )],
    );
    ScanoutInventory::new(vec![card_a, card_b])
}

fn cap(stage: Stage) -> Capability {
    Capability::new(
        BackendKind::Cuda,
        stage,
        Resolution::UHD4K,
        vec![PixelFormat::Nv12],
    )
}

fn full_caps() -> StageCaps {
    StageCaps::new(
        cap(Stage::Decode),
        cap(Stage::Composite),
        cap(Stage::Encode),
    )
}

fn candidate(id: &str, index: u32) -> GpuCandidate {
    GpuCandidate {
        device_id: nv(id, index),
        stage_caps: full_caps(),
        budget: CostBudget::new(1000.0, 1000.0, 1000.0),
    }
}

/// A 1080p single-tile demand at 30 fps, carrying `locality` as the scanout
/// affinity set (empty = no display sink).
fn demand(locality: Vec<DeviceId>) -> PipelineDemand {
    PipelineDemand::new(
        Rational::new(30, 1),
        vec![
            TileLoad::new(Stage::Decode, Resolution::HD1080),
            TileLoad::new(Stage::Composite, Resolution::HD1080),
            TileLoad::new(Stage::Encode, Resolution::HD1080),
        ],
        Resolution::HD1080,
        PixelFormat::Nv12,
        0,
        true,
    )
    .with_sink_locality(locality)
}

/// VRAM fraction `pct` (0..=100) of a 12 GB card.
fn vram_pct(id: &str, index: u32, pct: u64) -> DeviceLoad {
    let total = 12_000_000_000_u64;
    let used = total.saturating_mul(pct.min(100)) / 100;
    let mut load = DeviceLoad::unknown(nv(id, index));
    load.vram_used_bytes = Some(used);
    load.vram_total_bytes = Some(total);
    load
}

#[test]
fn named_connector_resolves_to_its_owning_gpu() {
    // The pure wiring the audit found unreachable: a named display connector
    // resolves, through the probe, to the `DeviceId` of the GPU that owns it.
    let probe = FixedProbe(two_gpu_display_on_b());
    let localities = scanout_localities(
        &probe,
        &[nv("GPU-A", 0), nv("GPU-B", 1)],
        &[ConnectorSelector::Name("HDMI-A-1".to_owned())],
    );
    assert_eq!(
        localities,
        vec![nv("GPU-B", 1)],
        "the connector's owning GPU (B) is the scanout locality, not the idle GPU-A"
    );
}

#[test]
fn auto_connector_resolves_to_the_first_connected_owner() {
    // `Auto` mirrors the sink's "first connected connector" pick: GPU-B owns the
    // only connected connector, so the Auto locality is GPU-B.
    let probe = FixedProbe(two_gpu_display_on_b());
    let localities = scanout_localities(
        &probe,
        &[nv("GPU-A", 0), nv("GPU-B", 1)],
        &[ConnectorSelector::Auto],
    );
    assert_eq!(localities, vec![nv("GPU-B", 1)]);
}

#[test]
fn no_display_heads_yields_no_constraint() {
    // No display heads → empty locality → admission keeps its load-only pick.
    let probe = FixedProbe(two_gpu_display_on_b());
    let localities = scanout_localities(&probe, &[nv("GPU-A", 0), nv("GPU-B", 1)], &[]);
    assert!(
        localities.is_empty(),
        "no display sink → no scanout constraint (unchanged behaviour)"
    );
}

#[test]
fn admission_places_the_compositor_on_the_connector_owning_gpu_not_the_least_loaded() {
    // THE load-bearing test. GPU-A is idle (the least-loaded, load-only winner);
    // GPU-B is moderately loaded BUT owns the display connector. Without the
    // scanout-affinity wiring admission picks GPU-A (least load) — which would
    // place the compositor on a GPU that does not scan the display out, forcing
    // the per-frame GPU→host→GPU copy ADR-0018 forbids. With the wiring the
    // display head's connector resolves to GPU-B and admission MUST place the
    // compositor on GPU-B.
    let probe = FixedProbe(two_gpu_display_on_b());
    let localities = scanout_localities(
        &probe,
        &[nv("GPU-A", 0), nv("GPU-B", 1)],
        &[ConnectorSelector::Name("HDMI-A-1".to_owned())],
    );
    let candidates = vec![candidate("GPU-A", 0), candidate("GPU-B", 1)];
    // GPU-A idle (12% VRAM), GPU-B busier (60%): load alone picks GPU-A.
    let loads = vec![vram_pct("GPU-A", 0, 12), vram_pct("GPU-B", 1, 60)];

    // Control: WITHOUT the locality, the least-loaded GPU-A wins (proves the
    // scenario actually diverges — the affinity is load-bearing, not a no-op).
    let load_only = select_device(
        &candidates,
        &demand(vec![]),
        &loads,
        &Pins::none(),
        PlacementPolicy::default(),
    )
    .expect("a viable GPU exists");
    assert_eq!(
        load_only.device,
        nv("GPU-A", 0),
        "sanity: load alone picks the idle GPU-A (the divergent baseline)"
    );

    // WITH the scanout locality wired in, admission must place the compositor on
    // the connector-owning GPU-B instead.
    let affine = select_device(
        &candidates,
        &demand(localities),
        &loads,
        &Pins::none(),
        PlacementPolicy::default(),
    )
    .expect("the connector-owning GPU is viable");
    assert_eq!(
        affine.device,
        nv("GPU-B", 1),
        "scanout affinity places the compositor on the connector-owning GPU-B, \
         not the least-loaded GPU-A"
    );
    // A display-bound pick is never reported as an operator pin — it is the
    // demand's hard affinity (so the engine's placement controller treats it as
    // display-bound: never Migrate/Split, only local shed — proven in
    // multiview-engine's placement_display.rs).
    assert!(
        !affine.pinned,
        "scanout affinity is a demand constraint, not an operator pin"
    );
}
