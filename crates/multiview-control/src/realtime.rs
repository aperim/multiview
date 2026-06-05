//! Realtime fan-out: WebSocket (primary) and SSE (one-way fallback).
//!
//! The realtime layer turns the engine's drop-oldest event broadcast
//! (`EnginePublisher::events`) into a per-client stream of
//! [`Envelope<Event>`](multiview_events::Envelope) frames, following the
//! snapshot-then-delta + resume-by-seq model (ADR-RT002/RT003):
//!
//! 1. On connect the client receives a **snapshot** frame establishing the
//!    `$control` baseline (and resume cursor).
//! 2. Then a stream of **delta** frames, one per engine event.
//! 3. A reconnecting client may present a `since_seq` resume cursor; the stream
//!    replays only events strictly after it (best-effort, bounded by the
//!    broadcast ring) and otherwise issues a fresh snapshot.
//!
//! **Isolation (invariant #10) is the load-bearing property.** The reader pulls
//! from the broadcast with [`EventSubscription::recv`]; a slow client that falls
//! behind observes [`RecvError::Lagged`] and the reader **skips/resubscribes**
//! (lagged-skip) rather than ever applying back-pressure. The engine's publish
//! path is a non-blocking `broadcast::send`, so no client — slow, stalled, or
//! malicious — can stall the engine. The client-facing socket write is the only
//! thing that can block, and it blocks only this client's task, never the
//! engine.
use std::convert::Infallible;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use multiview_core::time::MediaTime;
use multiview_engine::{EventSubscription, RecvError};
use multiview_events::{Envelope, Event, FrameKind, Hello, SchemaVersion, Seq, Topic};

use crate::auth::{Action, Principal};
use crate::state::{AppState, EngineStateSnapshot};

/// The session heartbeat interval advertised in `$hello`.
const HEARTBEAT_MS: u32 = 15_000;
/// The minimum clamped wire cadence advertised in `$hello`.
const MIN_RATE_HZ: u32 = 1;
/// The maximum clamped wire cadence advertised in `$hello`.
const MAX_RATE_HZ: u32 = 60;
/// The default wire cadence advertised in `$hello`.
const DEFAULT_RATE_HZ: u32 = 30;

/// One realtime frame plus the metadata the transport needs to emit it.
#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeFrame {
    /// Whether this is a snapshot baseline or a delta.
    pub kind: FrameKind,
    /// The envelope to serialize.
    pub envelope: Envelope<Event>,
}

impl RealtimeFrame {
    /// Serialize the envelope to a JSON string for the wire.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] only if the envelope is not serializable, which is
    /// not possible for the fixed [`Event`] union — callers may treat an error
    /// as "drop this frame".
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.envelope)
    }
}

/// Drives one realtime session: emits the snapshot then streams deltas with
/// lagged-skip semantics, issuing strictly-increasing per-connection `seq`s.
///
/// This is transport-agnostic so both the WS and SSE handlers (and tests) share
/// one tested core. It owns the engine subscription and the per-connection
/// sequence counter.
#[derive(Debug)]
pub struct SessionStream {
    sub: EventSubscription<Event>,
    session_id: String,
    next_seq: u64,
    snapshot_sent: bool,
    /// A resume cursor: deltas with an engine seq `<= resume_after` are skipped
    /// (already observed by the client before its disconnect).
    resume_after: Option<u64>,
}

impl SessionStream {
    /// Build a session over an engine event subscription.
    ///
    /// `resume_after` is the client's last observed engine sequence (`since_seq`
    /// / `Last-Event-ID`), or [`None`] for a fresh connection.
    #[must_use]
    pub fn new(
        sub: EventSubscription<Event>,
        session_id: impl Into<String>,
        resume_after: Option<u64>,
    ) -> Self {
        Self {
            sub,
            session_id: session_id.into(),
            next_seq: 0,
            snapshot_sent: false,
            resume_after,
        }
    }

