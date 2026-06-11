//! The Boot / Loaded / Running configuration model (ADR-W022).
//!
//! * **Loaded** is the immutable boot snapshot held in memory (and persisted
//!   as `loaded.toml`); **Running** is Loaded + every live change, debounced-
//!   persisted to `active.toml` (atomic rename, machine-written, NEVER
//!   watched), composed by the SAME document composition the export uses.
//! * `POST /api/v1/config/revert-to-start` applies `diff(running, loaded)`
//!   through the ONE ADR-W020 apply machinery (live where live, honest
//!   restart warnings elsewhere).
//! * `POST /api/v1/config/promote` writes Running to the BOOT file path via
//!   the watcher's `expect_write()` suppression seam — promoting must NOT
//!   re-trigger a file-watch apply.
//! * `GET /api/v1/config/boot-model` reports the per-section divergence the
//!   UI indicator shows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{
    finish_running_persist, load_resume_config, persist_running_now, spawn_running_persist,
    write_atomic, BootModel,
};
use multiview_control::config_watch::{spawn as spawn_watch, WatchOptions};
use multiview_control::{
    command_bus, run_warning_ingest, AppState, Command, CommandReceiver, EngineStateSnapshot,
    InMemoryRepository, InMemoryWarningStore, WarningFilter, WarningRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use support::{body_json, get, put_json, send, OPERATOR_TOKEN, VIEWER_TOKEN};

/// The fast poll the watcher-interplay tests run at (real default is 1 s).
const TEST_POLL: Duration = Duration::from_millis(80);

/// How long assertions wait for a task to act.
const SETTLE: Duration = Duration::from_secs(10);

/// The boot document: one solid source, one bars source, two cells, one
/// output, and a control listener (so the composed Running document carries a
/// `[control]` block like a real deployment's).
const BOOT_DOC: &str = r##"schema_version = 1
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
[control]
listen = "[::1]:0"
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
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
path = "/tmp/boot-config-model.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

/// The whole rig: a temp config dir holding the boot file, the shared
/// control-plane state (stores seeded from the boot doc + the boot model),
/// the router, and the engine-side command receiver.
struct Rig {
    /// Held for its `Drop` (removes the temp dir).
    _dir: tempfile::TempDir,
    boot_path: PathBuf,
    config: MultiviewConfig,
    state: AppState,
    router: Router,
    commands: CommandReceiver,
    warnings: Arc<dyn WarningRepository>,
}

/// Build the rig from `doc` written to a temp boot file, with the stores
/// seeded from it and a [`BootModel`] whose Loaded snapshot is the parsed
/// document (boot-mode start).
fn rig(doc: &str) -> Rig {
    rig_with(doc, 64)
}

/// [`rig`] with a chosen command-bus `capacity` (the shed tests run at 1).
fn rig_with(doc: &str, capacity: usize) -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, doc).expect("write boot config");
    let config = MultiviewConfig::load_from_toml(doc).expect("parse boot config");
    config.validate().expect("boot config validates");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, commands) = command_bus(capacity);
    let warnings: Arc<dyn WarningRepository> = Arc::new(InMemoryWarningStore::new());
    tokio::spawn(run_warning_ingest(
        publisher.subscribe(),
        Arc::clone(&warnings),
    ));
    let seeded = multiview_control::seed_resources(&config).expect("seed stores");
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&config).expect("boot doc to JSON"))
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )));
    let router = multiview_control::router(state.clone());
    Rig {
        _dir: dir,
        boot_path,
        config,
        state,
        router,
        commands,
        warnings,
    }
}

/// Whether the active warning list carries `code`.
fn has_active_warning(warnings: &Arc<dyn WarningRepository>, code: &str) -> bool {
    warnings
        .list(&WarningFilter::active_only())
        .expect("list warnings")
        .iter()
        .any(|w| w.code.as_str() == code)
}

