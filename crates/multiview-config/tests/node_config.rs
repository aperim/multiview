//! Display-node config tests (DEV-B5 / ADR-0045): the `multiview node`
//! minimal config — one supervised ingest (RTSP/SRT/HLS/TS), one or more
//! display heads, an audio flag, the DEV-C2 timing knob — its validation
//! rules, and the **lowering** into a full [`MultiviewConfig`] (one source →
//! one full-canvas cell → one `Output::Display` per head) so the node runs
//! the exact same pipeline as `multiview run` (ingest pacer/jitter/reconnect,
//! framestore ladder, slate-on-loss) with zero re-implementation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::node::{NodeConfig, NodeIngest};
use multiview_config::{FailoverSlate, Output};

/// The smallest valid node document: one RTSP ingest, one auto-connector head.
const MINIMAL: &str = r#"
[ingest]
kind = "rtsp"
url = "rtsp://[2001:db8::10]:8554/program"

[[displays]]
"#;

/// A fuller document exercising every knob.
const FULL: &str = r#"
schema_version = 1

[ingest]
kind = "hls"
url = "https://[2001:db8::20]/live/program.m3u8"

[[displays]]
connector = "HDMI-A-1"
mode = { width = 1920, height = 1080, refresh = "60000/1001" }
audio = true

[[displays]]
connector = "DP-1"
forced_mode = { width = 1280, height = 720, refresh = "50/1" }

[timing]
link_offset_ms = 250

[hotplug]
poll_secs = 2

[on_loss]
slate = "no_signal"
"#;

// ---------------------------------------------------------------------------
// Parsing + defaults
// ---------------------------------------------------------------------------

#[test]
fn minimal_document_parses_with_defaults() {
    let cfg = NodeConfig::load_from_toml(MINIMAL).expect("minimal node config parses");
    assert_eq!(cfg.schema_version, 1, "schema_version defaults to 1");
    match &cfg.ingest {
        NodeIngest::Rtsp { url } => assert_eq!(url, "rtsp://[2001:db8::10]:8554/program"),
        other => panic!("expected an rtsp ingest, got {other:?}"),
    }
    assert_eq!(cfg.displays.len(), 1);
    assert_eq!(
        cfg.displays[0].connector, "auto",
        "connector defaults to auto"
    );
    assert!(!cfg.displays[0].audio, "audio defaults to off");
    assert!(cfg.displays[0].mode.is_none());
    assert!(cfg.displays[0].forced_mode.is_none());
    assert_eq!(cfg.on_loss, FailoverSlate::Bars, "slate defaults to bars");
    assert_eq!(cfg.timing.link_offset_ms, 0, "link offset defaults to 0");
    assert_eq!(cfg.hotplug.poll_secs, 5, "polling fallback defaults to 5 s");
    assert!(cfg.canvas.is_none(), "canvas override defaults to absent");
    cfg.validate().expect("minimal config validates");
}

#[test]
fn full_document_parses_every_knob() {
    let cfg = NodeConfig::load_from_toml(FULL).expect("full node config parses");
    match &cfg.ingest {
        NodeIngest::Hls { url } => {
            assert_eq!(url, "https://[2001:db8::20]/live/program.m3u8");
        }
        other => panic!("expected an hls ingest, got {other:?}"),
    }
    assert_eq!(cfg.displays.len(), 2);
    assert_eq!(cfg.displays[0].connector, "HDMI-A-1");
    assert!(cfg.displays[0].audio);
    let mode = cfg.displays[0].mode.as_ref().expect("explicit mode");
    assert_eq!((mode.width, mode.height), (1920, 1080));
    assert_eq!(mode.refresh.rational().num, 60_000);
    assert_eq!(mode.refresh.rational().den, 1_001);
    assert!(cfg.displays[1].forced_mode.is_some());
    assert_eq!(cfg.timing.link_offset_ms, 250);
    assert_eq!(cfg.hotplug.poll_secs, 2);
    assert_eq!(cfg.on_loss, FailoverSlate::NoSignal);
    cfg.validate().expect("full config validates");
}

#[test]
fn node_config_round_trips_through_toml() {
    let cfg = NodeConfig::load_from_toml(FULL).expect("parses");
    let text = toml::to_string(&cfg).expect("serializes");
    let back = NodeConfig::load_from_toml(&text).expect("round-trips");
    assert_eq!(back, cfg);
}

// ---------------------------------------------------------------------------
// Validation rejections
// ---------------------------------------------------------------------------

/// Build a valid config then mutate it via TOML edits.
fn parse_err(toml_text: &str) -> String {
    match NodeConfig::load_from_toml(toml_text) {
        Ok(cfg) => match cfg.validate() {
            Ok(()) => panic!("expected an error for:\n{toml_text}"),
            Err(e) => e.to_string(),
        },
        Err(e) => e.to_string(),
    }
}

