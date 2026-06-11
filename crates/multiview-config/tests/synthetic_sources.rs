//! `bars` / `solid` / `clock` are first-class synthetic `SourceKind`s (ADR-0027),
//! modelled and validated like any decoded feed. `test` is retained as a
//! back-compat alias for `bars`.
//!
//! Every assertion is against the real public config API (parse → validate →
//! re-serialize) — no tautologies.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ClockFaceConfig, MultiviewConfig, SourceKind};

/// Wrap one source's `kind` fields in a minimal, structurally valid 1x1 grid
/// document so we exercise the real parse + validate path, not a bare enum.
fn config_with_source(source_kind_fields: &str) -> String {
    format!(
        r##"schema_version = 1
[canvas]
width = 320
height = 240
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
{source_kind_fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

#[test]
fn test_kind_is_a_back_compat_alias_for_bars() {
    let doc = config_with_source("kind = \"test\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    assert!(
        matches!(cfg.sources[0].kind, SourceKind::Bars),
        "`kind = \"test\"` must deserialize to Bars"
    );
    // Canonical re-serialization emits the new name, not the alias.
    let toml = cfg.to_toml().expect("to_toml");
    assert!(
        toml.contains("kind = \"bars\""),
        "canonical kind is `bars`, got:\n{toml}"
    );
    assert!(
        !toml.contains("kind = \"test\""),
        "the `test` alias must not round-trip back out"
    );
}

#[test]
fn bars_kind_parses_and_validates() {
    let cfg =
        MultiviewConfig::load_from_toml(&config_with_source("kind = \"bars\"")).expect("parse");
    cfg.validate().expect("validates");
    assert!(matches!(cfg.sources[0].kind, SourceKind::Bars));
}

#[test]
fn solid_source_round_trips_with_its_colour() {
    let doc = config_with_source("kind = \"solid\"\ncolor = \"#22aa44\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Solid { color } => assert_eq!(color, "#22aa44"),
        other => panic!("expected Solid, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(toml.contains("kind = \"solid\""));
    assert!(toml.contains("#22aa44"));
}

#[test]
fn solid_rejects_a_non_hex_colour() {
    let doc = config_with_source("kind = \"solid\"\ncolor = \"not-a-color\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg
        .validate()
        .expect_err("a non-hex solid colour must fail validation");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("color") || msg.contains("colour") || msg.contains("hex"),
        "the error must name the colour: {msg}"
    );
}

#[test]
fn clock_source_defaults_to_analog_utc_and_round_trips() {
    let doc = config_with_source("kind = \"clock\"");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            face,
            twelve_hour,
            tz_offset_minutes,
        } => {
            assert_eq!(*face, ClockFaceConfig::Analog, "clock defaults to analog");
            assert!(!*twelve_hour, "12-hour defaults off");
            assert_eq!(*tz_offset_minutes, 0, "tz defaults to UTC");
        }
        other => panic!("expected Clock, got {other:?}"),
    }
}

#[test]
fn digital_clock_with_tz_round_trips() {
    let doc = config_with_source(
        "kind = \"clock\"\nface = \"digital\"\ntwelve_hour = true\ntz_offset_minutes = 600",
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    cfg.validate().expect("validates");
    match &cfg.sources[0].kind {
        SourceKind::Clock {
            face,
            twelve_hour,
            tz_offset_minutes,
        } => {
            assert_eq!(*face, ClockFaceConfig::Digital);
            assert!(*twelve_hour);
            assert_eq!(*tz_offset_minutes, 600);
        }
        other => panic!("expected Clock, got {other:?}"),
    }
    let toml = cfg.to_toml().expect("to_toml");
    assert!(toml.contains("kind = \"clock\""));
    assert!(toml.contains("face = \"digital\""));
}

#[test]
fn clock_rejects_an_out_of_range_tz_offset() {
    // Real UTC offsets span -12:00..=+14:00 (-720..=840 min); 5000 is absurd.
    let doc = config_with_source("kind = \"clock\"\ntz_offset_minutes = 5000");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
    let err = cfg
        .validate()
        .expect_err("an out-of-range tz offset must fail validation");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("tz") || msg.contains("offset") || msg.contains("timezone"),
        "the error must name the tz offset: {msg}"
    );
}

#[test]
fn a_full_multiview_of_synthetic_sources_validates() {
    // bars + clock + solid wired into a 1x1 each must validate — proving they are
    // first-class peers of a decoded feed, not a special case.
    for fields in [
        "kind = \"bars\"",
        "kind = \"clock\"\nface = \"analog\"",
        "kind = \"solid\"\ncolor = \"#000000\"",
    ] {
        let cfg = MultiviewConfig::load_from_toml(&config_with_source(fields))
            .unwrap_or_else(|e| panic!("parse {fields:?}: {e}"));
        cfg.validate()
            .unwrap_or_else(|e| panic!("validate {fields:?}: {e}"));
    }
}

#[test]
fn live_apply_classification_partitions_every_kind() {
    // ADR-W018: `is_synthetic` (level 1) and `is_network_media` (level 2) are
    // the two live-apply classification points. They must be disjoint, and the
    // kinds outside both (ndi/youtube/aes67) apply on restart.
    let parse = |fields: &str| -> SourceKind {
        let doc = config_with_source(fields);
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
        cfg.sources.into_iter().next().expect("one source").kind
    };

    let synthetic = [
        parse("kind = \"bars\""),
        parse("kind = \"solid\"\ncolor = \"#22aa44\""),
        parse("kind = \"clock\""),
    ];
    for kind in &synthetic {
        assert!(kind.is_synthetic(), "{kind:?} is synthetic");
        assert!(
            !kind.is_network_media(),
            "{kind:?} must not classify as network media"
        );
    }

    let network = [
        parse("kind = \"rtsp\"\nurl = \"rtsp://[2001:db8::1]/s\""),
        parse("kind = \"hls\"\nurl = \"https://[2001:db8::1]/m.m3u8\""),
        parse("kind = \"ts\"\nurl = \"udp://[ff3e::1]:5004\""),
        parse("kind = \"srt\"\nurl = \"srt://[2001:db8::2]:7001\""),
        parse("kind = \"rtmp\"\nurl = \"rtmp://ingest.example/app/key\""),
        parse("kind = \"file\"\npath = \"/media/clip.ts\""),
    ];
    for kind in &network {
        assert!(kind.is_network_media(), "{kind:?} is network media");
        assert!(
            !kind.is_synthetic(),
            "{kind:?} must not classify as synthetic"
        );
    }

    let restart_only = [
        parse("kind = \"ndi\"\nname = \"STUDIO (CAM 1)\""),
        parse("kind = \"youtube\"\nurl = \"https://www.youtube.com/watch?v=x\""),
        parse("kind = \"aes67\"\nsdp = \"v=0\""),
    ];
    for kind in &restart_only {
        assert!(
            !kind.is_synthetic() && !kind.is_network_media(),
            "{kind:?} stays restart-only (no live-apply classification)"
        );
    }
}
