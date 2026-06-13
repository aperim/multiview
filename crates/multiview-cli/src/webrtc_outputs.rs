//! WHEP-serve + WHIP-push **output** wiring (ADR-0049) — feature `webrtc-native`.
//!
//! Connects the encode-once program fan-out to the WebRTC program outputs:
//!
//! * a configured `webrtc` output WHEP-serves the program to N browser viewers,
//! * a configured `whip_push` output publishes the program to a remote WHIP
//!   ingest.
//!
//! Both are **sinks in the encode-once fan-out** (invariant #7): the pipeline's
//! bake consumer encodes the program **once**; a `RunnableOutput::WebRtc` sink
//! runner re-stamps each coded [`multiview_ffmpeg::EncodedPacket`] into an
//! [`EgressSample`](multiview_webrtc::egress::EgressSample) and pushes it onto a
//! bounded drop-oldest [`EgressSink`]; the WHEP-serve driver / WHIP-push client
//! drains the paired [`EgressFeed`] and packetizes it into SRTP per session. The
//! per-viewer marginal cost is packetization only — never a re-encode.
//!
//! ## The registry rendezvous
//!
//! The pipeline owns the [`EgressSink`]s (created in `build_outputs`); the
//! control plane (WHEP) and the run path (WHIP-push) need the paired
//! [`EgressFeed`]s. The [`EgressRegistry`] is the rendezvous, mirroring the WHIP
//! ingest `WhipRegistry`: the pipeline registers one entry per `webrtc`/
//! `whip_push` output (its feed + its policy), and this module's
//! [`build_whep_output_provider`] / [`spawn_whip_push_clients`] read it.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! Every hand-off is the bounded drop-oldest [`EgressFeed`]; a stalled WHEP
//! player or a dead WHIP target loses *its* media and can never stall the
//! encode-once fan-out, another output, or the output clock. The endpoint/push
//! driver tasks never `.await` a peer and the engine never awaits them.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use multiview_config::{MultiviewConfig, Output};
use multiview_control::{
    SharedWhepOutput, WhepOutputAnswer, WhepOutputAuth, WhepOutputProvider, WhepOutputReject,
};
use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::egress::{egress_feed, EgressFeed, EgressSink};
use multiview_webrtc::error::WebRtcError;
use multiview_webrtc::transport::{
    WhepServeEndpoint, WhepServeHandle, WhipPushAnswer, WhipPushClient, WhipSignaller,
};

/// One configured WebRTC output's egress + policy, registered by the pipeline so
/// the control/run wiring can drive it.
#[derive(Clone)]
pub struct EgressEntry {
    /// The drop-oldest feed the WHEP/WHIP driver drains (the consumer end; the
    /// pipeline keeps the paired [`EgressSink`] in its sink runner).
    pub feed: EgressFeed,
    /// Whether this output carries the program Opus rendition.
    pub audio: bool,
    /// The per-output policy distinguishing a WHEP-serve from a WHIP-push output.
    pub policy: OutputPolicy,
}

/// Whether a WebRTC output WHEP-serves viewers or WHIP-pushes to a remote.
#[derive(Clone)]
pub enum OutputPolicy {
    /// A WHEP-serve (`webrtc`) output: the per-output viewer cap + optional token.
    Whep {
        /// The maximum concurrent WHEP viewer sessions on this output.
        max_viewers: u32,
        /// The optional per-output bearer token (RFC 6750); `None` ⇒ a View key.
        token: Option<String>,
    },
    /// A WHIP-push (`whip_push`) output: the remote URL + optional bearer token.
    WhipPush {
        /// The remote WHIP endpoint URL.
        url: String,
        /// The optional bearer token sent on the WHIP `POST`.
        token: Option<String>,
    },
}

/// The shared rendezvous between the pipeline (which owns the [`EgressSink`]s) and
/// the control/run wiring (which drives the [`EgressFeed`]s). Cheap to clone.
#[derive(Clone, Default)]
pub struct EgressRegistry {
    inner: Arc<Mutex<HashMap<String, EgressEntry>>>,
}

impl std::fmt::Debug for EgressRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.inner.lock().map_or(0, |m| m.len());
        f.debug_struct("EgressRegistry")
            .field("outputs", &n)
            .finish_non_exhaustive()
    }
}

impl EgressRegistry {
    /// A fresh empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create the `(EgressSink, EgressFeed)` pair for a WebRTC output, registering
    /// the feed + its policy and returning the **sink** for the pipeline's sink
    /// runner. Called once per `webrtc`/`whip_push` output at build time.
    #[must_use]
    pub fn register(&self, output_id: &str, audio: bool, policy: OutputPolicy) -> EgressSink {
        let (sink, feed) = egress_feed();
        if let Ok(mut map) = self.inner.lock() {
            map.insert(
                output_id.to_owned(),
                EgressEntry {
                    feed,
                    audio,
                    policy,
                },
            );
        }
        sink
    }

