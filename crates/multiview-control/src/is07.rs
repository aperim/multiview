//! An AMWA **NMOS IS-07** event & tally model: the IP-native equivalent of
//! GPI/GPO for tally and Boolean events (broadcast-multiviewer brief §2).
//!
//! IS-07 ("Event & Tally Specification") carries typed events between a *source*
//! and *receivers*. The **transport-agnostic message model** is pure JSON and is
//! defined here in full so it is exhaustively unit-testable with no sockets:
//!
//! * an [`Is07Message`] is either a `state` message (the current value of an
//!   event, with an [`Is07Timing`] block) or a `health` heartbeat;
//! * the value is an [`Is07EventType`]-discriminated [`Is07Payload`]
//!   (`boolean` / `number` / `string`), matching the IS-07 type vocabulary;
//! * a receiver subscribes to a set of source ids via an [`Is07Subscription`]
//!   command (the WebSocket subscription request).
//!
//! The pure model maps both ways to Multiview's own vocabulary: a `boolean` IS-07
//! event is a **GPI bit** ([`GpiEvent`]), and a tally lamp resolves to an IS-07
//! `number` (the 0/1/2/3 TSL palette code) so an external multiviewer can light
//! a lamp from Multiview's arbitrated state.
//!
//! ## Transports
//!
//! IS-07 is carried over **WebSocket** (the primary, always-available transport
//! — the control plane already runs a WS server, so the WS binding reuses the
//! existing realtime fan-out) and optionally **MQTT**. The MQTT transport pulls
//! a broker client (a native dependency) and is therefore behind the
//! **off-by-default `is07-mqtt` feature**; the pure message/codec model above is
//! always compiled and always tested. With the feature off, Multiview still speaks
//! IS-07 over WebSocket.
use multiview_core::tally::TallyColor;
use multiview_events::{TallyEvent, TallyTarget};
use serde::{Deserialize, Serialize};

/// The IS-07 event value type (the `payload.type` vocabulary).
///
/// Serialised **tagged** is unnecessary here — this is a unit discriminator
/// rendered as its lowercase wire string. `#[non_exhaustive]` so the `object`
/// composite type can be added later without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07EventType {
    /// A Boolean (on/off) event — the GPI/tally lamp primitive.
    Boolean,
    /// A numeric event (e.g. a tally palette code or a measured value).
    Number,
    /// A string event (e.g. a UMD label).
    String,
}

/// The typed value of an IS-07 event.
///
/// Internally tagged on `type` (the IS-07 `payload.type`), never `untagged`
/// (repo conventions). `#[non_exhaustive]` so further value types can be added.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Payload {
    /// A Boolean value.
    Boolean {
        /// The Boolean value.
        value: bool,
    },
    /// A numeric value.
    Number {
        /// The numeric value.
        value: i64,
        /// The scaling factor (IS-07 numbers are `value`/`scale`); `1` for an
        /// integer such as a tally palette code.
        #[serde(default = "one")]
        scale: i64,
    },
    /// A string value.
    String {
        /// The string value.
        value: String,
    },
}

/// The default IS-07 numeric scale (`1`).
const fn one() -> i64 {
    1
}

impl Is07Payload {
    /// The IS-07 event type of this payload.
    #[must_use]
    pub const fn event_type(&self) -> Is07EventType {
        match self {
            Self::Boolean { .. } => Is07EventType::Boolean,
            Self::Number { .. } => Is07EventType::Number,
            Self::String { .. } => Is07EventType::String,
        }
    }
}

/// The IS-07 timing block carried on a `state` message.
///
/// Both timestamps are TAI in the IS-07 `<seconds>:<nanoseconds>` string form.
/// The model carries them verbatim; the control plane stamps them from its own
/// clock at the transport boundary (never from an input PTS — invariant #1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Is07Timing {
    /// When the event was created (`creation_timestamp`).
    pub creation_timestamp: String,
    /// When the originating condition occurred (`origin_timestamp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_timestamp: Option<String>,
}

