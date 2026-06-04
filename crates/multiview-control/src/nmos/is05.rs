//! AMWA **NMOS IS-05** ("Device Connection Management") model.
//!
//! IS-05 is how a controller *connects* an NMOS sender to a receiver: it `PATCH`es
//! a **staged** endpoint with the transport parameters and an **activation**, then
//! the device promotes staged → **active** (broadcast-multiviewer brief §6/§8).
//! Multiview exposes this for its receivers (inputs) so a facility controller can
//! steer a 2110 flow into a tile, and for its senders (program/preview egress).
//!
//! This module is the **pure model** of the staged/active state and the
//! transport-parameter block, plus a minimal **SDP transport-file** parse/emit
//! for the ST 2110 fields a receiver needs to bind (multicast group, port,
//! source-filter). The live receiver bind (joining the multicast group on a real
//! NIC) is behind the off-by-default `nmos` feature; the model below is always
//! compiled and tested.
//!
//! ## Output-clock invariant (#1)
//!
//! Connecting a 2110 receiver only changes which essence Multiview *samples*; it
//! never paces the output. PTP (when present) disciplines a separate reference
//! clock — `out_pts = f(tick)` still holds. This module carries no timing; it
//! only describes the transport binding.
use serde::{Deserialize, Serialize};

/// The activation mode of an IS-05 connection change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActivationMode {
    /// Apply the staged change immediately.
    ActivateImmediate,
    /// Apply at a scheduled absolute TAI time.
    ActivateScheduledAbsolute,
    /// Apply at a scheduled time relative to the request.
    ActivateScheduledRelative,
}

/// An IS-05 **activation** block: when a staged change takes effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Activation {
    /// The activation mode, or [`None`] to stage without activating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<ActivationMode>,
    /// The requested activation time (TAI `<seconds>:<nanoseconds>`), for a
    /// scheduled mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_time: Option<String>,
}

impl Activation {
    /// An immediate activation.
    #[must_use]
    pub fn immediate() -> Self {
        Self {
            mode: Some(ActivationMode::ActivateImmediate),
            requested_time: None,
        }
    }

    /// A staging-only request (no activation).
    #[must_use]
    pub fn stage_only() -> Self {
        Self {
            mode: None,
            requested_time: None,
        }
    }
}

/// One leg's RTP transport parameters (IS-05 supports up to two legs for
/// ST 2022-7 redundancy; this models one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TransportParams {
    /// The destination multicast group address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_ip: Option<String>,
    /// The destination UDP port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_port: Option<u16>,
    /// The source IP for an `IGMPv3` source-specific-multicast filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    /// Whether RTP reception is enabled on this leg.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtp_enabled: Option<bool>,
}

/// An IS-05 connection request `PATCH`ed to a `/staged` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConnectionRequest {
    /// Whether the sender/receiver is enabled once active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_enable: Option<bool>,
    /// The activation directive.
    pub activation: Activation,
    /// One transport-param block per leg (1 for single-path, 2 for ST 2022-7).
    #[serde(default)]
    pub transport_params: Vec<TransportParams>,
    /// For a receiver: the sender id to subscribe to (the connection target).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// For a receiver: the SDP transport file describing the sender's stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_file: Option<String>,
}

/// The IS-05 connection state of one endpoint: its current active params and any
/// pending staged change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ConnectionState {
    /// The currently-active transport params (empty until first activated).
    #[serde(default)]
    pub active: Vec<TransportParams>,
    /// A staged-but-not-yet-active change, if one is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<ConnectionRequest>,
    /// Whether the endpoint is master-enabled.
    #[serde(default)]
    pub master_enable: bool,
}

impl ConnectionState {
    /// Stage a connection request (does not activate it).
    pub fn stage(&mut self, request: ConnectionRequest) {
        self.staged = Some(request);
    }

    /// Promote the staged request to active **iff** it carries an immediate
    /// activation, applying its transport params and master-enable. Returns
    /// whether a change was activated.
    ///
    /// A scheduled or staging-only request is left pending (a real device would
    /// activate it at the scheduled time; that timing is out of this pure model).
    pub fn activate_if_immediate(&mut self) -> bool {
        let immediate = matches!(
            self.staged.as_ref().and_then(|r| r.activation.mode),
            Some(ActivationMode::ActivateImmediate)
        );
        if !immediate {
            return false;
        }
        if let Some(request) = self.staged.take() {
            if let Some(enable) = request.master_enable {
                self.master_enable = enable;
            }
            self.active = request.transport_params;
            return true;
        }
        false
    }
}

