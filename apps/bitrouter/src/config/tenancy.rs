//! Explicit opt-in to local framework resource authority.

use anyhow::{Result, ensure};
use bitrouter_ai::control::ControlScope;
use serde::Deserialize;
use std::path::PathBuf;

/// Local authority is independent of external login and commercial policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TenancyConfig {
    /// Opt in to the tenant control service. Existing local startup stays unchanged.
    pub enabled: bool,
    /// Privileged JSON manifest imported before listeners open. Relative paths
    /// are relative to the server process's working directory, never a tenant.
    pub resource_import: Option<PathBuf>,
    /// Optional local learning registration manifest, authenticated after H bootstrap.
    /// Omission preserves startup behavior and does not erase durable schedules.
    pub learning_registrations: Option<PathBuf>,
    /// Operator environment variable containing a stable base64 32-byte key.
    /// The secret itself is never part of configuration or resource reports.
    pub wrapping_key_env: Option<String>,
}

impl TenancyConfig {
    /// Validate the supported owning-process profile before opening any store.
    ///
    /// # Errors
    /// Rejects anonymous ingress, missing operator authority, shared mode,
    /// non-file SQLite configurations and unsupported local ownership platforms.
    pub fn validate(&self, config: &super::Config) -> Result<()> {
        ensure!(
            self.enabled
                || (self.resource_import.is_none()
                    && self.wrapping_key_env.is_none()
                    && self.learning_registrations.is_none()),
            "tenancy resource provisioning requires tenancy.enabled"
        );
        ensure!(
            self.resource_import.is_none() || self.wrapping_key_env.is_some(),
            "tenancy resource import requires wrapping_key_env"
        );
        if let Some(path) = &self.resource_import {
            ensure!(
                !path.as_os_str().is_empty(),
                "tenancy resource import path is empty"
            );
        }
        if let Some(name) = &self.wrapping_key_env {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && name.bytes().enumerate().all(|(index, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (index > 0 && byte.is_ascii_digit())
                    }),
                "invalid tenancy wrapping key environment name"
            );
        }
        if let Some(path) = &self.learning_registrations {
            ensure!(
                !path.as_os_str().is_empty(),
                "learning registration path is empty"
            );
            ensure!(
                cfg!(all(feature = "core", feature = "harness")),
                "owned learning requires core and harness"
            );
        }
        if !self.enabled {
            return Ok(());
        }
        ensure!(cfg!(unix), "local tenancy currently requires a Unix host");
        ensure!(
            !config.server.skip_auth,
            "tenancy requires server.skip_auth=false"
        );
        ensure!(
            matches!(
                config.gateway_state,
                super::gateway_state::GatewayStateConfig::Local {}
            ),
            "tenancy requires gateway_state.mode=local; distributed tenant authority is not enabled"
        );
        ensure!(
            config.control.enabled
                && config
                    .control
                    .credentials
                    .iter()
                    .any(|credential| credential.scopes.contains(&ControlScope::Tenancy)),
            "tenancy requires an enabled control listener and explicit control:tenancy authority"
        );
        ensure!(
            !config.agent_api.enabled,
            "tenancy does not expose the host coding-task API"
        );
        for credential in &config.control.credentials {
            if credential.scopes.contains(&ControlScope::Tenancy) {
                bitrouter_ai::tenancy::identity::PrincipalId::for_control_credential(
                    &credential.id,
                )?;
            }
        }
        crate::tenancy::ownership::path(&config.database.url)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn provisioning_settings_require_explicit_authority_and_a_key_name() -> anyhow::Result<()> {
        for source in [
            "resource_import: manifest.json",
            "wrapping_key_env: WRAP_KEY",
            "enabled: true\nresource_import: manifest.json",
            "enabled: true\nwrapping_key_env: '9BAD'",
            "enabled: true\nwrapping_key_env: ''",
        ] {
            let settings: super::TenancyConfig = serde_saphyr::from_str(source)?;
            assert!(
                settings
                    .validate(&crate::config::Config::default())
                    .is_err()
            );
        }
        let settings: super::TenancyConfig = serde_saphyr::from_str(
            "enabled: true\nresource_import: manifest.json\nwrapping_key_env: TENANCY_WRAP_KEY",
        )?;
        assert_eq!(settings.resource_import, Some("manifest.json".into()));
        assert_eq!(
            settings.wrapping_key_env.as_deref(),
            Some("TENANCY_WRAP_KEY")
        );
        Ok(())
    }

    #[test]
    fn parsing_rejects_unsafe_owner_profiles_before_opening_a_store() {
        for document in [
            "tenancy: {enabled: true}",
            "tenancy: {enabled: true}\nserver: {skip_auth: false}",
            "tenancy: {enabled: true}\nserver: {skip_auth: false}\ncontrol: {enabled: true}",
        ] {
            assert!(crate::config::parse_resolved(document).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn parsing_accepts_explicit_local_owner_without_read_or_reload_grants() -> anyhow::Result<()> {
        let document = "tenancy: {enabled: true}\nserver: {skip_auth: false}\ncontrol:\n  enabled: true\n  credentials:\n    - id: tenancy-operator\n      token_env: TEST_TENANCY_OPERATOR\n      scopes: [control:tenancy]\n";
        let config = crate::config::parse_resolved(document)?;
        assert!(config.tenancy.enabled);
        assert_eq!(
            config.control.credentials[0].scopes,
            [bitrouter_ai::control::ControlScope::Tenancy]
        );
        let oversized_second = format!(
            "{document}    - id: {}\n      token_env: SECOND_TENANCY_OPERATOR\n      scopes: [control:tenancy]\n",
            "x".repeat(120)
        );
        assert!(crate::config::parse_resolved(&oversized_second).is_err());
        for suffix in [
            "database: {url: 'sqlite::memory:'}\n",
            "agent_api: {enabled: true}\n",
            "database: {url: 'sqlite://a%2fb.db'}\n",
        ] {
            assert!(crate::config::parse_resolved(&format!("{document}{suffix}")).is_err());
        }
        Ok(())
    }
}
