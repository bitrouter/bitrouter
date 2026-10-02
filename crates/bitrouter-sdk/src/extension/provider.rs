//! Typed provider-owned implementation of the `evaluate` operation.
//!
//! A provider declares executable models and its upstream wire semantics.
//! The host still selects accounts, applies authentication, performs HTTP I/O,
//! retries, cancellation, and terminal metering.

use crate::error::Result;
use std::collections::BTreeMap;

use http::{HeaderMap, Method};

use crate::evaluation::{
    EvaluationAnswer, EvaluationQuestionType, EvaluationRequest, EvaluationRoutingTarget,
    EvaluationUsage,
};

/// Provider-reported evaluation data, before the host attaches route identity
/// and settles cost.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluationProviderOutput {
    /// Actual upstream model version.
    pub model: String,
    /// Answers keyed by the caller's question ids.
    pub answers: BTreeMap<String, EvaluationAnswer>,
    /// Provider-reported usage; cost remains host-owned.
    pub usage: EvaluationUsage,
}

/// Provider-owned HTTP request details that do not grant transport authority.
/// The host still fixes the origin, endpoint, authentication and deadlines.
#[derive(Debug, Clone)]
pub struct EvaluationProviderWireRequest {
    /// HTTP method for the provider's evaluation endpoint.
    pub method: Method,
    /// Non-authentication headers required by the provider dialect.
    pub headers: HeaderMap,
    /// Provider-specific JSON body.
    pub body: serde_json::Value,
}

impl EvaluationProviderOutput {
    /// Check provider-independent answer invariants before the host accepts it.
    pub fn validate_against(&self, request: &EvaluationRequest) -> Result<()> {
        if self.model.is_empty() {
            return Err(crate::error::BitrouterError::UpstreamInvalidResponse {
                message: "evaluation model is missing".to_owned(),
            });
        }
        crate::evaluation::validate_answers_and_usage(&self.answers, &self.usage, request)
    }
}

/// One executable evaluation model claimed by a provider extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationProviderModel {
    /// Canonical BitRouter model id, namespaced to the provider id.
    pub id: String,
    /// Model id sent to the upstream provider.
    pub provider_model_id: String,
    /// Positively supported question kinds.
    pub question_types: Vec<EvaluationQuestionType>,
    /// Provider's maximum Choice option count, when known.
    pub max_choice_options: Option<usize>,
    /// Provider's maximum Score level count, when known.
    pub max_score_levels: Option<usize>,
}

/// Host-readable execution authority for one compiled provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationProviderDescriptor {
    /// Stable provider id used by route selectors and account configuration.
    pub provider_id: String,
    /// Default upstream origin; operators may override it in configuration.
    pub api_base: String,
    /// Environment variable from which the host obtains the bearer token.
    pub credential_env: String,
    /// Relative upstream path, constrained to the configured origin.
    pub endpoint: String,
    /// Exact model and operation claims executable by this extension.
    pub models: Vec<EvaluationProviderModel>,
}

/// Provider-owned evaluation dialect, with host-governed transport and policy.
///
/// Only typed canonical evaluation data crosses this boundary. JSON is the
/// provider's private upstream representation, not a generic operation hook.
pub trait EvaluationProvider: Send + Sync {
    /// Describe this provider's executable models and upstream requirements.
    fn descriptor(&self) -> EvaluationProviderDescriptor;

    /// Render one selected model's canonical request into constrained HTTP
    /// details. Credentials, origin and endpoint are never passed here.
    fn render_request(
        &self,
        request: &EvaluationRequest,
        target: &EvaluationRoutingTarget,
    ) -> Result<EvaluationProviderWireRequest>;

    /// Parse one successful upstream JSON response into canonical answers.
    fn parse_response(
        &self,
        body: serde_json::Value,
        request: &EvaluationRequest,
    ) -> Result<EvaluationProviderOutput>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        EvaluationProvider, EvaluationProviderDescriptor, EvaluationProviderModel,
        EvaluationProviderOutput, EvaluationProviderWireRequest,
    };
    use crate::error::{BitrouterError, Result};
    use crate::evaluation::{EvaluationQuestionType, EvaluationRequest, EvaluationRoutingTarget};
    use crate::extension::ExtensionApi;

    struct FixtureProvider {
        provider_id: &'static str,
        model_id: &'static str,
    }

    impl EvaluationProvider for FixtureProvider {
        fn descriptor(&self) -> EvaluationProviderDescriptor {
            EvaluationProviderDescriptor {
                provider_id: self.provider_id.into(),
                api_base: "https://provider.example".into(),
                credential_env: "PROVIDER_API_KEY".into(),
                endpoint: "/v1/evaluate".into(),
                models: vec![EvaluationProviderModel {
                    id: self.model_id.into(),
                    provider_model_id: "wire-model".into(),
                    question_types: vec![EvaluationQuestionType::Noul],
                    max_choice_options: None,
                    max_score_levels: None,
                }],
            }
        }

        fn render_request(
            &self,
            _request: &EvaluationRequest,
            _target: &EvaluationRoutingTarget,
        ) -> Result<EvaluationProviderWireRequest> {
            Err(BitrouterError::internal("fixture does not execute"))
        }

        fn parse_response(
            &self,
            _body: serde_json::Value,
            _request: &EvaluationRequest,
        ) -> Result<EvaluationProviderOutput> {
            Err(BitrouterError::internal("fixture does not execute"))
        }
    }

    #[test]
    fn registration_is_provider_scoped_and_ignored_duplicate_poisoned() -> Result<()> {
        let mut api = ExtensionApi::new();
        api.register_evaluation_provider(Arc::new(FixtureProvider {
            provider_id: "fixture",
            model_id: "fixture/model",
        }))?;
        assert_eq!(api.evaluation_provider_descriptors().len(), 1);
        assert!(api.evaluation_provider("fixture").is_ok());
        let _ignored = api.register_evaluation_provider(Arc::new(FixtureProvider {
            provider_id: "fixture",
            model_id: "fixture/other",
        }));
        assert!(api.into_registrations().is_err());
        Ok(())
    }

    #[test]
    fn foreign_namespace_cannot_claim_a_model() {
        let mut api = ExtensionApi::new();
        assert!(
            api.register_evaluation_provider(Arc::new(FixtureProvider {
                provider_id: "fixture",
                model_id: "other/model",
            }))
            .is_err()
        );
        assert!(api.into_registrations().is_err());
    }
}