/// The IS-07 message kind (`message_type`): a state update or a health heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "message_type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Message {
    /// The current value of an event source.
    State {
        /// The originating source id (NMOS UUID, opaque here).
        source_id: String,
        /// The flow id the event belongs to (opaque here).
        flow_id: String,
        /// The typed event value.
        payload: Is07Payload,
        /// The IS-07 timing block.
        timing: Is07Timing,
    },
    /// A heartbeat keeping the connection alive (carries the last-seen health
    /// timestamp).
    Health {
        /// The originating source id.
        source_id: String,
        /// The health heartbeat timestamp (TAI string).
        heartbeat: String,
    },
}

impl Is07Message {
    /// The IS-07 source id this message is about.
    #[must_use]
    pub fn source_id(&self) -> &str {
        match self {
            Self::State { source_id, .. } | Self::Health { source_id, .. } => source_id,
        }
    }
}

/// An IS-07 receiver subscription command (the WebSocket subscription request).
///
/// A receiver asks to receive state for a set of source ids over the WebSocket
/// connection; the server then streams [`Is07Message::State`] for each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Is07Subscription {
    /// The IS-07 command (always `subscription` for this request).
    pub command: Is07Command,
    /// The source ids to subscribe to.
    pub sources: Vec<String>,
}

/// The IS-07 WebSocket command vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Is07Command {
    /// Subscribe to a set of source ids.
    Subscription,
    /// A health heartbeat command.
    Health,
}

/// A general-purpose **interface** event: a Boolean GPI/GPO bit.
///
/// This is Multiview's own vocabulary for a Boolean tally/contact-closure event,
/// independent of IS-07's wire form. The IS-07 model converts to/from it so the
/// rest of the control plane (and the realtime stream) deals in [`GpiEvent`],
/// not raw IS-07 JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GpiEvent {
    /// The logical GPI line id this event is for.
    pub line: String,
    /// Whether the contact is closed / the bit is asserted.
    pub asserted: bool,
}

impl GpiEvent {
    /// Build a [`GpiEvent`] from an IS-07 `state` message carrying a Boolean
    /// payload, using the message's `source_id` as the GPI line id.
    ///
    /// Returns [`None`] for a non-`state` message or a non-Boolean payload.
    #[must_use]
    pub fn from_is07(message: &Is07Message) -> Option<Self> {
        match message {
            Is07Message::State {
                source_id, payload, ..
            } => match payload {
                Is07Payload::Boolean { value } => Some(Self {
                    line: source_id.clone(),
                    asserted: *value,
                }),
                _ => None,
            },
            Is07Message::Health { .. } => None,
        }
    }

    /// Render this GPI event as an IS-07 Boolean `state` message for emission to
    /// subscribed receivers, stamped with `creation_timestamp`.
    #[must_use]
    pub fn to_is07(&self, creation_timestamp: impl Into<String>) -> Is07Message {
        Is07Message::State {
            source_id: self.line.clone(),
            flow_id: self.line.clone(),
            payload: Is07Payload::Boolean {
                value: self.asserted,
            },
            timing: Is07Timing {
                creation_timestamp: creation_timestamp.into(),
                origin_timestamp: None,
            },
        }
    }
}

/// Map a [`TallyColor`] to its IS-07 numeric palette code (the TSL 0/1/2/3
/// vocabulary: off/red/green/amber).
#[must_use]
pub fn tally_color_to_is07(
    color: TallyColor,
    creation_timestamp: impl Into<String>,
) -> Is07Message {
    Is07Message::State {
        source_id: "tally".to_owned(),
        flow_id: "tally".to_owned(),
        payload: Is07Payload::Number {
            value: i64::from(color.tsl_code()),
            scale: 1,
        },
        timing: Is07Timing {
            creation_timestamp: creation_timestamp.into(),
            origin_timestamp: None,
        },
    }
}