    /// Snapshot the registered entries (id + entry), for the wiring to iterate.
    #[must_use]
    pub fn entries(&self) -> Vec<(String, EgressEntry)> {
        self.inner.lock().map_or_else(
            |_| Vec::new(),
            |m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
    }

    /// Whether any WebRTC output is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().map_or(true, |m| m.is_empty())
    }
}

/// The cli's [`WhepOutputProvider`] over the native [`WhepServeHandle`].
///
/// `negotiate` authorizes against the per-output token or a View key, then drives
/// the endpoint negotiation; the viewer session is fed the program over the
/// output's registered [`EgressFeed`].
struct CliWhepProvider {
    handle: WhepServeHandle,
    /// Per-output token policy keyed by output id (`None` ⇒ a View key suffices).
    tokens: HashMap<String, Option<String>>,
}

impl CliWhepProvider {
    /// Authorize a view against the per-output policy: the per-output token match
    /// or a View-scope API key.
    fn authorize(token: Option<&String>, auth: &WhepOutputAuth) -> Result<(), WhepOutputReject> {
        let token_ok = match token {
            Some(t) => auth.bearer.as_deref() == Some(t.as_str()),
            None => false,
        };
        if auth.view_key || token_ok {
            Ok(())
        } else if auth.bearer.is_none() {
            Err(WhepOutputReject::Unauthorized)
        } else {
            Err(WhepOutputReject::Forbidden)
        }
    }

    /// Map a crate [`WebRtcError`] from the endpoint onto a [`WhepOutputReject`].
    fn map_endpoint_error(err: &WebRtcError) -> WhepOutputReject {
        match err {
            WebRtcError::MalformedSdp(detail) => WhepOutputReject::Malformed((*detail).to_owned()),
            WebRtcError::NoCompatibleCodec => WhepOutputReject::NoCompatibleCodec,
            WebRtcError::UnknownSession(_) => WhepOutputReject::NotFound,
            // Capacity, config (no reachable candidate), transport, socket, turn:
            // the viewer cannot be admitted right now — shed honestly (503).
            _ => WhepOutputReject::Unavailable,
        }
    }
}

impl WhepOutputProvider for CliWhepProvider {
    fn negotiate(
        &self,
        output_id: &str,
        offer: &str,
        auth: &WhepOutputAuth,
    ) -> Result<WhepOutputAnswer, WhepOutputReject> {
        // Only a configured `webrtc` output can be viewed.
        let Some(token) = self.tokens.get(output_id) else {
            // Not a webrtc output — never anonymous, so a missing credential is the
            // dominant signal; otherwise it is simply not a viewable target.
            return Err(if auth.bearer.is_none() && !auth.view_key {
                WhepOutputReject::Unauthorized
            } else {
                WhepOutputReject::NotFound
            });
        };
        Self::authorize(token.as_ref(), auth)?;
        // `want_audio = true`: the endpoint negotiates the Opus m-line only if the
        // viewer's offer advertises it AND the output carries audio (the endpoint
        // gates on the offer's m-lines), so passing `true` here is safe — a
        // video-only output simply never has audio AUs to write.
        let negotiated = self
            .handle
            .negotiate(output_id, offer, true)
            .map_err(|e| Self::map_endpoint_error(&e))?;
        Ok(WhepOutputAnswer {
            session_id: negotiated.session_id.as_str().to_owned(),
            sdp: negotiated.answer_sdp,
        })
    }

    fn release(&self, output_id: &str, session_id: &str, _auth: &WhepOutputAuth) -> bool {
        self.handle.release(output_id, session_id)
    }

    fn active_sessions(&self) -> usize {
        self.tokens
            .keys()
            .map(|id| self.handle.live_viewer_count(id))
            .sum()
    }
}

/// Map the config `[webrtc]` section onto the crate's plain [`EndpointConfig`]
/// (ADR-0048 §9) — the shared mapping the WHIP **ingest** wiring also uses
/// ([`crate::webrtc_ingest::endpoint_config_from`]).
fn endpoint_config_from(config: &MultiviewConfig) -> EndpointConfig {
    crate::webrtc_ingest::endpoint_config_from(config)
}

