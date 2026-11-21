//! Model inspection for the operator CLI and HTTP control surface.
//! Reports retain every provider in each model's fallback chain.

use crate::actions::administration::PolicyReportRuntime;
use std::collections::BTreeSet;
use {
    bitrouter_ai::control::models::ModelsReport, bitrouter_ai::control::models::RouterReadiness,
    bitrouter_ai::control::models::RouterSelectionStatus,
    bitrouter_ai::control::models::RouterSource, bitrouter_ai::control::models::RouterStatus,
};

use crate::config::Config;
use crate::config::router::RouterConfigSource;
use crate::config::router::RouterInventorySelection;
use crate::config::routing_table::ConfigRoutingTable;
use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::routing::RoutingTable;

#[derive(Clone, Copy)]
pub(crate) enum PolicyReadiness<'a> {
    Known(&'a bitrouter_ai::control::administration::PolicyReport),
    Invalid,
}

use std::path::{Path, PathBuf};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::paths::ConfigSource;

/// Lists what BitRouter can route: the daemon's own catalog when one answers,
/// the configuration's projection otherwise.
pub struct RoutableModels {
    socket: Option<PathBuf>,
    source: ConfigSource,
}

impl RoutableModels {
    /// List through the daemon on `socket`, falling back to the config
    /// `source` resolves to.
    ///
    /// `socket` is `Option` because a caller that could not resolve one (no
    /// config file, an unreadable one) still gets the config answer rather
    /// than a failure.
    pub fn new(source: ConfigSource, socket: Option<PathBuf>) -> Self {
        Self { socket, source }
    }

    /// The catalog, live if a daemon answers and from config if not.
    ///
    /// Only the config path can fail, and only when the config itself cannot
    /// be read: a daemon that is merely absent is not an error, it is the
    /// reason the fallback exists.
    pub async fn report(&self) -> anyhow::Result<ModelsReport> {
        if let Some(report) = self.live_models().await {
            return Ok(report);
        }
        // Resolved per call, not snapshotted at construction: any long-lived
        // session must not answer from the config the machine had when it
        // started.
        let config = crate::paths::load_config(&self.source).await?;
        let mut resolved = crate::commands::resolve_static(config.clone());
        crate::config::discover_models(&mut resolved).await;
        let models = ConfigRoutingTable::from_config(resolved.clone()).list_models();
        let policies = disk_policy_report(&config, &self.source).await;
        let policy_readiness = match &policies {
            Ok(report) => PolicyReadiness::Known(report),
            Err(_) => PolicyReadiness::Invalid,
        };
        let routers = router_statuses(&resolved, policy_readiness).await?;
        Ok(ModelsReport::from_config(models).with_routers(Some(routers)))
    }

    /// The live routing table's catalog, or `None` when no daemon answered.
    ///
    /// Best-effort throughout: an absent socket, a daemon that is down, a
    /// transport error, or a daemon too old to know the command all mean
    /// "fall back", never "fail". The distinction the caller needs — live
    /// versus projected — is carried in the report itself, so degrading here
    /// is visible rather than silent.
    async fn live_models(&self) -> Option<ModelsReport> {
        let socket = self.socket.as_deref().filter(|s| endpoint_live(s))?;
        match daemon::send_command(socket, &DaemonCommand::Models).await {
            Ok(DaemonResponse::Models { models, routers }) => {
                Some(ModelsReport::live(models).with_routers(routers))
            }
            Ok(DaemonResponse::Error { message }) => {
                tracing::debug!(%message, "daemon refused `models` — listing from config");
                None
            }
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(error = %e, "daemon `models` failed — listing from config");
                None
            }
        }
    }
}

