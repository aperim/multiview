//! `AsyncAPI` 3.0 document generation from the multiview-events wire types.
//!
//! Per ADR-RT006 the realtime event API is documented in `AsyncAPI` 3.0, derived
//! from the same serde Rust types that carry the wire contract. The `asyncapi-rust`
//! crate (v0.2, Nov 2025) is young and **lacks WebSocket bindings**, so this module
//! uses the hand-assembly approach described in ADR-RT006's Consequences section:
//! build the document as a `serde_json::Value` tree from the canonical type
//! definitions in [`crate::event`], [`crate::envelope`], [`crate::topic`], and
//! [`crate::subscription`], then inject WS-specific binding blocks that the
//! generator would omit.
//!
//! The document structure follows the `AsyncAPI` 3.0 spec:
//! <https://www.asyncapi.com/docs/reference/specification/v3.0.0>
//!
//! Channels:
//! - `ws`  → `/api/v1/ws`  (WebSocket, bidirectional)
//! - `sse` → `/api/v1/events` (HTTP SSE, server→client only)
//!
//! Messages are the envelope-typed frames; the envelope `t` discriminator maps
//! to a JSON-Schema `oneOf` within each message's payload schema.
use serde_json::{json, Value};

/// Generate the complete `AsyncAPI` 3.0 document for the Multiview realtime API
/// as a pretty-printed JSON string.
///
/// The output is **deterministic**: calling this function twice with no
/// intervening mutation produces identical strings. This property is required by
/// the CI drift-gate (ADR-RT006 Decision: CI regenerates and fails on any diff).
///
/// # Structure
///
/// ```text
/// asyncapi: "3.0.0"
/// info: { title, version, description }
/// servers: { ws-server, sse-server }
/// channels: { ws, sse }
/// operations: { subscribe-ws, subscribe-sse }
/// messages: { Envelope, TileState, AudioMeter, … }
/// components/schemas: { … all payload schemas … }
/// ```
#[must_use]
pub fn generate_asyncapi_document() -> String {
    let doc = build_document();
    // to_string_pretty produces consistent output: sorted keys within each
    // serde_json::Map are insertion-ordered (serde_json preserves insertion
    // order by default), so the output is deterministic as long as the
    // construction order below is stable.
    match serde_json::to_string_pretty(&doc) {
        Ok(mut s) => {
            // Append a trailing newline for POSIX-cleanliness and diff friendliness.
            s.push('\n');
            s
        }
        // serde_json::to_string_pretty only fails on non-finite floats or
        // recursive values; neither can occur with the Value tree we build here.
        Err(_) => String::new(),
    }
}

/// Assemble the `AsyncAPI` 3.0 [`Value`] document.
fn build_document() -> Value {
    json!({
        "asyncapi": "3.0.0",
        "info": build_info(),
        "servers": build_servers(),
        "channels": build_channels(),
        "operations": build_operations(),
        "messages": build_messages(),
        "components": {
            "schemas": build_schemas()
        }
    })
}

fn build_info() -> Value {
    json!({
        "title": "Multiview Realtime API",
        "version": "1.0.0",
        "description": concat!(
            "WebSocket and SSE realtime event stream for the Multiview engine. ",
            "Every message uses a single versioned envelope (ADR-RT002). ",
            "Topics: tiles, inputs, outputs, audio.meters, alerts, alarms, tally, ",
            "layout, config, logs, jobs, preview, system, capabilities. ",
            "The WebSocket endpoint (/api/v1/ws) is bidirectional (events + subscription ",
            "control frames); the SSE endpoint (/api/v1/events) is server-to-client only.",
        ),
        "license": {
            "name": "MIT OR Apache-2.0"
        },
        "contact": {
            "name": "The Multiview Authors",
            "url": "https://github.com/aperim/multiview"
        },
        "externalDocs": {
            "description": "Realtime API reference",
            "url": "docs/api/realtime.md"
        }
    })
}

