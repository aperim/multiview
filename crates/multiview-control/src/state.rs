//! The shared [`AppState`] the axum router carries.
//!
//! Per ADR-W001/W008 state sharing is an idiomatic `Arc<AppState>` holding the
//! engine's outbound subscription, the inbound command bus sender, the resource
//! repository, the auth store, and the idempotency/operation bookkeeping. Every
//! handle here is either control-only state or one of the engine's
//! isolation-safe channels — nothing in `AppState` can back-pressure the engine
//! (invariant #10).
use std::sync::Arc;

use multiview_config::MultiviewConfig;
use multiview_core::time::MediaTime;
use multiview_engine::EnginePublisher;
use multiview_events::Event;

use crate::alarm_store::{AlarmRepository, InMemoryAlarmStore};
use crate::audit::{AuditRepository, InMemoryAuditLog};
use crate::auth::ApiKeyStore;
use crate::command::CommandSender;
use crate::concurrency::IdempotencyStore;
use crate::error::{ControlError, ControlResult};
use crate::nmos::NmosRegistry;
use crate::repository::{InMemoryRepository, LayoutInput, Repository};
use crate::resource_store::{
    InMemoryOutputStore, InMemoryOverlayStore, InMemorySourceStore, ResourceInput,
    ResourceRepository,
};
use crate::router::RouteTable;
use crate::salvo_store::{InMemorySalvoStore, SalvoRepository};
use crate::tally_state::{
    InMemoryProfileStore, OverrideRegistry, TallyMirror, TallyProfileRepository,
};
use crate::versioning::{ConfigVersionStore, InMemoryConfigVersionStore};

/// A monotonic source of acknowledgement timestamps on the media timeline.
///
/// Acknowledging an alarm records *when* an operator acked it. The control plane
/// is not on the media clock, so it injects a clock here; the default reads the
/// system clock as nanoseconds-since-Unix-epoch (saturating into `i64`), and
/// tests inject a deterministic clock. The clock is read **off** the engine and
/// never touches the data plane.
pub type AckClock = Arc<dyn Fn() -> MediaTime + Send + Sync>;

/// The engine state-snapshot type the realtime layer republishes.
///
/// The engine publishes its newest snapshot into the wait-free latest-state slot
/// (`EnginePublisher::state`); the control plane reads it to build the
/// subscribe-time snapshot frame. It is intentionally an opaque JSON value here
/// so this crate does not couple to the engine's internal state shape.
pub type EngineStateSnapshot = serde_json::Value;

/// The control-plane resource stores seeded from a loaded [`MultiviewConfig`].
///
/// Produced by [`seed_resources`] and installed onto an [`AppState`] with
/// [`AppState::with_seeded_resources`], so the web UI Sources/Outputs/Overlays
/// (and layout) pages are non-empty under a live `multiview run` instead of
/// starting blank. The stores are ordinary in-memory control-plane state:
/// read-mostly, never on the engine's data plane, so they cannot back-pressure
/// the engine (invariant #10). Seeding happens once at bind time, off the
/// per-tick hot loop.
pub struct SeededResources {
    /// The `sources` store, one resource per `config.sources`.
    pub sources: Arc<dyn ResourceRepository>,
    /// The `outputs` store, one resource per `config.outputs`.
    pub outputs: Arc<dyn ResourceRepository>,
    /// The `overlays` store, one resource per `config.overlays`.
    pub overlays: Arc<dyn ResourceRepository>,
    /// The layout store carrying the single working layout (canvas + cells).
    pub layouts: Arc<dyn Repository>,
}

/// Map a `serde_json` serialization fault to a repository error.
///
/// Serializing the config-as-code value types ([`Source`](multiview_config::Source)
/// / [`Output`](multiview_config::Output) / [`Overlay`](multiview_config::Overlay))
/// has no failing path in practice (plain derived `Serialize`, no non-string map
/// keys), but the guardrails forbid `unwrap`/`expect`, so the `Result` is
/// propagated rather than panicked.
fn to_body(value: &impl serde::Serialize) -> ControlResult<serde_json::Value> {
    serde_json::to_value(value)
        .map_err(|e| ControlError::Repository(format!("serializing a config resource body: {e}")))
}

