//! `multiview run` cold-start policy (ADR-W022): `[control] start = "resume"`
//! starts from the persisted `active.toml` Running state when it exists,
//! parses, AND validates — and falls back to the boot document with a warning
//! otherwise. The boot file stays the Loaded snapshot and the watch target in
//! BOTH modes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use multiview_cli::boot::resolve_start_config;
use multiview_config::MultiviewConfig;

/// A boot document with the given `[control]` extras spliced in.
fn boot_doc(control_extra: &str) -> String {
    format!(
        r##"schema_version = 1
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
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[control]
listen = "[::1]:0"
{control_extra}
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/boot-resume.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

/// Write `boot` to a temp config file (and optionally `active` to the state
/// dir next to it), returning the boot path + the parsed boot config.
fn stage(boot: &str, active: Option<&str>) -> (tempfile::TempDir, PathBuf, MultiviewConfig) {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, boot).expect("write boot file");
    if let Some(active) = active {
        let state_dir = dir.path().join(".multiview");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(state_dir.join("active.toml"), active).expect("write active.toml");
    }
    let config = MultiviewConfig::load_from_toml(boot).expect("parse boot");
    config.validate().expect("boot validates");
    (dir, boot_path, config)
}

/// The colour of `in_a` in a config, via its serialized body.
fn in_a_color(config: &MultiviewConfig) -> Option<String> {
    config
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        })
}

/// Pin (e): `start = "resume"` with a valid `active.toml` starts Running from
/// it, while Loaded stays the boot snapshot.
#[test]
fn resume_starts_from_a_valid_active_toml() {
    let boot = boot_doc("start = \"resume\"");
    let active = boot.replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(start.resumed, "a valid active.toml must be resumed");
    assert!(start.resume_fallback.is_none());
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#f0f0f0"),
        "Running must be the active.toml document"
    );
    assert_eq!(
        in_a_color(&start.loaded).as_deref(),
        Some("#101418"),
        "Loaded must stay the boot snapshot"
    );
}

/// Pin (f): a corrupt `active.toml` falls back to the boot document with a
/// surfaced reason.
#[test]
fn resume_falls_back_to_boot_on_a_corrupt_active() {
    let boot = boot_doc("start = \"resume\"");
    let (_dir, boot_path, boot_config) = stage(&boot, Some("this is [not the schema"));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed, "a corrupt active.toml must not resume");
    let reason = start
        .resume_fallback
        .expect("the fallback reason is surfaced");
    assert!(
        reason.contains("parse") || reason.contains("TOML") || reason.contains("read"),
        "the reason should be actionable, got: {reason}"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must fall back to the boot document"
    );
}

/// A missing `active.toml` under `start = "resume"` also falls back (warned).
#[test]
fn resume_falls_back_to_boot_when_active_is_missing() {
    let boot = boot_doc("start = \"resume\"");
    let (_dir, boot_path, boot_config) = stage(&boot, None);

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed);
    assert!(start.resume_fallback.is_some());
    assert_eq!(in_a_color(&start.running).as_deref(), Some("#101418"));
}

/// Review M1 — resume staleness: the storeless restart-only sections
/// (`control`, `placement`, `salvos`, `tally_profiles`, `walls`, `routing`,
/// `schema_version`) must be spliced from the BOOT file into the resumed
/// Running document — a boot-file `[control] listen` edit must take effect on
/// the restart the operator performed, while the live sections still resume
/// from `active.toml`.
#[test]
fn resume_splices_storeless_sections_from_the_boot_file() {
    // The previous run persisted active.toml under the OLD listen; the
    // operator then edited the BOOT file's [control] listen and restarted.
    let boot = boot_doc("start = \"resume\"").replace("[::1]:0", "[::1]:9099");
    let active = boot_doc("start = \"resume\"").replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(start.resumed, "the valid active.toml must still resume");
    assert_eq!(
        start
            .running
            .control
            .as_ref()
            .map(|control| control.listen.clone())
            .as_deref(),
        Some("[::1]:9099"),
        "the restart-only [control] section must come from the BOOT file, not the stale active.toml"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#f0f0f0"),
        "the live sections still resume from active.toml"
    );
    assert_eq!(
        start.running.schema_version, start.loaded.schema_version,
        "schema_version follows the boot document"
    );
}

/// Review M1 fallback branch: when the splice produces a document that no
/// longer validates (here: boot's salvo recalls a source the persisted
/// Running state no longer carries), the run falls back to the boot document
/// with the reason surfaced — never starts from an invalid combination.
#[test]
fn resume_falls_back_when_the_splice_does_not_validate() {
    let boot = format!(
        "{}[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n[[salvos]]\nid = \"s1\"\n\
         [[salvos.sources]]\ncell = \"cell_a\"\ninput_id = \"in_b\"\n",
        boot_doc("start = \"resume\"")
    );
    // The previous run removed in_b live; its persisted Running state has
    // neither in_b nor any salvos (machine-written, valid on its own).
    let active = boot_doc("start = \"resume\"").replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(
        !start.resumed,
        "a splice that does not validate must not resume"
    );
    let reason = start
        .resume_fallback
        .expect("the fallback reason is surfaced");
    assert!(
        reason.contains("validate"),
        "the reason should name the validation failure, got: {reason}"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must fall back to the boot document"
    );
}

/// The default policy is `boot`: an existing `active.toml` is IGNORED unless
/// the boot file opts into resume.
#[test]
fn the_default_boot_policy_ignores_an_existing_active() {
    let boot = boot_doc("");
    let active = boot.replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed, "start = boot must never resume");
    assert!(
        start.resume_fallback.is_none(),
        "no fallback warning either"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must be the boot document"
    );
}
