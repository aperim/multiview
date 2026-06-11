//! The `multiview node` support shell (DEV-B5 / ADR-0045): the build-feature
//! gate and the load → validate → lower path from a node TOML document to the
//! runnable [`MultiviewConfig`].
//!
//! The node **is a normal run** with one input and display outputs: the
//! lowered document (one source → one full-canvas cell → one
//! `Output::Display` per head) drives the exact same `ffmpeg` pipeline as
//! `multiview run` — the unchanged `multiview-input`
//! pacer/jitter/normalize/supervised-reconnect stack, the framestore
//! Live→Stale→Reconnecting→NoSignal ladder (last-good, then the configured
//! local slate), and the DEV-B1..B4 display sink + ALSA HDMI audio. The
//! binary-side run wiring (signal handling, sd_notify, the watchdog) lives in
//! `main.rs`; this module is the testable library surface.

use std::path::Path;

use anyhow::Context as _;
use multiview_config::node::NodeConfig;
use multiview_config::MultiviewConfig;

/// Whether THIS build can run as a display node, with a clear, actionable
/// error naming the missing feature otherwise (the DEV-B1 precedent: a config
/// the binary cannot honour fails loudly, never silently).
///
/// A node build needs:
/// - `display-kms` — the real DRM/KMS scanout backend (and the ALSA HDMI
///   audio leg);
/// - `ffmpeg` — the libav* demux/decode the supervised ingest runs on.
///
/// # Errors
///
/// A human-readable reason naming the missing feature and the rebuild flags.
pub fn ensure_node_supported() -> Result<(), String> {
    if !cfg!(feature = "display-kms") {
        return Err(
            "`multiview node` requires the display-kms build: this binary was built without \
             the `display-kms` feature, so it cannot drive a DRM/KMS display head. Rebuild \
             with `--features display-kms,ffmpeg` (add a hardware decode preset such as \
             `linux-vaapi` or `nvidia` for a deployment build)."
                .to_owned(),
        );
    }
    if !cfg!(feature = "ffmpeg") {
        return Err(
            "`multiview node` requires the `ffmpeg` feature: the node's supervised ingest \
             (RTSP/SRT/HLS/TS) demuxes and decodes through libav*, which this binary was \
             built without. Rebuild with `--features display-kms,ffmpeg`."
                .to_owned(),
        );
    }
    Ok(())
}

/// Load a node TOML document from `path`, validate it, and lower it into the
/// runnable [`MultiviewConfig`] (also validated). Returns both: the node
/// document carries the runner-side knobs (`hotplug.poll_secs`,
/// `timing.link_offset_ms`) that deliberately do not lower into the engine
/// document.
///
/// # Errors
///
/// A contextual error naming the path for read/parse failures, or the
/// validation detail for an invalid document.
pub fn load_node_run_config(path: &Path) -> anyhow::Result<(NodeConfig, MultiviewConfig)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading node config {}", path.display()))?;
    let node = NodeConfig::load_from_toml(&text)
        .with_context(|| format!("parsing node config {}", path.display()))?;
    node.validate()
        .with_context(|| format!("validating node config {}", path.display()))?;
    let lowered = node
        .to_multiview_config()
        .with_context(|| format!("lowering node config {}", path.display()))?;
    Ok((node, lowered))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_gate_and_the_build_agree() {
        // The gate is pure `cfg!` logic: it must be Ok exactly when both
        // features are compiled in.
        let supported = cfg!(all(feature = "display-kms", feature = "ffmpeg"));
        assert_eq!(ensure_node_supported().is_ok(), supported);
    }

    #[test]
    fn load_reports_parse_errors_with_the_path() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"this is { not toml").unwrap();
        let err = load_node_run_config(file.path()).expect_err("garbage must not parse");
        let text = format!("{err:#}");
        assert!(text.contains("parsing node config"), "{text}");
    }
}
