//! Policy definitions and their **combination semantics**.
//!
//! A request may be subject to several policies at once. v0's behaviour here
//! was implicit; v1 makes the combination rule explicit, per check kind:
//!
//! | check         | combination                                            |
//! |---------------|---------------------------------------------------------|
//! | model deny    | union — if *any* policy denies a model, it is denied    |
//! | model allow   | **intersect** — allowed by *every* policy with a list   |
//! | spend limit   | **minimum** (AND, strictest)                            |
//! | expiry        | **earliest** (AND)                                      |
//! | tool access   | **union** (OR) — a tool is OK if *any* policy allows it |
//! | rate limit    | **minimum** (AND, strictest)                            |

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
    /// Allowed tool names. `None` = all tools allowed. Combined by **union**
    /// across policies — a tool is permitted if *any* policy allows it.
    pub allowed_tools: Option<Vec<String>>,
    /// Requests-per-minute ceiling. Combined by **minimum** (strictest wins).
    pub max_requests_per_minute: Option<u32>,
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
    /// A requested tool is not permitted by policy.
    ToolNotAllowed(String),
    /// The requests-per-minute ceiling has been reached.
    RateLimitExceeded {
        /// Observed requests per minute.
        observed: u32,
        /// The ceiling that was hit.
        limit: u32,
    },
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
            PolicyViolation::ToolNotAllowed(t) => {
                write!(f, "tool '{t}' is not permitted by policy")
            }
            PolicyViolation::RateLimitExceeded { observed, limit } => {
                write!(f, "rate limit reached ({observed} / {limit} req/min)")
            }
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
    /// The effective allowed-tool set — the **union** of every policy's tool
    /// allowlist. `None` = unrestricted (a policy with no tool list, or no
    /// policies at all, leaves tools unrestricted).
    allowed_tools: Option<Vec<String>>,
    /// The effective (minimum) requests-per-minute ceiling.
    pub max_requests_per_minute: Option<u32>,
}

impl EffectivePolicy {
    /// Fold a set of policies into their combined effect.
    pub fn combine<'a>(policies: impl IntoIterator<Item = &'a Policy>) -> Self {
        let mut eff = EffectivePolicy::default();
        // Tool access is a UNION: track whether any policy left tools
        // unrestricted (→ effective unrestricted) vs. accumulating the union.
        let mut tool_union: Vec<String> = Vec::new();
        let mut any_tool_unrestricted = false;
        let mut any_tool_restricted = false;

        for p in policies {
            // denylists union — deny overrides
            for m in &p.denied_models {
                if !eff.denied_models.contains(m) {
                    eff.denied_models.push(m.clone());
                }
            }
            // model allowlists intersect
            if let Some(allow) = &p.allowed_models {
                eff.allowed_models = Some(match eff.allowed_models.take() {
                    None => allow.clone(),
                    Some(existing) => existing.into_iter().filter(|m| allow.contains(m)).collect(),
                });
            }
            // tool allowlists union (OR)
            match &p.allowed_tools {
                None => any_tool_unrestricted = true,
                Some(tools) => {
                    any_tool_restricted = true;
                    for t in tools {
                        if !tool_union.contains(t) {
                            tool_union.push(t.clone());
                        }
                    }
                }
            }
            // spend limits take the minimum
            if let Some(limit) = p.max_spend_micro_usd {
                eff.max_spend_micro_usd = Some(match eff.max_spend_micro_usd {
                    None => limit,
                    Some(existing) => existing.min(limit),
                });
            }
            // rate limits take the minimum (strictest)
            if let Some(rpm) = p.max_requests_per_minute {
                eff.max_requests_per_minute = Some(match eff.max_requests_per_minute {
                    None => rpm,
                    Some(existing) => existing.min(rpm),
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
        // Tools are restricted only if at least one policy declared a list AND
        // no policy left them unrestricted.
        eff.allowed_tools = if any_tool_restricted && !any_tool_unrestricted {
            Some(tool_union)
        } else {
            None
        };
        eff
    }

    /// Check a model name against the combined allow/deny rules.
    pub fn check_model(&self, model: &str) -> Result<(), PolicyViolation> {
        if self.denied_models.iter().any(|m| m == model) {
            return Err(PolicyViolation::ModelNotAllowed(model.to_string()));
        }
        if let Some(allow) = &self.allowed_models
            && !allow.iter().any(|m| m == model)
        {
            return Err(PolicyViolation::ModelNotAllowed(model.to_string()));
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

    /// Check a set of requested tool names against the combined (unioned)
    /// allowlist. `None` allowlist permits every tool.
    pub fn check_tools<'t>(
        &self,
        tools: impl IntoIterator<Item = &'t str>,
    ) -> Result<(), PolicyViolation> {
        if let Some(allow) = &self.allowed_tools {
            for tool in tools {
                if !allow.iter().any(|t| t == tool) {
                    return Err(PolicyViolation::ToolNotAllowed(tool.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Check an observed requests-per-minute rate against the combined ceiling.
    pub fn check_rate(&self, observed: u32) -> Result<(), PolicyViolation> {
        match self.max_requests_per_minute {
            Some(limit) if observed >= limit => {
                Err(PolicyViolation::RateLimitExceeded { observed, limit })
            }
            _ => Ok(()),
        }
    }

    /// Whether this effective policy restricts tool access.
    pub fn has_tool_restriction(&self) -> bool {
        self.allowed_tools.is_some()
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
        assert!(eff.check_tools(["any-tool"]).is_ok());
        assert!(eff.check_rate(u32::MAX).is_ok());
        assert!(!eff.has_tool_restriction());
    }

    #[test]
    fn tool_allowlists_union() {
        let a = Policy {
            id: "a".into(),
            allowed_tools: Some(vec!["search".into()]),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            allowed_tools: Some(vec!["calculator".into()]),
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        // union: a tool is OK if *any* policy allows it
        assert!(eff.check_tools(["search"]).is_ok());
        assert!(eff.check_tools(["calculator"]).is_ok());
        assert!(eff.check_tools(["search", "calculator"]).is_ok());
        assert!(eff.check_tools(["filesystem"]).is_err());
    }

    #[test]
    fn an_unrestricted_policy_makes_tools_unrestricted() {
        // policy `a` restricts tools, `b` does not (None) — union is "all".
        let a = Policy {
            id: "a".into(),
            allowed_tools: Some(vec!["search".into()]),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            allowed_tools: None,
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        assert!(!eff.has_tool_restriction());
        assert!(eff.check_tools(["anything-at-all"]).is_ok());
    }

    #[test]
    fn rate_limits_take_the_minimum() {
        let a = Policy {
            id: "a".into(),
            max_requests_per_minute: Some(120),
            ..Default::default()
        };
        let b = Policy {
            id: "b".into(),
            max_requests_per_minute: Some(30),
            ..Default::default()
        };
        let eff = EffectivePolicy::combine([&a, &b]);
        assert_eq!(eff.max_requests_per_minute, Some(30));
        assert!(eff.check_rate(29).is_ok());
        assert!(eff.check_rate(30).is_err());
    }
}
