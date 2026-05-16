//! Fallback policy for chain-shaped model routing.

use bitrouter_core::{errors::BitrouterError, routers::routing_table::RoutingTarget};

/// Decision returned by a [`FallbackPolicy`] after an attempt fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackDecision {
    /// Try the next target in the chain.
    Fallback,
    /// Stop and surface the error to the client.
    Stop,
}

/// Stateless policy that decides whether a model error should advance to the
/// next target in a routing chain.
///
/// Streaming fallback is only evaluated before the SSE response is committed:
/// when `model.stream(...).await` returns `Err`. Once it returns `Ok(stream)`,
/// stream item errors are reported for the already-executing target and never
/// replayed on a later target.
pub trait FallbackPolicy: Send + Sync {
    /// Classifies `err` for the target that just failed.
    fn classify(&self, err: &BitrouterError, attempted: &RoutingTarget) -> FallbackDecision;
}

/// Default retryability policy for API filters.
#[derive(Debug, Default)]
pub struct DefaultFallbackPolicy;

impl FallbackPolicy for DefaultFallbackPolicy {
    fn classify(&self, err: &BitrouterError, _attempted: &RoutingTarget) -> FallbackDecision {
        match err {
            BitrouterError::InvalidRequest { .. }
            | BitrouterError::AccessDenied { .. }
            | BitrouterError::Cancelled { .. } => FallbackDecision::Stop,
            BitrouterError::Provider { context, .. } => match context.status_code {
                Some(408 | 429) => FallbackDecision::Fallback,
                Some(status) if (500..=599).contains(&status) => FallbackDecision::Fallback,
                Some(status) if (400..=499).contains(&status) => FallbackDecision::Stop,
                _ => FallbackDecision::Fallback,
            },
            BitrouterError::Transport { .. } => FallbackDecision::Fallback,
            _ => FallbackDecision::Stop,
        }
    }
}

pub fn default_fallback_policy() -> std::sync::Arc<dyn FallbackPolicy> {
    std::sync::Arc::new(DefaultFallbackPolicy)
}

#[cfg(test)]
mod tests {
    use bitrouter_core::{
        errors::ProviderErrorContext,
        routers::routing_table::{ApiProtocol, BillingMode},
    };

    use super::*;

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "openai".to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_protocol: ApiProtocol::Openai,
            api_key_override: None,
            api_base_override: None,
            preset: None,
            billing_mode: BillingMode::default(),
        }
    }

    fn provider_error(status_code: u16) -> BitrouterError {
        BitrouterError::provider_error(
            "openai",
            "upstream failed",
            ProviderErrorContext {
                status_code: Some(status_code),
                error_type: None,
                code: None,
                param: None,
                request_id: None,
                body: None,
            },
        )
    }

    #[test]
    fn default_policy_falls_back_on_retryable_errors() {
        let policy = DefaultFallbackPolicy;
        let target = target();

        assert_eq!(
            policy.classify(&provider_error(500), &target),
            FallbackDecision::Fallback
        );
        assert_eq!(
            policy.classify(&provider_error(429), &target),
            FallbackDecision::Fallback
        );
        assert_eq!(
            policy.classify(
                &BitrouterError::transport(Some("openai"), "timeout"),
                &target
            ),
            FallbackDecision::Fallback
        );
    }

    #[test]
    fn default_policy_stops_on_non_retryable_errors() {
        let policy = DefaultFallbackPolicy;
        let target = target();

        assert_eq!(
            policy.classify(&provider_error(401), &target),
            FallbackDecision::Stop
        );
        assert_eq!(
            policy.classify(
                &BitrouterError::invalid_request(None, "bad request", None),
                &target
            ),
            FallbackDecision::Stop
        );
        assert_eq!(
            policy.classify(
                &BitrouterError::AccessDenied {
                    message: "denied".to_owned()
                },
                &target
            ),
            FallbackDecision::Stop
        );
    }
}