/// Build the redacted router inventory for one already-resolved config.
/// A known-invalid disk policy is `not_ready`. Older runtimes that cannot
/// supply this view omit the whole inventory before reaching this function.
pub(crate) async fn router_statuses(
    config: &Config,
    policy_readiness: PolicyReadiness<'_>,
) -> anyhow::Result<Vec<RouterStatus>> {
    let inventory = config.router_inventory()?;
    let table = ConfigRoutingTable::from_config(config.clone());
    let mut statuses = Vec::with_capacity(inventory.len() + 1);

    for entry in inventory {
        let (selection, bound_policy) = match entry.selection {
            RouterInventorySelection::Model { model, routing } => (
                RouterSelectionStatus::Model {
                    model,
                    routing: routing.into(),
                },
                None,
            ),
            RouterInventorySelection::Policy {
                policy,
                base_model,
                routing,
            } => (
                RouterSelectionStatus::Policy {
                    policy: policy.clone(),
                    base_model,
                    routing: routing.into(),
                },
                Some(policy),
            ),
        };
        let source = match entry.source {
            RouterConfigSource::User => RouterSource::User,
        };
        let address = match source {
            RouterSource::User => format!("bitrouter/{}", entry.id),
            RouterSource::Default => String::new(),
        };
        let policy_problem = bound_policy
            .as_deref()
            .map(|policy| match policy_readiness {
                PolicyReadiness::Known(report)
                    if report.policies.iter().any(|name| name == policy) =>
                {
                    None
                }
                PolicyReadiness::Known(_) => Some((
                    RouterReadiness::NotReady,
                    format!("bound policy '{policy}' is unavailable"),
                )),
                PolicyReadiness::Invalid => Some((
                    RouterReadiness::NotReady,
                    "policy lock is missing or invalid".to_string(),
                )),
            });
        let (readiness, reason) = match policy_problem.flatten() {
            Some((readiness, reason)) => (readiness, Some(reason)),
            None => match table.resolve_model(&address).await {
                Ok(resolution) => {
                    if let (Some(policy), PolicyReadiness::Known(report)) =
                        (bound_policy.as_deref(), policy_readiness)
                    {
                        let candidates = crate::actions::route::policy_candidate_models(
                            Some(report),
                            Some(policy),
                            &resolution.clean_model,
                        );
                        let routable = crate::actions::route::resolvable_policy_candidates(
                            &table,
                            candidates.clone(),
                            &resolution.prefs,
                        )
                        .await
                        .into_iter()
                        .collect::<BTreeSet<_>>();
                        let missing = candidates
                            .into_iter()
                            .filter(|candidate| !routable.contains(candidate))
                            .collect::<Vec<_>>();
                        if missing.is_empty() {
                            (RouterReadiness::Ready, None)
                        } else {
                            (
                                RouterReadiness::NotReady,
                                Some(format!(
                                    "bound policy '{policy}' has unroutable candidate models: {}",
                                    missing.join(", ")
                                )),
                            )
                        }
                    } else {
                        match table
                            .route_resolved(
                                &resolution.clean_model,
                                &resolution.prefs,
                                &CallerContext::local(),
                            )
                            .await
                        {
                            Ok(chain) if !chain.is_empty() => (RouterReadiness::Ready, None),
                            Ok(_) => (
                                RouterReadiness::NotReady,
                                Some("router resolves to an empty provider chain".to_string()),
                            ),
                            Err(error) => (RouterReadiness::NotReady, Some(error.to_string())),
                        }
                    }
                }
                Err(error) => (RouterReadiness::NotReady, Some(error.to_string())),
            },
        };
        statuses.push(RouterStatus {
            id: entry.id,
            address,
            source,
            readiness,
            reason,
            selection: Some(selection),
            system_prompt_default: entry.defaults.system_prompt_present,
            parameter_defaults: entry.defaults.param_keys,
            binding_digest: Some(entry.binding_digest),
        });
    }

    if !statuses.iter().any(|router| router.id == "coding") {
        statuses.push(RouterStatus {
            id: "coding".to_string(),
            address: "bitrouter/coding".to_string(),
            source: RouterSource::Default,
            readiness: RouterReadiness::Uninitialized,
            reason: Some("coding router is not initialized".to_string()),
            selection: None,
            system_prompt_default: false,
            parameter_defaults: Vec::new(),
            binding_digest: None,
        });
    }
    statuses.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(statuses)
}

pub async fn disk_router_statuses(source: &ConfigSource) -> Option<Vec<RouterStatus>> {
    let config = crate::paths::load_config(source).await.ok()?;
    disk_router_statuses_for_config(&config, source).await
}

pub(crate) async fn disk_router_statuses_for_config(
    config: &Config,
    source: &ConfigSource,
) -> Option<Vec<RouterStatus>> {
    let mut resolved = crate::commands::resolve_static(config.clone());
    crate::config::discover_models(&mut resolved).await;
    let policies = disk_policy_report(config, source).await;
    let policy_readiness = match &policies {
        Ok(report) => PolicyReadiness::Known(report),
        Err(_) => PolicyReadiness::Invalid,
    };
    router_statuses(&resolved, policy_readiness).await.ok()
}