/// Build a bodyless `POST` with a Bearer token and an `Idempotency-Key`.
fn post_idem(path: &str, token: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    builder.body(Body::empty()).expect("request should build")
}

/// Await `predicate` (polled every 20 ms) until it holds or `deadline` lapses.
async fn wait_until(deadline: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + deadline;
    while std::time::Instant::now() < end {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

/// Re-colour `in_a` through the REAL sources route (GET → `ETag` → PUT with
/// `If-Match`), exactly as the UI mutates Running.
async fn recolor_in_a(r: &Rig, color: &str) {
    let resp = send(&r.router, get("/api/v1/sources/in_a", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK, "in_a must be readable");
    let etag = support::etag(&resp).expect("source carries an ETag");
    let resp = send(
        &r.router,
        put_json(
            "/api/v1/sources/in_a",
            OPERATOR_TOKEN,
            Some(&etag),
            &serde_json::json!({
                "name": "in_a",
                "body": { "id": "in_a", "kind": "solid", "color": color }
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "the live edit must land");
}

/// The stored colour of `in_a` in the sources store.
fn stored_color(state: &AppState) -> Option<String> {
    state
        .sources
        .get("in_a")
        .ok()
        .and_then(|v| v.resource.body.get("color").cloned())
        .and_then(|v| v.as_str().map(str::to_owned))
}

/// `GET /config/boot-model` reports no divergence at start, then names the
/// changed section (vs Loaded AND vs the untouched boot file) after a live
/// API edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_model_status_reports_per_section_divergence() {
    let r = rig(BOOT_DOC);

    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["modeled"], serde_json::json!(true));
    assert_eq!(
        body["boot_path"],
        serde_json::json!(r.boot_path.display().to_string())
    );
    assert_eq!(body["start"], serde_json::json!("boot"));
    assert_eq!(body["resumed"], serde_json::json!(false));
    assert_eq!(
        body["diverged_from_loaded"],
        serde_json::json!([]),
        "a fresh run has not diverged from the Loaded snapshot"
    );
    assert_eq!(
        body["diverged_from_boot_file"],
        serde_json::json!([]),
        "a fresh run matches the boot file it started from"
    );

    recolor_in_a(&r, "#f0f0f0").await;

    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    let loaded_divergence = body["diverged_from_loaded"]
        .as_array()
        .expect("diverged_from_loaded is an array")
        .clone();
    assert!(
        loaded_divergence.contains(&serde_json::json!("sources")),
        "the live source edit must show as a sources divergence, got {loaded_divergence:?}"
    );
    let file_divergence = body["diverged_from_boot_file"]
        .as_array()
        .expect("diverged_from_boot_file is an array")
        .clone();
    assert!(
        file_divergence.contains(&serde_json::json!("sources")),
        "the untouched boot file must also differ, got {file_divergence:?}"
    );
}

/// A store-only deployment (no boot model) is honest: `modeled: false`, and
/// both actions refuse with a problem document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_boot_model_the_surface_is_honest() {
    let h = support::harness();

    let resp = send(&h.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["modeled"], serde_json::json!(false));

    let resp = send(
        &h.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "revert without a boot model must refuse with 409"
    );

    let resp = send(
        &h.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "promote without a boot model must refuse with 409"
    );
}

/// Revert-to-start is a write action: a read-only key is forbidden.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_and_promote_require_the_write_role() {
    let r = rig(BOOT_DOC);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", VIEWER_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", VIEWER_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// THE REVERT SLICE: after a live API edit, `revert-to-start` puts Running
/// back to Loaded THROUGH the one apply machinery — an `UpsertSource`
/// carrying the Loaded document rides the bounded bus, the store resyncs to
/// the Loaded value, and the action is audited under the requesting
/// principal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_to_start_restores_loaded_live() {
    let mut r = rig(BOOT_DOC);
    recolor_in_a(&r, "#f0f0f0").await;
    assert_eq!(stored_color(&r.state).as_deref(), Some("#f0f0f0"));
    // The live edit itself rides UpsertSource (ADR-W018); clear it so the
    // assertions below see only what the REVERT submits.
    let _ = r.commands.try_drain();

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(body["reverted"], serde_json::json!(true));
    assert!(
        body["operation_id"].as_str().is_some_and(|s| !s.is_empty()),
        "the 202 carries an operation id"
    );
    let summary = body["summary"].as_array().expect("summary array").clone();
    assert!(
        summary
            .iter()
            .any(|p| p.as_str().is_some_and(|s| s.contains("in_a"))),
        "the per-section summary names the reverted source, got {summary:?}"
    );

    // Running := Loaded — the store mirrors the boot colour again…
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "the sources store must resync to the Loaded snapshot"
    );
    // …and the SAME live machinery carried it: an UpsertSource with the
    // Loaded document's colour.
    let drained = r.commands.try_drain();
    let reverted = drained.iter().any(|c| match c {
        Command::UpsertSource { source, .. } => {
            source.id == "in_a"
                && serde_json::to_value(source.as_ref())
                    .ok()
                    .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                    .as_deref()
                    == Some("#101418")
        }
        _ => false,
    });
    assert!(
        reverted,
        "revert must ride UpsertSource with the Loaded colour, got {drained:?}"
    );

    // The action is audited under the requesting principal.
    let entries = r.state.audit.list(None).expect("audit list");
    assert!(
        entries
            .iter()
            .any(|e| e.actor == "operator-key" && e.object_id == "revert-to-start"),
        "revert must be audited under the principal"
    );
}

/// Reverting when Running already equals Loaded applies nothing and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_with_no_divergence_is_an_honest_noop() {
    let mut r = rig(BOOT_DOC);
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["reverted"],
        serde_json::json!(false),
        "an undiverged Running must report reverted: false"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "an undiverged revert must enqueue no engine commands"
    );
}

