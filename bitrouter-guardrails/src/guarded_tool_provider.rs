//! Tool provider wrapper that enforces parameter restrictions at call time.
//!
//! [`GuardedToolProvider`] mirrors [`GuardedModel`](crate::guarded_model::GuardedModel)
//! for models — it wraps a single [`DynToolProvider`] and applies policy
//! enforcement from the [`ToolGuardrail`] engine before forwarding calls.

use std::sync::Arc;

use bitrouter_core::errors::Result;
use bitrouter_core::tools::provider::DynToolProvider;
use bitrouter_core::tools::provider::ToolProvider;
use bitrouter_core::tools::result::ToolCallResult;

use crate::tool_engine::ToolGuardrail;

/// A tool provider wrapper that enforces parameter restrictions from the
/// [`ToolGuardrail`] engine before forwarding calls to the inner provider.
///
/// Parallel to [`GuardedModel`](crate::guarded_model::GuardedModel).
pub(crate) struct GuardedToolProvider {
    pub(crate) inner: Box<DynToolProvider<'static>>,
    pub(crate) guardrail: Arc<ToolGuardrail>,
}

impl ToolProvider for GuardedToolProvider {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        mut arguments: serde_json::Value,
    ) -> Result<ToolCallResult> {
        let server = self.inner.provider_name();
        self.guardrail
            .check_params(server, tool_id, &mut arguments)?;
        self.inner.call_tool(tool_id, arguments).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_config::{ToolGuardrailConfig, ToolProviderPolicy};
    use bitrouter_core::routers::admin::{ParamRestrictions, ParamRule, ParamViolationAction};
    use bitrouter_core::tools::result::ToolContent;
    use std::collections::HashMap;

    struct MockToolProvider {
        name: String,
    }

    impl ToolProvider for MockToolProvider {
        fn provider_name(&self) -> &str {
            &self.name
        }

        async fn call_tool(
            &self,
            _tool_id: &str,
            _arguments: serde_json::Value,
        ) -> Result<ToolCallResult> {
            Ok(ToolCallResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_owned(),
                }],
                is_error: false,
                metadata: None,
            })
        }
    }

    fn guarded_provider(name: &str, restrictions: ParamRestrictions) -> GuardedToolProvider {
        let mut providers = HashMap::new();
        providers.insert(
            name.to_owned(),
            ToolProviderPolicy {
                filter: None,
                param_restrictions: Some(restrictions),
            },
        );
        let config = ToolGuardrailConfig {
            enabled: true,
            providers,
        };
        GuardedToolProvider {
            inner: DynToolProvider::new_box(MockToolProvider {
                name: name.to_owned(),
            }),
            guardrail: Arc::new(ToolGuardrail::new(config)),
        }
    }

    #[tokio::test]
    async fn call_passes_when_no_restriction() {
        let config = ToolGuardrailConfig::default();
        let provider = GuardedToolProvider {
            inner: DynToolProvider::new_box(MockToolProvider {
                name: "github".to_owned(),
            }),
            guardrail: Arc::new(ToolGuardrail::new(config)),
        };

        let result = provider
            .call_tool("search", serde_json::json!({"query": "test"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn call_rejected_when_param_denied() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let provider = guarded_provider("github", restrictions);

        let result = provider
            .call_tool(
                "search",
                serde_json::json!({"query": "test", "force": true}),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn call_strips_denied_param_and_proceeds() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Strip,
                },
            )]),
        };
        let provider = guarded_provider("github", restrictions);

        let result = provider
            .call_tool(
                "search",
                serde_json::json!({"query": "test", "force": true}),
            )
            .await;
        assert!(result.is_ok());
    }
}