async fn disk_policy_report(
    config: &Config,
    source: &ConfigSource,
) -> anyhow::Result<bitrouter_ai::control::administration::PolicyReport> {
    let path = match source {
        ConfigSource::File(path) => Some(path.as_path()),
        _ => None,
    };
    let loaded = crate::policy_source::load_for_config(config, path).await?;
    Ok(
        bitrouter_ai::control::administration::PolicyReport::from_loaded(
            config,
            loaded.as_ref(),
            bitrouter_ai::control::administration::PolicyView::Disk,
        ),
    )
}

/// Whether the control endpoint is currently bound, so an abandoned socket
/// file does not cost a connect timeout on every call.
fn endpoint_live(socket: &Path) -> bool {
    daemon::endpoint_in_use(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config declaring one model behind **two** providers, plus one behind
    /// a single provider. No `auto_discover`, so nothing here touches the
    /// network.
    fn two_provider_config() -> (tempfile::TempDir, ConfigSource) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("bitrouter.yaml");
        std::fs::write(
            &config,
            r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k1
    models: [{ id: gpt-5 }, { id: o4-mini }]
  azure:
    api_base: https://example.openai.azure.com
    api_key: k2
    models: [{ id: gpt-5 }]
"#,
        )
        .expect("write config");
        (dir, ConfigSource::File(config))
    }

    #[tokio::test]
    async fn report_keeps_every_provider_of_a_model() -> anyhow::Result<()> {
        let (dir, source) = two_provider_config();
        let models = RoutableModels::new(source, Some(dir.path().join("nothing.sock")));
        let report = models.report().await?;
        let gpt5 = report
            .models
            .iter()
            .find(|m| m.id == "gpt-5")
            .ok_or_else(|| anyhow::anyhow!("report lost gpt-5"))?;
        assert_eq!(
            gpt5.providers,
            vec!["azure".to_string(), "openai".to_string()]
        );
        let o4 = report
            .models
            .iter()
            .find(|m| m.id == "o4-mini")
            .ok_or_else(|| anyhow::anyhow!("report lost o4-mini"))?;
        assert_eq!(o4.providers, vec!["openai".to_string()]);
        Ok(())
    }

    /// A stopped daemon leaves the configuration view available.
    #[tokio::test]
    async fn report_answers_with_no_daemon_running() {
        let (dir, source) = two_provider_config();
        let report = RoutableModels::new(source, Some(dir.path().join("nothing.sock")))
            .report()
            .await
            .expect("a stopped daemon must not fail list_models");
        assert_eq!(
            report.resolved_via,
            bitrouter_ai::control::models::ModelsSource::Config
        );
        assert!(!report.models.is_empty());
    }

    /// An unresolvable socket is the same answer, not a different failure.
    #[tokio::test]
    async fn no_socket_at_all_still_lists_from_config() {
        let (_dir, source) = two_provider_config();
        let report = RoutableModels::new(source, None)
            .report()
            .await
            .expect("no socket must not fail list_models");
        assert!(report.models.iter().any(|m| m.id == "gpt-5"));
    }

    /// Every adapter uses the same provider filter over the shared report.
    #[tokio::test]
    async fn the_provider_filter_narrows_the_shared_report() {
        let (_dir, source) = two_provider_config();
        let report = RoutableModels::new(source, None)
            .report()
            .await
            .expect("report");
        let azure = report.clone().filtered(Some("azure"));
        assert_eq!(
            azure.models.iter().map(|m| &m.id).collect::<Vec<_>>(),
            vec!["gpt-5"],
            "azure declares only gpt-5"
        );
        // Filtering keeps the *whole* chain of the models it keeps — it is a
        // row filter, not a column one.
        assert_eq!(
            azure.models[0].providers,
            vec!["azure".to_string(), "openai".to_string()]
        );
        assert!(report.filtered(Some("nobody")).models.is_empty());
    }

    #[tokio::test]
    async fn router_inventory_is_redacted_and_coding_is_not_callable_until_bound()
    -> anyhow::Result<()> {
        let config = crate::config::parse(
            r#"
inherit_defaults: false
providers:
  demo:
    api_base: https://api.example.test
    api_key: key
    active: true
    models: [{ id: demo-model }]
routers:
  review:
    selection:
      kind: model
      model: demo-model
    defaults:
      system_prompt: top secret prompt
      params:
        private_token: top secret value
"#,
        )?;
        let policies = bitrouter_ai::control::administration::PolicyReport::from_loaded(
            &config,
            None,
            bitrouter_ai::control::administration::PolicyView::Disk,
        );
        let routers = router_statuses(&config, PolicyReadiness::Known(&policies)).await?;

        let review = routers
            .iter()
            .find(|router| router.id == "review")
            .ok_or_else(|| anyhow::anyhow!("review router missing"))?;
        assert_eq!(review.address, "bitrouter/review");
        assert_eq!(review.source, RouterSource::User);
        assert!(review.system_prompt_default);
        assert_eq!(review.parameter_defaults, ["private_token"]);

        let coding = routers
            .iter()
            .find(|router| router.id == "coding")
            .ok_or_else(|| anyhow::anyhow!("coding diagnostic missing"))?;
        assert_eq!(coding.readiness, RouterReadiness::Uninitialized);
        assert!(
            !ConfigRoutingTable::from_config(config.clone())
                .list_models()
                .iter()
                .any(|model| model.id == "bitrouter/coding"),
            "an uninitialized diagnostic must not advertise a callable model"
        );

        let json = serde_json::to_string(&routers)?;
        assert!(!json.contains("top secret prompt"));
        assert!(!json.contains("top secret value"));
        assert!(json.contains("private_token"));
        Ok(())
    }

    #[tokio::test]
    async fn disk_router_with_missing_policy_is_not_ready_instead_of_unknown() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.yaml");
        std::fs::write(
            &path,
            r#"
inherit_defaults: false
providers:
  demo:
    api_base: https://api.example.test
    api_key: key
    active: true
    models: [{ id: demo-model }]
routers:
  coding:
    selection:
      kind: policy
      policy: missing
      base_model: demo-model
"#,
        )?;

        let report = RoutableModels::new(ConfigSource::File(path), None)
            .report()
            .await?;
        let coding = report
            .routers
            .as_deref()
            .and_then(|routers| routers.iter().find(|router| router.id == "coding"))
            .ok_or_else(|| anyhow::anyhow!("coding router diagnostic missing"))?;
        assert_eq!(coding.readiness, RouterReadiness::NotReady);
        assert!(
            coding
                .reason
                .as_deref()
                .is_some_and(|reason| reason != "policy runtime readiness is unavailable"),
            "reason: {:?}",
            coding.reason
        );
        Ok(())
    }

    #[tokio::test]
    async fn policy_router_with_unroutable_policy_target_is_not_ready() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.yaml");
        std::fs::write(
            &path,
            r#"
inherit_defaults: false
providers:
  demo:
    api_base: https://api.example.test
    api_key: key
    active: true
    models: [{ id: demo-model }]
routers:
  coding:
    selection:
      kind: policy
      policy: coding
      base_model: demo-model
"#,
        )?;
        std::fs::write(
            dir.path().join("policy.yaml"),
            r#"spec_version: policy/1.0.0-beta.1
policies:
  coding:
    tiers:
      strong: { model: unavailable-model }
    routes: {}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#,
        )?;

        let report = RoutableModels::new(ConfigSource::File(path), None)
            .report()
            .await?;
        let coding = report
            .routers
            .as_deref()
            .and_then(|routers| routers.iter().find(|router| router.id == "coding"))
            .ok_or_else(|| anyhow::anyhow!("coding router diagnostic missing"))?;
        assert_eq!(coding.readiness, RouterReadiness::NotReady);
        assert!(
            coding
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("unavailable-model")),
            "reason: {:?}",
            coding.reason
        );
        Ok(())
    }

    #[tokio::test]
    async fn listed_subscription_selector_is_accepted_by_route_preview() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("bitrouter.yaml");
        std::fs::write(
            &path,
            r#"
inherit_defaults: false
providers:
  openai-codex:
    api_base: https://chatgpt.com/backend-api/codex
    api_key: subscription
    class: first-party-subscription
    models:
      - id: openai/gpt-5.6-sol
        provider_model_id: gpt-5.6-sol
"#,
        )?;
        let source = ConfigSource::File(path);
        let models = RoutableModels::new(source.clone(), None).report().await?;
        let selector = models
            .models
            .first()
            .ok_or_else(|| anyhow::anyhow!("subscription model was not listed"))?;
        assert_eq!(selector.id, "openai-codex:openai/gpt-5.6-sol");

        let route = crate::actions::route::RouteAction::new(source, None)
            .report(bitrouter_ai::control::route::RouteInput {
                model: selector.id.clone(),
                prompt: None,
            })
            .await?;
        assert_eq!(route.provider_chain.len(), 1);
        assert_eq!(route.provider_chain[0].provider, "openai-codex");
        assert_eq!(route.provider_chain[0].service_id, "gpt-5.6-sol");
        Ok(())
    }
}
