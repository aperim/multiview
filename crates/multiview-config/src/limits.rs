//! Management-plane connection + rate limits (SEC-14) — the control-plane DoS
//! floor, as operator configuration.
//!
//! [`ManagementLimits`] is the `[control.limits]` config section: the
//! concurrent-request cap and the per-IP (pre-auth) / per-API-key (post-auth)
//! token-bucket rates the `multiview-control` middleware enforces. Absent ⇒ the
//! secure defaults below (limits **on**). The runtime limiter lives in
//! `multiview-control`; this crate only models + validates the knobs.

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Default concurrent in-flight request cap: generous for a handful of operators
/// plus the SPA and its realtime stream, while bounding a request flood.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 256;
/// Default per-IP burst — covers the SPA's initial fan-out of resource fetches on
/// load without throttling a legitimate operator.
const DEFAULT_PER_IP_BURST: u32 = 120;
/// Default per-IP steady-state rate (requests/second), pre-auth.
const DEFAULT_PER_IP_REFILL_PER_SEC: u32 = 40;
/// Default per-API-key burst — more generous than per-IP: an authenticated
/// client is trusted further.
const DEFAULT_PER_API_KEY_BURST: u32 = 240;
/// Default per-API-key steady-state rate (requests/second), post-auth.
const DEFAULT_PER_API_KEY_REFILL_PER_SEC: u32 = 80;

fn default_enabled() -> bool {
    true
}

fn default_max_concurrent_requests() -> usize {
    DEFAULT_MAX_CONCURRENT_REQUESTS
}

fn default_per_ip() -> RateLimitConfig {
    RateLimitConfig {
        burst: DEFAULT_PER_IP_BURST,
        refill_per_sec: DEFAULT_PER_IP_REFILL_PER_SEC,
    }
}

fn default_per_api_key() -> RateLimitConfig {
    RateLimitConfig {
        burst: DEFAULT_PER_API_KEY_BURST,
        refill_per_sec: DEFAULT_PER_API_KEY_REFILL_PER_SEC,
    }
}

/// A token-bucket rate: a `burst` ceiling replenished at `refill_per_sec`
/// requests per second. Over-budget requests get `429` + `Retry-After`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RateLimitConfig {
    /// The maximum number of requests admitted in an instantaneous burst.
    pub burst: u32,
    /// The steady-state replenishment rate, in requests per second.
    pub refill_per_sec: u32,
}

impl RateLimitConfig {
    /// Reject a nonsensical rate.
    ///
    /// # Errors
    /// [`ConfigError::Validation`] if `burst` or `refill_per_sec` is zero — a zero
    /// burst would reject every request forever, and a zero refill would never
    /// replenish the bucket. `which` names the section for the message.
    fn validate(&self, which: &str) -> Result<(), ConfigError> {
        // STUB (RED): the GREEN commit implements the zero-value rejection so the
        // zero-* tests below fail here first.
        let _ = which;
        Ok(())
    }
}

/// The `[control.limits]` section: the management-plane connection + rate caps
/// (SEC-14 control-plane DoS floor).
///
/// Absent ⇒ the secure defaults (limits enabled). Every field defaults
/// individually, so a partial section overrides only the knobs it names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManagementLimits {
    /// Whether the limits are enforced at all. Default `true` (secure default);
    /// set `false` only for a fully-trusted single-tenant deployment.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The cap on concurrent in-flight requests. Over-cap requests get `503` +
    /// `Retry-After` rather than queueing. Default `256`.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// The per-source-IP token bucket, applied **before** authentication to
    /// protect the auth path from brute-force. Keyed on the peer IP.
    #[serde(default = "default_per_ip")]
    pub per_ip: RateLimitConfig,
    /// The per-API-key token bucket, applied **after** authentication so one
    /// credential cannot monopolise the management plane. Keyed on the
    /// authenticated key id.
    #[serde(default = "default_per_api_key")]
    pub per_api_key: RateLimitConfig,
}

impl Default for ManagementLimits {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            max_concurrent_requests: default_max_concurrent_requests(),
            per_ip: default_per_ip(),
            per_api_key: default_per_api_key(),
        }
    }
}

impl ManagementLimits {
    /// Validate the limits.
    ///
    /// # Errors
    /// [`ConfigError::Validation`] if the concurrency cap is zero, or either
    /// token-bucket rate has a zero `burst` or `refill_per_sec` — each would turn
    /// the DoS floor into a self-inflicted outage. Validated regardless of
    /// `enabled` so a typo is caught even while the limits are temporarily off.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // STUB (RED): the GREEN commit implements the checks; returning `Ok`
        // here makes the zero-value rejection tests fail first.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{ManagementLimits, RateLimitConfig};

    #[test]
    fn secure_defaults_validate_ok() {
        let limits = ManagementLimits::default();
        assert!(limits.enabled);
        assert_eq!(limits.max_concurrent_requests, 256);
        assert_eq!(limits.per_ip.burst, 120);
        assert_eq!(limits.per_ip.refill_per_sec, 40);
        assert_eq!(limits.per_api_key.burst, 240);
        assert_eq!(limits.per_api_key.refill_per_sec, 80);
        limits.validate().expect("the secure defaults must validate");
    }

    #[test]
    fn a_zero_concurrency_cap_is_rejected() {
        let limits = ManagementLimits {
            max_concurrent_requests: 0,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero concurrency cap would reject every request and must fail config load"
        );
    }

    #[test]
    fn a_zero_per_ip_burst_is_rejected() {
        let limits = ManagementLimits {
            per_ip: RateLimitConfig {
                burst: 0,
                refill_per_sec: 40,
            },
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero per-IP burst would reject every request and must fail config load"
        );
    }

    #[test]
    fn a_zero_per_api_key_refill_is_rejected() {
        let limits = ManagementLimits {
            per_api_key: RateLimitConfig {
                burst: 240,
                refill_per_sec: 0,
            },
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero per-API-key refill would never replenish and must fail config load"
        );
    }

    #[test]
    fn an_absent_section_deserialises_to_the_secure_defaults() {
        // A `[control.limits]` written with no fields (or omitted entirely) fills
        // every knob from its default — the secure default posture.
        let parsed: ManagementLimits =
            serde_json::from_str("{}").expect("an empty object fills every default");
        assert_eq!(parsed, ManagementLimits::default());
    }

    #[test]
    fn a_partial_section_overrides_only_named_knobs() {
        let parsed: ManagementLimits = serde_json::from_str(r#"{"max_concurrent_requests": 16}"#)
            .expect("a partial section fills the rest from defaults");
        assert_eq!(parsed.max_concurrent_requests, 16);
        // Untouched knobs keep their secure defaults.
        assert_eq!(parsed.per_ip, ManagementLimits::default().per_ip);
        assert!(parsed.enabled);
    }
}