/// Build fresh in-memory resource stores seeded from `config`, mirroring one
/// resource per `config.sources` / `config.outputs` / `config.overlays` plus the
/// single working layout (canvas + cells) into the layout store.
///
/// Each resource's `body` is the typed config value serialized to canonical JSON
/// (`serde_json::to_value`), so it round-trips back to the config type — engine-
/// side validation still happens at apply time; the store keeps the document
/// opaque (`resource_store` doc). Ids:
/// * **sources** / **overlays** use their intrinsic config `id`;
/// * **outputs** have no intrinsic id in the schema, so a stable, index-derived
///   `output-{n}` id is assigned in config order (deterministic across runs of
///   the same config).
///
/// The function never fails on an otherwise-runnable config: config validation
/// (run before this) already enforces unique source ids, so the `create` calls
/// cannot collide; a serialization fault (not expected for these derived types)
/// surfaces as [`ControlError::Repository`] rather than a panic.
///
/// Isolation (invariant #10): this allocates plain control-plane stores and runs
/// once at bind time — it touches no engine channel and is off the hot loop.
///
/// # Errors
///
/// [`ControlError::Repository`] if a config value fails to serialize, and any
/// [`ControlError`] a backing `create` surfaces (e.g. a duplicate id — not
/// expected for a validated config).
pub fn seed_resources(config: &MultiviewConfig) -> ControlResult<SeededResources> {
    let sources = InMemorySourceStore::new();
    for source in &config.sources {
        let name = source
            .display_name
            .clone()
            .unwrap_or_else(|| source.id.clone());
        sources.create(
            &source.id,
            ResourceInput {
                name,
                body: to_body(source)?,
            },
        )?;
    }

    let overlays = InMemoryOverlayStore::new();
    for overlay in &config.overlays {
        overlays.create(
            &overlay.id,
            ResourceInput {
                name: overlay.id.clone(),
                body: to_body(overlay)?,
            },
        )?;
    }

    let outputs = InMemoryOutputStore::new();
    for (index, output) in config.outputs.iter().enumerate() {
        // Outputs carry no intrinsic id in the config schema; assign a stable,
        // config-order id so the resource is addressable. The `kind` tag (read
        // back from the serialized body) is the human-friendly name.
        let body = to_body(output)?;
        let kind = body
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("output")
            .to_owned();
        let id = format!("output-{index}");
        outputs.create(&id, ResourceInput { name: kind, body })?;
    }

    let layouts = InMemoryRepository::new();
    seed_working_layout(config, &layouts)?;

    Ok(SeededResources {
        sources: Arc::new(sources),
        outputs: Arc::new(outputs),
        overlays: Arc::new(overlays),
        layouts: Arc::new(layouts),
    })
}

/// Seed the single working layout (canvas + grid/layout strategy + cells) into
/// the layout store so the web UI layout page is non-empty under a live run.
///
/// The body is the authored shape — `{ canvas, layout, cells }` as canonical
/// JSON — kept opaque exactly like every other layout document; the editor reads
/// and the engine validates it on apply. The id/name is the solved working
/// layout's name when the config solves, else the stable fallback `"working"`
/// (seeding must not fail just because a config would not yet solve — it still
/// mirrors the authored cells).
fn seed_working_layout(
    config: &MultiviewConfig,
    layouts: &InMemoryRepository,
) -> ControlResult<()> {
    let id = config
        .solve_layout()
        .ok()
        .map_or_else(|| "working".to_owned(), |layout| layout.name);
    let body = serde_json::json!({
        "canvas": to_body(&config.canvas)?,
        "layout": to_body(&config.layout)?,
        "cells": to_body(&config.cells)?,
    });
    layouts.create_layout(
        &id,
        LayoutInput {
            name: id.clone(),
            body,
        },
    )?;
    Ok(())
}

