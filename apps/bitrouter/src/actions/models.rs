//! The `list_models` action: *what can BitRouter route, and who can serve it?*
//!
//! One report type is shared by `bro models`, Code sessions, and typed remote
//! control so all retained surfaces preserve the same JSON shape.
//!
//! The element type is [`ModelInfo`] itself — `bitrouter-sdk`'s, not a copy.
//! The copy this crate used to keep held a single `provider: String` and was
//! filled with `providers.first()`, so every model served by more than one
//! provider lost its fallback chain on the way to an agent: it asked what could
//! serve a model and was told one answer where there were three. The wire has
//! always carried the whole list.

use bitrouter_sdk::language_model::routing::ModelInfo;

use super::ToolError;

/// Which view of the catalog answered.
///
/// The two are not interchangeable, and an agent deciding whether it may route
/// a model needs to know which it is holding: [`Self::Live`] is what a running
/// router *will* accept right now, [`Self::Config`] is what a configuration
/// *would* accept if one were started.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelsSource {
    /// A running router answered: the local daemon's live routing table, or a
    /// metered account's catalog. Reflects `reload`s and the state the router
    /// actually resolved at start-up.
    Live,
    /// No router was reachable, so the catalog was projected from static
    /// configuration, resolved the way a daemon would resolve it at start-up
    /// (built-in defaults, stored credentials). Honest but weaker: it is what
    /// the config says *now*, so a file edited since a daemon started would be
    /// listed even though that daemon would refuse it, and nothing a running
    /// daemon learned after start-up is reflected.
    Config,
}

/// Every model BitRouter can route, each with the providers that can serve it.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ModelsReport {
    /// The routable models. Each carries **every** provider that declares it,
    /// in fallback order — not just the first.
    pub models: Vec<ModelInfo>,
    /// Which view produced [`Self::models`].
    pub resolved_via: ModelsSource,
}

impl ModelsReport {
    /// A running router's own catalog.
    pub fn live(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Live,
        }
    }

    /// A static configuration's projection, used when no router answered.
    pub fn from_config(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Config,
        }
    }

    /// Keep only the models `provider` can serve; `None` keeps everything.
    ///
    /// The filter lives on the report rather than in the port so every adapter
    /// narrows the same list with the same rule.
    pub fn filtered(mut self, provider: Option<&str>) -> Self {
        if let Some(provider) = provider {
            self.models
                .retain(|m| m.providers.iter().any(|p| p == provider));
        }
        self
    }
}

/// The `list_models` port shared by local consumers.
///
/// Filtering is deliberately absent: an implementation returns the whole
/// catalog and adapters narrow it with [`ModelsReport::filtered`].
#[async_trait::async_trait]
pub trait ModelsQuery: Send + Sync {
    /// Every routable model, or a `ToolError` when the lookup itself failed.
    async fn list_models(&self) -> Result<ModelsReport, ToolError>;
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
        if let Some(models) = self.live_models().await {
            return Ok(ModelsReport::live(models));
        }
        // Resolved per call, not snapshotted at construction: any long-lived
        // session must not answer from the config the machine had when it
        // started.
        let config = crate::paths::load_config(&self.source).await?;
        Ok(ModelsReport::from_config(
            crate::commands::list_models(&config).await?,
        ))
    }

    /// The live routing table's catalog, or `None` when no daemon answered.
    ///
    /// Best-effort throughout: an absent socket, a daemon that is down, a
    /// transport error, or a daemon too old to know the command all mean
    /// "fall back", never "fail". The distinction the caller needs — live
    /// versus projected — is carried in the report itself, so degrading here
    /// is visible rather than silent.
    async fn live_models(&self) -> Option<Vec<bitrouter_sdk::language_model::routing::ModelInfo>> {
        let socket = self.socket.as_deref().filter(|s| endpoint_live(s))?;
        match daemon::send_command(socket, &DaemonCommand::Models).await {
            Ok(DaemonResponse::Models { models }) => Some(models),
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

/// Whether the control endpoint is currently bound, so an abandoned socket
/// file does not cost a connect timeout on every call.
fn endpoint_live(socket: &Path) -> bool {
    daemon::endpoint_in_use(socket)
}

#[async_trait::async_trait]
impl ModelsQuery for RoutableModels {
    /// `caller` is ignored, and that is a property of this deployment rather
    /// than an oversight: the control socket is a single-machine channel and
    /// the config is this machine's, so there is only one catalog to report.
    /// The parameter stays on the port because the cloud implementation of the
    /// same action does forward it, listing each caller's own catalog.
    async fn list_models(&self) -> Result<ModelsReport, ToolError> {
        self.report()
            .await
            .map_err(|e| ToolError::new(e.to_string()))
    }
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

    /// A model two providers can serve must list both on the direct path and
    /// through the injected port, and the two must be the same bytes.
    #[tokio::test]
    async fn both_surfaces_keep_every_provider_of_a_model() {
        let (dir, source) = two_provider_config();
        // No daemon: `socket` names a path nothing is listening on, which is
        // also the point of the next assertion.
        let models = RoutableModels::new(source, Some(dir.path().join("nothing.sock")));

        let cli = models.report().await.expect("cli surface");
        let port = ModelsQuery::list_models(&models)
            .await
            .expect("port surface");

        for (surface, report) in [("cli", &cli), ("port", &port)] {
            let gpt5 = report
                .models
                .iter()
                .find(|m| m.id == "gpt-5")
                .unwrap_or_else(|| panic!("{surface} surface lost gpt-5"));
            assert_eq!(
                gpt5.providers,
                vec!["azure".to_string(), "openai".to_string()],
                "{surface} surface dropped a provider from the fallback chain"
            );
            // A single-provider model is unaffected — the chain is reported as
            // it is, not padded.
            let o4 = report
                .models
                .iter()
                .find(|m| m.id == "o4-mini")
                .unwrap_or_else(|| panic!("{surface} surface lost o4-mini"));
            assert_eq!(o4.providers, vec!["openai".to_string()]);
        }
        assert_eq!(
            serde_json::to_value(&cli).expect("cli json"),
            serde_json::to_value(&port).expect("port json"),
            "the two surfaces of one action must be the same bytes"
        );
    }

    /// The injected port answers with no daemon running and reports which view
    /// it used, so an agent is not left guessing.
    #[tokio::test]
    async fn the_port_answers_with_no_daemon_running() {
        let (dir, source) = two_provider_config();
        let report = ModelsQuery::list_models(&RoutableModels::new(
            source,
            Some(dir.path().join("nothing.sock")),
        ))
        .await
        .expect("a stopped daemon must not fail list_models");
        assert_eq!(
            report.resolved_via,
            crate::actions::models::ModelsSource::Config
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
            .report(crate::actions::route::RouteInput {
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