    /// Allocate the next per-connection sequence number.
    fn issue_seq(&mut self) -> Seq {
        let seq = Seq::new(self.next_seq);
        self.next_seq = self.next_seq.saturating_add(1);
        seq
    }

    /// Build the connect-time `$hello` snapshot frame on the `$control` topic.
    ///
    /// `snapshot_seq` reports the engine state sequence the snapshot is current
    /// as of, so a later resume can reason about staleness.
    #[must_use]
    pub fn snapshot_frame(&mut self, snapshot_seq: u64) -> RealtimeFrame {
        self.snapshot_sent = true;
        let seq = self.issue_seq();
        let hello = Hello {
            session_id: self.session_id.clone(),
            server_v: vec![SchemaVersion::V1],
            heartbeat_ms: HEARTBEAT_MS,
            min_rate_hz: MIN_RATE_HZ,
            max_rate_hz: MAX_RATE_HZ,
            default_rate_hz: DEFAULT_RATE_HZ,
            replay_ring: u32::try_from(self.sub.len()).unwrap_or(u32::MAX),
        };
        let envelope = Envelope::new(
            Topic::Control,
            seq,
            MediaTime::from_nanos(i64::try_from(snapshot_seq).unwrap_or(i64::MAX)),
            Event::Hello(hello),
        );
        RealtimeFrame {
            kind: FrameKind::Snapshot,
            envelope,
        }
    }

    /// Receive the next delta frame, applying **lagged-skip** isolation.
    ///
    /// Returns:
    /// * `Ok(Some(frame))` — a delta to emit.
    /// * `Ok(None)` — a resumed event that the client already saw (skipped) or a
    ///   lag the reader recovered from; the caller should poll again.
    /// * `Err(RecvError::Closed)` — the engine is gone; end the session.
    ///
    /// On [`RecvError::Lagged`] this **does not** propagate back-pressure: it
    /// re-subscribes from the channel head and returns `Ok(None)`. The client
    /// will re-baseline (a `$lag`/`$resync` is the next layer's concern); the
    /// engine is never blocked.
    ///
    /// # Errors
    ///
    /// [`RecvError::Closed`] when every engine publish handle has been dropped.
    pub async fn next_delta(&mut self) -> Result<Option<RealtimeFrame>, RecvError> {
        match self.sub.recv().await {
            Ok(seq_event) => {
                // Skip events the resuming client already observed.
                if let Some(after) = self.resume_after {
                    if seq_event.seq <= after {
                        return Ok(None);
                    }
                }
                let seq = self.issue_seq();
                let event = (*seq_event.event).clone();
                let topic = topic_for_event(&event);
                // Resource scope (the tile/input/output id) the client keys this
                // delta by — read before the event is moved into the envelope.
                let scope = event_scope_id(&event);
                let mut envelope = Envelope::new(
                    topic,
                    seq,
                    MediaTime::from_nanos(i64::try_from(seq_event.seq).unwrap_or(i64::MAX)),
                    event,
                );
                if let Some(id) = scope {
                    envelope = envelope.with_id(id);
                }
                Ok(Some(RealtimeFrame {
                    kind: FrameKind::Delta,
                    envelope,
                }))
            }
            Err(RecvError::Lagged(_)) => {
                // Drop-oldest overflow for THIS slow client only: resubscribe at
                // the head and let the client re-baseline. The engine never saw
                // any back-pressure (invariant #10).
                self.sub = self.sub.resubscribe();
                Ok(None)
            }
            Err(RecvError::Closed) => Err(RecvError::Closed),
        }
    }
}

/// The resource-scope id (the `Envelope::id`) a client keys an event delta by.
/// For a tile-state change it is the bound input/tile id, which the monitoring
/// UI uses to address the tile. Other events carry their scope in the envelope
/// the producer builds, so they return `None` here.
#[must_use]
pub fn event_scope_id(event: &Event) -> Option<String> {
    match event {
        Event::TileState(tile) => tile.input.clone(),
        _ => None,
    }
}

