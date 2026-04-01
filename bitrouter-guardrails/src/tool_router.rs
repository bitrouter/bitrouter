use std::sync::{Arc, RwLock};

use bitrouter_core::{
    errors::Result,
    routers::{router::ToolRouter, routing_table::RoutingTarget},
    tools::provider::{DynToolProvider, ToolProvider},
};

use crate::guarded_tool_provider::GuardedToolProvider;
use crate::tool_engine::ToolGuardrail;

/// A [`ToolRouter`] wrapper that applies policy enforcement to every tool
/// provider returned by the inner router.
///
/// Parallel to [`GuardedRouter`](crate::router::GuardedRouter) for models.
///
/// The guardrail is held behind a `RwLock<Arc<..>>` so it can be swapped
/// atomically during hot-reload without dropping in-flight requests.
///
/// When the tool guardrail has no restrictions for a provider, the wrapper
/// is a zero-cost pass-through — it returns the inner provider unchanged.
pub struct GuardedToolRouter<R> {
    inner: R,
    guardrail: Arc<RwLock<Arc<ToolGuardrail>>>,
}

impl<R> GuardedToolRouter<R> {
    /// Wrap an existing tool router with guardrail enforcement.
    pub fn new(inner: R, guardrail: Arc<ToolGuardrail>) -> Self {
        Self {
            inner,
            guardrail: Arc::new(RwLock::new(guardrail)),
        }
    }

    /// Wrap an existing tool router with a shared guardrail lock for
    /// hot-reload support. The reload closure can write to the same lock.
    pub fn with_shared_guardrail(inner: R, guardrail: Arc<RwLock<Arc<ToolGuardrail>>>) -> Self {
        Self { inner, guardrail }
    }

    /// Snapshot the current guardrail `Arc` without holding the lock.
    fn read_guardrail(&self) -> Arc<ToolGuardrail> {
        self.guardrail
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }
}

impl<R> ToolRouter for GuardedToolRouter<R>
where
    R: std::ops::Deref + Send + Sync,
    R::Target: ToolRouter + Send + Sync,
{
    async fn route_tool(&self, target: RoutingTarget) -> Result<Box<DynToolProvider<'static>>> {
        let provider = self.inner.route_tool(target).await?;
        let guardrail = self.read_guardrail();

        if guardrail.is_disabled() {
            return Ok(provider);
        }

        // Skip wrapping when no restrictions apply to this provider.
        if !guardrail.has_restrictions_for(provider.provider_name()) {
            return Ok(provider);
        }

        Ok(DynToolProvider::new_box(GuardedToolProvider {
            inner: provider,
            guardrail,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_config::{ToolGuardrailConfig, ToolProviderPolicy};
    use bitrouter_core::routers::admin::{ParamRestrictions, ParamRule, ParamViolationAction};
    use bitrouter_core::tools::provider::ToolProvider;
    use bitrouter_core::tools::result::{ToolCallResult, ToolContent};
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

    struct MockToolRouter;

    impl ToolRouter for MockToolRouter {
        async fn route_tool(
            &self,
            _target: RoutingTarget,
        ) -> Result<Box<DynToolProvider<'static>>> {
            Ok(DynToolProvider::new_box(MockToolProvider {
                name: "github".to_owned(),
            }))
        }
    }

    fn test_target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "github".to_owned(),
            service_id: "search".to_owned(),
            api_protocol: bitrouter_core::routers::routing_table::ApiProtocol::Rest,
        }
    }

    #[tokio::test]
    async fn disabled_guardrail_returns_unwrapped_provider() {
        let config = ToolGuardrailConfig {
            enabled: false,
            ..Default::default()
        };
        let router = GuardedToolRouter::new(&MockToolRouter, Arc::new(ToolGuardrail::new(config)));

        let provider = router.route_tool(test_target()).await.unwrap();
        assert_eq!(provider.provider_name(), "github");
        // Should succeed without any restriction check
        let result = provider
            .call_tool("search", serde_json::json!({"force": true}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn no_restrictions_returns_unwrapped_provider() {
        let config = ToolGuardrailConfig::default();
        let router = GuardedToolRouter::new(&MockToolRouter, Arc::new(ToolGuardrail::new(config)));

        let provider = router.route_tool(test_target()).await.unwrap();
        let result = provider
            .call_tool("search", serde_json::json!({"force": true}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn restrictions_enforced_on_wrapped_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "github".to_owned(),
            ToolProviderPolicy {
                filter: None,
                param_restrictions: Some(ParamRestrictions {
                    rules: HashMap::from([(
                        "search".to_owned(),
                        ParamRule {
                            deny: Some(vec!["force".to_owned()]),
                            allow: None,
                            action: ParamViolationAction::Reject,
                        },
                    )]),
                }),
            },
        );
        let config = ToolGuardrailConfig {
            enabled: true,
            providers,
        };
        let router = GuardedToolRouter::new(&MockToolRouter, Arc::new(ToolGuardrail::new(config)));

        let provider = router.route_tool(test_target()).await.unwrap();
        let result = provider
            .call_tool("search", serde_json::json!({"force": true}))
            .await;
        assert!(result.is_err());

        // Non-denied params should pass
        let provider = router.route_tool(test_target()).await.unwrap();
        let result = provider
            .call_tool("search", serde_json::json!({"query": "test"}))
            .await;
        assert!(result.is_ok());
    }
}