/// The shared application state.
///
/// Cloned cheaply (`Arc`) into every handler via axum's `State` extractor. It
/// bundles:
///
/// * `engine` — the engine's outbound [`EnginePublisher`]: a wait-free
///   latest-state slot plus a drop-oldest event broadcast. The control plane
///   only ever **reads/subscribes**, never sends on a path the engine awaits.
/// * `commands` — the inbound [`CommandSender`] (bounded, non-blocking
///   `try_submit`): the only channel control->engine, designed so it can never
///   block the engine.
/// * `repository` — the resource store (in-memory by default).
/// * `api_keys` — the API-key/RBAC store.
/// * `idempotency` — the `Idempotency-Key` dedupe store.
#[derive(Clone)]
pub struct AppState {
    /// The engine's outbound publisher (state slot + event broadcast).
    pub engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    /// The inbound, bounded, non-blocking command bus sender.
    pub commands: CommandSender,
    /// The resource repository (CRUD persistence).
    pub repository: Arc<dyn Repository>,
    /// The sources store (versioned CRUD over config-as-code managed inputs).
    pub sources: Arc<dyn ResourceRepository>,
    /// The outputs store (versioned CRUD over config-as-code managed sinks).
    pub outputs: Arc<dyn ResourceRepository>,
    /// The overlays store (versioned CRUD over config-as-code overlay layers).
    pub overlays: Arc<dyn ResourceRepository>,
    /// The alarm mirror store (versioned, fed from the engine event stream).
    pub alarms: Arc<dyn AlarmRepository>,
    /// The salvo definition store (versioned CRUD over config-as-code salvos).
    pub salvos: Arc<dyn SalvoRepository>,
    /// The resolved-tally mirror (latest-wins, fed from the engine event stream).
    pub tally: Arc<TallyMirror>,
    /// The operator manual-tally-override registry (the control-plane record of
    /// override requests submitted to the engine).
    pub tally_overrides: Arc<OverrideRegistry>,
    /// The tally-profile store (versioned CRUD over config-as-code profiles).
    pub tally_profiles: Arc<dyn TallyProfileRepository>,
    /// The NMOS resource registry (node/device/sender/receiver + IS-05
    /// connection state) served by the NMOS Node API.
    pub nmos: Arc<NmosRegistry>,
    /// The router crosspoint mirror feeding route-follow (control-plane only).
    pub routes: Arc<RouteTable>,
    /// The API-key + RBAC store.
    pub api_keys: Arc<ApiKeyStore>,
    /// The optional `OAuth2`/JWT validator. When configured, a `Bearer` token that
    /// is not a native API key is validated as an IS-10-aligned JWT (signature +
    /// issuer/audience/expiry, `alg=none` refused) and its claims mapped to a
    /// [`Role`](crate::auth::Role). `None` means JWT auth is disabled (native
    /// API keys only). This is an **alternative** authn path, not a replacement:
    /// per-object/per-output authorization (BOLA defense) is enforced
    /// identically for both.
    pub jwt: Option<Arc<crate::jwt::JwtValidator>>,
    /// The audience name (resource server id) a JWT must target, and the NMOS
    /// API name the claim's grant is read against when mapping to a role. Unused
    /// when [`AppState::jwt`] is `None`.
    pub jwt_api_name: String,
    /// The change audit log: every successful mutation is recorded here
    /// (who/what/when) and queryable read-only over HTTP.
    pub audit: Arc<dyn AuditRepository>,
    /// The config/layout revision store (immutable revisions + diff + rollback).
    pub config_versions: Arc<dyn ConfigVersionStore>,
    /// The `Idempotency-Key` deduplication store.
    pub idempotency: Arc<IdempotencyStore>,
    /// The clock used to stamp alarm acknowledgements **and** audit entries.
    pub ack_clock: AckClock,
    /// The live-preview provider (program + per-input JPEG stills). The default
    /// ([`NoPreview`](crate::preview::NoPreview)) yields no frames; the binary
    /// swaps in an engine-backed provider. Isolation-safe (invariant #10).
    pub preview: crate::preview::SharedPreview,
    /// The WHEP focus transport seam (sub-second WebRTC focus per scope). The
    /// default ([`NoWhep`](crate::preview::NoWhep)) refuses every focus with an
    /// honest `503 fallback`; the binary swaps in a transport-backed provider
    /// (str0m / sidecar) behind a further gate. Isolation-safe (invariant #10):
    /// a focus session is a best-effort preview consumer that can never
    /// back-pressure the engine.
    pub whep: crate::preview::SharedWhep,
}

/// The default [`AckClock`]: system time as nanoseconds since the Unix epoch.
#[must_use]
fn system_ack_clock() -> MediaTime {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    MediaTime::from_nanos(nanos)
}