/// The coarse topic an event is published on (the realtime-api topic map).
#[must_use]
pub fn topic_for_event(event: &Event) -> Topic {
    match event {
        Event::TileState(_) => Topic::Tiles,
        Event::AudioMeter(_) => Topic::AudioMeters,
        Event::OutputStatus(_) => Topic::Outputs,
        Event::AlertRaised(_) | Event::AlertCleared(_) => Topic::Alerts,
        Event::InputConnection(_) => Topic::Inputs,
        Event::JobProgress(_) => Topic::Jobs,
        // Broadcast monitoring/control events ride their own topics so a client
        // can subscribe to the alarm or tally firehose independently.
        Event::AlarmRaised(_)
        | Event::AlarmUpdated(_)
        | Event::AlarmCleared(_)
        | Event::AlarmAcked(_) => Topic::Alarms,
        Event::TallyState(_)
        | Event::SalvoArmed(_)
        | Event::SalvoTaken(_)
        | Event::SalvoCancelled(_) => Topic::Tally,
        _ => Topic::Control,
    }
}

/// Read the engine's latest state snapshot, defaulting to JSON `null` when the
/// engine has not published yet.
#[must_use]
fn current_engine_snapshot(state: &AppState) -> (EngineStateSnapshot, u64) {
    let seq = state.engine.state.sequence();
    let snapshot = state
        .engine
        .state
        .latest()
        .map_or(serde_json::Value::Null, |arc| (*arc).clone());
    (snapshot, seq)
}

/// `GET /api/v1/ws` — the primary WebSocket transport.
///
/// **Authenticated before the upgrade.** Streaming the engine event firehose
/// (tile state, alerts, input/output status) is a privileged read: a valid
/// `Bearer` API key with at least [`Action::Read`] ([`Role::Viewer`]) is
/// required. Auth is resolved *pre-upgrade* — the [`Principal`] extractor and
/// the role gate run before [`WebSocketUpgrade::on_upgrade`], so an
/// unauthenticated or under-privileged request fails as a debuggable
/// `401`/`403` `problem+json` HTTP response rather than a silently-closed socket
/// (realtime-api §6). API/non-browser clients send `Authorization: Bearer`
/// directly on the upgrade.
///
/// On success, upgrades the connection and runs a [`SessionStream`] that emits
/// the snapshot then streams deltas, writing each as a text frame. A write that
/// blocks blocks only this client's task; the engine is never awaited.
///
/// [`Role::Viewer`]: crate::auth::Role::Viewer
pub async fn ws_handler(
    // The auth gate is the FIRST extractor, so authentication is decided before
    // axum's `WebSocketUpgrade` extractor runs — an unauthenticated request is a
    // `401`/`403` `problem+json`, never pre-empted by the upgrade extractor's
    // `426` on a request without the upgrade handshake.
    RealtimeViewer(_principal): RealtimeViewer,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| run_ws_session(socket, state))
}

/// A pre-upgrade auth gate for the realtime transports.
///
/// As a [`FromRequestParts`] extractor it resolves the `Bearer` / JWT /
/// `?access_token=` [`Principal`] and enforces [`Action::Read`] — and because
/// extractors run in argument order, placing it before [`WebSocketUpgrade`] makes
/// authentication strictly precede the upgrade. So an unauthenticated /
/// under-privileged client always gets a debuggable `401`/`403` HTTP response,
/// not a `426` from the upgrade extractor (nor a silently-closed socket).
pub struct RealtimeViewer(pub Principal);

impl FromRequestParts<AppState> for RealtimeViewer {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // A browser WebSocket/EventSource cannot set an `Authorization` header,
        // so the bearer token is also accepted as `?access_token=` (header wins).
        let access_token = Query::<AccessTokenQuery>::from_request_parts(parts, state)
            .await
            .ok()
            .and_then(|q| q.0.access_token);
        let principal = resolve_principal(state, &parts.headers, access_token.as_deref())
            .map_err(IntoResponse::into_response)?;
        principal
            .role
            .require(Action::Read)
            .map_err(IntoResponse::into_response)?;
        Ok(Self(principal))
    }
}

