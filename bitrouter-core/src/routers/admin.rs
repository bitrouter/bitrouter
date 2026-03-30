//! Admin types and traits for runtime management of routes and tools.
//!
//! Provides extension traits that layer admin (mutation / inspection) capabilities
//! on top of the read-only discovery traits in [`registry`](super::registry):
//!
//! | Entity  | Discovery trait    | Admin trait            |
//! |---------|--------------------|------------------------|
//! | Models  | `RoutingTable`     | `AdminRoutingTable`    |
//! | Tools   | `ToolRegistry`     | `AdminToolRegistry`    |

use std::collections::HashMap;
use std::future::Future;

use serde::{Deserialize, Serialize};

use crate::errors::Result;

use super::registry::ToolRegistry;
use super::routing_table::{ApiProtocol, RoutingTable};

/// A single endpoint in a dynamic route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEndpoint {
    /// Provider name (must exist in the providers section or built-ins).
    pub provider: String,
    /// Upstream service identifier (model ID or tool ID).
    #[serde(alias = "model_id")]
    pub service_id: String,
    /// API protocol for this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<ApiProtocol>,
}

/// Whether a route targets a model or a tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteKind {
    /// Route resolves to a language model endpoint.
    #[default]
    Model,
    /// Route resolves to a tool endpoint.
    Tool,
}

/// Strategy for distributing requests across multiple endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStrategy {
    /// Try endpoints in declared order.
    #[default]
    Priority,
    /// Distribute requests evenly via round-robin.
    LoadBalance,
}

/// A dynamically-configured route definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicRoute {
    /// The virtual service name (e.g. "research", "fast").
    #[serde(alias = "model")]
    pub name: String,
    /// Whether this route targets a model or a tool.
    #[serde(default)]
    pub kind: RouteKind,
    /// Routing strategy across endpoints.
    #[serde(default)]
    pub strategy: RouteStrategy,
    /// One or more upstream endpoints to route to.
    pub endpoints: Vec<RouteEndpoint>,
}

/// Extension trait for routing tables that support runtime route management.
///
/// Implementations store dynamic routes separately from config-defined routes.
/// Dynamic routes take precedence during resolution.
pub trait AdminRoutingTable: RoutingTable {
    /// Create or update a dynamic route.
    fn add_route(&self, route: DynamicRoute) -> Result<()>;

    /// Remove a dynamically-added route. Returns `true` if the route existed.
    ///
    /// Config-defined routes cannot be removed.
    fn remove_route(&self, name: &str) -> Result<bool>;

    /// List all dynamically-added routes.
    fn list_dynamic_routes(&self) -> Vec<DynamicRoute>;
}

// ── Tool admin ──────────────────────────────────────────────────────

/// Allow/deny filter applied to an upstream tool server.
///
/// When both `allow` and `deny` are set, deny takes precedence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolFilter {
    /// If set, only tools whose un-namespaced name appears in this list are visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Vec<String>>,
    /// Tools whose un-namespaced name appears in this list are hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Vec<String>>,
}

impl ToolFilter {
    /// Returns `true` if `tool_name` (un-namespaced) passes this filter.
    pub fn accepts(&self, tool_name: &str) -> bool {
        if let Some(deny) = &self.deny
            && deny.iter().any(|d| d == tool_name)
        {
            return false;
        }
        if let Some(allow) = &self.allow {
            return allow.iter().any(|a| a == tool_name);
        }
        true
    }
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

/// Per-server parameter restrictions applied before forwarding tool calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParamRestrictions {
    /// Per-tool parameter rules. Keys are un-namespaced tool names.
    #[serde(default)]
    pub rules: HashMap<String, ParamRule>,
}

impl ParamRestrictions {
    /// Validate and optionally mutate tool call arguments.
    ///
    /// Returns `Ok(())` if allowed (possibly with stripped params).
    /// Returns `Err` if a parameter is denied and action is `Reject`.
    pub fn check(
        &self,
        tool_name: &str,
        arguments: &mut Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Result<()> {
        let Some(rule) = self.rules.get(tool_name) else {
            return Ok(());
        };
        let Some(args) = arguments.as_mut() else {
            return Ok(());
        };

        // Deny list takes precedence.
        if let Some(deny) = &rule.deny {
            let denied: Vec<String> = args
                .keys()
                .filter(|k| deny.iter().any(|d| d == *k))
                .cloned()
                .collect();
            for key in &denied {
                match rule.action {
                    ParamViolationAction::Reject => {
                        return Err(crate::errors::BitrouterError::invalid_request(
                            None,
                            format!("parameter '{key}' denied on tool '{tool_name}'"),
                            None,
                        ));
                    }
                    ParamViolationAction::Strip => {
                        args.remove(key);
                    }
                }
            }
        }

        // Allow list: reject/strip any key NOT in the list.
        if let Some(allow) = &rule.allow {
            let disallowed: Vec<String> = args
                .keys()
                .filter(|k| !allow.iter().any(|a| a == *k))
                .cloned()
                .collect();
            for key in &disallowed {
                match rule.action {
                    ParamViolationAction::Reject => {
                        return Err(crate::errors::BitrouterError::invalid_request(
                            None,
                            format!("parameter '{key}' denied on tool '{tool_name}'"),
                            None,
                        ));
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

/// Metadata about a connected upstream tool server.
#[derive(Debug, Clone, Serialize)]
pub struct ToolUpstreamEntry {
    /// Server name.
    pub name: String,
    /// Number of tools currently visible (after filtering).
    pub tool_count: usize,
    /// Active tool filter, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<ToolFilter>,
    /// Active parameter restrictions, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param_restrictions: Option<ParamRestrictions>,
}

/// Admin interface for managing tool registries at runtime.
///
/// Parallel to [`AdminRoutingTable`] for models. Extends [`ToolRegistry`]
/// with methods for inspecting upstreams and updating filters and parameter
/// restrictions without requiring config rewrites or daemon restarts.
pub trait AdminToolRegistry: ToolRegistry {
    /// List all upstream tool servers with their current state.
    fn list_upstreams(&self) -> impl Future<Output = Vec<ToolUpstreamEntry>> + Send;
    /// Update the tool filter for a specific upstream server.
    fn update_filter(
        &self,
        server: &str,
        filter: Option<ToolFilter>,
    ) -> impl Future<Output = Result<()>> + Send;
    /// Update parameter restrictions for a specific upstream server.
    fn update_param_restrictions(
        &self,
        server: &str,
        restrictions: ParamRestrictions,
    ) -> impl Future<Output = Result<()>> + Send;
}