impl AppState {
    /// Assemble an [`AppState`] from its parts, with an in-memory alarm store and
    /// the system acknowledgement clock.
    #[must_use]
    pub fn new(
        engine: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
        commands: CommandSender,
        repository: Arc<dyn Repository>,
        api_keys: Arc<ApiKeyStore>,
    ) -> Self {
        Self {
            engine,
            commands,
            repository,
            sources: Arc::new(InMemorySourceStore::new()),
            outputs: Arc::new(InMemoryOutputStore::new()),
            overlays: Arc::new(InMemoryOverlayStore::new()),
            alarms: Arc::new(InMemoryAlarmStore::new()),
            salvos: Arc::new(InMemorySalvoStore::new()),
            tally: Arc::new(TallyMirror::new()),
            tally_overrides: Arc::new(OverrideRegistry::new()),
            tally_profiles: Arc::new(InMemoryProfileStore::new()),
            nmos: Arc::new(NmosRegistry::new()),
            routes: Arc::new(RouteTable::new()),
            api_keys,
            jwt: None,
            jwt_api_name: "multiview".to_owned(),
            audit: Arc::new(InMemoryAuditLog::new()),
            config_versions: Arc::new(InMemoryConfigVersionStore::new()),
            idempotency: Arc::new(IdempotencyStore::new()),
            ack_clock: Arc::new(system_ack_clock),
            preview: crate::preview::no_preview(),
            whep: crate::preview::no_whep(),
        }
    }

    /// Replace the live-preview provider (the binary wires an engine-backed one;
    /// the default yields no frames).
    #[must_use]
    pub fn with_preview(mut self, preview: crate::preview::SharedPreview) -> Self {
        self.preview = preview;
        self
    }

    /// Replace the WHEP focus transport provider (the binary wires a
    /// transport-backed one; the default refuses every focus with a `fallback`).
    #[must_use]
    pub fn with_whep(mut self, whep: crate::preview::SharedWhep) -> Self {
        self.whep = whep;
        self
    }

    /// Enable `OAuth2`/JWT authentication as an alternative to API keys, validating
    /// tokens with `validator` and mapping the grant for `api_name` to a role.
    #[must_use]
    pub fn with_jwt(
        mut self,
        validator: Arc<crate::jwt::JwtValidator>,
        api_name: impl Into<String>,
    ) -> Self {
        self.jwt = Some(validator);
        self.jwt_api_name = api_name.into();
        self
    }

    /// Replace the audit log (e.g. to share one store with a test or a
    /// persistent backend).
    #[must_use]
    pub fn with_audit_log(mut self, audit: Arc<dyn AuditRepository>) -> Self {
        self.audit = audit;
        self
    }

    /// Replace the config-version store (e.g. to share one with a test).
    #[must_use]
    pub fn with_config_versions(mut self, config_versions: Arc<dyn ConfigVersionStore>) -> Self {
        self.config_versions = config_versions;
        self
    }

    /// Record a successful mutation in the audit log, stamped with the current
    /// acknowledgement clock. Convenience used by the mutating handlers so the
    /// who/what/when is captured in one call after the mutation succeeds.
    pub fn audit(
        &self,
        actor: &str,
        action: crate::audit::AuditAction,
        object_kind: &str,
        object_id: &str,
        detail: Option<serde_json::Value>,
    ) {
        self.audit.record(
            actor,
            action,
            object_kind,
            object_id,
            self.ack_now(),
            detail,
        );
    }

    /// Replace the alarm store (e.g. to share one store with an ingest task or
    /// to use the SQLite-backed implementation).
    #[must_use]
    pub fn with_alarm_store(mut self, alarms: Arc<dyn AlarmRepository>) -> Self {
        self.alarms = alarms;
        self
    }