fn build_servers() -> Value {
    json!({
        "ws-server": {
            "host": "{host}",
            "pathname": "/api/v1/ws",
            "protocol": "ws",
            "description": "WebSocket endpoint (bidirectional). Primary transport.",
            "variables": {
                "host": {
                    "description": "Hostname and optional port of the Multiview daemon.",
                    "default": "localhost:8080"
                }
            }
        },
        "sse-server": {
            "host": "{host}",
            "pathname": "/api/v1/events",
            "protocol": "http",
            "description": "Server-Sent Events endpoint (server→client only). Fallback transport.",
            "variables": {
                "host": {
                    "description": "Hostname and optional port of the Multiview daemon.",
                    "default": "localhost:8080"
                }
            }
        }
    })
}

/// Build the two channel definitions.
///
/// ADR-RT006 Consequences note that `asyncapi-rust` v0.2 lacks WS bindings;
/// the `bindings.ws` block on the `ws` channel is the **post-processed injection**
/// described there — it is hand-assembled here alongside the rest of the document
/// rather than in a separate post-process step, since this module IS the generator.
fn build_channels() -> Value {
    json!({
        "ws": {
            "address": "/api/v1/ws",
            "title": "WebSocket realtime channel",
            "description": concat!(
                "Bidirectional WebSocket channel. The server sends event envelopes; ",
                "the client sends subscription control frames ($subscribe, $unsubscribe, ",
                "$set_rate, $resume, $pong). Negotiated subprotocol: `multiview.v1` ",
                "(or `multiview.bin.v1` for compact binary meter frames).",
            ),
            "servers": [{ "$ref": "#/servers/ws-server" }],
            "messages": {
                "EnvelopeMessage": { "$ref": "#/messages/Envelope" }
            },
            // WS binding injected here: asyncapi-rust v0.2 lacks bindings support.
            // Spec ref: https://github.com/asyncapi/bindings/blob/master/websockets
            "bindings": {
                "ws": {
                    "method": "GET",
                    "query": {
                        "type": "object",
                        "description": "Optional authentication token as a query parameter.",
                        "properties": {
                            "token": {
                                "type": "string",
                                "description": "Bearer token (alternative to Authorization header)."
                            }
                        }
                    },
                    "headers": {
                        "type": "object",
                        "properties": {
                            "Authorization": {
                                "type": "string",
                                "description": "Bearer token: `Bearer <jwt>`"
                            },
                            "Sec-WebSocket-Protocol": {
                                "type": "string",
                                "enum": ["multiview.v1", "multiview.bin.v1"],
                                "description": "Subprotocol selection; makes the envelope major explicit."
                            }
                        }
                    },
                    "bindingVersion": "0.1.0"
                }
            }
        },
        "sse": {
            "address": "/api/v1/events",
            "title": "SSE realtime channel (fallback)",
            "description": concat!(
                "Server-Sent Events fallback for proxies or environments that strip the ",
                "WebSocket Upgrade header. Server-to-client only: identical envelope shape, ",
                "no client subscription control frames or WHEP signaling.",
            ),
            "servers": [{ "$ref": "#/servers/sse-server" }],
            "messages": {
                "EnvelopeMessage": { "$ref": "#/messages/Envelope" }
            }
        }
    })
}

fn build_operations() -> Value {
    json!({
        "subscribe-ws": {
            "action": "receive",
            "channel": { "$ref": "#/channels/ws" },
            "title": "Receive realtime events (WebSocket)",
            "description": concat!(
                "Receive all event types on the WebSocket channel after subscribing to ",
                "the desired topics via $subscribe control frames.",
            ),
            "messages": [{ "$ref": "#/messages/Envelope" }]
        },
        "subscribe-sse": {
            "action": "receive",
            "channel": { "$ref": "#/channels/sse" },
            "title": "Receive realtime events (SSE)",
            "description": "Receive all event types on the SSE fallback channel.",
            "messages": [{ "$ref": "#/messages/Envelope" }]
        }
    })
}

