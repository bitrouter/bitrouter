//! Compile-time native extension registration.
//!
//! A custom host links trusted Rust code and registers typed capabilities
//! before assembling an application. Registration does not configure or
//! activate a provider route, and this API is not a dynamic-library ABI.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::{Config, OperationFormatConfig};
use crate::error::{BitrouterError, Result};
use crate::evaluation::{EvaluationRequest, EvaluationResult, EvaluationRoutingTarget};
use crate::inference::InferenceOperation;

/// Exact compatibility identity of one evaluation JSON format facet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationFormatDescriptor {
    /// Stable native extension package id.
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

/// Trusted, process-local capabilities explicitly linked by a custom host.
#[derive(Default, Clone)]
pub struct ExtensionApi {
    evaluation_formats: BTreeMap<(String, String), RegisteredEvaluationFormat>,
}

#[derive(Clone)]
struct RegisteredEvaluationFormat {
    revision: u32,
    adapter: Arc<dyn EvaluationFormatAdapter>,
}

impl ExtensionApi {
    /// Create an empty registry, as used by the stock `bro` host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one exact native evaluation format. Duplicate ids are errors,
    /// even if the duplicate advertises a different revision.
    pub fn register_evaluation_format(
        &mut self,
        adapter: Arc<dyn EvaluationFormatAdapter>,
    ) -> Result<()> {
        let descriptor = adapter.descriptor();
        if descriptor.extension_id.trim().is_empty()
            || descriptor.adapter_id.trim().is_empty()
            || descriptor.revision == 0
        {
            return Err(BitrouterError::bad_request(
                "evaluation format descriptor has an empty id or zero revision",
            ));
        }
        let key = (descriptor.extension_id, descriptor.adapter_id);
        if self.evaluation_formats.contains_key(&key) {
            return Err(BitrouterError::bad_request(format!(
                "duplicate evaluation format registration '{}/{}'",
                key.0, key.1
            )));
        }
        self.evaluation_formats.insert(
            key,
            RegisteredEvaluationFormat {
                revision: descriptor.revision,
                adapter,
            },
        );
        Ok(())
    }

    /// Resolve a configured format only when its revision matches exactly.
    pub fn evaluation_format(
        &self,
        format: &OperationFormatConfig,
    ) -> Result<Arc<dyn EvaluationFormatAdapter>> {
        let key = (format.extension.clone(), format.adapter.clone());
        let Some(registered) = self.evaluation_formats.get(&key) else {
            return Err(BitrouterError::bad_request(format!(
                "missing evaluation format extension '{}/{}@{}'",
                key.0, key.1, format.revision
            )));
        };
        if registered.revision != format.revision {
            return Err(BitrouterError::bad_request(format!(
                "evaluation format revision mismatch for '{}/{}': configured {}, registered {}",
                key.0, key.1, format.revision, registered.revision
            )));
        }
        Ok(Arc::clone(&registered.adapter))
    }

    /// Fail active evaluation bindings before database assembly or dispatch.
    /// Valid registrations without a configured provider remain inactive.
    pub fn validate_evaluation_bindings(&self, config: &Config) -> Result<()> {
        config.validate_operations()?;
        for (provider_id, provider) in &config.providers {
            if !provider.active {
                continue;
            }
            if let Some(operation) = provider.operations.get(&InferenceOperation::Evaluate) {
                self.evaluation_format(&operation.format).map_err(|error| {
                    BitrouterError::bad_request(format!(
                        "provider '{provider_id}' evaluation binding: {error}"
                    ))
                })?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{EvaluationFormatAdapter, EvaluationFormatDescriptor, ExtensionApi};
    use crate::config::{Config, ProviderConfig};
    use crate::error::Result;
    use crate::evaluation::{EvaluationRequest, EvaluationResult, EvaluationRoutingTarget};

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