/// Watch-interplay pin (c): revert AFTER a file-watch apply returns to
/// **Loaded**, not to the current (edited) boot file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revert_after_a_file_watch_apply_returns_to_loaded() {
    let mut r = rig(BOOT_DOC);
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );

    // External boot-file edit: in_a goes near-white; the watcher applies it.
    std::fs::write(&r.boot_path, BOOT_DOC.replace("#101418", "#f0f0f0")).expect("edit boot file");
    assert!(
        wait_until(SETTLE, || {
            r.state.config_watch.snapshot().applied_count >= 1
        })
        .await,
        "the watcher must apply the external edit"
    );
    assert_eq!(stored_color(&r.state).as_deref(), Some("#f0f0f0"));
    let _ = r.commands.try_drain();

    // Revert: Running goes back to the LOADED snapshot (#101418), even though
    // the boot file on disk now says #f0f0f0.
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#101418"),
        "revert must target Loaded, not the edited boot file"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the revert must ride the live machinery, got {drained:?}"
    );
    watch.stop();
}

/// THE PROMOTE SLICE: promote writes the current Running document to the
/// boot file path (valid TOML carrying the live edit), commits a `boot`
/// config revision, audits, and reports the written path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_writes_running_to_the_boot_file_and_versions_it() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#e0e0e0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body["path"],
        serde_json::json!(r.boot_path.display().to_string())
    );
    assert!(
        body["revision"].as_u64().is_some_and(|n| n >= 1),
        "promote must commit a config revision, got {body:?}"
    );

    // The boot file now IS the Running document: parses, validates, carries
    // the live edit.
    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted boot file");
    let promoted = MultiviewConfig::load_from_toml(&text).expect("promoted file parses");
    promoted.validate().expect("promoted file validates");
    let color = promoted
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(
        color.as_deref(),
        Some("#e0e0e0"),
        "the promoted file must carry the live edit"
    );

    // Versioned + audited.
    let history = r
        .state
        .config_versions
        .history("boot")
        .expect("boot history");
    assert_eq!(history.len(), 1, "one promote = one committed revision");
    let entries = r.state.audit.list(None).expect("audit list");
    assert!(
        entries
            .iter()
            .any(|e| e.actor == "operator-key" && e.object_id == "promote"),
        "promote must be audited under the principal"
    );

    // After promote the Running document matches the boot FILE (the Loaded
    // snapshot divergence remains — promote moves Boot, not Loaded).
    let resp = send(&r.router, get("/api/v1/config/boot-model", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    assert_eq!(
        body["diverged_from_boot_file"],
        serde_json::json!([]),
        "after promote, Running == the boot file"
    );
    let loaded_divergence = body["diverged_from_loaded"]
        .as_array()
        .expect("array")
        .clone();
    assert!(
        loaded_divergence.contains(&serde_json::json!("sources")),
        "the Loaded snapshot still differs (promote does not move Loaded)"
    );
}

/// A replayed `Idempotency-Key` answers without rewriting the file or
/// committing another revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_idempotency_replay_does_not_rewrite() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#d0d0d0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-1")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let first = body_json(resp).await;
    assert_eq!(first["replayed"], serde_json::json!(false));
    let written = std::fs::read_to_string(&r.boot_path).expect("read boot file");

    // A later live edit makes Running differ again; the REPLAY must not
    // re-promote it.
    recolor_in_a(&r, "#0a0a0a").await;
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, Some("promote-1")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let replay = body_json(resp).await;
    assert_eq!(replay["replayed"], serde_json::json!(true));
    assert_eq!(
        replay["operation_id"], first["operation_id"],
        "the replay echoes the original operation id"
    );
    assert_eq!(
        std::fs::read_to_string(&r.boot_path).expect("read boot file"),
        written,
        "a replay must not rewrite the boot file"
    );
    let history = r
        .state
        .config_versions
        .history("boot")
        .expect("boot history");
    assert_eq!(history.len(), 1, "a replay must not commit a new revision");
}