    /// Replace the salvo store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_salvo_store(mut self, salvos: Arc<dyn SalvoRepository>) -> Self {
        self.salvos = salvos;
        self
    }

    /// Replace the sources store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_sources_store(mut self, sources: Arc<dyn ResourceRepository>) -> Self {
        self.sources = sources;
        self
    }

    /// Replace the outputs store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_outputs_store(mut self, outputs: Arc<dyn ResourceRepository>) -> Self {
        self.outputs = outputs;
        self
    }

    /// Replace the overlays store (e.g. to share one store with a test).
    #[must_use]
    pub fn with_overlays_store(mut self, overlays: Arc<dyn ResourceRepository>) -> Self {
        self.overlays = overlays;
        self
    }

    /// Install resource stores seeded from a loaded config ([`seed_resources`]),
    /// replacing the empty default sources/outputs/overlays stores **and** the
    /// layout repository in one call.
    ///
    /// The binary uses this so the web UI resource pages reflect the running
    /// config's sources/outputs/overlays/layout. Read-mostly control-plane state;
    /// installed once at bind time, off the engine hot loop (invariant #10).
    #[must_use]
    pub fn with_seeded_resources(mut self, seeded: SeededResources) -> Self {
        self.sources = seeded.sources;
        self.outputs = seeded.outputs;
        self.overlays = seeded.overlays;
        self.repository = seeded.layouts;
        self
    }

    /// Replace the resolved-tally mirror (e.g. to share one with the tally
    /// ingest task or a test).
    #[must_use]
    pub fn with_tally_mirror(mut self, tally: Arc<TallyMirror>) -> Self {
        self.tally = tally;
        self
    }

    /// Replace the tally-profile store.
    #[must_use]
    pub fn with_tally_profiles(mut self, profiles: Arc<dyn TallyProfileRepository>) -> Self {
        self.tally_profiles = profiles;
        self
    }

    /// Replace the NMOS registry (e.g. to share one seeded registry with a test
    /// or the NMOS registration task).
    #[must_use]
    pub fn with_nmos(mut self, nmos: Arc<NmosRegistry>) -> Self {
        self.nmos = nmos;
        self
    }

    /// Replace the router crosspoint mirror (e.g. to share one with a
    /// route-follow ingest task or a test).
    #[must_use]
    pub fn with_routes(mut self, routes: Arc<RouteTable>) -> Self {
        self.routes = routes;
        self
    }

    /// Replace the acknowledgement clock (used by tests for determinism).
    #[must_use]
    pub fn with_ack_clock(mut self, ack_clock: AckClock) -> Self {
        self.ack_clock = ack_clock;
        self
    }

    /// The current acknowledgement timestamp on the media timeline.
    #[must_use]
    pub fn ack_now(&self) -> MediaTime {
        (self.ack_clock)()
    }

    /// Attempt `OAuth2`/JWT authentication of an `Authorization` header value.
    ///
    /// Returns:
    /// * `None` — JWT auth is not configured, or the header is absent/not a
    ///   `Bearer` token (so the caller keeps the native API-key error).
    /// * `Some(Ok(principal))` — the JWT validated (signature + issuer/audience/
    ///   expiry, `alg=none` refused) and its grant mapped to a
    ///   [`Principal`](crate::Principal).
    /// * `Some(Err(_))` — a `Bearer` token was present and a validator is
    ///   configured, but validation failed; the caller surfaces this rejection.
    ///
    /// The validation time is read from the acknowledgement clock (Unix seconds),
    /// off the engine.
    #[must_use]
    pub fn authenticate_jwt(
        &self,
        header_value: Option<&str>,
    ) -> Option<Result<crate::auth::Principal, crate::error::ControlError>> {
        let validator = self.jwt.as_ref()?;
        let value = header_value?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))?
            .trim();
        let now_secs = self.ack_now().as_nanos().div_euclid(1_000_000_000);
        Some(self.map_jwt(validator, token, now_secs))
    }

    /// Validate `token` and map its claims to a [`Principal`](crate::Principal).
    fn map_jwt(
        &self,
        validator: &crate::jwt::JwtValidator,
        token: &str,
        now_secs: i64,
    ) -> Result<crate::auth::Principal, crate::error::ControlError> {
        let claims = validator
            .validate(token, now_secs)
            .map_err(|_| crate::error::ControlError::Unauthenticated)?;
        // The subject is the audit/authz identity; the NMOS grant for the
        // configured API maps to a role. A token granting no usable access is
        // forbidden (authenticated but unauthorized).
        let role = claims.role_for(&self.jwt_api_name).map_err(|_| {
            crate::error::ControlError::Forbidden(format!(
                "JWT grants no access to API {:?}",
                self.jwt_api_name
            ))
        })?;
        Ok(crate::auth::Principal {
            key_id: claims.sub,
            role,
            // JWT principals are not object/output scoped here; per-object and
            // per-output BOLA guards still run and pass (unscoped). A deployment
            // mapping JWT claims to scopes would populate these.
            scoped_object_ids: None,
            scoped_output_ids: None,
        })
    }
}

