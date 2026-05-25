//! Typed enums mirrored from the BitRouter Cloud crate.
//!
//! These shapes are stable parts of the `/v1/*` wire contract, so we
//! re-declare them here rather than depend on the server crate. Each
//! enum has the same `#[serde(rename_all = "snake_case")]` rendering
//! as its server-side counterpart.

use serde::{Deserialize, Serialize};

/// Discriminator stored in `policies.kind`.
///
/// Mirrors `bitrouter_cloud::policy::spec::PolicyKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyKind {
    /// Spend cap over a rolling window.
    Budget,
    /// Request / token rate limit over a sliding window.
    RateLimit,
    /// Per-request constraints (model allow/deny, max tokens, etc.).
    Guardrail,
    /// Named bundle of the other three kinds.
    Preset,
}

impl PolicyKind {
    /// Wire-form string for this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyKind::Budget => "budget",
            PolicyKind::RateLimit => "rate_limit",
            PolicyKind::Guardrail => "guardrail",
            PolicyKind::Preset => "preset",
        }
    }
}

/// Rolling-spend window for a budget policy.
///
/// Mirrors `bitrouter_cloud::policy::spec::BudgetWindow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetWindow {
    /// 24 hours rolling.
    Day,
    /// 30 days rolling.
    Month,
    /// Lifetime — accumulates indefinitely.
    Total,
}

/// BYOK posture for a guardrail policy.
///
/// Mirrors `bitrouter_cloud::policy::spec::ByokRequirement`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ByokRequirement {
    /// Inference call MUST be served via a BYOK provider key.
    Required,
    /// Inference call MUST NOT be served via a BYOK provider key.
    Forbidden,
    /// Either path is acceptable. Equivalent to no constraint.
    Optional,
}

/// OAuth client kind.
///
/// Mirrors `bitrouter_cloud::oauth::clients::ClientType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    /// Backend-credentialed client; presents `client_secret` at the
    /// token endpoint.
    Confidential,
    /// No client secret; PKCE is mandatory.
    Public,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_kind_round_trips_through_json() {
        for k in [
            PolicyKind::Budget,
            PolicyKind::RateLimit,
            PolicyKind::Guardrail,
            PolicyKind::Preset,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: PolicyKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn budget_window_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&BudgetWindow::Day).unwrap(),
            "\"day\""
        );
        assert_eq!(
            serde_json::to_string(&BudgetWindow::Month).unwrap(),
            "\"month\""
        );
        assert_eq!(
            serde_json::to_string(&BudgetWindow::Total).unwrap(),
            "\"total\""
        );
    }

    #[test]
    fn client_type_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ClientType::Confidential).unwrap(),
            "\"confidential\""
        );
        assert_eq!(
            serde_json::to_string(&ClientType::Public).unwrap(),
            "\"public\""
        );
    }

    #[test]
    fn byok_requirement_wire_form_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Required).unwrap(),
            "\"required\""
        );
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Forbidden).unwrap(),
            "\"forbidden\""
        );
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Optional).unwrap(),
            "\"optional\""
        );
    }
}