/// Render a resolved [`TallyEvent`] as an IS-07 `number` `state` message keyed by
/// the target, so an external IS-07 receiver can light a lamp from Multiview's
/// arbitrated tally state.
#[must_use]
pub fn tally_event_to_is07(
    event: &TallyEvent,
    creation_timestamp: impl Into<String>,
) -> Is07Message {
    let source_id = match &event.target {
        TallyTarget::Tile { index } => format!("tile:{index}"),
        TallyTarget::Element { name } => format!("element:{name}"),
        _ => "tally".to_owned(),
    };
    let flow_id = source_id.clone();
    Is07Message::State {
        source_id,
        flow_id,
        payload: Is07Payload::Number {
            value: i64::from(event.state.color.tsl_code()),
            scale: 1,
        },
        timing: Is07Timing {
            creation_timestamp: creation_timestamp.into(),
            origin_timestamp: None,
        },
    }
}

/// Recover a [`TallyState`](multiview_core::tally::TallyState)'s lamp colour from an
/// IS-07 `number` `state` message carrying a TSL palette code (`0..=3`).
///
/// Returns [`None`] for a non-`state` message, a non-`number` payload, or a
/// numeric value outside the TSL palette range.
#[must_use]
pub fn tally_color_from_is07(message: &Is07Message) -> Option<TallyColor> {
    match message {
        Is07Message::State { payload, .. } => match payload {
            Is07Payload::Number { value, scale } => {
                if *scale != 1 {
                    return None;
                }
                let code = u8::try_from(*value).ok()?;
                TallyColor::from_tsl_code(code)
            }
            _ => None,
        },
        Is07Message::Health { .. } => None,
    }
}

#[cfg(feature = "is07-mqtt")]
pub mod mqtt {
    //! MQTT transport bindings for IS-07 (off-by-default `is07-mqtt` feature).
    //!
    //! IS-07 may be carried over MQTT instead of (or alongside) WebSocket. An
    //! MQTT client is a native dependency, so this transport is feature-gated;
    //! the pure message model is always available, so Multiview always speaks IS-07
    //! over WebSocket regardless of this feature.
    //!
    //! This module is intentionally a thin, well-typed seam: it pins the MQTT
    //! **topic convention** for IS-07 (the pure, testable part) and leaves the
    //! live broker connection to the deployment. No raw broker client is
    //! compiled into the default build.
    use super::Is07Message;

    /// The MQTT topic an IS-07 message for `source_id` is published on, following
    /// the NMOS convention `x-nmos/events/v1.0/sources/<source_id>`.
    #[must_use]
    pub fn topic_for_source(source_id: &str) -> String {
        format!("x-nmos/events/v1.0/sources/{source_id}")
    }