/// Build the top-level `messages` block.
///
/// Each entry is a named message definition carrying a `payload` schema. The
/// `Envelope` message carries the full versioned envelope shape; the per-event
/// messages carry only their `data` payload schemas and are referenced from the
/// envelope's `oneOf` discriminator.
fn build_messages() -> Value {
    let mut map = serde_json::Map::new();
    // Core envelope message.
    map.extend(build_messages_envelope());
    // Data event messages.
    map.extend(build_messages_data_events());
    // Control frame messages.
    map.extend(build_messages_control_frames());
    Value::Object(map)
}

/// Build the `Envelope` message definition.
fn build_messages_envelope() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("Envelope".to_owned(), json!({
        "name": "Envelope",
        "title": "Versioned realtime envelope",
        "summary": "The single wire frame used for all realtime messages (ADR-RT002).",
        "description": concat!(
            "Every message in both directions uses this envelope. The `t` field is the ",
            "discriminator selecting the `data` schema. The `seq` field is a per-connection ",
            "monotonic cursor; gaps indicate dropped frames and trigger resume/resync. ",
            "The `ts` field carries engine-monotonic nanoseconds (same clock family as ",
            "output PTS). `v` is the envelope schema major; clients MUST reject unknown majors.",
        ),
        "contentType": "application/json",
        "payload": envelope_schema()
    }));
    map
}

/// Build data-event message definitions (tile, audio, output, alert, …).
fn build_messages_data_events() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert("TileState".to_owned(), json!({
        "name": "TileState",
        "title": "Tile state machine transition",
        "summary": "A tile transitioned between lifecycle states (LIVE/STALE/RECONNECTING/NO_SIGNAL).",
        "description": concat!(
            "Emitted on topic `tiles` whenever a tile's state machine transitions. ",
            "Invariant #2: Live → Stale → Reconnecting → NoSignal. The compositor ",
            "holds the last-good frame while STALE/RECONNECTING; renders a placeholder on NO_SIGNAL.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/TileState" }
    }));
    map.insert("AudioMeter".to_owned(), json!({
        "name": "AudioMeter",
        "title": "High-rate audio meter sample",
        "summary": "Per-input/track peak/RMS/clip meter sample (numeric only, never audio data).",
        "description": concat!(
            "Emitted on topic `audio.meters`. This is the sole high-rate conflated topic; ",
            "frames are conflated/sampled at 10–30 Hz on the wire. A slow consumer can only ",
            "lose its own meter frames — the engine never back-pressures. The binary fast-path ",
            "(subprotocol `multiview.bin.v1`) carries the same fields in compact form; ",
            "this schema describes the decoded (JSON) shape.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/AudioMeter" }
    }));
    map.insert("SystemMetrics".to_owned(), system_metrics_message());
    map.insert("OutputStatus".to_owned(), json!({
        "name": "OutputStatus",
        "title": "Output sink status update",
        "summary": "An output sink changed run state (starting/running/migrating/error).",
        "description": "Emitted on topic `outputs`. Make-before-break migration emits `migrating` during the parallel-output window.",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/OutputStatus" }
    }));
    map.insert("Alert".to_owned(), json!({
        "name": "Alert",
        "title": "Operator alert raised or cleared",
        "summary": "An alert was raised (alert.raised) or cleared (alert.cleared) on topic `alerts`.",
        "description": concat!(
            "The `key` field is a stable dedupe identifier for the condition; multiple ",
            "`alert.raised` frames with the same key coalesce rather than stacking.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/Alert" }
    }));
    map.insert("InputConnection".to_owned(), json!({
        "name": "InputConnection",
        "title": "Input source connection state change",
        "summary": "An input source transitioned connection state (input.connection on topic `inputs`).",
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/InputConnection" }
    }));
    map.insert(
        "JobProgress".to_owned(),
        json!({
            "name": "JobProgress",
            "title": "Long-running job progress",
            "summary": "Progress of a REST command job correlated by `corr` (topic `jobs`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/JobProgress" }
        }),
    );
    map.insert(
        "AlarmTransition".to_owned(),
        json!({
            "name": "AlarmTransition",
            "title": "Monitoring alarm lifecycle event",
            "summary": "An alarm was raised, updated, cleared, or acknowledged (topic `alarms`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/AlarmTransition" }
        }),
    );
    map.insert(
        "TallyEvent".to_owned(),
        json!({
            "name": "TallyEvent",
            "title": "Tally lamp/UMD state change",
            "summary": "Resolved tally state for one tile/element changed (topic `tally`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/TallyEvent" }
        }),
    );
    map.insert(
        "SalvoEvent".to_owned(),
        json!({
            "name": "SalvoEvent",
            "title": "Salvo arm/take lifecycle",
            "summary": "A salvo was armed, taken, or cancelled (topic `tally`).",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/SalvoEvent" }
        }),
    );
    map
}

