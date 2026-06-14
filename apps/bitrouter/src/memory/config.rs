//! Memory-scope policy parsed from `plugins.bitrouter-memory` in `bitrouter.yaml`.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

/// The `plugins.bitrouter-memory` config block.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryScopeConfig {
    /// The `mcp_servers` id pointing at the Walrus relayer. Only `tools/call`s
    /// routed to this server are scoped. Empty ⇒ scoping disabled (passthrough).
    #[serde(default)]
    pub server: String,
    /// Namespace injected when a scoped agent omits one and has no per-agent
    /// default. Empty ⇒ omitted namespaces are left for the relayer's default.
    #[serde(default)]
    pub default_namespace: String,
    /// Per-agent scopes, keyed by the `x-bitrouter-agent` identity.
    #[serde(default)]
    pub agents: HashMap<String, AgentScope>,
    /// Optional "always-on memory" behaviour. When enabled, the memory server
    /// is auto-wired into the server-side tool loop, recall is forced as the
    /// first tool call, and the model is instructed to persist via remember.
    #[serde(default)]
    pub always: Option<AlwaysMemoryConfig>,
}

/// The `plugins.bitrouter-memory.always` block: force every agent turn to recall
/// before answering and instruct it to persist afterwards.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AlwaysMemoryConfig {
    /// Master switch. `false` ⇒ behaves exactly as without the block.
    pub enabled: bool,
    /// The unprefixed recall tool name forced as the first tool call. The
    /// server's `tool_prefix` is prepended at wiring time.
    pub recall_tool: String,
    /// System-prompt instruction prepended to every turn. The `{remember}`
    /// placeholder is replaced with the prefixed remember tool name.
    pub remember_instruction: String,
}

impl Default for AlwaysMemoryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            recall_tool: "memwal_recall".to_string(),
            remember_instruction: "You have a persistent memory across turns. \
                Before ending your turn, you MUST call the `{remember}` tool to \
                persist any new facts, decisions, or user preferences from this \
                exchange so future turns can recall them."
                .to_string(),
        }
    }
}

/// One agent's namespace scope.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentScope {
    /// Allowed namespaces. The literal `"*"` grants unrestricted access.
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// Namespace injected when this agent omits one. Falls back to the
    /// top-level `default_namespace`.
    #[serde(default)]
    pub default: Option<String>,
}

/// What the scope table decides for a single memory call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDecision {
    /// Leave the request untouched.
    Passthrough,
    /// Set `params.arguments.namespace` to this value.
    Inject(String),
    /// Reject — the requested namespace is outside the agent's allowed set.
    Deny { agent: String, requested: String },
}

/// Resolved, lookup-optimised form of [`MemoryScopeConfig`].
#[derive(Debug, Clone, Default)]
pub struct MemoryScopeTable {
    server: String,
    default_namespace: Option<String>,
    agents: HashMap<String, ResolvedScope>,
}

#[derive(Debug, Clone)]
struct ResolvedScope {
    /// `None` = unrestricted (the `"*"` wildcard).
    allowed: Option<HashSet<String>>,
    default: Option<String>,
}

impl MemoryScopeTable {
    /// Build from parsed config. An empty `server` yields a disabled table.
    pub fn from_config(cfg: &MemoryScopeConfig) -> Self {
        let default_namespace = if cfg.default_namespace.is_empty() {
            None
        } else {
            Some(cfg.default_namespace.clone())
        };
        let agents = cfg
            .agents
            .iter()
            .map(|(name, scope)| {
                let allowed = if scope.namespaces.iter().any(|n| n == "*") {
                    None
                } else {
                    Some(scope.namespaces.iter().cloned().collect::<HashSet<_>>())
                };
                (
                    name.clone(),
                    ResolvedScope {
                        allowed,
                        default: scope.default.clone(),
                    },
                )
            })
            .collect();
        Self {
            server: cfg.server.clone(),
            default_namespace,
            agents,
        }
    }

    /// The configured memory server id (the `mcp_servers` key to scope).
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Whether scoping is active. Disabled tables short-circuit to passthrough.
    pub fn is_enabled(&self) -> bool {
        !self.server.is_empty()
    }

    /// Decide what to do for a memory call by `agent`, requesting `namespace`
    /// (`None` = the agent omitted it).
    ///
    /// Unknown agents are treated as fully restricted (empty allowed set), so
    /// they may never *name* a namespace — they only ever receive the injected
    /// default. This fails closed.
    pub fn decide(&self, agent: &str, requested: Option<&str>) -> ScopeDecision {
        if !self.is_enabled() {
            return ScopeDecision::Passthrough;
        }
        let restricted_unknown = ResolvedScope {
            allowed: Some(HashSet::new()),
            default: None,
        };
        let scope = self.agents.get(agent).unwrap_or(&restricted_unknown);

        // Unrestricted agent: never touched.
        let Some(allowed) = scope.allowed.as_ref() else {
            return ScopeDecision::Passthrough;
        };

        let effective_default = scope
            .default
            .clone()
            .or_else(|| self.default_namespace.clone());

        match requested {
            None => match effective_default {
                Some(ns) => ScopeDecision::Inject(ns),
                None => ScopeDecision::Passthrough,
            },
            Some(ns) if allowed.contains(ns) => ScopeDecision::Passthrough,
            Some(ns) => ScopeDecision::Deny {
                agent: agent.to_string(),
                requested: ns.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> MemoryScopeTable {
        let cfg: MemoryScopeConfig = serde_json::from_value(serde_json::json!({
            "server": "memory",
            "default_namespace": "shared",
            "agents": {
                "orchestrator": { "namespaces": ["*"] },
                "researcher":   { "namespaces": ["research"], "default": "research" }
            }
        }))
        .unwrap();
        MemoryScopeTable::from_config(&cfg)
    }

    #[test]
    fn disabled_table_is_passthrough() {
        let t = MemoryScopeTable::from_config(&MemoryScopeConfig::default());
        assert!(!t.is_enabled());
        assert_eq!(t.decide("anyone", Some("x")), ScopeDecision::Passthrough);
    }

    #[test]
    fn unrestricted_agent_passes_through() {
        assert_eq!(
            table().decide("orchestrator", Some("anything")),
            ScopeDecision::Passthrough
        );
        // Even with no namespace, an unrestricted agent is untouched.
        assert_eq!(
            table().decide("orchestrator", None),
            ScopeDecision::Passthrough
        );
    }

    #[test]
    fn scoped_agent_allowed_namespace_passes_through() {
        assert_eq!(
            table().decide("researcher", Some("research")),
            ScopeDecision::Passthrough
        );
    }

    #[test]
    fn scoped_agent_disallowed_namespace_is_denied() {
        assert_eq!(
            table().decide("researcher", Some("secret")),
            ScopeDecision::Deny {
                agent: "researcher".into(),
                requested: "secret".into(),
            }
        );
    }

    #[test]
    fn scoped_agent_omitting_namespace_gets_its_default() {
        assert_eq!(
            table().decide("researcher", None),
            ScopeDecision::Inject("research".into())
        );
    }

    #[test]
    fn unknown_agent_naming_a_namespace_is_denied() {
        assert_eq!(
            table().decide("ghost", Some("research")),
            ScopeDecision::Deny {
                agent: "ghost".into(),
                requested: "research".into(),
            }
        );
    }

    #[test]
    fn unknown_agent_omitting_namespace_gets_global_default() {
        assert_eq!(
            table().decide("ghost", None),
            ScopeDecision::Inject("shared".into())
        );
    }
}
