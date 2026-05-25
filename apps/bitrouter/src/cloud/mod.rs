//! Glue between [`bitrouter_cloud_sdk`] and the bitrouter assembly
//! layer, plus the `bitrouter cloud …` CLI entry points (see [`cli`]).
//!
//! Two daemon-side responsibilities, both keyed on the `"bitrouter"`
//! provider id:
//!
//! - [`enable_in_zero_config`] — auto-add the `bitrouter` provider to the in-memory
//!   zero-config `providers:` map when the user has signed in via
//!   `bitrouter auth login` (an `account-credentials.json` file is present
//!   at the default path). The env-var path (`$BITROUTER_API_KEY`) is
//!   already covered by [`bitrouter_providers::zero_config`].
//! - [`build_auth_applier`] — construct the
//!   [`bitrouter_cloud_sdk::BitrouterCloudAuthApplier`] for registration
//!   against the SDK executor.
//!
//! These are kept here rather than inside `bitrouter-providers` so that
//! the providers crate stays free of any dependency on
//! `bitrouter-cloud-sdk`; the SDK and the catalog can be consumed
//! independently by downstream tooling.
//!
//! The [`cli`] sub-module owns the `bitrouter cloud` subcommand surface
//! — typed wrappers around every endpoint on
//! [`bitrouter_cloud_sdk::management::ManagementClient`].

pub mod cli;

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_cloud_sdk::BitrouterCloudAuthApplier;
use bitrouter_cloud_sdk::auth::credentials::default_credentials_path;
use bitrouter_cloud_sdk::provider::PROVIDER_ID;
use bitrouter_sdk::config::{Config, ProviderConfig};
use bitrouter_sdk::language_model::{AuthApplier, auth::AuthAppliers};

/// Insert the `bitrouter` provider into `config.providers` when the user
/// has run `bitrouter auth login` (i.e. the credentials file exists at the
/// default path) and the entry is not already present.
///
/// No-op when the credentials file is absent — `bitrouter_providers::zero_config`
/// already handles the `$BITROUTER_API_KEY` env-var path. Together the two
/// paths give a signed-in user the cloud provider on every fresh
/// `bitrouter serve` regardless of which credential source they chose.
pub fn enable_in_zero_config(config: &mut Config) {
    let Ok(path) = default_credentials_path() else {
        return;
    };
    enable_in_zero_config_with_path(config, &path);
}

/// Inner form taking the credentials path explicitly so unit tests can
/// drive the logic without mutating process environment.
fn enable_in_zero_config_with_path(config: &mut Config, credentials_path: &std::path::Path) {
    if config.providers.contains_key(PROVIDER_ID) {
        return;
    }
    if !credentials_path.exists() {
        return;
    }
    config.providers.insert(
        PROVIDER_ID.to_string(),
        ProviderConfig {
            auto_discover: true,
            ..ProviderConfig::default()
        },
    );
}

/// Build the BitRouter Cloud `AuthApplier`. Reads the credentials file at
/// [`bitrouter_cloud_sdk::auth::credentials::default_credentials_path`].
pub fn build_auth_applier() -> Result<Arc<dyn AuthApplier>> {
    let path = default_credentials_path().context("resolving BitRouter Cloud credentials path")?;
    let applier =
        BitrouterCloudAuthApplier::new(path).context("building the BitRouter Cloud AuthApplier")?;
    Ok(Arc::new(applier))
}

/// Register the BitRouter Cloud applier on `appliers` when the `bitrouter`
/// provider appears in `config.providers`. No-op otherwise.
pub fn register_if_configured(config: &Config, appliers: &mut AuthAppliers) -> Result<()> {
    if !config.providers.contains_key(PROVIDER_ID) {
        return Ok(());
    }
    let applier = build_auth_applier()?;
    appliers.register(PROVIDER_ID, applier);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fresh_tmp_creds_path(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-glue-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("account-credentials.json")
    }

    #[test]
    fn enable_in_zero_config_noop_when_no_credentials_file() {
        let path = fresh_tmp_creds_path("noop");
        // path's parent exists; the file itself does not.
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        assert!(!config.providers.contains_key(PROVIDER_ID));
    }

    #[test]
    fn enable_in_zero_config_inserts_when_credentials_file_present() {
        let path = fresh_tmp_creds_path("inserts");
        fs::write(&path, "{}").unwrap();
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        let provider = config
            .providers
            .get(PROVIDER_ID)
            .expect("`bitrouter` provider should be auto-enabled when creds file is present");
        assert!(
            provider.auto_discover,
            "auto_discover should be true so /models populates the routable list"
        );
    }

    #[test]
    fn enable_in_zero_config_noop_when_already_configured() {
        let path = fresh_tmp_creds_path("already");
        fs::write(&path, "{}").unwrap();
        let mut config = Config::default();
        // Pre-populate with a sentinel `api_base` so we can prove we didn't
        // overwrite the existing entry.
        config.providers.insert(
            PROVIDER_ID.to_string(),
            ProviderConfig {
                api_base: "https://example.invalid".to_string(),
                ..ProviderConfig::default()
            },
        );
        enable_in_zero_config_with_path(&mut config, &path);
        assert_eq!(
            config.providers.get(PROVIDER_ID).unwrap().api_base,
            "https://example.invalid",
            "existing entry must not be overwritten"
        );
    }
}
