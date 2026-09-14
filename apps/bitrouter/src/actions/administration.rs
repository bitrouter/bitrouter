//! Typed, content-limited host inspection shared by the CLI and control API.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use bitrouter_sdk::config::{Config, ConfigRoutingTable, PolicyModelTarget};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::daemon::ObserveStatusProvider;
use crate::paths::ConfigSource;
use crate::policy_lock::{LoadedPolicyLock, PolicyRuntime};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PolicyView {
    #[default]
    Active,
    Disk,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyInput {
    #[serde(default)]
    pub view: PolicyView,
    pub name: Option<String>,
}

impl PolicyInput {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.name {
            validate_identifier(name)?;
        }
        Ok(())
    }
}

pub fn validate_identifier(value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control),
        "identifier must contain 1–256 bytes and no control characters"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProviderEntry {
    pub id: String,
    pub models: usize,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProvidersReport {
    pub resolved_via: String,
    pub providers: Vec<ProviderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentEntry {
    pub id: String,
    pub configured: bool,
    pub in_catalog: bool,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AgentsReport {
    pub resolved_via: String,
    pub readiness_checked: bool,
    pub agents: Vec<AgentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ObserveReport {
    pub daemon_reachable: bool,
    pub compiled_in: bool,
    pub exporter_wired: bool,
    pub sampler: Option<String>,
    pub sampler_arg: Option<f64>,
    pub metrics_enabled: bool,
    pub header_count: usize,
    pub resource_attribute_count: usize,
    pub api_key_count: usize,
    pub api_key_cap: usize,
    pub user_id_count: usize,
    pub user_id_cap: usize,
    pub active_spans: usize,
}

impl ObserveReport {
    pub fn from_snapshot(snapshot: crate::daemon::ObserveStatusPayload, reachable: bool) -> Self {
        Self {
            daemon_reachable: reachable,
            compiled_in: snapshot.compiled_in,
            exporter_wired: snapshot.exporter_wired,
            sampler: snapshot.sampler,
            sampler_arg: snapshot.sampler_arg,
            metrics_enabled: snapshot.metrics_enabled,
            header_count: snapshot.header_count,
            resource_attribute_count: snapshot.resource_attribute_count,
            api_key_count: snapshot.api_key_count,
            api_key_cap: snapshot.api_key_cap,
            user_id_count: snapshot.user_id_count,
            user_id_cap: snapshot.user_id_cap,
            active_spans: snapshot.active_spans,
        }
    }
}

/// Explicitly selected policy fields; never serialize the source document.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyDetail {
    pub tiers: BTreeMap<String, PolicyModelTarget>,
    pub routes: BTreeMap<String, String>,
    pub default_tier: Option<String>,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: Vec<String>,
    pub certificates: BTreeMap<String, CertificateIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CertificateIdentity {
    pub selected_tier: String,
    pub evidence_digest: String,
    pub compiler_config_digest: String,
    pub evaluator_config_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PolicyReport {
    pub view: PolicyView,
    pub availability: String,
    pub digest: Option<String>,
    pub mode: String,
    pub policies: Vec<String>,
    pub bindings: BTreeMap<String, String>,
    pub evidence_root: Option<String>,
    pub eval_snapshot_root: Option<String>,
    pub definitions: BTreeMap<String, PolicyDetail>,
}

impl PolicyReport {
    pub fn from_loaded(
        config: &Config,
        loaded: Option<&LoadedPolicyLock>,
        view: PolicyView,
    ) -> Self {
        let definitions = loaded
            .map(|loaded| {
                loaded
                    .document
                    .policies
                    .iter()
                    .map(|(name, definition)| {
                        let certificates = loaded
                            .document
                            .certificates
                            .get(name)
                            .into_iter()
                            .flat_map(|entries| entries.iter())
                            .map(|(key, certificate)| {
                                (
                                    key.clone(),
                                    CertificateIdentity {
                                        selected_tier: certificate.selected_tier.clone(),
                                        evidence_digest: certificate.evidence_digest.clone(),
                                        compiler_config_digest: certificate
                                            .compiler_config_digest
                                            .clone(),
                                        evaluator_config_digest: certificate
                                            .evaluator_config_digest
                                            .clone(),
                                    },
                                )
                            })
                            .collect();
                        (
                            name.clone(),
                            PolicyDetail {
                                tiers: definition.tiers.clone(),
                                routes: definition.routes.clone(),
                                default_tier: definition.default_tier.clone(),
                                tool_use_tier: definition.tool_use_tier.clone(),
                                tool_safe_tiers: definition.tool_safe_tiers.clone(),
                                certificates,
                            },
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let artifact = loaded.and_then(|loaded| loaded.document.artifact.as_ref());
        Self {
            view,
            availability: if loaded.is_some() {
                "available"
            } else {
                "not_configured"
            }
            .into(),
            digest: loaded.map(|loaded| loaded.digest.clone()),
            mode: match config.policy.mode {
                bitrouter_sdk::config::PolicyRuntimeMode::Frozen => "frozen",
                bitrouter_sdk::config::PolicyRuntimeMode::Adaptive => "adaptive",
            }
            .into(),
            policies: definitions.keys().cloned().collect(),
            bindings: config
                .presets
                .iter()
                .filter_map(|(name, preset)| {
                    preset
                        .policy
                        .as_ref()
                        .map(|policy| (name.clone(), policy.clone()))
                })
                .collect(),
            evidence_root: artifact.map(|artifact| artifact.evidence_root.clone()),
            eval_snapshot_root: artifact.and_then(|artifact| artifact.eval_snapshot_root.clone()),
            definitions,
        }
    }

    pub fn selected(mut self, name: Option<&str>) -> Result<Self> {
        match name {
            Some(name) => {
                validate_identifier(name)?;
                anyhow::ensure!(self.definitions.contains_key(name), "policy_not_found");
                self.definitions.retain(|key, _| key == name);
            }
            None => self.definitions.clear(),
        }
        Ok(self)
    }
}

/// Live host ports; construction does not consult a remote client's files.
#[derive(Clone)]
pub struct Administration {
    pub source: ConfigSource,
    pub routing: Arc<ConfigRoutingTable>,
    pub policy: Arc<PolicyRuntime>,
    pub observe: Arc<dyn ObserveStatusProvider>,
}

impl Administration {
    pub fn providers(&self) -> ProvidersReport {
        providers(&self.routing.snapshot_config(), "live")
    }

    pub fn agents(&self) -> AgentsReport {
        agents(&self.routing.snapshot_config(), "live")
    }

    pub fn observe(&self) -> ObserveReport {
        ObserveReport::from_snapshot(self.observe.status(), true)
    }

    pub async fn policy(&self, input: PolicyInput) -> Result<PolicyReport> {
        input.validate()?;
        let report = match input.view {
            PolicyView::Active => self.policy.administration_snapshot(),
            PolicyView::Disk => disk_policy(&self.source).await?,
        };
        report.selected(input.name.as_deref())
    }
}

pub async fn disk_policy(source: &ConfigSource) -> Result<PolicyReport> {
    let config = crate::paths::load_config(source)
        .await
        .map_err(|_| anyhow::anyhow!("invalid_configuration"))?;
    let path = match source {
        ConfigSource::File(path) => Some(path.as_path()),
        _ => None,
    };
    let loaded = crate::policy_lock::load_for_config(&config, path)
        .await
        .map_err(|_| anyhow::anyhow!("invalid_policy"))?;
    Ok(PolicyReport::from_loaded(
        &config,
        loaded.as_ref(),
        PolicyView::Disk,
    ))
}

pub fn providers(config: &Config, source: &str) -> ProvidersReport {
    ProvidersReport {
        resolved_via: source.into(),
        providers: crate::commands::list_providers(config)
            .into_iter()
            .map(|row| ProviderEntry {
                id: row.id,
                models: row.model_count,
                active: row.active,
            })
            .collect(),
    }
}

pub fn agents(config: &Config, source: &str) -> AgentsReport {
    AgentsReport {
        resolved_via: source.into(),
        readiness_checked: false,
        agents: crate::agents::list(config)
            .into_iter()
            .map(|row| AgentEntry {
                id: row.id,
                configured: row.configured,
                in_catalog: row.in_catalog,
                description: if row.in_catalog {
                    row.description
                } else {
                    "Custom configured agent".into()
                },
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_agent_commands_are_never_reported() -> Result<()> {
        let config = bitrouter_sdk::config::parse(
            "agents:\n  custom:\n    name: custom\n    transport:\n      type: stdio\n      command: secret-command\n      args: [secret-token]\n",
        )?;
        let report = serde_json::to_string(&agents(&config, "live"))?;
        assert!(!report.contains("secret-command"));
        assert!(!report.contains("secret-token"));
        assert!(report.contains("Custom configured agent"));
        Ok(())
    }

    #[test]
    fn telemetry_omits_endpoint_and_service_strings() -> Result<()> {
        let mut snapshot = crate::daemon::ObserveStatusPayload::unwired(true);
        snapshot.endpoint = Some("https://secret:token@example.test/path?key=private".into());
        snapshot.service_name = Some("secret-service".into());
        let report = serde_json::to_string(&ObserveReport::from_snapshot(snapshot, true))?;
        assert!(!report.contains("secret"));
        assert!(!report.contains("endpoint"));
        Ok(())
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[tokio::test]
    async fn active_mode_and_bindings_change_only_with_the_policy_snapshot() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        let lock = directory.path().join("policy-lock.yaml");
        let initial_yaml = "inherit_defaults: false\npolicy:\n  mode: frozen\npresets:\n  coding:\n    model: fixture:old\n    policy: coding\n";
        let candidate_yaml = "inherit_defaults: false\npolicy:\n  mode: adaptive\npresets:\n  chat:\n    model: fixture:new\n    policy: coding\n";
        let policy_yaml = |model: &str| {
            format!(
                "lockfileVersion: 1\npolicies:\n  coding:\n    key_strategy: agent_trace\n    tiers: {{ strong: fixture:{model} }}\n    routes: {{}}\n    default_tier: strong\n    tool_use_tier: strong\n    tool_safe_tiers: [strong]\n"
            )
        };
        std::fs::write(&path, initial_yaml)?;
        std::fs::write(&lock, policy_yaml("old"))?;
        let initial = bitrouter_sdk::config::load(&path).await?;
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let runtime = PolicyRuntime::new(
            &initial,
            Some(&path),
            db,
            None,
            crate::eval::settlement::PendingEvalDecisionStore::default(),
            None,
        )
        .await?;
        let active_before = runtime.administration_snapshot();
        assert_eq!(active_before.mode, "frozen");
        assert_eq!(
            active_before.bindings.get("coding").map(String::as_str),
            Some("coding")
        );

        std::fs::write(&path, candidate_yaml)?;
        std::fs::write(&lock, policy_yaml("new"))?;
        let disk = disk_policy(&ConfigSource::File(path.clone())).await?;
        assert_eq!(disk.mode, "adaptive");
        assert!(disk.bindings.contains_key("chat"));
        assert_ne!(disk.digest, active_before.digest);
        let still_active = runtime.administration_snapshot();
        assert_eq!(still_active.mode, active_before.mode);
        assert_eq!(still_active.bindings, active_before.bindings);
        assert_eq!(still_active.digest, active_before.digest);

        let candidate = bitrouter_sdk::config::load(&path).await?;
        let prepared = runtime.prepare_for_config(&candidate, Some(&path)).await?;
        assert_eq!(
            runtime.administration_snapshot().digest,
            active_before.digest
        );
        runtime.commit(prepared);
        let active_after = runtime.administration_snapshot();
        assert_eq!(active_after.view, PolicyView::Active);
        assert_eq!(active_after.mode, disk.mode);
        assert_eq!(active_after.bindings, disk.bindings);
        assert_eq!(active_after.digest, disk.digest);
        Ok(())
    }
}
