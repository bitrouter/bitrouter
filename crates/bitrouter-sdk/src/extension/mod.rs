//! Restricted author API for statically linked BitRouter extensions.
//!
//! Registration only supplies capability implementations. The host retains
//! configuration validation, router binding, execution limits and diagnostics.

use std::collections::{BTreeMap, HashMap, HashSet, hash_map::Entry};
use std::sync::Arc;

#[cfg(feature = "config_file")]
use crate::config::Config;
use crate::error::{BitrouterError, Result};
#[cfg(feature = "config_file")]
use crate::inference::InferenceOperation;
use request_check::{Callback, Registration};

/// Typed, provider-owned implementation of evaluation models.
pub mod provider;
pub mod request_check;

use provider::{EvaluationProvider, EvaluationProviderDescriptor};

#[derive(Clone)]
struct RegisteredEvaluationProvider {
    descriptor: EvaluationProviderDescriptor,
    implementation: Arc<dyn EvaluationProvider>,
}

/// Collects trusted, statically linked capability implementations for a host.
///
/// Construct this during startup, pass a mutable reference to ordinary Rust
/// registration functions, then let the host consume it before activating the
/// application. Registration does not execute callbacks or bind them globally.
#[derive(Default, Clone)]
pub struct ExtensionApi {
    request_checks: HashMap<String, Registration>,
    evaluation_providers: BTreeMap<String, RegisteredEvaluationProvider>,
    invalid: Option<String>,
}

impl ExtensionApi {
    /// Start an empty extension registration phase.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one native request-check implementation.
    ///
    /// `id` uses the same grammar as checker ids in BitRouter configuration.
    /// `revision` identifies the code or rules used for execution diagnostics.
    /// It must satisfy the same bounds as configured native revisions. Duplicate
    /// ids are rejected and never replace the first registration.
    ///
    /// Any registration error invalidates the whole collection. This prevents
    /// an extension that ignores the returned error from activating a partial
    /// set of capabilities.
    pub fn request_check(
        &mut self,
        id: &str,
        revision: &str,
        callback: Arc<Callback>,
    ) -> Result<()> {
        if let Some(message) = &self.invalid {
            return Err(BitrouterError::bad_request(message.clone()));
        }
        if !valid_checker_id(id) {
            return self.reject(format!(
                "invalid checker id '{id}' (use a lowercase letter followed by up to 63 lowercase letters, digits, '_' or '-')"
            ));
        }
        if let Err(error) = request_check::validate_revision(revision) {
            return self.reject(format!(
                "checker '{id}' native revision is invalid: {error}"
            ));
        }
        match self.request_checks.entry(id.to_owned()) {
            Entry::Occupied(_) => {
                let message = format!("native request checker '{id}' is already registered");
                self.invalid = Some(message.clone());
                Err(BitrouterError::bad_request(message))
            }
            Entry::Vacant(entry) => {
                entry.insert(Registration::new(revision, callback));
                Ok(())
            }
        }
    }