/// Watch-interplay pin (a): a promote does NOT re-trigger a file-watch apply
/// — the server-side write is suppressed via `expect_write()` and adopted as
/// the watcher's new baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_does_not_retrigger_a_file_watch_apply() {
    let mut r = rig(BOOT_DOC);
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );

    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Several poll cycles elapse: the watcher must adopt (not apply) the
    // promote's own write.
    tokio::time::sleep(TEST_POLL * 10).await;
    assert_eq!(
        r.state.config_watch.snapshot().applied_count,
        0,
        "a promote must not count as a file-watch apply"
    );
    assert!(
        r.commands.try_drain().is_empty(),
        "a promote's own write must enqueue no watcher commands"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#c0c0c0"),
        "the stores keep the promoted Running state untouched"
    );

    // Watching resumes against the ADOPTED baseline: a real external edit
    // still applies.
    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted file");
    std::fs::write(&r.boot_path, text.replace("#c0c0c0", "#101418")).expect("external edit");
    assert!(
        wait_until(SETTLE, || {
            r.state.config_watch.snapshot().applied_count >= 1
        })
        .await,
        "an external edit after a promote must still hot-apply"
    );
    watch.stop();
}

/// Watch-interplay pin (b): in a RESUMED run (the watcher's baseline is the
/// resumed Running document, the file still holds the boot document) an
/// external boot-file edit still hot-applies — and the diff is computed
/// against the RESUMED baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_external_boot_edit_during_a_resumed_run_still_hot_applies() {
    // The resumed Running state: in_a near-white (differs from the boot doc).
    let active_doc = BOOT_DOC.replace("#101418", "#f0f0f0");
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let boot_config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse boot");
    let active_config = MultiviewConfig::load_from_toml(&active_doc).expect("parse active");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, mut commands) = command_bus(64);
    // Stores seeded from the RESUMED document (it is the starting Running
    // state), Loaded stays the BOOT snapshot.
    let seeded = multiview_control::seed_resources(&active_config).expect("seed stores");
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(&active_config).expect("active doc to JSON"))
    .with_boot_model(Arc::new(BootModel::new(
        boot_path.clone(),
        boot_config,
        StartMode::Resume,
        true,
        None,
    )));
    // The watcher's baseline is the RESUMED Running document.
    let watch = spawn_watch(
        boot_path.clone(),
        active_config,
        state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );

    // External edit to the BOOT file: add a new source (in_a stays at the
    // boot colour, which DIFFERS from the resumed baseline's).
    std::fs::write(
        &boot_path,
        format!("{BOOT_DOC}[[sources]]\nid = \"live9\"\nkind = \"bars\"\n"),
    )
    .expect("edit boot file");
    assert!(
        wait_until(SETTLE, || {
            state.config_watch.snapshot().applied_count >= 1
        })
        .await,
        "the boot-file edit must hot-apply during a resumed run"
    );
    let drained = commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "live9"
        )),
        "the added source must ride UpsertSource, got {drained:?}"
    );
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "in_a must be re-applied: the file's colour differs from the RESUMED baseline, got {drained:?}"
    );
    watch.stop();
}