/// Build control-frame message definitions ($hello, $subscribe, …).
fn build_messages_control_frames() -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    map.insert(
        "Hello".to_owned(),
        json!({
            "name": "Hello",
            "title": "$hello control frame",
            "summary": "First server frame after auth; advertises connection parameters.",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/Hello" }
        }),
    );
    map.insert(
        "Subscribe".to_owned(),
        json!({
            "name": "Subscribe",
            "title": "$subscribe control frame (client to server)",
            "summary": "Client requests subscription to one or more topics.",
            "contentType": "application/json",
            "payload": { "$ref": "#/components/schemas/Subscribe" }
        }),
    );
    map
}

/// The envelope schema: the shape every frame must conform to.
///
/// Per ADR-RT002 the `t` + `data` pair is the discriminator + body.
/// Optional fields (`id`, `corr`) are marked `required: false`.
fn envelope_schema() -> Value {
    json!({
        "type": "object",
        "required": ["v", "t", "topic", "seq", "ts", "data"],
        "properties": {
            "v": {
                "type": "integer",
                "format": "uint16",
                "description": "Envelope schema major. A receiver MUST reject an unknown major.",
                "const": 1
            },
            "t": {
                "type": "string",
                "description": "Dotted event type; the discriminator selecting the `data` schema."
            },
            "topic": {
                "type": "string",
                "description": "Subscription routing key. Control frames use `$control`.",
                "enum": [
                    "$control", "system", "capabilities", "inputs", "tiles", "outputs",
                    "audio.meters", "audio.loudness", "alerts", "alarms", "tally",
                    "layout", "config", "logs", "jobs", "preview"
                ]
            },
            "id": {
                "type": "string",
                "description": "Optional resource scope (tile/input/output/job id)."
            },
            "seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Per-connection monotonic resume cursor. A gap means frames were dropped."
            },
            "ts": {
                "type": "integer",
                "format": "int64",
                "description": "Engine monotonic nanoseconds (same clock family as output PTS)."
            },
            "corr": {
                "type": "string",
                "description": "Optional correlation id echoing a REST command / job."
            },
            "data": {
                "type": "object",
                "description": "Typed payload selected by `t`. Schema depends on the event type.",
                "oneOf": event_payload_one_of()
            }
        }
    })
}

/// Build the `oneOf` discriminator list for the envelope `data` field.
///
/// Each entry is a `$ref` into `#/components/schemas` for a known payload type.
/// The discriminator is the envelope-level `t` field (not inside `data` itself).
fn event_payload_one_of() -> Value {
    json!([
        { "$ref": "#/components/schemas/TileState" },
        { "$ref": "#/components/schemas/AudioMeter" },
        { "$ref": "#/components/schemas/OutputStatus" },
        { "$ref": "#/components/schemas/Alert" },
        { "$ref": "#/components/schemas/InputConnection" },
        { "$ref": "#/components/schemas/JobProgress" },
        { "$ref": "#/components/schemas/AlarmTransition" },
        { "$ref": "#/components/schemas/TallyEvent" },
        { "$ref": "#/components/schemas/SalvoEvent" },
        { "$ref": "#/components/schemas/Hello" },
        { "$ref": "#/components/schemas/Subscribe" },
        { "$ref": "#/components/schemas/Subscribed" },
        { "$ref": "#/components/schemas/Unsubscribe" },
        { "$ref": "#/components/schemas/SetRate" },
        { "$ref": "#/components/schemas/Resume" },
        { "$ref": "#/components/schemas/Resync" },
        { "$ref": "#/components/schemas/Lag" },
        { "$ref": "#/components/schemas/ProtocolError" }
    ])
}

