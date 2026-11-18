//! Typed, content-limited host inspection shared by the CLI and control API.

use std::sync::Arc;
use {
    bitrouter_ai::control::administration::AgentEntry,
    bitrouter_ai::control::administration::AgentsReport,
    bitrouter_ai::control::administration::ObserveReport,
    bitrouter_ai::control::administration::PolicyInput,
    bitrouter_ai::control::administration::PolicyReport,
    bitrouter_ai::control::administration::PolicyView,
    bitrouter_ai::control::administration::ProviderEntry,
    bitrouter_ai::control::administration::ProvidersReport,
    bitrouter_ai::control::administration::validate_identifier,
};

use crate::config::Config;
use crate::config::routing_table::ConfigRoutingTable;
use anyhow::Result;

use crate::daemon::ObserveStatusProvider;
use crate::paths::ConfigSource;
use crate::policy_lock::{LoadedPolicy, PolicyRuntime};

pub(crate) trait ObserveReportRuntime: Sized {
    fn from_snapshot(snapshot: crate::daemon::ObserveStatusPayload, reachable: bool) -> Self;
}

impl ObserveReportRuntime for ObserveReport {
    fn from_snapshot(snapshot: crate::daemon::ObserveStatusPayload, reachable: bool) -> Self {
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

pub(crate) trait PolicyReportRuntime: Sized {
    fn from_loaded(config: &Config, loaded: Option<&LoadedPolicy>, view: PolicyView) -> Self;
    fn selected(self, name: Option<&str>) -> Result<Self>;
}

impl PolicyReportRuntime for PolicyReport {
    fn from_loaded(config: &Config, loaded: Option<&LoadedPolicy>, view: PolicyView) -> Self {
        let source = loaded.map(|loaded| loaded.document.clone());
        Self {
            view,
            availability: if loaded.is_some() {
                "available"
            } else {
                "not_configured"
            }
            .into(),
            source_digest: loaded.map(|loaded| loaded.digest.clone()),
            policies: source
                .as_ref()
                .map(|source| source.policies.keys().cloned().collect())
                .unwrap_or_default(),
            bindings: config
                .router_policy_bindings()
                .map(|(router_id, policy, _)| (router_id.to_string(), policy.to_string()))
                .collect(),
            source,
        }
    }

    fn selected(self, name: Option<&str>) -> Result<Self> {
        if let Some(name) = name {
            validate_identifier(name)?;
            anyhow::ensure!(
                self.source
                    .as_ref()
                    .is_some_and(|source| source.policies.contains_key(name)),
                "policy_not_found"
            );
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
    // Reduced builds retain configuration inspection without loading an agent
    // execution catalog or claiming that a configured agent can run locally.
    let agents = {
        let mut ids: Vec<_> = config.agents.keys().collect();
        ids.sort();
        ids.into_iter()
            .map(|id| AgentEntry {
                id: id.clone(),
                configured: true,
                in_catalog: false,
                description: "Custom configured agent".into(),
            })
            .collect()
    };
    AgentsReport {
        skill_delivery: skill_delivery_paths(),
        resolved_via: source.into(),
        readiness_checked: false,
        agents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_agent_commands_are_never_reported() -> Result<()> {
        let config = crate::config::parse(
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

    #[test]
    fn named_show_preserves_the_complete_source_and_its_digest() -> Result<()> {
        let document: bitrouter_ai::policy::Document = serde_json::from_str(
            r#"{"spec_version":"policy/1.0.0-beta.1","policies":{"auto":{"tiers":{"strong":{"model":"fixture/strong"}},"default_tier":"strong"},"auto:cost":{"tiers":{"cheap":{"model":"fixture/cheap"}},"default_tier":"cheap"}}}"#,
        )?;
        let digest = crate::policy_lock::semantic_digest(&document)?;
        let loaded = LoadedPolicy {
            path: None,
            document: document.clone(),
            digest: digest.clone(),
        };
        let config = crate::config::parse("inherit_defaults: false\n")?;
        let report = PolicyReport::from_loaded(&config, Some(&loaded), PolicyView::Active);
        let selected = report.clone().selected(Some("auto:cost"))?;
        assert_eq!(selected.source, Some(document));
        assert_eq!(selected.source_digest.as_deref(), Some(digest.as_str()));
        assert_eq!(selected.policies, ["auto", "auto:cost"]);
        assert!(report.selected(Some("missing")).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[tokio::test]
    async fn active_source_and_bindings_change_only_with_the_policy_snapshot() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("bitrouter.yaml");
        let lock = directory.path().join("policy.yaml");
        let initial_yaml = "inherit_defaults: false\nrouters:\n  coding:\n    selection: { kind: policy, policy: coding, base_model: fixture:old }\n";
        let candidate_yaml = "inherit_defaults: false\nrouters:\n  chat:\n    selection: { kind: policy, policy: coding, base_model: fixture:new }\n";
        let policy_yaml = |model: &str| {
            format!(
                "spec_version: policy/1.0.0-beta.1\npolicies:\n  coding:\n    tiers: {{ strong: {{ model: fixture:{model} }} }}\n    default_tier: strong\n"
            )
        };
        std::fs::write(&path, initial_yaml)?;
        std::fs::write(&lock, policy_yaml("old"))?;
        let initial = crate::config::load(&path).await?;
        let runtime = PolicyRuntime::new(
            &initial,
            Some(&path),
            None,
            crate::gateway::observability::decision::PendingEvalDecisionStore::default(),
            None,
        )
        .await?;
        let active_before = runtime.administration_snapshot();
        assert_eq!(
            active_before.bindings.get("coding").map(String::as_str),
            Some("coding")
        );

        std::fs::write(&path, candidate_yaml)?;
        std::fs::write(&lock, policy_yaml("new"))?;
        let disk = disk_policy(&ConfigSource::File(path.clone())).await?;
        assert!(disk.bindings.contains_key("chat"));
        assert_eq!(
            disk.source
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing disk source"))?
                .policies["coding"]
                .tiers["strong"]
                .model,
            "fixture:new"
        );
        assert_ne!(disk.source_digest, active_before.source_digest);
        let still_active = runtime.administration_snapshot();
        assert_eq!(still_active.bindings, active_before.bindings);
        assert_eq!(still_active.source_digest, active_before.source_digest);

        let candidate = crate::config::load(&path).await?;
        let prepared = runtime.prepare_for_config(&candidate, Some(&path)).await?;
        assert_eq!(
            runtime.administration_snapshot().source_digest,
            active_before.source_digest
        );
        runtime.commit(prepared);
        let active_after = runtime.administration_snapshot();
        assert_eq!(active_after.view, PolicyView::Active);
        assert_eq!(active_after.bindings, disk.bindings);
        assert_eq!(active_after.source_digest, disk.source_digest);
        Ok(())
    }
}

pub fn skill_delivery_paths() -> std::collections::BTreeMap<String, String> {
    [
        ("native", "native_activation"),
        (
            "supervised_acp",
            "local_prompt_instructions_when_negotiated",
        ),
        ("supervised_acp_mcp_origin", "unsupported"),
        ("native_launcher", "unsupported"),
        ("inference_only", "unsupported"),
        ("transparent_or_standalone_acp", "unsupported"),
    ]
    .into_iter()
    .map(|(path, mode)| (path.into(), mode.into()))
    .collect()
}