/// Pin (d): the persisted `active.toml` round-trips
/// `MultiviewConfig::load_from_toml` + `validate`, and carries the live edit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_toml_round_trips_validate() {
    let r = rig(BOOT_DOC);
    recolor_in_a(&r, "#b0b0b0").await;

    persist_running_now(&r.state).await.expect("persist Running");

    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    let text = std::fs::read_to_string(&active_path).expect("read active.toml");
    let active = MultiviewConfig::load_from_toml(&text).expect("active.toml parses");
    active.validate().expect("active.toml validates");
    let color = active
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(
        color.as_deref(),
        Some("#b0b0b0"),
        "active.toml must carry the live Running state"
    );
}

/// Pins (e)/(f) at the loader: a valid `active.toml` loads as the resume
/// state; a corrupt one is an error the caller falls back on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_resume_config_loads_valid_and_rejects_corrupt_active() {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let state_dir = dir.path().join(".multiview");
    std::fs::create_dir_all(&state_dir).expect("state dir");

    // Missing active: an error (the caller warns + falls back to boot).
    assert!(load_resume_config(&boot_path).is_err());

    // Valid active: loads as the starting Running state.
    std::fs::write(
        state_dir.join("active.toml"),
        BOOT_DOC.replace("#101418", "#f0f0f0"),
    )
    .expect("write active");
    let resumed = load_resume_config(&boot_path).expect("valid active loads");
    let color = resumed
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        });
    assert_eq!(color.as_deref(), Some("#f0f0f0"));

    // Corrupt active: an error naming the failure.
    std::fs::write(state_dir.join("active.toml"), "this is [not toml").expect("corrupt active");
    let err = load_resume_config(&boot_path).expect_err("corrupt active must be rejected");
    assert!(
        err.contains("parse") || err.contains("TOML") || err.contains("read"),
        "the fallback reason should be actionable, got: {err}"
    );
}

/// The Running persister: an audit-recorded mutation (the ONE choke point)
/// leads — debounced — to an atomic `active.toml` write that follows further
/// changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_persister_writes_active_toml_on_audited_changes() {
    let r = rig(BOOT_DOC);
    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    let task = spawn_running_persist(r.state.clone(), Duration::from_millis(50));

    // The choke point: a real route mutation records an audit entry, which
    // triggers the debounced persist.
    recolor_in_a(&r, "#a0a0a0").await;
    assert!(
        wait_until(SETTLE, || {
            std::fs::read_to_string(&active_path)
                .ok()
                .is_some_and(|t| t.contains("#a0a0a0"))
        })
        .await,
        "the persister must write active.toml carrying the audited change"
    );

    // A further change follows within another debounce window.
    recolor_in_a(&r, "#909090").await;
    assert!(
        wait_until(SETTLE, || {
            std::fs::read_to_string(&active_path)
                .ok()
                .is_some_and(|t| t.contains("#909090"))
        })
        .await,
        "the persister must follow subsequent changes"
    );
    // Atomic write hygiene: no temp residue next to the state files.
    let residue: Vec<String> = std::fs::read_dir(active_path.parent().expect("state dir"))
        .expect("read state dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "active.toml" && n != "loaded.toml")
        .collect();
    assert!(
        residue.is_empty(),
        "atomic writes must leave no temp residue, got {residue:?}"
    );
    task.abort();
}