#[test]
fn rejects_zero_displays() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"
"#,
    );
    assert!(
        err.contains("display"),
        "error names the missing displays: {err}"
    );
}

#[test]
fn rejects_mode_and_forced_mode_together() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]
connector = "HDMI-A-1"
mode = { width = 1920, height = 1080, refresh = "60/1" }
forced_mode = { width = 1280, height = 720, refresh = "50/1" }
"#,
    );
    assert!(
        err.contains("mode") && err.contains("forced_mode"),
        "error names the exclusive fields: {err}"
    );
}

#[test]
fn rejects_duplicate_connectors() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]
connector = "HDMI-A-1"

[[displays]]
connector = "HDMI-A-1"
"#,
    );
    assert!(
        err.contains("HDMI-A-1"),
        "error names the duplicated connector: {err}"
    );
}

#[test]
fn rejects_auto_connector_on_a_multi_head_node() {
    // `auto` (first connected) is only well-defined for a single head: two
    // heads racing "first connected" would fight over one connector.
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]

[[displays]]
connector = "DP-1"
"#,
    );
    assert!(err.contains("auto"), "error names the auto conflict: {err}");
}

#[test]
fn rejects_mismatched_url_schemes() {
    for (kind, url) in [
        ("rtsp", "https://[::1]/x"),
        ("srt", "rtsp://[::1]:8554/x"),
        ("hls", "srt://[::1]:9000"),
        ("ts", "no-scheme-here"),
    ] {
        let text = format!("[ingest]\nkind = \"{kind}\"\nurl = \"{url}\"\n\n[[displays]]\n");
        let err = parse_err(&text);
        assert!(
            err.contains(kind) || err.contains("scheme") || err.contains("url"),
            "{kind} with url {url} must be rejected; got: {err}"
        );
    }
}

#[test]
fn rejects_empty_url() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "   "

[[displays]]
"#,
    );
    assert!(err.contains("url"), "error names the empty url: {err}");
}

#[test]
fn rejects_out_of_range_link_offset() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]

[timing]
link_offset_ms = 10001
"#,
    );
    assert!(
        err.contains("link_offset"),
        "error names the out-of-range knob: {err}"
    );
}

#[test]
fn rejects_out_of_range_poll_secs() {
    for bad in ["0", "61"] {
        let text = format!(
            "[ingest]\nkind = \"rtsp\"\nurl = \"rtsp://[::1]:8554/x\"\n\n\
             [[displays]]\n\n[hotplug]\npoll_secs = {bad}\n"
        );
        let err = parse_err(&text);
        assert!(
            err.contains("poll_secs"),
            "poll_secs={bad} must be rejected; got: {err}"
        );
    }
}

#[test]
fn rejects_float_fps() {
    // Frame rates are exact rationals, never floats (invariant #3): a float
    // canvas fps must fail to even deserialize.
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]

[canvas]
width = 1920
height = 1080
fps = 59.94
"#,
    );
    assert!(
        err.contains("rational") || err.contains("string"),
        "a float fps must be rejected at parse time: {err}"
    );
}

#[test]
fn rejects_zero_canvas_geometry() {
    let err = parse_err(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]

[canvas]
width = 0
height = 1080
fps = "60/1"
"#,
    );
    assert!(err.contains("canvas"), "error names the canvas: {err}");
}

// ---------------------------------------------------------------------------
// Canvas derivation
// ---------------------------------------------------------------------------

#[test]
fn canvas_defaults_to_1080p60_when_nothing_specifies_it() {
    let cfg = NodeConfig::load_from_toml(MINIMAL).expect("parses");
    let lowered = cfg.to_multiview_config().expect("lowers");
    assert_eq!(lowered.canvas.width, 1920);
    assert_eq!(lowered.canvas.height, 1080);
    assert_eq!(lowered.canvas.fps.rational().num, 60);
    assert_eq!(lowered.canvas.fps.rational().den, 1);
}

#[test]
fn canvas_derives_from_the_first_head_mode() {
    let cfg = NodeConfig::load_from_toml(FULL).expect("parses");
    let lowered = cfg.to_multiview_config().expect("lowers");
    // FULL's first head pins 1920x1080 @ 60000/1001: the canvas follows it so
    // the composite is 1:1 with the scanout raster.
    assert_eq!(lowered.canvas.width, 1920);
    assert_eq!(lowered.canvas.height, 1080);
    assert_eq!(lowered.canvas.fps.rational().num, 60_000);
    assert_eq!(lowered.canvas.fps.rational().den, 1_001);
}

#[test]
fn canvas_derives_from_a_forced_mode_when_no_explicit_mode_exists() {
    let cfg = NodeConfig::load_from_toml(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]
connector = "DP-1"
forced_mode = { width = 1280, height = 720, refresh = "50/1" }
"#,
    )
    .expect("parses");
    let lowered = cfg.to_multiview_config().expect("lowers");
    assert_eq!(lowered.canvas.width, 1280);
    assert_eq!(lowered.canvas.height, 720);
    assert_eq!(lowered.canvas.fps.rational().num, 50);
}