    /// The MQTT topic for a message, derived from its source id.
    #[must_use]
    pub fn topic_for_message(message: &Is07Message) -> String {
        topic_for_source(message.source_id())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use multiview_core::tally::{TallyColor, TallyState};
    use multiview_events::{TallyEvent, TallyTarget};

    use super::{
        tally_color_from_is07, tally_color_to_is07, tally_event_to_is07, GpiEvent, Is07Command,
        Is07EventType, Is07Message, Is07Payload, Is07Subscription, Is07Timing,
    };

    fn timing() -> Is07Timing {
        Is07Timing {
            creation_timestamp: "1700000000:0".to_owned(),
            origin_timestamp: None,
        }
    }

    #[test]
    fn state_message_round_trips_through_json_tagged() {
        let msg = Is07Message::State {
            source_id: "src-1".to_owned(),
            flow_id: "flow-1".to_owned(),
            payload: Is07Payload::Boolean { value: true },
            timing: timing(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["message_type"], "state");
        assert_eq!(json["payload"]["type"], "boolean");
        assert_eq!(json["payload"]["value"], true);
        let back: Is07Message = serde_json::from_value(json).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn payload_event_type_matches_variant() {
        assert_eq!(
            Is07Payload::Boolean { value: false }.event_type(),
            Is07EventType::Boolean
        );
        assert_eq!(
            Is07Payload::Number { value: 3, scale: 1 }.event_type(),
            Is07EventType::Number
        );
        assert_eq!(
            Is07Payload::String {
                value: "x".to_owned()
            }
            .event_type(),
            Is07EventType::String
        );
    }

    #[test]
    fn subscription_command_serialises_to_the_wire_vocabulary() {
        let sub = Is07Subscription {
            command: Is07Command::Subscription,
            sources: vec!["a".to_owned(), "b".to_owned()],
        };
        let json = serde_json::to_value(&sub).unwrap();
        assert_eq!(json["command"], "subscription");
        assert_eq!(json["sources"][0], "a");
        let back: Is07Subscription = serde_json::from_value(json).unwrap();
        assert_eq!(back, sub);
    }

    #[test]
    fn gpi_round_trips_through_is07_boolean_state() {
        let gpi = GpiEvent {
            line: "preview-bus".to_owned(),
            asserted: true,
        };
        let msg = gpi.to_is07("1700000000:0");
        // The IS-07 message carries the Boolean and the line as the source id.
        match &msg {
            Is07Message::State {
                source_id, payload, ..
            } => {
                assert_eq!(source_id, "preview-bus");
                assert_eq!(*payload, Is07Payload::Boolean { value: true });
            }
            other => panic!("expected state, got {other:?}"),
        }
        let back = GpiEvent::from_is07(&msg).unwrap();
        assert_eq!(back, gpi);
    }

    #[test]
    fn gpi_from_is07_rejects_non_boolean_and_health() {
        let number = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number { value: 1, scale: 1 },
            timing: timing(),
        };
        assert!(GpiEvent::from_is07(&number).is_none());
        let health = Is07Message::Health {
            source_id: "s".to_owned(),
            heartbeat: "1700000000:0".to_owned(),
        };
        assert!(GpiEvent::from_is07(&health).is_none());
    }

    #[test]
    fn tally_color_round_trips_through_is07_number() {
        for color in [
            TallyColor::Off,
            TallyColor::Red,
            TallyColor::Green,
            TallyColor::Amber,
        ] {
            let msg = tally_color_to_is07(color, "1700000000:0");
            assert_eq!(tally_color_from_is07(&msg), Some(color));
        }
    }

    #[test]
    fn tally_color_from_is07_rejects_out_of_palette_and_scaled() {
        let bad_code = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number { value: 9, scale: 1 },
            timing: timing(),
        };
        assert_eq!(tally_color_from_is07(&bad_code), None);
        let scaled = Is07Message::State {
            source_id: "s".to_owned(),
            flow_id: "f".to_owned(),
            payload: Is07Payload::Number {
                value: 1,
                scale: 100,
            },
            timing: timing(),
        };
        assert_eq!(tally_color_from_is07(&scaled), None);
    }

    #[test]
    fn tally_event_to_is07_keys_by_target_and_carries_palette_code() {
        let event = TallyEvent {
            target: TallyTarget::Tile { index: 5 },
            state: TallyState::program(),
        };
        let msg = tally_event_to_is07(&event, "1700000000:0");
        match &msg {
            Is07Message::State {
                source_id, payload, ..
            } => {
                assert_eq!(source_id, "tile:5");
                assert_eq!(
                    *payload,
                    Is07Payload::Number {
                        value: i64::from(TallyColor::Red.tsl_code()),
                        scale: 1
                    }
                );
            }
            other => panic!("expected state, got {other:?}"),
        }
    }

    #[cfg(feature = "is07-mqtt")]
    #[test]
    fn mqtt_topic_follows_the_nmos_convention() {
        use super::mqtt;
        assert_eq!(
            mqtt::topic_for_source("abc"),
            "x-nmos/events/v1.0/sources/abc"
        );
        let msg = Is07Message::Health {
            source_id: "xyz".to_owned(),
            heartbeat: "1700000000:0".to_owned(),
        };
        assert_eq!(
            mqtt::topic_for_message(&msg),
            "x-nmos/events/v1.0/sources/xyz"
        );
    }
}