/// Review M2 — the teardown ordering: `finish_running_persist` aborts the
/// persister, AWAITS its termination, and only then runs the final persist,
/// capturing changes younger than the debounce even when the task is parked
/// deep inside a long debounce window — with no temp residue (the
/// deterministic `.tmp` stays single-writer through the teardown).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_running_persist_captures_changes_younger_than_the_debounce() {
    let r = rig(BOOT_DOC);
    let active_path = r
        .state
        .boot_model
        .as_ref()
        .expect("boot model")
        .active_path();
    // A debounce far longer than the test: the change below stays younger
    // than the debounce throughout, so ONLY the ordered teardown persist can
    // capture it.
    let task = spawn_running_persist(r.state.clone(), Duration::from_secs(600));
    assert!(
        wait_until(SETTLE, || active_path.exists()).await,
        "the startup persist writes the starting Running state"
    );

    // The audited change fires the notify; the task then sleeps its 600 s
    // debounce — parked, with the change unpersisted.
    recolor_in_a(&r, "#717171").await;

    finish_running_persist(task, &r.state).await;

    let text = std::fs::read_to_string(&active_path).expect("read active.toml");
    assert!(
        text.contains("#717171"),
        "the final persist must capture the change younger than the debounce"
    );
    let residue: Vec<String> = std::fs::read_dir(active_path.parent().expect("state dir"))
        .expect("read state dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "active.toml" && n != "loaded.toml")
        .collect();
    assert!(
        residue.is_empty(),
        "the ordered teardown leaves no temp residue, got {residue:?}"
    );
}

/// Fail-soft: a state whose stores do not compose (no working layout) warns
/// and writes nothing — the persister never crashes and never wedges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_persister_is_fail_soft_when_composition_fails() {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _commands) = command_bus(64);
    // NO seeded resources: there is no working layout, so composition fails.
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_boot_model(Arc::new(BootModel::new(
        boot_path,
        config,
        StartMode::Boot,
        false,
        None,
    )));
    let active_path = state.boot_model.as_ref().expect("boot model").active_path();
    let task = spawn_running_persist(state.clone(), Duration::from_millis(50));

    state.audit(
        "test",
        multiview_control::AuditAction::Update,
        "source",
        "in_a",
        None,
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !active_path.exists(),
        "a failed composition must not write active.toml"
    );
    assert!(
        !task.is_finished(),
        "the persister must survive a composition failure (fail-soft)"
    );
    task.abort();
}

/// `write_atomic` replaces the destination in one rename and leaves no
/// temp-file residue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_replaces_and_leaves_no_residue() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("state.toml");
    write_atomic(&path, "first = 1\n").expect("first write");
    write_atomic(&path, "second = 2\n").expect("replace write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "second = 2\n"
    );
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["state.toml".to_owned()]);
}

/// The file mode of `path` (permission bits only).
#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o7777
}

/// Review M3: `write_atomic` must preserve the DESTINATION's mode — a
/// `chmod 600` boot/state file must not silently widen to the umask default
/// when it is atomically replaced.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_atomic_preserves_the_destination_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("state.toml");
    write_atomic(&path, "first = 1\n").expect("first write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
    write_atomic(&path, "second = 2\n").expect("replace write");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "second = 2\n"
    );
    assert_eq!(
        mode_of(&path),
        0o600,
        "an atomic replace must preserve the destination's mode (chmod-600 stays 600)"
    );
}