#[cfg(test)]
mod seed_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use multiview_config::{MultiviewConfig, Output, Overlay, Source};

    use super::seed_resources;

    /// A config carrying 3 sources, 2 outputs, and 1 overlay (plus the
    /// canvas/layout/cells the parser requires).
    const SEED_DOC: &str = r##"schema_version = 1
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
id = "cam_a"
display_name = "Camera A"
kind = "rtsp"
url = "rtsp://x/a"
[[sources]]
id = "cam_b"
kind = "hls"
url = "https://x/b.m3u8"
[[sources]]
id = "cam_c"
kind = "ndi"
name = "STUDIO (CAM C)"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "cam_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "cam_b"
[[overlays]]
id = "clock_1"
kind = "clock"
target = "canvas"
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
[[outputs]]
kind = "ll_hls"
path = "/srv/hls"
codec = "h264"
"##;

    /// A minimal config with NO sources/outputs/overlays (still parses: canvas +
    /// grid layout only).
    const EMPTY_DOC: &str = r##"schema_version = 1
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
"##;

    #[test]
    fn seeds_one_resource_per_config_source() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        let listed = seeded.sources.list().expect("list sources");
        let ids: Vec<&str> = listed.iter().map(|v| v.resource.id.as_str()).collect();
        // id-sorted, exactly the three config source ids.
        assert_eq!(ids, vec!["cam_a", "cam_b", "cam_c"]);

        // The display name is mirrored when present, else the id.
        let cam_a = seeded.sources.get("cam_a").expect("cam_a present");
        assert_eq!(cam_a.resource.name, "Camera A");
        let cam_b = seeded.sources.get("cam_b").expect("cam_b present");
        assert_eq!(cam_b.resource.name, "cam_b");
    }

    #[test]
    fn mirror_roundtrips_source_body() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        for want in &config.sources {
            let stored = seeded.sources.get(&want.id).expect("source present");
            let got: Source =
                serde_json::from_value(stored.resource.body.clone()).expect("body is a Source");
            assert_eq!(
                &got, want,
                "seeded body must round-trip to the config source"
            );
        }
    }

    #[test]
    fn seeds_outputs_and_overlays_with_roundtrip_bodies() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        let outputs = seeded.outputs.list().expect("list outputs");
        assert_eq!(outputs.len(), 2, "two config outputs seeded");
        // Each stored output body round-trips to the typed config Output, in order.
        for (versioned, want) in outputs.iter().zip(config.outputs.iter()) {
            let got: Output =
                serde_json::from_value(versioned.resource.body.clone()).expect("body is an Output");
            assert_eq!(&got, want);
        }

        let overlays = seeded.overlays.list().expect("list overlays");
        let overlay_ids: Vec<&str> = overlays.iter().map(|v| v.resource.id.as_str()).collect();
        assert_eq!(overlay_ids, vec!["clock_1"]);
        let stored = seeded.overlays.get("clock_1").expect("overlay present");
        let got: Overlay =
            serde_json::from_value(stored.resource.body.clone()).expect("body is an Overlay");
        assert_eq!(got, config.overlays[0]);
    }

    #[test]
    fn empty_config_yields_empty_stores() {
        let config = MultiviewConfig::load_from_toml(EMPTY_DOC).expect("parse empty config");
        let seeded = seed_resources(&config).expect("seed resources");

        assert!(seeded.sources.list().expect("list").is_empty());
        assert!(seeded.outputs.list().expect("list").is_empty());
        assert!(seeded.overlays.list().expect("list").is_empty());
    }

    #[test]
    fn seeds_a_layout_resource_from_canvas_and_cells() {
        let config = MultiviewConfig::load_from_toml(SEED_DOC).expect("parse seed config");
        let seeded = seed_resources(&config).expect("seed resources");

        // The single working layout is mirrored so the web UI layout page is
        // non-empty under a live run; its body carries the two authored cells.
        let layouts = seeded.layouts.list_layouts().expect("list layouts");
        assert_eq!(layouts.len(), 1, "one working layout seeded");
        let body = &layouts[0].layout.body;
        let cells = body
            .get("cells")
            .and_then(|c| c.as_array())
            .expect("layout body carries a cells array");
        assert_eq!(
            cells.len(),
            2,
            "both authored cells mirrored into the layout"
        );
    }
}