    /// Register one executable evaluation provider. Duplicate or invalid
    /// claims poison startup even if the caller ignores the returned error.
    pub fn register_evaluation_provider(
        &mut self,
        implementation: Arc<dyn EvaluationProvider>,
    ) -> Result<()> {
        if let Some(message) = &self.invalid {
            return Err(BitrouterError::bad_request(message.clone()));
        }
        let descriptor = implementation.descriptor();
        if !valid_checker_id(&descriptor.provider_id)
            || !url::Url::parse(&descriptor.api_base).is_ok_and(|url| {
                url.scheme() == "https"
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
            || descriptor.credential_env.is_empty()
            || !valid_evaluation_endpoint(&descriptor.endpoint)
            || descriptor.models.is_empty()
        {
            return self.reject("evaluation provider descriptor is invalid".to_owned());
        }
        let mut seen = HashSet::new();
        for model in &descriptor.models {
            if !model
                .id
                .starts_with(&format!("{}/", descriptor.provider_id))
                || model.id.len() <= descriptor.provider_id.len() + 1
                || model.provider_model_id.is_empty()
                || model.question_types.is_empty()
                || model
                    .question_types
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>()
                    .len()
                    != model.question_types.len()
                || model.max_choice_options.is_some_and(|value| value < 2)
                || model.max_score_levels.is_some_and(|value| value < 2)
                || !seen.insert(&model.id)
            {
                return self.reject(format!(
                    "evaluation provider '{}' has an invalid or duplicate model '{}'",
                    descriptor.provider_id, model.id
                ));
            }
        }
        if self
            .evaluation_providers
            .contains_key(&descriptor.provider_id)
        {
            return self.reject(format!(
                "duplicate evaluation provider registration '{}'",
                descriptor.provider_id
            ));
        }
        self.evaluation_providers.insert(
            descriptor.provider_id.clone(),
            RegisteredEvaluationProvider {
                descriptor,
                implementation,
            },
        );
        Ok(())
    }

    /// Return executable provider declarations for host-owned config merging.
    pub fn evaluation_provider_descriptors(&self) -> Vec<EvaluationProviderDescriptor> {
        self.evaluation_providers
            .values()
            .map(|registered| registered.descriptor.clone())
            .collect()
    }

    /// Resolve an exact registered provider for an evaluation route.
    pub fn evaluation_provider(&self, id: &str) -> Result<Arc<dyn EvaluationProvider>> {
        self.evaluation_providers
            .get(id)
            .map(|registered| Arc::clone(&registered.implementation))
            .ok_or_else(|| {
                BitrouterError::bad_request(format!("missing evaluation provider extension '{id}'"))
            })
    }

    /// Fail active evaluation claims that disagree with compiled providers.
    #[cfg(feature = "config_file")]
    #[cfg_attr(docsrs, doc(cfg(feature = "config_file")))]
    pub fn validate_evaluation_bindings(&self, config: &Config) -> Result<()> {
        config.validate_operations()?;
        for (provider_id, provider) in &config.providers {
            if !provider.active {
                continue;
            }
            if self.evaluation_providers.contains_key(provider_id)
                && (provider
                    .operations
                    .keys()
                    .any(|operation| *operation != InferenceOperation::Evaluate)
                    || provider.models.iter().any(|model| {
                        model.operations.as_ref().is_none_or(|operations| {
                            operations.len() != 1
                                || !operations.contains_key(&InferenceOperation::Evaluate)
                        })
                    }))
            {
                return Err(BitrouterError::bad_request(format!(
                    "provider '{provider_id}' claims an operation its evaluation extension does not implement"
                )));
            }
            if let Some(operation) = provider.operations.get(&InferenceOperation::Evaluate) {
                if provider.primary_api_key().is_empty() {
                    return Err(BitrouterError::bad_request(format!(
                        "provider '{provider_id}' has no evaluation credential"
                    )));
                }
                let registered = self.evaluation_providers.get(provider_id).ok_or_else(|| {
                    BitrouterError::bad_request(format!(
                        "missing evaluation provider extension '{provider_id}'"
                    ))
                })?;
                if operation.endpoint != registered.descriptor.endpoint {
                    return Err(BitrouterError::bad_request(format!(
                        "provider '{provider_id}' evaluate endpoint conflicts with its extension"
                    )));
                }
                for model in &provider.models {
                    if !model.supports_operation(InferenceOperation::Evaluate) {
                        continue;
                    }
                    let declared = registered.descriptor.models.iter()
                        .find(|declared| declared.id == model.id)
                        .ok_or_else(|| BitrouterError::bad_request(format!(
                            "provider '{provider_id}' model '{}' is not claimed by its evaluation extension",
                            model.id
                        )))?;
                    if model.provider_model_id.as_deref() != Some(&declared.provider_model_id)
                        || model
                            .operations
                            .as_ref()
                            .and_then(|ops| ops.get(&InferenceOperation::Evaluate))
                            .is_none_or(|limits| {
                                limits.question_types != declared.question_types
                                    || limits.max_choice_options != declared.max_choice_options
                                    || limits.max_score_levels != declared.max_score_levels
                            })
                    {
                        return Err(BitrouterError::bad_request(format!(
                            "provider '{provider_id}' model '{}' evaluate claim conflicts with its extension",
                            model.id
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// Finish registration before activating the host and return the
    /// request-check subset. The host can retain a clone for evaluation-provider
    /// lookup. Any prior error prevents consumption, even when the caller
    /// ignored that registration error.
    pub fn into_registrations(self) -> Result<HashMap<String, Registration>> {
        match self.invalid {
            Some(message) => Err(BitrouterError::bad_request(message)),
            None => Ok(self.request_checks),
        }
    }

    fn reject(&mut self, message: String) -> Result<()> {
        self.invalid = Some(message.clone());
        Err(BitrouterError::bad_request(message))
    }
}

fn valid_evaluation_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with('/')
        && !endpoint.starts_with("//")
        && !endpoint.contains("..")
        && !endpoint.contains(['?', '#'])
}

fn valid_checker_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::request_check::Decision;

    use super::*;

    fn allow_callback() -> Arc<Callback> {
        Arc::new(|_| Decision::Allow)
    }

    #[test]
    fn completed_registration_preserves_callback_and_revision() -> Result<()> {
        let callback = allow_callback();
        let mut api = ExtensionApi::new();
        api.request_check("secrets", "rules-v1", callback.clone())?;

        let registrations = api.into_registrations()?;
        let registration = registrations
            .get("secrets")
            .ok_or_else(|| BitrouterError::internal("registration was lost"))?;
        assert_eq!(registration.revision, "rules-v1");
        assert!(Arc::ptr_eq(&registration.callback, &callback));
        Ok(())
    }

    #[test]
    fn duplicate_id_is_rejected_without_overwriting() -> Result<()> {
        let mut api = ExtensionApi::new();
        api.request_check("secrets", "rules-v1", allow_callback())?;

        let error = api
            .request_check("secrets", "rules-v2", allow_callback())
            .err()
            .ok_or_else(|| {
                BitrouterError::internal("duplicate registration unexpectedly succeeded")
            })?;

        assert!(error.to_string().contains("already registered"));
        assert_eq!(api.request_checks.len(), 1);
        assert!(api.request_checks.contains_key("secrets"));
        assert!(api.into_registrations().is_err());
        Ok(())
    }

    #[test]
    fn revision_must_match_configuration_bounds() {
        let mut api = ExtensionApi::new();

        let result = api.request_check("secrets", "rules:v1", allow_callback());

        assert!(result.is_err());
        assert!(api.request_checks.is_empty());
        assert!(api.into_registrations().is_err());
    }

    #[test]
    fn checker_id_uses_configuration_grammar() {
        for invalid in ["", "1secrets", "Secrets", "secret.check"] {
            let mut api = ExtensionApi::new();
            assert!(
                api.request_check(invalid, "rules-v1", allow_callback())
                    .is_err()
            );
            assert!(api.request_checks.is_empty());
        }
    }

    #[test]
    fn ignored_error_keeps_partial_registry_inert() -> Result<()> {
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let callback: Arc<Callback> = Arc::new(move |_| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
            Decision::Allow
        });
        let mut api = ExtensionApi::new();
        api.request_check("first", "rules-v1", callback)?;

        let _ignored = api.request_check("invalid.id", "rules-v1", allow_callback());
        let later = api.request_check("second", "rules-v1", allow_callback());

        assert!(later.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(api.request_checks.len(), 1);
        assert!(api.into_registrations().is_err());
        Ok(())
    }
}