/// Review M3 at the route: a promote over a `chmod 600` boot file keeps the
/// boot file at mode 600 (credentials in the config must not leak to the
/// umask default through the temp-file + rename).
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promote_preserves_the_boot_file_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let r = rig(BOOT_DOC);
    std::fs::set_permissions(&r.boot_path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 600 the boot file");
    recolor_in_a(&r, "#e8e8e8").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let text = std::fs::read_to_string(&r.boot_path).expect("read promoted file");
    assert!(
        text.contains("#e8e8e8"),
        "the promoted file carries the live edit"
    );
    assert_eq!(
        mode_of(&r.boot_path),
        0o600,
        "promote must preserve the boot file's mode (chmod-600 stays 600)"
    );
}

/// Review B1 interleaving (1) — the settle-window race: promote writes W and
/// banks its expect token, but an external edit E lands BEFORE W settles, so
/// the watcher's first settled observation is E. E is a REAL external change:
/// it must be APPLIED through the one machinery, never adopted against the
/// banked (W-content) token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_promote_racing_an_external_edit_applies_the_edit() {
    let mut r = rig(BOOT_DOC);
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );

    recolor_in_a(&r, "#c0c0c0").await;
    let _ = r.commands.try_drain();
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The external edit lands immediately after the promote's write — well
    // inside the watcher's two-poll settle window, so the first SETTLED
    // observation is E, not the promote's W.
    std::fs::write(&r.boot_path, BOOT_DOC.replace("#101418", "#123456"))
        .expect("external edit inside the settle window");

    assert!(
        wait_until(SETTLE, || {
            r.state.config_watch.snapshot().applied_count >= 1
        })
        .await,
        "the racing external edit must be APPLIED, not adopted against the promote token"
    );
    assert_eq!(
        stored_color(&r.state).as_deref(),
        Some("#123456"),
        "the stores must follow the external edit"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| match c {
            Command::UpsertSource { source, .. } => {
                source.id == "in_a"
                    && serde_json::to_value(source.as_ref())
                        .ok()
                        .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
                        .as_deref()
                        == Some("#123456")
            }
            _ => false,
        }),
        "the edit must ride UpsertSource with E's colour, got {drained:?}"
    );
    watch.stop();
}