/// Build the `components/schemas` map: one schema per wire payload type.
///
/// These schemas mirror the serde wire shapes of the types in [`crate::event`]
/// and [`crate::subscription`]. The canonical source of truth is the Rust type
/// definitions (any drift here must be corrected in favour of the Rust code).
fn build_schemas() -> Value {
    json!({
        "LifecycleState": lifecycle_state_schema(),
        "TileState": tile_state_schema(),
        "AudioMeter": audio_meter_schema(),
        "SystemMetrics": system_metrics_schema(),
        "GpuMetrics": gpu_metrics_schema(),
        "GpuVendor": gpu_vendor_schema(),
        "OutputRunState": output_run_state_schema(),
        "OutputStatus": output_status_schema(),
        "AlertSeverity": alert_severity_schema(),
        "Alert": alert_schema(),
        "InputConnection": input_connection_schema(),
        "JobProgress": job_progress_schema(),
        "AlarmTransition": alarm_transition_schema(),
        "TallyTarget": tally_target_schema(),
        "TallyEvent": tally_event_schema(),
        "SalvoEvent": salvo_event_schema(),
        "Hello": hello_schema(),
        "Subscribe": subscribe_schema(),
        "Subscribed": subscribed_schema(),
        "Unsubscribe": unsubscribe_schema(),
        "SetRate": set_rate_schema(),
        "Resume": resume_schema(),
        "ResyncReason": resync_reason_schema(),
        "Resync": resync_schema(),
        "LagAction": lag_action_schema(),
        "Lag": lag_schema(),
        "ProtocolError": protocol_error_schema()
    })
}

// --- Individual payload schemas ---
// Each function mirrors the serde wire shape of its Rust counterpart.

fn lifecycle_state_schema() -> Value {
    json!({
        "type": "string",
        "description": "Tile/input lifecycle state (invariant #2: Live → Stale → Reconnecting → NoSignal).",
        "enum": ["LIVE", "STALE", "RECONNECTING", "NO_SIGNAL"]
    })
}

fn tile_state_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `tile.state` event: a tile lifecycle transition.",
        "required": ["from", "to", "trigger"],
        "properties": {
            "from": { "$ref": "#/components/schemas/LifecycleState" },
            "to": { "$ref": "#/components/schemas/LifecycleState" },
            "input": {
                "type": "string",
                "description": "The input bound to the tile at the time of the transition, if any."
            },
            "trigger": {
                "type": "string",
                "description": "Short machine-readable trigger label (e.g. `nosignal_timeout`)."
            }
        }
    })
}

fn audio_meter_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `audio.meter` event: a high-rate per-input/track peak/RMS/clip ",
            "sample. This is numeric metadata only — never audio content. ",
            "Conflated/sampled at 10–30 Hz on the wire (high-rate lane).",
        ),
        "required": ["track", "peak_db", "rms_db", "clip", "overflow", "sampled_hz"],
        "properties": {
            "track": {
                "type": "integer",
                "format": "uint32",
                "description": "Track index."
            },
            "peak_db": {
                "type": "array",
                "items": { "type": "number", "format": "float" },
                "description": "Per-channel peak level (dBFS)."
            },
            "rms_db": {
                "type": "array",
                "items": { "type": "number", "format": "float" },
                "description": "Per-channel RMS level (dBFS)."
            },
            "clip": {
                "type": "boolean",
                "description": "Whether any channel clipped in this window."
            },
            "overflow": {
                "type": "boolean",
                "description": "Whether the meter pipeline overflowed (dropped windows)."
            },
            "sampled_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Effective wire cadence (Hz)."
            }
        }
    })
}

