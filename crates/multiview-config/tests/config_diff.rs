//! The pure per-section structural diff between two configuration documents
//! (ADR-W020): the unit the config-file watcher turns into apply actions.
//!
//! Exhaustive over every `MultiviewConfig` section: sources (by id —
//! added/changed/removed), canvas (pinned signal vs cosmetic axes),
//! layout+cells, and the restart-only sections (outputs, overlays, probes,
//! audio, control, placement, salvos, tally_profiles, walls, devices,
//! sync_groups, routing, schema_version).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ConfigDiff, MultiviewConfig, SourceChange};

/// The baseline two-source, two-cell document every test perturbs.
const BASE_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "solid"
color = "#103050"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[outputs]]
kind = "hls"
path = "/tmp/diff-base.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn base() -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(BASE_DOC).expect("parse base doc");
    config.validate().expect("base doc validates");
    config
}

/// Parse a perturbed document built by string-editing the base TOML.
fn parsed(doc: &str) -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(doc).expect("parse perturbed doc");
    config.validate().expect("perturbed doc validates");
    config
}

#[test]
fn identical_documents_diff_empty() {
    let diff = ConfigDiff::between(&base(), &base());
    assert!(diff.is_empty(), "identical documents must diff empty: {diff:?}");
    assert!(diff.sources.is_empty());
    assert!(!diff.canvas_signal_changed);
    assert!(!diff.canvas_cosmetic_changed);
    assert!(!diff.layout_changed);
    assert!(diff.changed_sections.is_empty());
}

#[test]
fn an_added_source_is_reported_added_with_its_document() {
    let next = parsed(&format!(
        "{BASE_DOC}[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(!diff.is_empty());
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Added(source) => assert_eq!(source.id, "in_c"),
        other => panic!("expected Added(in_c), got {other:?}"),
    }
    // A pure source add touches nothing else.
    assert!(!diff.layout_changed && !diff.canvas_signal_changed);
    assert!(diff.changed_sections.is_empty());
}

#[test]
fn a_changed_source_carries_previous_and_next() {
    let next = parsed(&BASE_DOC.replace("#103050", "#f0f0f0"));
    let diff = ConfigDiff::between(&base(), &next);
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Changed { previous, next } => {
            assert_eq!(previous.id, "in_a");
            assert_eq!(next.id, "in_a");
            assert_ne!(previous, next, "the changed pair differs");
        }
        other => panic!("expected Changed(in_a), got {other:?}"),
    }
}

#[test]
fn a_removed_source_is_reported_by_id() {
    let next = parsed(
        &BASE_DOC.replace("[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n", ""),
    );
    let diff = ConfigDiff::between(&base(), &next);
    assert_eq!(diff.sources.len(), 1);
    match &diff.sources[0] {
        SourceChange::Removed(id) => assert_eq!(id, "in_b"),
        other => panic!("expected Removed(in_b), got {other:?}"),
    }
}

#[test]
fn add_change_and_remove_report_together_deterministically() {
    // in_a changed, in_b removed, in_c added — Added/Changed ride `next`
    // declaration order, then Removed in `running` declaration order.
    let next = parsed(&format!(
        "{}[[sources]]\nid = \"in_c\"\nkind = \"bars\"\n",
        BASE_DOC
            .replace("#103050", "#f0f0f0")
            .replace("[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n", "")
    ));
    let diff = ConfigDiff::between(&base(), &next);
    let kinds: Vec<&str> = diff
        .sources
        .iter()
        .map(|c| match c {
            SourceChange::Added(s) => {
                assert_eq!(s.id, "in_c");
                "added"
            }
            SourceChange::Changed { next, .. } => {
                assert_eq!(next.id, "in_a");
                "changed"
            }
            SourceChange::Removed(id) => {
                assert_eq!(id, "in_b");
                "removed"
            }
        })
        .collect();
    assert_eq!(kinds, vec!["changed", "added", "removed"]);
}

#[test]
fn canvas_geometry_change_is_a_signal_change() {
    let next = parsed(&BASE_DOC.replace("width = 64", "width = 128"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.canvas_signal_changed, "geometry is the pinned signal");
    assert!(!diff.canvas_cosmetic_changed);
}

#[test]
fn canvas_cadence_change_is_a_signal_change() {
    let next = parsed(&BASE_DOC.replace("fps = \"25/1\"", "fps = \"30/1\""));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.canvas_signal_changed, "cadence is the pinned signal");
}

#[test]
fn an_equivalent_non_reduced_cadence_is_not_a_canvas_change() {
    // 50/2 == 25/1 by value (Rational cross-multiplies): the SAME pinned
    // signal, so this must NOT report a canvas change (ADR-W019 MINOR-3).
    let next = parsed(&BASE_DOC.replace("fps = \"25/1\"", "fps = \"50/2\""));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        !diff.canvas_signal_changed,
        "an equivalent non-reduced cadence is the same signal"
    );
    assert!(
        !diff.canvas_cosmetic_changed,
        "cadence spelling is not a cosmetic change either"
    );
}