/// Review B1 interleaving (3) — the failed-write token leak: a promote that
/// banks its expect token and then FAILS to write the boot file must release
/// the token; a later REAL external edit that happens to carry the same
/// content must be APPLIED, never silently adopted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_promote_write_releases_the_banked_expect_token() {
    let mut r = rig(BOOT_DOC);
    let watch = spawn_watch(
        r.boot_path.clone(),
        r.config.clone(),
        r.state.clone(),
        WatchOptions::default().with_poll_interval(TEST_POLL),
    );
    recolor_in_a(&r, "#e0e0e0").await;
    let _ = r.commands.try_drain();

    // The exact content the promote will render: the export TOML (the same
    // `compose_running_config` + `to_toml` path the promote route uses).
    let resp = send(&r.router, get("/api/v1/config/export", OPERATOR_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let running_toml =
        String::from_utf8(support::body_bytes(resp).await).expect("export is UTF-8 TOML");

    // Force the atomic write to fail: occupy write_atomic's deterministic
    // temp name with a DIRECTORY (root-proof, unlike a permissions trick).
    let tmp_blocker = r.boot_path.parent().expect("dir").join(".multiview.toml.tmp");
    std::fs::create_dir(&tmp_blocker).expect("block the temp path");
    let before = std::fs::read_to_string(&r.boot_path).expect("read boot file");
    let resp = send(
        &r.router,
        post_idem("/api/v1/config/promote", OPERATOR_TOKEN, None),
    )
    .await;
    assert!(
        resp.status().is_server_error(),
        "the blocked write must fail the promote, got {}",
        resp.status()
    );
    assert_eq!(
        std::fs::read_to_string(&r.boot_path).expect("read boot file"),
        before,
        "a failed promote must leave the boot file untouched"
    );
    std::fs::remove_dir(&tmp_blocker).expect("unblock the temp path");

    // A REAL external edit now lands with exactly the content the failed
    // promote announced. The promote never wrote it, so it must be APPLIED
    // (UpsertSource for in_a's new colour) — a leaked token would eat it.
    std::fs::write(&r.boot_path, &running_toml).expect("external edit matching the failed write");
    assert!(
        wait_until(SETTLE, || {
            r.state.config_watch.snapshot().applied_count >= 1
        })
        .await,
        "the external edit must be APPLIED — the failed promote's token must not eat it"
    );
    let drained = r.commands.try_drain();
    assert!(
        drained.iter().any(|c| matches!(
            c,
            Command::UpsertSource { source, .. } if source.id == "in_a"
        )),
        "the matching-content edit must still ride UpsertSource, got {drained:?}"
    );
    watch.stop();
}

/// Review M4 — revert 202 honesty under a full bus: when engine command(s)
/// are SHED the response must NOT claim a full revert (`reverted: false`,
/// `shed` count surfaced, partial summary) and the
/// `config-file-apply-incomplete` warning path fires so the operator sees it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shed_revert_reports_partial_and_raises_the_warning() {
    let r = rig_with(BOOT_DOC, 1);
    // The live edit's own UpsertSource fills the capacity-1 bus; the revert's
    // submission below is shed.
    recolor_in_a(&r, "#f0f0f0").await;

    let resp = send(
        &r.router,
        post_idem("/api/v1/config/revert-to-start", OPERATOR_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body["reverted"],
        serde_json::json!(false),
        "a shed revert must NOT claim reverted: true, got {body:?}"
    );
    assert!(
        body["shed"].as_u64().is_some_and(|n| n >= 1),
        "the shed count must be surfaced, got {body:?}"
    );
    let summary = body["summary"].as_array().expect("summary array").clone();
    assert!(
        !summary.is_empty(),
        "the partial summary still names what was attempted"
    );
    // The stores resync to Loaded on the first pass either way…
    assert_eq!(stored_color(&r.state).as_deref(), Some("#101418"));
    // …and the operator is told the engine did not get every command.
    assert!(
        wait_until(SETTLE, || has_active_warning(
            &r.warnings,
            "config-file-apply-incomplete"
        ))
        .await,
        "a shed revert must raise the config-file-apply-incomplete warning"
    );
}

/// Review m3 — reservation release on failed compose: a revert/promote whose
/// Running composition FAILS (422) must release its `Idempotency-Key`
/// reservation, so a retry with the same key actually runs (never answers as
/// a replay of a request that did nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_compose_releases_the_idempotency_reservation() {
    // A state with a boot model but NO seeded resources: there is no working
    // layout, so `compose_running_config` fails with 422.
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, BOOT_DOC).expect("write boot file");
    let config = MultiviewConfig::load_from_toml(BOOT_DOC).expect("parse");
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _commands) = command_bus(64);
    let state = AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_boot_model(Arc::new(BootModel::new(
        boot_path,
        config,
        StartMode::Boot,
        false,
        None,
    )));
    let router = multiview_control::router(state);

    for (path, key) in [
        ("/api/v1/config/revert-to-start", "revert-key-1"),
        ("/api/v1/config/promote", "promote-key-1"),
    ] {
        for attempt in 1..=2_u8 {
            let resp = send(&router, post_idem(path, OPERATOR_TOKEN, Some(key))).await;
            assert_eq!(
                resp.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{path} attempt {attempt} must fail compose with 422 — a replayed \
                 reservation would answer 2xx instead"
            );
        }
    }
}