fn system_metrics_message() -> Value {
    json!({
        "name": "SystemMetrics",
        "title": "High-rate whole-system metrics sample",
        "summary": "CPU / GPU / encoder-decoder utilisation sample (numeric only).",
        "description": concat!(
            "Emitted on topic `system`. A high-rate conflated lane like `audio.meters`: ",
            "samples are latest-only at ~1-2 Hz on the wire and excluded from the lossless ",
            "replay ring. PUSHED, never polled — a slow UI loses only its own samples and the ",
            "engine never back-pressures (inv #10). Live values only; historic windows (the ",
            "data decisions are made from) are a separate cold REST query.",
        ),
        "contentType": "application/json",
        "payload": { "$ref": "#/components/schemas/SystemMetrics" }
    })
}

fn system_metrics_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of the `system.metrics` event: a high-rate whole-system sample ",
            "(cpu / gpu / encoder-decoder). Numeric only; conflated at ~1-2 Hz (high-rate lane).",
        ),
        "required": ["cpu_util", "sampled_hz"],
        "properties": {
            "cpu_util": { "type": "number", "format": "float", "description": "Whole-system CPU utilisation, 0.0-1.0." },
            "mem_used_bytes": { "type": "integer", "format": "uint64", "description": "Host memory in use (bytes), if known." },
            "mem_total_bytes": { "type": "integer", "format": "uint64", "description": "Total host memory (bytes), if known." },
            "gpus": {
                "type": "array",
                "items": { "$ref": "#/components/schemas/GpuMetrics" },
                "description": "Per-GPU utilisation samples; empty on a GPU-free host."
            },
            "program_fps": { "type": "number", "format": "float", "description": "Aggregate program output rate (fps), if running." },
            "sampled_hz": { "type": "integer", "format": "uint32", "description": "Effective wire sampling cadence (Hz)." }
        }
    })
}

fn gpu_metrics_schema() -> Value {
    json!({
        "type": "object",
        "description": "A per-GPU utilisation sample. Optional fields are absent where the vendor does not expose that signal.",
        "required": ["id", "vendor", "compute_util", "mem_used_bytes", "mem_total_bytes"],
        "properties": {
            "id": { "type": "string", "description": "Stable device identity (UUID where available, else an index)." },
            "vendor": { "$ref": "#/components/schemas/GpuVendor" },
            "name": { "type": "string", "description": "Human-readable device name, if known." },
            "compute_util": { "type": "number", "format": "float", "description": "Compute (graphics/CUDA) utilisation, 0.0-1.0." },
            "mem_used_bytes": { "type": "integer", "format": "uint64", "description": "VRAM in use (bytes)." },
            "mem_total_bytes": { "type": "integer", "format": "uint64", "description": "Total VRAM (bytes)." },
            "encoder_util": { "type": "number", "format": "float", "description": "Encoder (NVENC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent)." },
            "decoder_util": { "type": "number", "format": "float", "description": "Decoder (NVDEC/QSV) ASIC utilisation, 0.0-1.0 (vendor-dependent)." },
            "encoder_sessions": { "type": "integer", "format": "uint32", "description": "Active concurrent hardware encode sessions (NVIDIA)." },
            "encoder_session_ceiling": { "type": "integer", "format": "uint32", "description": "Runtime-discovered concurrent encode-session ceiling (NVIDIA)." }
        }
    })
}

fn gpu_vendor_schema() -> Value {
    json!({
        "type": "string",
        "description": "GPU/accelerator hardware vendor (selects which per-engine signals to expect).",
        "enum": ["nvidia", "intel", "amd", "apple", "other"]
    })
}

fn output_run_state_schema() -> Value {
    json!({
        "type": "string",
        "description": "Running state of an output sink.",
        "enum": ["starting", "running", "migrating", "error"]
    })
}

fn output_status_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `output.status` event.",
        "required": ["state"],
        "properties": {
            "state": { "$ref": "#/components/schemas/OutputRunState" },
            "bitrate_bps": {
                "type": "integer",
                "format": "uint64",
                "description": "Measured output bitrate (bits/sec), if known."
            },
            "clients": {
                "type": "integer",
                "format": "uint32",
                "description": "Number of currently-connected consumers, if known."
            }
        }
    })
}

