//! Node pipeline wiring tests (DEV-B5 / ADR-0045), `ffmpeg` + `display-kms`:
//! the lowered node document **builds** the standard full pipeline without
//! touching hardware (display sinks light at run start, not at build), and
//! the node's hotplug polling cadence threads into the pipeline. Real
//! scanout/ingest stay hardware-validated (the t630 leg).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::Duration;

use multiview_cli::pipeline::Pipeline;
use multiview_config::node::NodeConfig;

const NODE_TOML: &str = r#"
[ingest]
kind = "rtsp"
url = "rtsp://[2001:db8::10]:8554/program"

[[displays]]
connector = "HDMI-A-1"
audio = true

[hotplug]
poll_secs = 3
"#;

#[test]
fn a_lowered_node_document_builds_the_standard_pipeline_without_hardware() {
    let node = NodeConfig::load_from_toml(NODE_TOML).expect("parses");
    let lowered = node.to_multiview_config().expect("lowers");
    // Build = plans only (ingest threads and display sinks start at run
    // start): a display-only document must build hardware-free.
    let pipeline = Pipeline::build(&lowered).expect("the node pipeline builds");
    assert_eq!(pipeline.source_count(), 1, "one supervised ingest");
    let cadence = pipeline.cadence();
    assert_eq!(
        (cadence.num, cadence.den),
        (60, 1),
        "the default node canvas cadence"
    );
}

#[test]
fn the_node_hotplug_polling_cadence_threads_into_the_pipeline() {
    let node = NodeConfig::load_from_toml(NODE_TOML).expect("parses");
    let lowered = node.to_multiview_config().expect("lowers");
    let mut pipeline = Pipeline::build(&lowered).expect("builds");
    assert_eq!(
        pipeline.display_hotplug_poll(),
        Duration::from_secs(5),
        "the polling fallback defaults to 5 s"
    );
    pipeline.set_display_hotplug_poll(Duration::from_secs(node.hotplug.poll_secs));
    assert_eq!(pipeline.display_hotplug_poll(), Duration::from_secs(3));
}