/// The browser-transport token fallback: a WebSocket / `EventSource` cannot set
/// an `Authorization` header, so the bearer token may be passed as the
/// `access_token` query parameter (same-origin only).
#[derive(Debug, serde::Deserialize)]
pub struct AccessTokenQuery {
    /// The raw `key_id.secret` bearer token, when not sent as a header.
    access_token: Option<String>,
}

/// Resolve a [`Principal`] from the `Authorization` header (API key, then JWT) or,
/// failing that, the `access_token` query parameter (the browser WS/SSE path).
fn resolve_principal(
    state: &AppState,
    headers: &HeaderMap,
    access_token: Option<&str>,
) -> Result<Principal, crate::error::ControlError> {
    let header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if let Ok(principal) = state.api_keys.verify_authorization(header) {
        return Ok(principal);
    }
    if let Some(Ok(principal)) = state.authenticate_jwt(header) {
        return Ok(principal);
    }
    if let Some(token) = access_token {
        return state.api_keys.verify(token);
    }
    Err(crate::error::ControlError::Unauthenticated)
}

/// Run one upgraded WebSocket session to completion.
async fn run_ws_session(mut socket: WebSocket, state: AppState) {
    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    let mut session = SessionStream::new(sub, session_id, None);

    let (_snapshot, snapshot_seq) = current_engine_snapshot(&state);
    let hello = session.snapshot_frame(snapshot_seq);
    if let Ok(text) = hello.to_json() {
        if socket.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }

    loop {
        match session.next_delta().await {
            Ok(Some(frame)) => {
                let Ok(text) = frame.to_json() else { continue };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    // The client write failed: drop this session. The engine was
                    // never blocked by it.
                    break;
                }
            }
            Ok(None) => {}
            Err(_closed) => break,
        }
    }
}

/// `GET /api/v1/events` — the one-way SSE fallback transport.
///
/// **Authenticated** with the same `Bearer` API key as the WebSocket transport:
/// SSE uses the `Authorization` header (realtime-api §6), and streaming the
/// engine event firehose requires at least [`Action::Read`] ([`Role::Viewer`]).
/// An unauthenticated or under-privileged request is rejected with a
/// `401`/`403` `problem+json` response before any event is emitted.
///
/// On success, emits the same snapshot-then-delta envelope stream as named SSE
/// events. The underlying [`Sse`] body applies no back-pressure to the engine:
/// it pulls from the per-client [`SessionStream`], which lagged-skips a slow
/// consumer.
///
/// [`Role::Viewer`]: crate::auth::Role::Viewer
pub async fn sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth): Query<AccessTokenQuery>,
) -> Response {
    let principal = match resolve_principal(&state, &headers, auth.access_token.as_deref()) {
        Ok(principal) => principal,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = principal.role.require(Action::Read) {
        return err.into_response();
    }

    let sub = state.engine.subscribe();
    let session_id = uuid_session_id();
    let mut session = SessionStream::new(sub, session_id, None);
    let (_snapshot, snapshot_seq) = current_engine_snapshot(&state);

    let stream = async_stream::stream! {
        let hello = session.snapshot_frame(snapshot_seq);
        if let Ok(text) = hello.to_json() {
            yield Ok::<_, Infallible>(SseEvent::default().event("snapshot").data(text));
        }
        loop {
            match session.next_delta().await {
                Ok(Some(frame)) => {
                    if let Ok(text) = frame.to_json() {
                        let id = frame.envelope.seq.get().to_string();
                        yield Ok(SseEvent::default().event("delta").id(id).data(text));
                    }
                }
                Ok(None) => {}
                Err(_closed) => break,
            }
        }
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Mint a fresh session id.
fn uuid_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
