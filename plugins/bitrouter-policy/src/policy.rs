//! Policy definitions and their **combination semantics**.
//!
//! A request may be subject to several policies at once (e.g. a key-level and
//! an org-level policy). v0's behaviour here was implicit; v1 makes the
//! combination rule explicit (004 §4.2):
//!
//! - **deny overrides** — if *any* policy denies a model, it is denied;
//! - **allowlists intersect** — a model must be allowed by *every* policy that
//!   declares an allowlist;
//! - **limits take the minimum** — the effective spend ceiling is the smallest
//!   non-`None` ceiling across all policies;
//! - **expiry takes the earliest** — the effective expiry is the earliest set.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// One named policy, as loaded from a policy file.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// The policy id (matched against an api key's `policy_id`).
    pub id: String,
    /// If set, only these models are permitted (an allowlist).
    pub allowed_models: Option<Vec<String>>,
    /// These models are always denied (a denylist; overrides any allowlist).
    pub denied_models: Vec<String>,
    /// Monthly spend ceiling in micro-USD.
    pub max_spend_micro_usd: Option<u64>,
    /// Hard expiry — requests after this instant are denied.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Why a policy check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyViolation {
    /// The requested model is denied (denylist or not in an allowlist).
    ModelNotAllowed(String),
    /// The monthly spend ceiling has been reached.
    SpendLimitExceeded {
        /// Micro-USD already spent.
        spent: u64,
        /// The ceiling that was hit.
        limit: u64,
    },
    /// The policy has expired.
    Expired,
}

impl std::fmt::Display for PolicyViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyViolation::ModelNotAllowed(m) => {
                write!(f, "model '{m}' is not permitted by policy")
            }
            PolicyViolation::SpendLimitExceeded { spent, limit } => {
                write!(f, "spend limit reached ({spent} / {limit} micro-USD)")
            }
            PolicyViolation::Expired => write!(f, "policy has expired"),
        }
    }
}

/// The combined effect of zero or more policies — the result of folding a set
/// of [`Policy`] together by the documented combination semantics.
#[derive(Debug, Clone, Default)]
pub struct EffectivePolicy {
    allowed_models: Option<Vec<String>>,
    denied_models: Vec<String>,
    /// The effective (minimum) spend ceiling.
    pub max_spend_micro_usd: Option<u64>,
    /// The effective (earliest) expiry.
    pub expires_at: Option<DateTime<Utc>>,
}

impl EffectivePolicy {
    /// Fold a set of policies into their combined effect.
    pub fn combine<'a>(policies: impl IntoIterator<Item = &'a Policy>) -> Self {
        let mut eff = EffectivePolicy::default();
        for p in policies {
            // denylists union — deny overrides
            for m in &p.denied_models {
                if !eff.denied_models.contains(m) {
                    eff.denied_models.push(m.clone());
                }
            }
            // allowlists intersect
            if let Some(allow) = &p.allowed_models {
                eff.allowed_models = Some(match eff.allowed_models.take() {
                    None => allow.clone(),
                    Some(existing) => existing.into_iter().filter(|m| allow.contains(m)).collect(),
                });
            }
            // limits take the minimum
            if let Some(limit) = p.max_spend_micro_usd {
                eff.max_spend_micro_usd = Some(match eff.max_spend_micro_usd {
                    None => limit,
                    Some(existing) => existing.min(limit),
                });
            }
            // expiry takes the earliest
            if let Some(exp) = p.expires_at {
                eff.expires_at = Some(match eff.expires_at {
                    None => exp,
                    Some(existing) => existing.min(exp),
                });
            }
        }
        eff
    }

    /// Check a model name against the combined allow/deny rules.
    pub fn check_model(&self, model: &str) -> Result<(), PolicyViolation> {
        if self.denied_models.iter().any(|m| m == model) {
            return Err(PolicyViolation::ModelNotAllowed(model.to_string()));
        }
        if let Some(allow) = &self.allowed_models {
            if !allow.iter().any(|m| m == model) {
                return Err(PolicyViolation::ModelNotAllowed(model.to_string()));
            }
        }
        Ok(())
    }

    /// Check the policy's hard expiry against now.
    pub fn check_expiry(&self, now: DateTime<Utc>) -> Result<(), PolicyViolation> {
        match self.expires_at {
            Some(exp) if exp <= now => Err(PolicyViolation::Expired),
            _ => Ok(()),
        }
    }

    /// Check accrued spend against the combined ceiling.
    pub fn check_spend(&self, spent: u64) -> Result<(), PolicyViolation> {
        match self.max_spend_micro_usd {
            Some(limit) if spent >= limit => {
                Err(PolicyViolation::SpendLimitExceeded { spent, limit })
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_overrides_across_policies() {
        let a = Policy {
            id: "a".into(),
            allowed_models: Some(vec!["gpt-5".into(), "claude".into()]),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            denied_models: vec!["claude".into()],
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        assert!(eff.check_model("gpt-5").is_ok());
        // claude is allowed by `a` but denied by `b` — deny wins
        assert!(eff.check_model("claude").is_err());
    }

    #[test]
    fn allowlists_intersect() {
        let a = Policy {
            id: "a".into(),
            allowed_models: Some(vec!["gpt-5".into(), "claude".into()]),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            allowed_models: Some(vec!["claude".into(), "gemini".into()]),
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        // only `claude` is in both allowlists
        assert!(eff.check_model("claude").is_ok());
        assert!(eff.check_model("gpt-5").is_err());
        assert!(eff.check_model("gemini").is_err());
    }

    #[test]
    fn limits_take_the_minimum() {
        let a = Policy {
            id: "a".into(),
            max_spend_micro_usd: Some(1_000),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            max_spend_micro_usd: Some(500),
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        assert_eq!(eff.max_spend_micro_usd, Some(500));
        assert!(eff.check_spend(499).is_ok());
        assert!(eff.check_spend(500).is_err());
    }

    #[test]
    fn empty_policy_set_permits_everything() {
        let eff = EffectivePolicy::combine([]);
        assert!(eff.check_model("anything").is_ok());
        assert!(eff.check_spend(u64::MAX).is_ok());
        assert!(eff.check_expiry(Utc::now()).is_ok());
    }
}
