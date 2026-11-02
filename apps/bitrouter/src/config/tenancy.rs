//! Explicit opt-in to local framework resource authority.

use anyhow::{Result, ensure};
use bitrouter_ai::control::ControlScope;
use serde::Deserialize;
use std::path::PathBuf;

/// Local authority is independent of external login and commercial policy.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
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
    /// Exact HTTP(S) origins allowed for tenant-managed routes. Empty keeps
    /// route provisioning operator-only; installation operators remain trusted.
    pub resource_origins: Vec<String>,
    /// Maximum time for submission body upload and execution admission.
    #[serde(default = "default_submission_timeout")]
    pub submission_timeout_secs: u64,
    /// Optional total execution lifetime, starting after capacity admission.
    pub execution_timeout_secs: Option<u64>,
    /// Maximum idle provider read or blocked output handoff, independently of
    /// total execution lifetime. Progress renews this bound.
    #[serde(default = "default_stream_idle_timeout")]
    pub stream_idle_timeout_secs: u64,
}

const fn default_submission_timeout() -> u64 {
    60
}
const fn default_stream_idle_timeout() -> u64 {
    120
}

impl Default for TenancyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            resource_import: None,
            learning_registrations: None,
            wrapping_key_env: None,
            resource_origins: Vec::new(),
            submission_timeout_secs: default_submission_timeout(),
            execution_timeout_secs: None,
            stream_idle_timeout_secs: default_stream_idle_timeout(),
        }
    }
}

impl TenancyConfig {
    /// Validate the supported owning-process profile before opening any store.
    ///
    /// # Errors
    /// Rejects anonymous ingress, missing operator authority, shared mode,
    /// non-file SQLite configurations and unsupported local ownership platforms.
    pub fn validate(&self, config: &super::Config) -> Result<()> {
        self.validate_lifetime()?;
        ensure!(
            self.resource_origins.len() <= 256,
            "too many tenancy resource origins"
        );
        ensure!(
            self.enabled || self.resource_origins.is_empty(),
            "tenancy resource origins require tenancy.enabled"
        );
        let mut origins = std::collections::BTreeSet::new();
        for origin in &self.resource_origins {
            let url = url::Url::parse(origin)?;
            ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && matches!(url.path(), "" | "/")
                    && !origin.bytes().any(|byte| byte.is_ascii_whitespace())
                    && origin.split_once("://").is_some_and(|(_, authority)| {
                        !authority.contains(['@', '\\'])
                            && authority
                                .split_once('/')
                                .is_none_or(|(_, path)| path.is_empty())
                    }),
                "tenancy resource origin must be an exact HTTP(S) origin"
            );
            ensure!(
                origins.insert(url.origin().ascii_serialization()),
                "duplicate tenancy resource origin"
            );
        }
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

    pub(crate) fn validate_lifetime(&self) -> Result<()> {
        for (name, seconds) in [
            (
                "submission_timeout_secs",
                Some(self.submission_timeout_secs),
            ),
            ("execution_timeout_secs", self.execution_timeout_secs),
            (
                "stream_idle_timeout_secs",
                Some(self.stream_idle_timeout_secs),
            ),
        ] {
            if let Some(seconds) = seconds {
                // One year is below the runtime timer range on supported hosts.
                ensure!(
                    (1..=31_536_000).contains(&seconds),
                    "tenancy.{name} must be between 1 and 31536000 seconds"
                );
                ensure!(
                    std::time::Instant::now()
                        .checked_add(std::time::Duration::from_secs(seconds + 30))
                        .is_some(),
                    "tenancy.{name} exceeds the host clock range"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lifetime_defaults_and_invalid_bounds() -> anyhow::Result<()> {
        let defaults: super::TenancyConfig = serde_saphyr::from_str("{}")?;
        assert_eq!(defaults, super::TenancyConfig::default());
        assert_eq!(defaults.submission_timeout_secs, 60);
        assert_eq!(defaults.execution_timeout_secs, None);
        assert_eq!(defaults.stream_idle_timeout_secs, 120);
        assert!(defaults.resource_origins.is_empty());
        for field in [
            "submission_timeout_secs",
            "execution_timeout_secs",
            "stream_idle_timeout_secs",
        ] {
            for invalid in ["0", "31536001", "18446744073709551615"] {
                let settings: super::TenancyConfig =
                    serde_saphyr::from_str(&format!("{field}: {invalid}"))?;
                assert!(settings.validate_lifetime().is_err(), "{field}: {invalid}");
            }
            for invalid in ["-1", "18446744073709551616"] {
                assert!(
                    serde_saphyr::from_str::<super::TenancyConfig>(&format!("{field}: {invalid}"))
                        .is_err()
                );
            }
            for valid in [1, 180, 31_536_000] {
                let settings: super::TenancyConfig =
                    serde_saphyr::from_str(&format!("{field}: {valid}"))?;
                settings.validate_lifetime()?;
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn resource_origins_are_bounded_exact_origins_and_require_opt_in() -> anyhow::Result<()> {
        let config = crate::config::parse_resolved(
            "tenancy: {enabled: true}\nserver: {skip_auth: false}\ncontrol:\n  enabled: true\n  credentials:\n    - id: tenancy-operator\n      token_env: TEST_TENANCY_OPERATOR\n      scopes: [control:tenancy]\n",
        )?;
        let mut settings = config.tenancy.clone();
        for origin in [
            "https://example.com",
            "http://127.0.0.1:8000/",
            "https://[::1]:8443",
            "HTTPS://EXAMPLE.COM:443/",
        ] {
            settings.resource_origins = vec![origin.into()];
            settings.validate(&config)?;
        }
        for origin in [
            "ftp://example.com",
            "https://user@example.com",
            "https://@example.com",
            "https://example.com?",
            "https://example.com#",
            "https://example.com/v1",
            "https://example.com/a/..",
            " https://example.com",
            "https://exa\tmple.com",
            "https:example.com",
            "https://example.com\\",
        ] {
            settings.resource_origins = vec![origin.into()];
            assert!(settings.validate(&config).is_err(), "{origin}");
        }
        settings.resource_origins = vec![
            "https://example.com".into(),
            "https://EXAMPLE.COM:443/".into(),
        ];
        assert!(settings.validate(&config).is_err());
        settings.resource_origins = (0..256)
            .map(|index| format!("https://host-{index}.example"))
            .collect();
        settings.validate(&config)?;
        settings
            .resource_origins
            .push("https://extra.example".into());
        assert!(settings.validate(&config).is_err());
        settings.resource_origins = vec!["https://example.com".into()];
        settings.enabled = false;
        assert!(settings.validate(&config).is_err());
        Ok(())
    }

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