/// A minimal **SDP transport file** parse for the ST 2110 fields a receiver
/// needs to bind: the `c=` connection (multicast group), the `m=video` port, and
/// the `a=source-filter` source IP.
///
/// This is intentionally a focused, total parser over the lines IS-05/2110 care
/// about — not a full RFC 8866 SDP implementation. Unknown lines are ignored.
#[must_use]
pub fn parse_sdp_transport(sdp: &str) -> TransportParams {
    let mut params = TransportParams {
        destination_ip: None,
        destination_port: None,
        source_ip: None,
        rtp_enabled: Some(true),
    };
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(conn) = line.strip_prefix("c=") {
            // c=IN IP4 <group>/<ttl>
            if let Some(addr) = conn.split_whitespace().nth(2) {
                let group = addr.split('/').next().unwrap_or(addr);
                params.destination_ip = Some(group.to_owned());
            }
        } else if let Some(media) = line.strip_prefix("m=video ") {
            // m=video <port> RTP/AVP <fmt>
            if let Some(port_str) = media.split_whitespace().next() {
                if let Ok(port) = port_str.parse::<u16>() {
                    params.destination_port = Some(port);
                }
            }
        } else if let Some(filter) = line.strip_prefix("a=source-filter:") {
            // a=source-filter: incl IN IP4 <dest> <source>
            if let Some(source) = filter.split_whitespace().last() {
                params.source_ip = Some(source.to_owned());
            }
        }
    }
    params
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        parse_sdp_transport, Activation, ActivationMode, ConnectionRequest, ConnectionState,
        TransportParams,
    };

    fn request(immediate: bool) -> ConnectionRequest {
        ConnectionRequest {
            master_enable: Some(true),
            activation: if immediate {
                Activation::immediate()
            } else {
                Activation::stage_only()
            },
            transport_params: vec![TransportParams {
                destination_ip: Some("239.0.0.1".to_owned()),
                destination_port: Some(5004),
                source_ip: Some("192.0.2.10".to_owned()),
                rtp_enabled: Some(true),
            }],
            sender_id: Some("snd-1".to_owned()),
            transport_file: None,
        }
    }

    #[test]
    fn connection_request_round_trips_through_json() {
        let req = request(true);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["activation"]["mode"], "activate_immediate");
        assert_eq!(json["transport_params"][0]["destination_port"], 5004);
        let back: ConnectionRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn staging_then_immediate_activation_applies_params() {
        let mut state = ConnectionState::default();
        assert!(state.active.is_empty());
        assert!(!state.master_enable);

        state.stage(request(true));
        // Staged, not yet active.
        assert!(state.staged.is_some());
        assert!(state.active.is_empty());

        let activated = state.activate_if_immediate();
        assert!(activated);
        assert!(state.staged.is_none());
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].destination_port, Some(5004));
        assert!(state.master_enable);
    }

    #[test]
    fn staging_only_request_does_not_activate() {
        let mut state = ConnectionState::default();
        state.stage(request(false));
        let activated = state.activate_if_immediate();
        assert!(!activated, "a stage-only request must not auto-activate");
        assert!(state.staged.is_some(), "it stays staged");
        assert!(state.active.is_empty());
    }

    #[test]
    fn scheduled_activation_is_not_treated_as_immediate() {
        let mut state = ConnectionState::default();
        let mut req = request(true);
        req.activation = Activation {
            mode: Some(ActivationMode::ActivateScheduledAbsolute),
            requested_time: Some("1700001000:0".to_owned()),
        };
        state.stage(req);
        assert!(!state.activate_if_immediate());
        assert!(state.staged.is_some());
    }

    #[test]
    fn sdp_transport_parse_extracts_2110_binding_fields() {
        let sdp = "\
v=0
o=- 1443716955 1443716955 IN IP4 192.0.2.10
s=Multiview Program
t=0 0
m=video 5004 RTP/AVP 96
c=IN IP4 239.10.20.30/64
a=source-filter: incl IN IP4 239.10.20.30 192.0.2.10
a=rtpmap:96 raw/90000
";
        let params = parse_sdp_transport(sdp);
        assert_eq!(params.destination_ip.as_deref(), Some("239.10.20.30"));
        assert_eq!(params.destination_port, Some(5004));
        assert_eq!(params.source_ip.as_deref(), Some("192.0.2.10"));
        assert_eq!(params.rtp_enabled, Some(true));
    }

    #[test]
    fn sdp_transport_parse_tolerates_missing_fields() {
        // An SDP with no media line yields no port but does not panic.
        let params = parse_sdp_transport("v=0\nc=IN IP4 239.1.1.1\n");
        assert_eq!(params.destination_ip.as_deref(), Some("239.1.1.1"));
        assert_eq!(params.destination_port, None);
        assert_eq!(params.source_ip, None);
    }
}
