//! The `list_models` action, implemented over the daemon's live routing table
//! with a static-config fallback.
//!
//! One implementation, two surfaces: `bitrouter models` calls
//! [`RoutableModels::report`] directly, and the origin MCP server's
//! `list_models` tool calls it through the [`ModelsQuery`] port. Both get the
//! same [`ModelsReport`], so the CLI's `--json` and the tool's structured
//! content cannot drift.
//!
//! **Daemon-first, config as the fallback** — the order `route_preview` and
//! `bitrouter route` already use, and for the same reason. The live routing
//! table is what a request will actually be routed against: it reflects
//! `reload`s and providers whose credential is resolved at daemon start-up
//! rather than declared in `bitrouter.yaml` (the `claude-code` and `google-ai`
//! subscription providers), which a static projection marks inactive and drops.
//! It matters more here than for `route_preview`, because `complete` on the
//! same MCP server goes to that daemon: a catalog that disagreed with it would
//! be telling an agent it can route something the next call refuses.
//!
//! The fallback is what buys the phase its headline: with no daemon running at
//! all, `list_models` still answers — from config, and it says so.

use std::path::{Path, PathBuf};

use bitrouter_mcp::actions::models::{ModelsQuery, ModelsReport};
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;

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
        // Resolved per call, not snapshotted at construction: a long-lived
        // `mcp serve` must not answer from the config the machine had when it
        // started (the staleness `route_preview` still has).
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
    async fn list_models(&self, _caller: &CallerAuth) -> Result<ModelsReport, ToolError> {
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

    /// The drift phase 2 removes, asserted across **both** surfaces of the one
    /// action: a model two providers can serve must list both on the CLI path
    /// and through the MCP port, and the two must be the same bytes.
    ///
    /// The MCP surface used to answer this over `GET /v1/models` and keep
    /// `providers.first()`, so an agent asking what could serve `gpt-5` was
    /// told `openai` and never learned `azure` was behind it.
    #[tokio::test]
    async fn both_surfaces_keep_every_provider_of_a_model() {
        let (dir, source) = two_provider_config();
        // No daemon: `socket` names a path nothing is listening on, which is
        // also the point of the next assertion.
        let models = RoutableModels::new(source, Some(dir.path().join("nothing.sock")));

        let cli = models.report().await.expect("cli surface");
        let tool = ModelsQuery::list_models(&models, &CallerAuth::default())
            .await
            .expect("mcp surface");

        for (surface, report) in [("cli", &cli), ("mcp", &tool)] {
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
            serde_json::to_value(&tool).expect("mcp json"),
            "the two surfaces of one action must be the same bytes"
        );
    }

    /// The headline of the phase: the MCP surface answers with **no daemon
    /// running**, where it used to be a `GET /v1/models` that failed outright.
    /// The report says which view it is, so an agent is not left guessing.
    #[tokio::test]
    async fn the_mcp_surface_answers_with_no_daemon_running() {
        let (dir, source) = two_provider_config();
        let report = ModelsQuery::list_models(
            &RoutableModels::new(source, Some(dir.path().join("nothing.sock"))),
            &CallerAuth::default(),
        )
        .await
        .expect("a stopped daemon must not fail list_models");
        assert_eq!(
            report.resolved_via,
            bitrouter_mcp::actions::models::ModelsSource::Config
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

    /// The filter both surfaces share, over the shared report: `--provider
    /// azure` and the tool's `provider: "azure"` are the same narrowing of the
    /// same list.
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
}