fn alert_severity_schema() -> Value {
    json!({
        "type": "string",
        "description": "Severity of an operator alert.",
        "enum": ["info", "warning", "critical"]
    })
}

fn alert_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `alert.raised` and `alert.cleared` events. ",
            "The `key` field is a stable dedupe identifier: multiple raised frames ",
            "with the same key coalesce.",
        ),
        "required": ["key", "severity", "title", "active"],
        "properties": {
            "key": {
                "type": "string",
                "description": "Stable dedupe key for the condition."
            },
            "severity": { "$ref": "#/components/schemas/AlertSeverity" },
            "title": {
                "type": "string",
                "description": "Short human-readable title."
            },
            "detail": {
                "type": "string",
                "description": "Optional longer detail."
            },
            "active": {
                "type": "boolean",
                "description": "Whether the condition is currently active."
            }
        }
    })
}

fn input_connection_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of the `input.connection` event.",
        "required": ["state"],
        "properties": {
            "state": { "$ref": "#/components/schemas/LifecycleState" },
            "attempt": {
                "type": "integer",
                "format": "uint32",
                "description": "Reconnect attempt counter, if reconnecting."
            }
        }
    })
}

fn job_progress_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `job.progress`; correlated to a REST command via the envelope `corr`.",
        "required": ["phase", "pct"],
        "properties": {
            "phase": {
                "type": "string",
                "description": "Short machine-readable phase label."
            },
            "pct": {
                "type": "integer",
                "minimum": 0,
                "maximum": 100,
                "description": "Percent complete (0–100)."
            },
            "message": {
                "type": "string",
                "description": "Optional human-readable progress message."
            }
        }
    })
}

fn alarm_transition_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `alarm.raised`, `alarm.updated`, `alarm.cleared`, `alarm.acked`. ",
            "Carries the current AlarmRecord value after the transition (X.733-aligned).",
        ),
        "required": ["record"],
        "properties": {
            "record": {
                "type": "object",
                "description": "The current alarm record after the transition.",
                "additionalProperties": true
            }
        }
    })
}

fn tally_target_schema() -> Value {
    json!({
        "type": "object",
        "description": "What a tally state applies to (tagged by `kind`).",
        "required": ["kind"],
        "discriminator": { "propertyName": "kind" },
        "oneOf": [
            {
                "type": "object",
                "required": ["kind", "index"],
                "properties": {
                    "kind": { "type": "string", "const": "tile" },
                    "index": { "type": "integer", "format": "uint32", "description": "Zero-based tile index." }
                }
            },
            {
                "type": "object",
                "required": ["kind", "name"],
                "properties": {
                    "kind": { "type": "string", "const": "element" },
                    "name": { "type": "string", "description": "Element name." }
                }
            }
        ]
    })
}

fn tally_event_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `tally.state`: resolved tally lamp/UMD state for one element.",
        "required": ["target", "state"],
        "properties": {
            "target": { "$ref": "#/components/schemas/TallyTarget" },
            "state": {
                "type": "object",
                "description": "The resolved tally lamp/UMD state.",
                "additionalProperties": true
            }
        }
    })
}

fn salvo_event_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `salvo.armed`, `salvo.taken`, `salvo.cancelled`.",
        "required": ["salvo", "phase"],
        "properties": {
            "salvo": { "type": "string", "description": "Stable salvo identifier/name." },
            "phase": {
                "type": "string",
                "enum": ["armed", "taken", "cancelled"],
                "description": "The lifecycle phase this event reports."
            },
            "head": {
                "type": "string",
                "description": "Output head this recall applies to, if scoped."
            }
        }
    })
}