/// Build the WHEP-serve [`WhepOutputProvider`] for the configured `webrtc`
/// outputs over the shared egress `registry`, binding the WHEP-serve endpoint and
/// spawning its driver task. Returns `None` (so the control plane keeps the
/// default `NoWhepOutput` `503`) when no `webrtc` output is configured, or the
/// endpoint bind fails (the run continues; WHEP viewing is unavailable).
///
/// The driver task owns the socket on its own tokio task for the run's lifetime;
/// it can never back-pressure the engine (invariant #10).
#[must_use]
pub fn build_whep_output_provider(
    config: &MultiviewConfig,
    registry: &EgressRegistry,
) -> Option<SharedWhepOutput> {
    // Collect the WHEP-serve outputs (id -> (max_viewers, token, feed, audio)).
    let mut whep: Vec<(String, u32, Option<String>, EgressFeed)> = Vec::new();
    let mut tokens: HashMap<String, Option<String>> = HashMap::new();
    for (id, entry) in registry.entries() {
        if let OutputPolicy::Whep { max_viewers, token } = &entry.policy {
            tokens.insert(id.clone(), token.clone());
            whep.push((id.clone(), *max_viewers, token.clone(), entry.feed.clone()));
        }
    }
    if whep.is_empty() {
        return None;
    }
    let endpoint_config = endpoint_config_from(config);
    let (endpoint, handle) = match WhepServeEndpoint::bind(endpoint_config) {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(error = %err, "WHEP-serve endpoint bind failed; WHEP output viewing unavailable this run");
            return None;
        }
    };
    // Register each output's feed + cap with the endpoint driver.
    for (id, max_viewers, _token, feed) in whep {
        handle.register_output(&id, max_viewers, feed);
    }
    let stop = Arc::new(AtomicBool::new(false));
    tokio::spawn(async move {
        if let Err(err) = endpoint.run(stop).await {
            tracing::warn!(error = %err, "WHEP-serve endpoint driver exited");
        }
    });
    tracing::info!(
        outputs = tokens.len(),
        "WHEP-serve endpoint bound; viewers may POST /api/v1/whep/{{output}}"
    );
    Some(Arc::new(CliWhepProvider { handle, tokens }))
}

/// A reqwest-backed [`WhipSignaller`]: `POST`s the offer to the remote WHIP origin
/// with the optional Bearer, follows https-only 307/308 redirects (reqwest's
/// default redirect policy, capped), and resolves the answer + session `Location`.
struct ReqwestWhipSignaller {
    url: String,
    token: Option<String>,
    client: reqwest::blocking::Client,
}

impl ReqwestWhipSignaller {
    fn new(url: String, token: Option<String>) -> Result<Self, WebRtcError> {
        // https-only redirects, depth-capped (ADR-0049 §5.2): an https->http
        // downgrade aborts. reqwest's redirect policy preserves method+headers for
        // 307/308 and we cap the depth.
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= usize::from(multiview_webrtc::transport::MAX_REDIRECTS) {
                attempt.error("too many WHIP redirects")
            } else if attempt.url().scheme() != "https" {
                attempt.error("https->http downgrade forbidden")
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::blocking::Client::builder()
            .redirect(policy)
            .build()
            .map_err(|e| WebRtcError::Transport(format!("whip_push http client: {e}")))?;
        Ok(Self { url, token, client })
    }
}

impl WhipSignaller for ReqwestWhipSignaller {
    fn post_offer(&self, offer_sdp: &str) -> Result<WhipPushAnswer, WebRtcError> {
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/sdp")
            .body(offer_sdp.to_owned());
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .map_err(|e| WebRtcError::Transport(format!("whip_push POST: {e}")))?;
        let status = resp.status();
        if status != reqwest::StatusCode::CREATED {
            return Err(WebRtcError::Transport(format!(
                "whip_push POST returned {status}, expected 201"
            )));
        }
        // The session resource URL, resolved against the post-redirect effective
        // URL (reqwest already resolved relative Locations during redirects; the
        // `Location` header here is the session resource for DELETE).
        let final_url = resp.url().clone();
        let resource_url = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| final_url.join(loc).ok())
            .map(|u| u.to_string());
        let answer_sdp = resp
            .text()
            .map_err(|e| WebRtcError::Transport(format!("whip_push answer body: {e}")))?;
        Ok(WhipPushAnswer {
            answer_sdp,
            resource_url,
        })
    }

