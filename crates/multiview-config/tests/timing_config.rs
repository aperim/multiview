//! DEV-C1 (ADR-M010): the `[timing]` config block — the per-deployment
//! outbound **link offset** (AES67 semantics applied to video: a fixed
//! receiver-side presentation delay; uniformity across nodes over smallness)
//! plus the optional PHC device path the `ptp`-feature build samples.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, TimingConfig};

/// A minimal, valid document (mirrors `validation.rs`'s BASE).
const BASE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"
"##;

#[test]
fn timing_block_is_optional_and_defaults_apply() {
    let cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    cfg.validate().expect("base document is valid");
    assert!(cfg.timing.is_none(), "no [timing] block parses as None");
    // The default link offset (ADR-M010: typically 100-300 ms; ours is 150 ms).
    let d = TimingConfig::default();
    assert_eq!(d.link_offset_ms, 150);
    assert_eq!(d.link_offset_ns(), 150_000_000);
    assert_eq!(d.ptp_phc, None);
}

#[test]
fn timing_block_parses_from_toml() {
    let doc = format!(
        "{BASE}\n[timing]\nlink_offset_ms = 200\nptp_phc = \"/dev/ptp0\"\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate().expect("a 200 ms link offset is valid");
    let timing = cfg.timing.expect("timing block present");
    assert_eq!(timing.link_offset_ms, 200);
    assert_eq!(timing.link_offset_ns(), 200_000_000);
    assert_eq!(timing.ptp_phc.as_deref(), Some("/dev/ptp0"));
}

#[test]
fn timing_block_round_trips_toml_and_json() {
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 120\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    let json = cfg.to_json().expect("serialize json");
    let back = MultiviewConfig::load_from_json(&json).expect("parse json");
    assert_eq!(back.timing, cfg.timing);
    let toml = cfg.to_toml().expect("serialize toml");
    let back2 = MultiviewConfig::load_from_toml(&toml).expect("parse toml");
    assert_eq!(back2.timing, cfg.timing);
}

#[test]
fn an_absurd_link_offset_is_rejected() {
    // Beyond 10 s the value is a typo, not a presentation-delay policy
    // (the same bound rationale as the sync-group offset cap).
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 10001\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    let err = cfg.validate().expect_err("a 10+ second link offset must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("link_offset"),
        "the error must name the offending knob, got: {msg}"
    );
}

#[test]
fn the_boundary_link_offset_is_accepted() {
    let doc = format!("{BASE}\n[timing]\nlink_offset_ms = 10000\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate().expect("exactly 10 s is the inclusive bound");
}
