//! Explicit opt-in to local framework resource authority.

use anyhow::{Result, ensure};
use bitrouter_ai::control::ControlScope;
use serde::Deserialize;

/// Local authority is independent of external login and commercial policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TenancyConfig {
    /// Opt in to the tenant control service. Existing local startup stays unchanged.
    pub enabled: bool,
}

impl TenancyConfig {
    /// Validate the supported owning-process profile before opening any store.
    ///
    /// # Errors
    /// Rejects anonymous ingress, missing operator authority, shared mode,
    /// non-file SQLite configurations and unsupported local ownership platforms.
    pub fn validate(&self, config: &super::Config) -> Result<()> {
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
