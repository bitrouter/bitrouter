use std::sync::{Arc, RwLock};

use bitrouter_core::{
    agents::provider::DynAgentProvider, errors::Result, routers::router::AgentRouter,
};

use crate::agent_engine::AgentGuardrail;

/// An [`AgentRouter`] wrapper that applies guardrail policy to every agent
/// provider returned by the inner router.
///
/// Parallel to [`GuardedToolRouter`](crate::GuardedToolRouter) for tools.
///
/// The guardrail is held behind a `RwLock<Arc<..>>` so it can be swapped
/// atomically during hot-reload without dropping in-flight requests.
///
/// Agent guardrail inspection is not yet implemented — this wrapper
/// currently always passes through. The structure exists so that future
/// inspection logic can be added without changing callers.
pub struct GuardedAgentRouter<R> {
    inner: R,
    /// Retained for future inspection logic and hot-reload support.
    guardrail: Arc<RwLock<Arc<AgentGuardrail>>>,
}

impl<R> GuardedAgentRouter<R> {
    /// Wrap an existing agent router with guardrail enforcement.
    pub fn new(inner: R, guardrail: Arc<AgentGuardrail>) -> Self {
        Self {
            inner,
            guardrail: Arc::new(RwLock::new(guardrail)),
        }
    }

    /// Wrap an existing agent router with a shared guardrail lock for
    /// hot-reload support. The reload closure can write to the same lock.
    pub fn with_shared_guardrail(inner: R, guardrail: Arc<RwLock<Arc<AgentGuardrail>>>) -> Self {
        Self { inner, guardrail }
    }

    /// Snapshot the current guardrail `Arc` without holding the lock.
    ///
    /// Not yet used — will be called from `route_agent` when inspection
    /// logic is added.
    pub fn guardrail_snapshot(&self) -> Arc<AgentGuardrail> {
        self.guardrail
            .read()
            .map(|g| Arc::clone(&g))
            .unwrap_or_else(|poisoned| Arc::clone(&poisoned.into_inner()))
    }
}

impl<R> AgentRouter for GuardedAgentRouter<R>
where
    R: std::ops::Deref + Send + Sync,
    R::Target: AgentRouter + Send + Sync,
{
    async fn route_agent(&self, agent_name: &str) -> Result<Box<DynAgentProvider<'static>>> {
        let provider = self.inner.route_agent(agent_name).await?;

        // Agent guardrail inspection is not yet implemented; always pass through.
        // When inspection logic is added, check `self.guardrail_snapshot().is_disabled()`
        // and wrap the provider accordingly.
        Ok(provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_config::AgentGuardrailConfig;
    use bitrouter_core::agents::event::AgentEvent;
    use bitrouter_core::agents::provider::AgentProvider;
    use bitrouter_core::agents::session::{AgentCapabilities, AgentSessionInfo};
    use bitrouter_core::errors::BitrouterError;

    struct MockAgentProvider;

    impl AgentProvider for MockAgentProvider {
        fn agent_name(&self) -> &str {
            "mock"
        }

        fn protocol_name(&self) -> &str {
            "test"
        }

        async fn connect(&self) -> Result<AgentSessionInfo> {
            Ok(AgentSessionInfo {
                session_id: "s1".to_owned(),
                agent_name: "mock".to_owned(),
                capabilities: AgentCapabilities {
                    supports_permissions: false,
                    supports_thinking: false,
                },
            })
        }

        async fn submit(
            &self,
            _session_id: &str,
            _text: String,
        ) -> Result<tokio::sync::mpsc::Receiver<AgentEvent>> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }

        async fn respond_permission(
            &self,
            _session_id: &str,
            _request_id: bitrouter_core::agents::event::PermissionRequestId,
            _response: bitrouter_core::agents::event::PermissionResponse,
        ) -> Result<()> {
            Ok(())
        }

        async fn disconnect(&self, _session_id: &str) -> Result<()> {
            Ok(())
        }
    }

    struct MockAgentRouter;

    impl AgentRouter for MockAgentRouter {
        async fn route_agent(&self, agent_name: &str) -> Result<Box<DynAgentProvider<'static>>> {
            if agent_name == "mock" {
                Ok(DynAgentProvider::new_box(MockAgentProvider))
            } else {
                Err(BitrouterError::invalid_request(
                    None,
                    format!("unknown agent: {agent_name}"),
                    None,
                ))
            }
        }
    }

    #[tokio::test]
    async fn passes_through_with_disabled_guardrail() {
        let config = AgentGuardrailConfig { enabled: false };
        let guardrail = Arc::new(AgentGuardrail::new(config));
        assert!(guardrail.is_disabled());

        let router = GuardedAgentRouter::new(&MockAgentRouter, guardrail);
        let provider = router.route_agent("mock").await;
        assert!(provider.is_ok());
        assert_eq!(provider.as_ref().ok().map(|p| p.agent_name()), Some("mock"));
    }

    #[tokio::test]
    async fn passes_through_with_enabled_guardrail() {
        let config = AgentGuardrailConfig { enabled: true };
        let guardrail = Arc::new(AgentGuardrail::new(config));
        assert!(!guardrail.is_disabled());

        let router = GuardedAgentRouter::new(&MockAgentRouter, guardrail);
        let provider = router.route_agent("mock").await;
        assert!(provider.is_ok());
        assert_eq!(provider.as_ref().ok().map(|p| p.agent_name()), Some("mock"));
    }

    #[tokio::test]
    async fn guardrail_snapshot_returns_current() {
        let config = AgentGuardrailConfig { enabled: false };
        let router =
            GuardedAgentRouter::new(&MockAgentRouter, Arc::new(AgentGuardrail::new(config)));

        let snapshot = router.guardrail_snapshot();
        assert!(snapshot.is_disabled());
    }

    #[tokio::test]
    async fn unknown_agent_propagates_error() {
        let config = AgentGuardrailConfig::default();
        let router =
            GuardedAgentRouter::new(&MockAgentRouter, Arc::new(AgentGuardrail::new(config)));

        assert!(router.route_agent("unknown").await.is_err());
    }
}
