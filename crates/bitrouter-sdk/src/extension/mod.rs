//! Restricted author API for statically linked BitRouter extensions.
//!
//! Registration only supplies capability implementations. The host retains
//! configuration validation, router binding, execution limits and receipts.

use std::collections::{HashMap, hash_map::Entry};
use std::sync::Arc;

use crate::error::{BitrouterError, Result};
use request_check::{Callback, Registration};

pub mod request_check;

/// Collects trusted, statically linked capability implementations for a host.
///
/// Construct this during startup, pass a mutable reference to ordinary Rust
/// registration functions, then let the host consume it before activating the
/// application. Registration does not execute callbacks or bind them globally.
#[derive(Default)]
pub struct ExtensionApi {
    request_checks: HashMap<String, Registration>,
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
    /// `revision` identifies the code or rules used for execution and receipts.
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

    /// Finish registration before activating the host. Any prior error prevents
    /// consumption, even when the caller ignored that registration error.
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
