//! Map the running binary's HAL probe + compiled Cargo features + resolved
//! compositor adapter onto the [`multiview_control::system::SystemCapabilities`]
//! DTO served by `GET /api/v1/system/capabilities` (ADR-W030).
//!
//! This lives in the `multiview` binary — not in `multiview-control` — because
//! only the binary depends on `multiview-hal` and knows its own compiled
//! features. The result is a **static startup snapshot** installed via
//! [`multiview_control::AppState::with_capabilities`]; it never opens an engine
//! channel (invariant #10).

use multiview_control::system::{
    BuildInfo, CompositorCapability, CompositorClass, NdiAttribution, SystemCapabilities,
};
use multiview_hal::AdapterReport;

/// Resolve the honest default-build capability surface from the HAL probe, the
/// compiled features, and the resolved compositor `adapter` (as read by
/// [`crate::capability_warn::read_compositor_adapter`]; `None` when no adapter
/// resolves — the CPU composite path).
///
/// Graceful by construction: an absent/failing backend probes to
/// `available = false` and never panics at startup.
#[must_use]
pub fn resolve_system_capabilities(adapter: Option<&AdapterReport>) -> SystemCapabilities {
    // NOTE (RED): the HAL backend-probe mapping lands in the follow-up commit;
    // this first cut reports no backends so the failing test pins the required
    // software-available surface (rule 18).
    let _ = adapter;
    SystemCapabilities {
        backends: Vec::new(),
        compositor: CompositorCapability {
            class: CompositorClass::None,
            device_type: None,
            driver: None,
        },
        build: BuildInfo::resolve(
            cfg!(feature = "gpl-codecs"),
            cfg!(feature = "ndi"),
            compiled_features(),
        ),
        ndi_attribution: cfg!(feature = "ndi").then(NdiAttribution::required),
    }
}

/// The compiled, capability/licence-relevant Cargo features of this binary.
///
/// Only real declared `multiview-cli` features appear here — `cfg!` on an
/// undeclared feature would trip `unexpected_cfgs`. Umbrella presets
/// (`nvidia`/`apple`/`linux-vaapi`/`full`) are omitted: they only turn the leaf
/// features below on.
fn compiled_features() -> Vec<String> {
    let mut features = Vec::new();
    macro_rules! push_if {
        ($feat:literal) => {
            if cfg!(feature = $feat) {
                features.push($feat.to_owned());
            }
        };
    }
    push_if!("software");
    push_if!("ffmpeg");
    push_if!("gpl-codecs");
    push_if!("ndi");
    push_if!("ndi-bindings");
    push_if!("webrtc");
    push_if!("webrtc-native");
    push_if!("cuda");
    push_if!("gpu");
    push_if!("overlay");
    push_if!("display-kms");
    push_if!("rist");
    push_if!("youtube");
    push_if!("ntp");
    push_if!("ptp");
    push_if!("mesh-mdns");
    push_if!("heartbeat");
    push_if!("discovery");
    push_if!("devices-net");
    push_if!("tls");
    push_if!("web");
    features
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_control::system::{
        BackendKind, CompositorDeviceType, EffectiveLicense, PipelineStage,
    };
    use multiview_hal::AdapterDeviceType;

    fn has_available(caps: &SystemCapabilities, kind: BackendKind, stage: PipelineStage) -> bool {
        caps.backends
            .iter()
            .any(|b| b.kind == kind && b.stage == stage && b.available)
    }

    #[test]
    fn software_backends_are_available_on_the_default_build() {
        let caps = resolve_system_capabilities(None);
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Decode
        ));
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Composite
        ));
        assert!(has_available(
            &caps,
            BackendKind::Software,
            PipelineStage::Encode
        ));
    }

    #[test]
    fn default_build_omits_ndi_attribution_and_is_lgpl_clean() {
        let caps = resolve_system_capabilities(None);
        assert!(caps.ndi_attribution.is_none());
        assert_eq!(caps.build.effective_license, EffectiveLicense::LgplClean);
        assert!(caps.build.redistributable);
    }

    #[test]
    fn a_resolved_adapter_flows_through_to_the_compositor() {
        let report = AdapterReport {
            device_type: AdapterDeviceType::DiscreteGpu,
            driver: "test-driver".to_owned(),
            is_gl_backend: false,
        };
        let caps = resolve_system_capabilities(Some(&report));
        // A resolved adapter is classified (not `None`) and its device/driver
        // flow through unchanged.
        assert_ne!(caps.compositor.class, CompositorClass::None);
        assert_eq!(
            caps.compositor.device_type,
            Some(CompositorDeviceType::DiscreteGpu)
        );
        assert_eq!(caps.compositor.driver.as_deref(), Some("test-driver"));
    }
}
