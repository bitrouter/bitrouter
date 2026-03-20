//! Server-side parameter restrictions for MCP tool calls.
//!
//! Allows administrators to deny or allow specific parameters on a per-tool
//! basis. Parameters can either be silently stripped or cause the entire call
//! to be rejected.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::McpGatewayError;

/// Per-server parameter restrictions applied before forwarding tool calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParamRestrictions {
    /// Per-tool parameter rules. Keys are un-namespaced tool names.
    #[serde(default)]
    pub rules: HashMap<String, ParamRule>,
}

/// Restriction rules for a single tool's parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamRule {
    /// Parameters to deny. Deny takes precedence over allow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
    /// If set, only these parameters are allowed through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// What to do when a restricted parameter is found.
    #[serde(default)]
    pub action: ParamViolationAction,
}

/// Action taken when a parameter violates restrictions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamViolationAction {
    /// Remove the parameter silently, proceed with call.
    Strip,
    /// Reject the entire tool call.
    #[default]
    Reject,
}

impl ParamRestrictions {
    /// Validate and optionally mutate tool call arguments.
    ///
    /// Returns `Ok(())` if allowed (possibly with stripped params).
    /// Returns `Err(ParamDenied)` if rejected.
    pub fn check(
        &self,
        tool_name: &str,
        arguments: &mut Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(), McpGatewayError> {
        let Some(rule) = self.rules.get(tool_name) else {
            return Ok(());
        };
        let Some(args) = arguments.as_mut() else {
            return Ok(());
        };

        // Deny list takes precedence
        if let Some(deny) = &rule.deny {
            let denied_keys: Vec<String> = args
                .keys()
                .filter(|k| deny.iter().any(|d| d == *k))
                .cloned()
                .collect();

            for key in &denied_keys {
                match rule.action {
                    ParamViolationAction::Reject => {
                        return Err(McpGatewayError::ParamDenied {
                            tool: tool_name.to_owned(),
                            param: key.clone(),
                        });
                    }
                    ParamViolationAction::Strip => {
                        args.remove(key);
                    }
                }
            }
        }

        // Allow list: reject/strip any key NOT in the list
        if let Some(allow) = &rule.allow {
            let disallowed_keys: Vec<String> = args
                .keys()
                .filter(|k| !allow.iter().any(|a| a == *k))
                .cloned()
                .collect();

            for key in &disallowed_keys {
                match rule.action {
                    ParamViolationAction::Reject => {
                        return Err(McpGatewayError::ParamDenied {
                            tool: tool_name.to_owned(),
                            param: key.clone(),
                        });
                    }
                    ParamViolationAction::Strip => {
                        args.remove(key);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_args(keys: &[&str]) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut map = serde_json::Map::new();
        for k in keys {
            map.insert((*k).to_string(), serde_json::Value::Bool(true));
        }
        Some(map)
    }

    #[test]
    fn deny_rejects() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "delete_repo".into(),
                ParamRule {
                    deny: Some(vec!["force".into()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let mut args = make_args(&["name", "force"]);
        let result = restrictions.check("delete_repo", &mut args);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("force"));
        assert!(err.contains("delete_repo"));
    }

    #[test]
    fn deny_strips() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "delete_repo".into(),
                ParamRule {
                    deny: Some(vec!["force".into()]),
                    allow: None,
                    action: ParamViolationAction::Strip,
                },
            )]),
        };
        let mut args = make_args(&["name", "force"]);
        let result = restrictions.check("delete_repo", &mut args);
        assert!(result.is_ok());
        let map = args.as_ref().map(|a| a.len());
        assert_eq!(map, Some(1));
        assert!(args.as_ref().is_some_and(|a| a.contains_key("name")));
    }

    #[test]
    fn allow_rejects_unknown() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "create_issue".into(),
                ParamRule {
                    deny: None,
                    allow: Some(vec!["title".into(), "body".into()]),
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let mut args = make_args(&["title", "body", "secret"]);
        let result = restrictions.check("create_issue", &mut args);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("secret"));
    }

    #[test]
    fn allow_strips_unknown() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "create_issue".into(),
                ParamRule {
                    deny: None,
                    allow: Some(vec!["title".into(), "body".into()]),
                    action: ParamViolationAction::Strip,
                },
            )]),
        };
        let mut args = make_args(&["title", "body", "secret"]);
        let result = restrictions.check("create_issue", &mut args);
        assert!(result.is_ok());
        let map = args.as_ref().map(|a| a.len());
        assert_eq!(map, Some(2));
    }

    #[test]
    fn no_rule_passthrough() {
        let restrictions = ParamRestrictions::default();
        let mut args = make_args(&["anything"]);
        assert!(restrictions.check("any_tool", &mut args).is_ok());
    }

    #[test]
    fn empty_args_passthrough() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "tool".into(),
                ParamRule {
                    deny: Some(vec!["x".into()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let mut args: Option<serde_json::Map<String, serde_json::Value>> = None;
        assert!(restrictions.check("tool", &mut args).is_ok());
    }

    #[test]
    fn deny_takes_precedence_over_allow() {
        // If a param is in both deny and allow, deny wins
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "tool".into(),
                ParamRule {
                    deny: Some(vec!["shared".into()]),
                    allow: Some(vec!["shared".into(), "ok".into()]),
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let mut args = make_args(&["shared", "ok"]);
        let result = restrictions.check("tool", &mut args);
        assert!(result.is_err());
    }

    #[test]
    fn serde_roundtrip() {
        let json = r#"{
            "rules": {
                "delete_repo": {
                    "deny": ["force", "destructive"],
                    "action": "reject"
                },
                "create_issue": {
                    "allow": ["title", "body", "labels"],
                    "action": "strip"
                }
            }
        }"#;
        let restrictions: ParamRestrictions = serde_json::from_str(json).unwrap_or_default();
        assert!(restrictions.rules.contains_key("delete_repo"));
        assert!(restrictions.rules.contains_key("create_issue"));
        assert_eq!(
            restrictions.rules["delete_repo"].action,
            ParamViolationAction::Reject
        );
        assert_eq!(
            restrictions.rules["create_issue"].action,
            ParamViolationAction::Strip
        );
    }
}