#[test]
fn canvas_background_change_is_cosmetic_not_signal() {
    let next = parsed(&BASE_DOC.replace("#101014", "#000000"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(!diff.canvas_signal_changed);
    assert!(diff.canvas_cosmetic_changed, "background is a cosmetic axis");
}

#[test]
fn a_cell_rebinding_is_a_layout_change() {
    // cell_a re-bound from in_a to in_b: the layout/cells section changed.
    let next = parsed(&BASE_DOC.replace(
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_a\"",
        "[[cells]]\nid = \"cell_a\"\narea = \"a\"\n[cells.source]\ninput_id = \"in_b\"",
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.layout_changed, "a cell rebinding changes the layout");
    assert!(!diff.canvas_signal_changed);
    assert!(diff.sources.is_empty());
}

#[test]
fn a_layout_strategy_change_is_a_layout_change() {
    let next = parsed(&BASE_DOC.replace(
        "columns = [\"1fr\", \"1fr\"]",
        "columns = [\"2fr\", \"1fr\"]",
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(diff.layout_changed, "grid tracks changed");
}

/// Every restart-only section reports under its authored TOML name.
#[test]
fn each_restart_only_section_is_reported_by_name() {
    let cases: Vec<(&str, String)> = vec![
        (
            "outputs",
            BASE_DOC.replace("segment_ms = 1000", "segment_ms = 2000"),
        ),
        (
            "overlays",
            format!("{BASE_DOC}[[overlays]]\nid = \"clk\"\nkind = \"clock\"\ntarget = \"canvas\"\n"),
        ),
        (
            "probes",
            format!(
                "{BASE_DOC}[[probes]]\nid = \"black_a\"\ncell = \"cell_a\"\nkind = \"black\"\nluma_threshold = 16\n"
            ),
        ),
        (
            "audio",
            format!("{BASE_DOC}[audio]\nsample_rate_hz = 48000\n"),
        ),
        (
            "control",
            format!("{BASE_DOC}[control]\nlisten = \"[::1]:8080\"\n"),
        ),
        (
            "salvos",
            format!(
                "{BASE_DOC}[[salvos]]\nid = \"s1\"\n[[salvos.sources]]\ncell = \"cell_a\"\ninput_id = \"in_b\"\n"
            ),
        ),
        (
            "tally_profiles",
            format!("{BASE_DOC}[[tally_profiles]]\nid = \"tsl\"\n"),
        ),
        (
            "devices",
            format!(
                "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n"
            ),
        ),
        (
            "sync_groups",
            format!(
                "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n\n[[sync_groups]]\nid = \"g1\"\ntarget_skew_ms = 50\n[[sync_groups.members]]\ndevice = \"box1\"\n"
            ),
        ),
    ];
    for (section, doc) in cases {
        let next = parsed(&doc);
        let diff = ConfigDiff::between(&base(), &next);
        assert!(
            diff.changed_sections.contains(section),
            "a {section} change must report section {section:?}; got {:?}",
            diff.changed_sections
        );
    }
    // sync_groups perturbation necessarily adds a device too; pin the pair.
    let with_group = parsed(&format!(
        "{BASE_DOC}[[devices]]\nid = \"box1\"\ndriver = \"zowietek\"\naddress = \"http://[2001:db8::10]\"\n\n[[sync_groups]]\nid = \"g1\"\ntarget_skew_ms = 50\n[[sync_groups.members]]\ndevice = \"box1\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &with_group);
    assert!(diff.changed_sections.contains("devices"));
    assert!(diff.changed_sections.contains("sync_groups"));
}

#[test]
fn a_schema_version_change_is_reported() {
    let next = parsed(&BASE_DOC.replace("schema_version = 1", "schema_version = 2"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("schema_version"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_placement_change_is_reported() {
    let next = parsed(&format!("{BASE_DOC}[placement]\nreserve_headroom = 0.25\n"));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("placement"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_walls_change_is_reported() {
    let next = parsed(&format!(
        "{BASE_DOC}[[walls]]\nname = \"wall1\"\ncols = 1\nrows = 1\n[[walls.heads]]\nid = \"h1\"\nwidth = 64\nheight = 64\nfps = \"25/1\"\nlayout = \"grid\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("walls"),
        "got {:?}",
        diff.changed_sections
    );
}

#[test]
fn a_routing_change_is_reported() {
    let next = parsed(&format!(
        "{BASE_DOC}\n[[routing.video]]\ncell = \"cell_a\"\n[routing.video.source]\ninput_id = \"in_b\"\nkind = \"video\"\n"
    ));
    let diff = ConfigDiff::between(&base(), &next);
    assert!(
        diff.changed_sections.contains("routing"),
        "got {:?}",
        diff.changed_sections
    );
}
