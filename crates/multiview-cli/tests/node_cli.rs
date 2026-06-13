//! `multiview node` CLI tests (DEV-B5 / ADR-0045): the subcommand grammar,
//! the build-feature gate (the node FAILS clearly without `display-kms` /
//! `ffmpeg` — the DEV-B1 precedent, never a silent skip), and the
//! load-validate-lower path from a node TOML on disk to the runnable
//! `MultiviewConfig`. All hardware-free: parsing, gating, and lowering only —
//! real scanout/ingest run on hardware.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io::Write as _;

use clap::Parser as _;
use multiview_cli::cli::{Cli, Command};
use multiview_cli::node;
use multiview_core::time::Rational;

/// A minimal valid node document for the load path.
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

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

#[test]
fn node_subcommand_parses_config_path() {
    let cli = Cli::try_parse_from(["multiview", "node", "/etc/multiview/node.toml"])
        .expect("node subcommand parses");
    let Command::Node(args) = cli.command else {
        panic!("expected the node subcommand");
    };
    assert_eq!(
        args.config.to_str(),
        Some("/etc/multiview/node.toml"),
        "the positional CONFIG path is captured"
    );
    assert!(args.ticks.is_none());
    assert!(args.duration.is_none());
}

#[test]
fn node_subcommand_requires_a_config_path() {
    assert!(
        Cli::try_parse_from(["multiview", "node"]).is_err(),
        "node without a config path is a usage error"
    );
}

#[test]
fn node_tick_budget_resolves_like_run() {
    let cli = Cli::try_parse_from(["multiview", "node", "node.toml", "--ticks", "7"])
        .expect("parses with --ticks");
    let Command::Node(args) = cli.command else {
        panic!("expected the node subcommand");
    };
    assert_eq!(args.tick_budget(Rational::new(60, 1)), Some(7));

    let cli = Cli::try_parse_from(["multiview", "node", "node.toml", "--duration", "100"])
        .expect("parses with --duration");
    let Command::Node(args) = cli.command else {
        panic!("expected the node subcommand");
    };
    // 100 s at 30000/1001 fps = 2997.002… → 2997 whole ticks (exact integer
    // arithmetic — never float fps).
    assert_eq!(args.tick_budget(Rational::new(30_000, 1_001)), Some(2_997));

    let cli = Cli::try_parse_from(["multiview", "node", "node.toml"]).expect("parses bare");
    let Command::Node(args) = cli.command else {
        panic!("expected the node subcommand");
    };
    assert_eq!(
        args.tick_budget(Rational::new(60, 1)),
        None,
        "no bound flags ⇒ an unbounded daemon run"
    );
}

// ---------------------------------------------------------------------------
// Build-feature gate (the DEV-B1 precedent: a clear error, never a skip)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "display-kms"))]
#[test]
fn node_without_display_kms_fails_with_a_clear_error() {
    let err = node::ensure_node_supported().expect_err("node must be unsupported");
    assert!(
        err.contains("display-kms"),
        "the error names the missing feature: {err}"
    );
    assert!(
        err.contains("multiview node"),
        "the error names the subcommand: {err}"
    );
}

#[cfg(all(feature = "display-kms", not(feature = "ffmpeg")))]
#[test]
fn node_without_ffmpeg_fails_with_a_clear_error() {
    let err = node::ensure_node_supported().expect_err("node needs libav ingest");
    assert!(
        err.contains("ffmpeg"),
        "the error names the missing ingest feature: {err}"
    );
}

#[cfg(all(feature = "display-kms", feature = "ffmpeg"))]
#[test]
fn node_is_supported_in_a_full_node_build() {
    node::ensure_node_supported().expect("display-kms + ffmpeg builds run the node");
}

// ---------------------------------------------------------------------------
// Load → validate → lower
// ---------------------------------------------------------------------------

#[test]
fn load_node_run_config_lowers_a_valid_document() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    file.write_all(NODE_TOML.as_bytes()).expect("write");
    let (node_cfg, lowered) =
        node::load_node_run_config(file.path()).expect("valid node document loads");
    assert_eq!(node_cfg.hotplug.poll_secs, 3);
    assert_eq!(lowered.sources.len(), 1);
    assert_eq!(lowered.cells.len(), 1);
    assert_eq!(lowered.outputs.len(), 1);
    assert!(lowered.control.is_none());
}

#[test]
fn load_node_run_config_names_a_missing_file() {
    let err = node::load_node_run_config(std::path::Path::new(
        "/nonexistent/multiview-node-test.toml",
    ))
    .expect_err("missing file errors");
    let text = format!("{err:#}");
    assert!(
        text.contains("multiview-node-test.toml"),
        "the error names the path: {text}"
    );
}

#[test]
fn load_node_run_config_rejects_an_invalid_document() {
    let mut file = tempfile::NamedTempFile::new().expect("temp file");
    // srt kind with an rtsp URL: parses as TOML, fails node validation.
    file.write_all(b"[ingest]\nkind = \"srt\"\nurl = \"rtsp://[::1]/x\"\n\n[[displays]]\n")
        .expect("write");
    let err = node::load_node_run_config(file.path()).expect_err("invalid document errors");
    let text = format!("{err:#}");
    assert!(
        text.contains("srt"),
        "the error carries the validation detail: {text}"
    );
}
