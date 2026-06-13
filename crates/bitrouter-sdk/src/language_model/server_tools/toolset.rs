//! The [`RouterToolset`] executor seam and a [`ToolsetRegistry`] that composes
//! several toolsets and resolves an intercepted tool call to its owner.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::Result;
use crate::language_model::context::PipelineContext;
use crate::language_model::types::{Tool, ToolResultOutput};
use crate::plugin::PluginId;

/// An owned, cheap-to-clone snapshot of the per-request context handed to a
/// [`RouterToolset`]. Carries the [`CallerContext`] and the request's
/// plugin-scoped metadata map — where a consumer stashes request-scoped state
/// such as the resolved principal or a per-request tool declaration.
///
/// Owned rather than a `&PipelineContext` because a server-tool execution can
/// outlive the borrow: the streaming stitcher moves it into a `'static` stream.
#[derive(Debug, Clone)]
pub struct ToolContext {
    caller: CallerContext,
    metadata: HashMap<PluginId, serde_json::Value>,
}

impl ToolContext {
    /// Build a context from explicit parts (consumers, tests).
    pub fn new(caller: CallerContext, metadata: HashMap<PluginId, serde_json::Value>) -> Self {
        Self { caller, metadata }
    }

    /// Snapshot the live pipeline context for a server-tool execution.
    pub fn from_pipeline(ctx: &PipelineContext) -> Self {
        Self {
            caller: ctx.caller().clone(),
            metadata: ctx.metadata().clone(),
        }
    }

    /// The authenticated caller.
    pub fn caller(&self) -> &CallerContext {
        &self.caller
    }

    /// Read a plugin's request-scoped metadata blob (e.g. a resolved principal,
    /// or a per-request tool declaration stashed by a pre-request hook).
    pub fn get_metadata(&self, plugin_id: &PluginId) -> Option<&serde_json::Value> {
        self.metadata.get(plugin_id)
    }
}

/// A set of tools that BitRouter advertises to the model and executes itself.
///
/// Provider/transport-agnostic: an implementation may be backed by MCP, an
/// in-process registry, or anything else. Tool names are provider-namespaced
/// (prefixed) by the implementation so they cannot collide with the caller's
/// own tools.
#[async_trait]
pub trait RouterToolset: Send + Sync {
    /// Tools to advertise on this request, as canonical IR function tools. `ctx`
    /// exposes the caller and the request's metadata, so an implementation may
    /// advertise tools conditionally on the request.
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>>;

    /// Execute one model-issued call this set owns. `arguments` is the raw
    /// JSON string carried on the model's `Content::ToolCall`; `ctx` carries the
    /// caller and request metadata.
    async fn call_tool(
        &self,
        name: &str,
        arguments: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResultOutput>;

    /// Whether this set owns `name`.
    fn owns(&self, name: &str) -> bool;

    /// The MCP server backing this set, when applicable. Lets the loop label a
    /// router tool call with its server so a framing encoder can reproduce the
    /// native `mcp_tool_use` block. Default `None` (e.g. an in-process set).
    fn server_name(&self) -> Option<&str> {
        None
    }
}

/// Composes several [`RouterToolset`]s: aggregates their advertised tools and
/// routes an intercepted call to the owning set.
pub struct ToolsetRegistry {
    sets: Vec<Arc<dyn RouterToolset>>,
}

impl ToolsetRegistry {
    /// Build a registry over `sets`.
    pub fn new(sets: Vec<Arc<dyn RouterToolset>>) -> Self {
        Self { sets }
    }

    /// Every set's advertised tools, paired with the set of router-owned tool
    /// names (used by the loop to classify which calls it must execute).
    pub async fn list_all(&self, ctx: &ToolContext) -> Result<(Vec<Tool>, BTreeSet<String>)> {
        let mut tools = Vec::new();
        let mut owned = BTreeSet::new();
        for set in &self.sets {
            for tool in set.list_tools(ctx).await? {
                owned.insert(tool.name().to_string());
                tools.push(tool);
            }
        }
        Ok((tools, owned))
    }

    /// The set that owns `name`, if any.
    pub fn resolve(&self, name: &str) -> Option<&Arc<dyn RouterToolset>> {
        self.sets.iter().find(|set| set.owns(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::ProviderMetadata;

    struct MockToolset {
        tools: Vec<Tool>,
    }

    #[async_trait]
    impl RouterToolset for MockToolset {
        async fn list_tools(&self, _ctx: &ToolContext) -> Result<Vec<Tool>> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            name: &str,
            _arguments: &str,
            _ctx: &ToolContext,
        ) -> Result<ToolResultOutput> {
            Ok(ToolResultOutput::Text {
                value: format!("ran {name}"),
            })
        }
        fn owns(&self, name: &str) -> bool {
            self.tools.iter().any(|t| t.name() == name)
        }
    }

    fn func(name: &str) -> Tool {
        Tool::Function {
            name: name.to_string(),
            description: None,
            parameters: serde_json::json!({}),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    #[tokio::test]
    async fn registry_lists_tools_and_resolves_owner() {
        let mock = Arc::new(MockToolset {
            tools: vec![func("search"), func("fetch")],
        });
        let reg = ToolsetRegistry::new(vec![mock]);
        let ctx = ToolContext::new(CallerContext::local(), Default::default());
        let (tools, owned) = reg.list_all(&ctx).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert!(owned.contains("search"));
        assert!(owned.contains("fetch"));
        assert!(reg.resolve("search").is_some());
        assert!(reg.resolve("missing").is_none());
    }
}