fn hello_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$hello`: the first server frame after auth.",
        "required": [
            "session_id", "server_v", "heartbeat_ms",
            "min_rate_hz", "max_rate_hz", "default_rate_hz", "replay_ring"
        ],
        "properties": {
            "session_id": { "type": "string", "description": "Server-assigned session id." },
            "server_v": {
                "type": "array",
                "items": { "type": "integer", "format": "uint16" },
                "description": "Envelope schema majors this server can speak."
            },
            "heartbeat_ms": {
                "type": "integer",
                "format": "uint32",
                "description": "Heartbeat interval (milliseconds)."
            },
            "min_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Minimum clamped wire cadence (Hz)."
            },
            "max_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Maximum clamped wire cadence (Hz)."
            },
            "default_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Default wire cadence when `rate_hz` is omitted from `$subscribe`."
            },
            "replay_ring": {
                "type": "integer",
                "format": "uint32",
                "description": "Replay ring size (frames per session/topic)."
            }
        }
    })
}

fn subscribe_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$subscribe` (client→server): subscribe to topics.",
        "required": ["topics"],
        "properties": {
            "topics": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics to subscribe to."
            },
            "ids": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Optional resource-id allowlist."
            },
            "rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Optional max cadence (Hz); server clamps and reports effective."
            },
            "since_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Optional resume cursor: subscribe + replay from after this seq."
            }
        }
    })
}

fn subscribed_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$subscribed`: per-topic ack before the snapshot.",
        "required": ["topic", "effective_rate_hz", "snapshot_seq"],
        "properties": {
            "topic": { "type": "string", "description": "The topic that was subscribed." },
            "effective_rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Actual cadence after server clamping."
            },
            "snapshot_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "The `seq` the forthcoming snapshot is current as of."
            }
        }
    })
}

fn unsubscribe_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$unsubscribe` (client→server): stop receiving topics.",
        "required": ["topics"],
        "properties": {
            "topics": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics to stop receiving."
            }
        }
    })
}

fn set_rate_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$set_rate` (client→server): change a topic's wire cadence.",
        "required": ["topic", "rate_hz"],
        "properties": {
            "topic": { "type": "string", "description": "Topic whose cadence is changing." },
            "rate_hz": {
                "type": "integer",
                "format": "uint32",
                "description": "Requested cadence (Hz); server clamps to [min, max]."
            }
        }
    })
}

fn resume_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$resume` (client→server): present last-seen cursor on reconnect.",
        "required": ["session_id", "last_seq"],
        "properties": {
            "session_id": { "type": "string", "description": "Session to resume." },
            "last_seq": {
                "type": "integer",
                "format": "uint64",
                "description": "Last `seq` the client successfully observed."
            }
        }
    })
}

fn resync_reason_schema() -> Value {
    json!({
        "type": "string",
        "description": "Why a $resync was issued.",
        "enum": ["seq_evicted", "unknown_session", "session_expired"]
    })
}

fn resync_schema() -> Value {
    json!({
        "type": "object",
        "description": concat!(
            "Data body of `$resync`: the gap is unrecoverable; the client MUST rebuild state ",
            "from the fresh snapshot that follows.",
        ),
        "required": ["reason", "resubscribe"],
        "properties": {
            "reason": { "$ref": "#/components/schemas/ResyncReason" },
            "resubscribe": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Topics the client must rebuild."
            }
        }
    })
}

fn lag_action_schema() -> Value {
    json!({
        "type": "string",
        "description": "What the server did with the dropped frames.",
        "enum": ["conflated", "resnapshot"]
    })
}

fn lag_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$lag`: this connection's queue overflowed for a topic.",
        "required": ["topic", "dropped_n", "action"],
        "properties": {
            "topic": { "type": "string", "description": "Topic whose frames were dropped." },
            "dropped_n": {
                "type": "integer",
                "format": "uint64",
                "description": "Number of frames dropped."
            },
            "action": { "$ref": "#/components/schemas/LagAction" }
        }
    })
}

fn protocol_error_schema() -> Value {
    json!({
        "type": "object",
        "description": "Data body of `$error`: a control-plane error.",
        "required": ["code", "message"],
        "properties": {
            "code": { "type": "string", "description": "Short stable machine-readable error code." },
            "message": { "type": "string", "description": "Human-readable description." }
        }
    })
}
