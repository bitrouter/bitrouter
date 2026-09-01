//! Typed enums for the BitRouter Cloud wire contract.
//!
//! These shapes are stable parts of the `/v1/*` wire contract, so we
//! re-declare them here rather than depend on the server crate. Each
//! enum has the same `#[serde(rename_all = "snake_case")]` rendering
//! as its server-side counterpart.

use serde::{Deserialize, Serialize};

/// Discriminator stored in `policies.kind`.
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
    fn policy_kind_round_trips_through_json() -> anyhow::Result<()> {
        for k in [
            PolicyKind::Budget,
            PolicyKind::RateLimit,
            PolicyKind::Guardrail,
            PolicyKind::Preset,
        ] {
            let s = serde_json::to_string(&k)?;
            let back: PolicyKind = serde_json::from_str(&s)?;
            assert_eq!(back, k);
        }
        Ok(())
    }

    #[test]
    fn budget_window_wire_form_is_snake_case() -> anyhow::Result<()> {
        assert_eq!(serde_json::to_string(&BudgetWindow::Day)?, "\"day\"");
        assert_eq!(serde_json::to_string(&BudgetWindow::Month)?, "\"month\"");
        assert_eq!(serde_json::to_string(&BudgetWindow::Total)?, "\"total\"");
        Ok(())
    }

    #[test]
    fn client_type_wire_form_is_snake_case() -> anyhow::Result<()> {
        assert_eq!(
            serde_json::to_string(&ClientType::Confidential)?,
            "\"confidential\""
        );
        assert_eq!(serde_json::to_string(&ClientType::Public)?, "\"public\"");
        Ok(())
    }

    #[test]
    fn byok_requirement_wire_form_is_snake_case() -> anyhow::Result<()> {
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Required)?,
            "\"required\""
        );
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Forbidden)?,
            "\"forbidden\""
        );
        assert_eq!(
            serde_json::to_string(&ByokRequirement::Optional)?,
            "\"optional\""
        );
        Ok(())
    }
}