    fn delete_resource(&self, resource_url: &str) {
        let mut req = self.client.delete(resource_url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        // Best-effort teardown; a failure is logged, never propagated (the remote
        // also times the session out).
        if let Err(e) = req.send() {
            tracing::debug!(error = %e, "whip_push DELETE failed (best-effort teardown)");
        }
    }
}

/// Spawn one supervised [`WhipPushClient`] task per configured `whip_push` output
/// over the shared egress `registry`. Each binds its own outbound socket, builds
/// a sendonly offer, `POST`s it to the remote origin, and publishes the program;
/// it reconnects with backoff on drop (supervised, like RTMP/SRT push). A
/// run-lifetime task — never joined here (process exit tears it down), and it can
/// never back-pressure the engine (invariant #10).
pub fn spawn_whip_push_clients(config: &MultiviewConfig, registry: &EgressRegistry) {
    for (id, entry) in registry.entries() {
        let OutputPolicy::WhipPush { url, token } = &entry.policy else {
            continue;
        };
        let endpoint_config = endpoint_config_from(config);
        let client = match WhipPushClient::bind(endpoint_config, entry.feed.clone(), entry.audio) {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!(output = %id, error = %err, "whip_push client bind failed; this output is unavailable this run");
                continue;
            }
        };
        let signaller = match ReqwestWhipSignaller::new(url.clone(), token.clone()) {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(output = %id, error = %err, "whip_push signaller build failed; this output is unavailable this run");
                continue;
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let output_id = id.clone();
        tokio::spawn(async move {
            if let Err(err) = client.run(signaller, stop).await {
                tracing::warn!(output = %output_id, error = %err, "whip_push client task exited");
            }
        });
        tracing::info!(output = %id, url = %url, "whip_push client started (publishing the program to the remote WHIP ingest)");
    }
}

/// Build the [`EgressRegistry`] from a config's WebRTC outputs, registering each
/// `webrtc`/`whip_push` output's egress feed + policy and returning the registry
/// **with the per-output [`EgressSink`]s** keyed by id for the pipeline to wire
/// into its sink runners.
#[must_use]
pub fn build_egress_registry(
    config: &MultiviewConfig,
) -> (EgressRegistry, HashMap<String, EgressSink>) {
    let registry = EgressRegistry::new();
    let mut sinks = HashMap::new();
    for output in &config.outputs {
        match output {
            Output::Webrtc {
                max_viewers,
                token,
                audio,
                ..
            } => {
                let id = output.id();
                let sink = registry.register(
                    &id,
                    audio.is_some(),
                    OutputPolicy::Whep {
                        max_viewers: *max_viewers,
                        token: token.clone(),
                    },
                );
                sinks.insert(id, sink);
            }
            Output::WhipPush {
                url, token, audio, ..
            } => {
                let id = output.id();
                let sink = registry.register(
                    &id,
                    audio.is_some(),
                    OutputPolicy::WhipPush {
                        url: url.clone(),
                        token: token.clone(),
                    },
                );
                sinks.insert(id, sink);
            }
            _ => {}
        }
    }
    (registry, sinks)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn authorize_accepts_token_or_view_key() {
        let token = Some("v13w".to_owned());
        // Matching token.
        assert!(CliWhepProvider::authorize(
            token.as_ref(),
            &WhepOutputAuth {
                bearer: Some("v13w".to_owned()),
                view_key: false
            }
        )
        .is_ok());
        // View key, no bearer.
        assert!(CliWhepProvider::authorize(
            token.as_ref(),
            &WhepOutputAuth {
                bearer: None,
                view_key: true
            }
        )
        .is_ok());
        // No credential -> 401.
        assert!(matches!(
            CliWhepProvider::authorize(token.as_ref(), &WhepOutputAuth::default()),
            Err(WhepOutputReject::Unauthorized)
        ));
        // Wrong token, no view key -> 403.
        assert!(matches!(
            CliWhepProvider::authorize(
                token.as_ref(),
                &WhepOutputAuth {
                    bearer: Some("nope".to_owned()),
                    view_key: false
                }
            ),
            Err(WhepOutputReject::Forbidden)
        ));
    }

    #[test]
    fn token_less_output_requires_view_key() {
        assert!(matches!(
            CliWhepProvider::authorize(
                None,
                &WhepOutputAuth {
                    bearer: Some("anything".to_owned()),
                    view_key: false
                }
            ),
            Err(WhepOutputReject::Forbidden)
        ));
        assert!(CliWhepProvider::authorize(
            None,
            &WhepOutputAuth {
                bearer: None,
                view_key: true
            }
        )
        .is_ok());
    }

    #[test]
    fn map_endpoint_error_covers_the_signalling_rows() {
        assert!(matches!(
            CliWhepProvider::map_endpoint_error(&WebRtcError::UnknownSession("x".to_owned())),
            WhepOutputReject::NotFound
        ));
        assert!(matches!(
            CliWhepProvider::map_endpoint_error(&WebRtcError::AtCapacity),
            WhepOutputReject::Unavailable
        ));
        assert!(matches!(
            CliWhepProvider::map_endpoint_error(&WebRtcError::NoCompatibleCodec),
            WhepOutputReject::NoCompatibleCodec
        ));
    }
}