#[test]
fn explicit_canvas_overrides_head_modes() {
    let cfg = NodeConfig::load_from_toml(
        r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:8554/x"

[[displays]]
connector = "HDMI-A-1"
mode = { width = 3840, height = 2160, refresh = "30/1" }

[canvas]
width = 1920
height = 1080
fps = "25/1"
"#,
    )
    .expect("parses");
    let lowered = cfg.to_multiview_config().expect("lowers");
    assert_eq!(lowered.canvas.width, 1920);
    assert_eq!(lowered.canvas.height, 1080);
    assert_eq!(lowered.canvas.fps.rational().num, 25);
}

// ---------------------------------------------------------------------------
// Lowering — the node IS a normal run
// ---------------------------------------------------------------------------

#[test]
fn lowering_builds_one_source_one_full_canvas_cell_one_display_per_head() {
    let cfg = NodeConfig::load_from_toml(FULL).expect("parses");
    let lowered = cfg.to_multiview_config().expect("lowers");

    // One managed source carrying the ingest URL.
    assert_eq!(lowered.sources.len(), 1);
    let source = &lowered.sources[0];
    assert_eq!(source.id, "ingest");

    // One absolutely-placed full-canvas cell bound to it, carrying the node's
    // slate-on-loss policy (the framestore ladder ends in this slate).
    assert_eq!(lowered.cells.len(), 1);
    let cell = &lowered.cells[0];
    assert_eq!(cell.source.input_id.as_deref(), Some("ingest"));
    assert_eq!(cell.on_loss, FailoverSlate::NoSignal);
    let rect = cell.rect.expect("full-canvas absolute rect");
    assert_eq!(
        (rect.x, rect.y, rect.w, rect.h),
        (0.0, 0.0, 1.0, 1.0),
        "the single cell covers the whole canvas"
    );

    // One display output per head, mapping connector/mode/forced_mode/audio.
    assert_eq!(lowered.outputs.len(), 2);
    let Output::Display {
        connector,
        mode,
        forced_mode,
        audio,
        ..
    } = &lowered.outputs[0]
    else {
        panic!("expected a display output, got {:?}", lowered.outputs[0]);
    };
    assert_eq!(connector, "HDMI-A-1");
    assert!(mode.is_some());
    assert!(forced_mode.is_none());
    assert!(
        audio.is_some(),
        "audio = true lowers to a program-bus block"
    );
    let Output::Display {
        connector: c2,
        audio: a2,
        forced_mode: f2,
        ..
    } = &lowered.outputs[1]
    else {
        panic!("expected a display output, got {:?}", lowered.outputs[1]);
    };
    assert_eq!(c2, "DP-1");
    assert!(a2.is_none(), "audio = false lowers to no audio block");
    assert!(f2.is_some());

    // No control plane: node enrollment/management is DEV-B6, and the node
    // must not silently open a listener nobody configured.
    assert!(lowered.control.is_none());

    // The lowered document is a VALID MultiviewConfig — the node runs the
    // standard pipeline with zero special-casing.
    lowered.validate().expect("lowered config validates");
}

#[test]
fn lowering_every_ingest_kind_maps_to_the_matching_source_kind() {
    use multiview_config::SourceKind;
    for (kind, url, expect) in [
        ("rtsp", "rtsp://[::1]:8554/x", "rtsp"),
        ("srt", "srt://[::1]:9000", "srt"),
        ("hls", "https://[::1]/x.m3u8", "hls"),
        ("ts", "udp://[ff3e::1]:5004", "ts"),
    ] {
        let text = format!("[ingest]\nkind = \"{kind}\"\nurl = \"{url}\"\n\n[[displays]]\n");
        let cfg = NodeConfig::load_from_toml(&text).expect("parses");
        let lowered = cfg.to_multiview_config().expect("lowers");
        let got = match &lowered.sources[0].kind {
            SourceKind::Rtsp { url: u, .. } => ("rtsp", u.clone()),
            SourceKind::Srt { url: u } => ("srt", u.clone()),
            SourceKind::Hls { url: u } => ("hls", u.clone()),
            SourceKind::Ts { url: u } => ("ts", u.clone()),
            other => panic!("unexpected lowered kind {other:?}"),
        };
        assert_eq!(got, (expect, url.to_owned()));
        lowered.validate().expect("lowered config validates");
    }
}

// ---------------------------------------------------------------------------
// The shipped example stays honest
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_display_node_example_loads_validates_and_lowers() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("display-node.toml");
    let text = std::fs::read_to_string(&path).expect("examples/display-node.toml exists");
    let cfg = NodeConfig::load_from_toml(&text).expect("the shipped example parses");
    cfg.validate().expect("the shipped example validates");
    let lowered = cfg
        .to_multiview_config()
        .expect("the shipped example lowers");
    lowered.validate().expect("the lowered example validates");
}
