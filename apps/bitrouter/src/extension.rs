//! Restricted author API for statically linked BitRouter extensions.
//!
//! Registration only supplies capability implementations. The host retains
//! configuration validation, router binding, execution limits and receipts.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use bitrouter_checker_protocol::capability::CheckCallback;

use crate::request_checks::NativeChecker;

/// Collects trusted, statically linked capability implementations for a host.
///
/// Construct this during startup, pass a mutable reference to ordinary Rust
/// registration functions, then let the host consume it before activating the
/// application. Registration does not execute callbacks or bind them globally.
#[derive(Default)]
pub struct ExtensionApi {
    request_checks: HashMap<String, NativeChecker>,
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
    /// `revision` is returned as the v1 implementation version, so it must
    /// satisfy that protocol field's bounds. Duplicate ids are rejected and
    /// never replace the first registration.
    ///
    /// Any registration error invalidates the whole collection. This prevents
    /// an extension that ignores the returned error from activating a partial
    /// set of capabilities.
    pub fn request_check(
        &mut self,
        id: &str,
        revision: &str,
        callback: Arc<CheckCallback>,
    ) -> Result<()> {
        if let Some(message) = &self.invalid {
            return Err(anyhow!(message.clone()));
        }
        if !valid_checker_id(id) {
            return self.reject(format!(
                "invalid checker id '{id}' (use a lowercase letter followed by up to 63 lowercase letters, digits, '_' or '-')"
            ));
        }
        if let Err(error) =
            bitrouter_checker_protocol::v1::validate_implementation_version(Some(revision))
        {
            return self.reject(format!(
                "checker '{id}' native revision is invalid for request-check v1: {error}"
            ));
        }
        match self.request_checks.entry(id.to_owned()) {
            Entry::Occupied(_) => {
                let message = format!("native request checker '{id}' is already registered");
                self.invalid = Some(message.clone());
                Err(anyhow!(message))
            }
            Entry::Vacant(entry) => {
                entry.insert(NativeChecker::new(revision, callback));
                Ok(())
            }
        }
    }

    pub(crate) fn into_native(self) -> Result<HashMap<String, NativeChecker>> {
        match self.invalid {
            Some(message) => Err(anyhow!(message)),
            None => Ok(self.request_checks),
        }
    }

    fn reject(&mut self, message: String) -> Result<()> {
        self.invalid = Some(message.clone());
        Err(anyhow!(message))
    }
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

    use bitrouter_checker_protocol::capability::CheckDecision;

    use super::*;

    fn allow_callback() -> Arc<CheckCallback> {
        Arc::new(|_| CheckDecision::Allow)
    }

    #[test]
    fn duplicate_id_is_rejected_without_overwriting() -> Result<()> {
        let mut api = ExtensionApi::new();
        api.request_check("secrets", "rules-v1", allow_callback())?;

        let error = api
            .request_check("secrets", "rules-v2", allow_callback())
            .err()
            .ok_or_else(|| anyhow!("duplicate registration unexpectedly succeeded"))?;

        assert!(error.to_string().contains("already registered"));
        assert_eq!(api.request_checks.len(), 1);
        assert!(api.request_checks.contains_key("secrets"));
        assert!(api.into_native().is_err());
        Ok(())
    }

    #[test]
    fn revision_must_be_valid_for_the_shared_protocol() {
        let mut api = ExtensionApi::new();

        let result = api.request_check("secrets", "rules:v1", allow_callback());

        assert!(result.is_err());
        assert!(api.request_checks.is_empty());
        assert!(api.into_native().is_err());
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
        let callback: Arc<CheckCallback> = Arc::new(move |_| {
            callback_calls.fetch_add(1, Ordering::Relaxed);
            CheckDecision::Allow
        });
        let mut api = ExtensionApi::new();
        api.request_check("first", "rules-v1", callback)?;

        let _ignored = api.request_check("invalid.id", "rules-v1", allow_callback());
        let later = api.request_check("second", "rules-v1", allow_callback());

        assert!(later.is_err());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(api.request_checks.len(), 1);
        assert!(api.into_native().is_err());
        Ok(())
    }
}
