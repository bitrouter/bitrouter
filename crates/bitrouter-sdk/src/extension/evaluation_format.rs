//! Typed JSON wire-format facet for the `evaluate` operation.
//!
//! A host registers this facet through [`crate::extension::ExtensionApi`]. The facet owns
//! only request rendering and response parsing, not transport or credentials.

use crate::error::Result;
use crate::evaluation::{EvaluationRequest, EvaluationResult, EvaluationRoutingTarget};

/// Exact compatibility identity of one evaluation JSON format facet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationFormatDescriptor {
    /// Stable wire-format extension id, independent of provider identity.
    pub extension_id: String,
    /// Adapter id scoped to the extension package.
    pub adapter_id: String,
    /// Exact format-contract revision.
    pub revision: u32,
}

/// Converts typed evaluation data to and from one upstream JSON dialect.
///
/// The adapter receives no URL, credentials, HTTP client, or mutable host
/// context. The host owns transport, authentication, deadlines, and evidence.
pub trait EvaluationFormatAdapter: Send + Sync {
    /// Describe the facet used by provider configuration to bind it.
    fn descriptor(&self) -> EvaluationFormatDescriptor;

    /// Render the canonical request for a selected provider model.
    fn render_request(
        &self,
        request: &EvaluationRequest,
        target: &EvaluationRoutingTarget,
    ) -> Result<serde_json::Value>;

    /// Parse a successful upstream response to the canonical answer shape.
    fn parse_response(
        &self,
        body: serde_json::Value,
        request: &EvaluationRequest,
    ) -> Result<EvaluationResult>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{EvaluationFormatAdapter, EvaluationFormatDescriptor};
    use crate::config::{Config, ProviderConfig};
    use crate::error::Result;
    use crate::evaluation::{EvaluationRequest, EvaluationResult, EvaluationRoutingTarget};
    use crate::extension::ExtensionApi;

    struct TestAdapter {
        revision: u32,
    }

    impl EvaluationFormatAdapter for TestAdapter {
        fn descriptor(&self) -> EvaluationFormatDescriptor {
            EvaluationFormatDescriptor {
                extension_id: "fixture".into(),
                adapter_id: "decisions".into(),
                revision: self.revision,
            }
        }

        fn render_request(
            &self,
            _request: &EvaluationRequest,
            _target: &EvaluationRoutingTarget,
        ) -> Result<serde_json::Value> {
            Ok(json!({}))
        }

        fn parse_response(
            &self,
            body: serde_json::Value,
            _request: &EvaluationRequest,
        ) -> Result<EvaluationResult> {
            serde_json::from_value(body)
                .map_err(|error| crate::error::BitrouterError::bad_request(error.to_string()))
        }
    }

    fn config(active: bool) -> std::result::Result<Config, serde_json::Error> {
        let mut config = Config::default();
        let provider: ProviderConfig = serde_json::from_value(json!({
            "active": active,
            "api_base": "https://example.test",
            "operations": {
                "evaluate": {
                    "endpoint": "/v1/evaluate",
                    "format": {
                        "extension": "fixture",
                        "adapter": "decisions",
                        "revision": 1
                    }
                }
            },
            "models": [{
                "id": "fixture/model",
                "operations": {
                    "evaluate": {"question_types": ["noul"]}
                }
            }]
        }))?;
        config.providers.insert("fixture".into(), provider);
        Ok(config)
    }

    #[test]
    fn registration_and_binding_are_exact_and_inactive_registration_is_valid()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut extensions = ExtensionApi::new();
        extensions.register_evaluation_format(Arc::new(TestAdapter { revision: 1 }))?;
        extensions.validate_evaluation_bindings(&Config::default())?;
        extensions.validate_evaluation_bindings(&config(true)?)?;
        let duplicate =
            extensions.register_evaluation_format(Arc::new(TestAdapter { revision: 2 }));
        assert!(duplicate.is_err());
        Ok(())
    }

    #[test]
    fn missing_or_wrong_revision_fails_only_for_active_bindings()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let active_config = config(true)?;
        assert!(
            ExtensionApi::new()
                .validate_evaluation_bindings(&active_config)
                .is_err()
        );
        let mut extensions = ExtensionApi::new();
        extensions.register_evaluation_format(Arc::new(TestAdapter { revision: 2 }))?;
        assert!(
            extensions
                .validate_evaluation_bindings(&active_config)
                .is_err()
        );
        extensions.validate_evaluation_bindings(&config(false)?)?;
        Ok(())
    }
}
