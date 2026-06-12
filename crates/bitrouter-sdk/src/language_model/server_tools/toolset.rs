//! The [`RouterToolset`] executor seam and a [`ToolsetRegistry`] that composes
//! several toolsets and resolves an intercepted tool call to its owner.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::error::Result;
use crate::language_model::types::{Tool, ToolResultOutput};

/// A set of tools that BitRouter advertises to the model and executes itself.
///
/// Provider/transport-agnostic: an implementation may be backed by MCP, an
/// in-process registry, or anything else. Tool names are provider-namespaced
/// (prefixed) by the implementation so they cannot collide with the caller's
/// own tools.
#[async_trait]
pub trait RouterToolset: Send + Sync {
    /// Tools to advertise on this request, as canonical IR function tools.
    async fn list_tools(&self, caller: &CallerContext) -> Result<Vec<Tool>>;

    /// Execute one model-issued call this set owns. `arguments` is the raw
    /// JSON string carried on the model's `Content::ToolCall`.
    async fn call_tool(
        &self,
        name: &str,
        arguments: &str,
        caller: &CallerContext,
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
    pub async fn list_all(&self, caller: &CallerContext) -> Result<(Vec<Tool>, BTreeSet<String>)> {
        let mut tools = Vec::new();
        let mut owned = BTreeSet::new();
        for set in &self.sets {
            for tool in set.list_tools(caller).await? {
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
        async fn list_tools(&self, _caller: &CallerContext) -> Result<Vec<Tool>> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            name: &str,
            _arguments: &str,
            _caller: &CallerContext,
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
        let (tools, owned) = reg.list_all(&CallerContext::local()).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert!(owned.contains("search"));
        assert!(owned.contains("fetch"));
        assert!(reg.resolve("search").is_some());
        assert!(reg.resolve("missing").is_none());
    }
}
