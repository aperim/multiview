//! The shared [`AppState`] the axum router carries.
//!
//! Per ADR-W001/W008 state sharing is an idiomatic `Arc<AppState>` holding the
//! engine's outbound subscription, the inbound command bus sender, the resource
//! repository, the auth store, and the idempotency/operation bookkeeping. Every
//! handle here is either control-only state or one of the engine's
//! isolation-safe channels — nothing in `AppState` can back-pressure the engine
//! (invariant #10).
use std::sync::Arc;

use mosaic_core::time::MediaTime;
use mosaic_engine::EnginePublisher;
use mosaic_events::Event;

use crate::alarm_store::{AlarmRepository, InMemoryAlarmStore};
use crate::audit::{AuditRepository, InMemoryAuditLog};
use crate::auth::ApiKeyStore;
use crate::command::CommandSender;
use crate::concurrency::IdempotencyStore;
use crate::nmos::NmosRegistry;
use crate::repository::Repository;
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
            alarms: Arc::new(InMemoryAlarmStore::new()),
            salvos: Arc::new(InMemorySalvoStore::new()),
            tally: Arc::new(TallyMirror::new()),
            tally_overrides: Arc::new(OverrideRegistry::new()),
            tally_profiles: Arc::new(InMemoryProfileStore::new()),
            nmos: Arc::new(NmosRegistry::new()),
            routes: Arc::new(RouteTable::new()),
            api_keys,
            jwt: None,
            jwt_api_name: "mosaic".to_owned(),
            audit: Arc::new(InMemoryAuditLog::new()),
            config_versions: Arc::new(InMemoryConfigVersionStore::new()),
            idempotency: Arc::new(IdempotencyStore::new()),
            ack_clock: Arc::new(system_ack_clock),
        }
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
