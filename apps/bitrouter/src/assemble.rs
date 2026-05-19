//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config.

use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use bitrouter_sdk::App;
use bitrouter_sdk::acp::{AcpStdioExecutor, ConfigAcpRoutingTable};
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::protocol::OutboundDispatch;
use bitrouter_sdk::language_model::{AuthAppliers, HttpExecutor, HttpTimeouts};
use bitrouter_sdk::mcp::{ConfigMcpRoutingTable, RmcpExecutor};

use bitrouter_auth::AuthHook;
use bitrouter_guardrails::{Action, GuardrailPreHook, GuardrailRule, GuardrailStreamHook, RuleSet};
use bitrouter_observe::{OtlpExportHook, PrometheusHook};
use bitrouter_policy::{PolicyHook, PolicyStore};
use bitrouter_sdk::MetricsRenderer;

use crate::metering::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable};

/// A running application plus the database pool it was assembled over (the
/// caller keeps the pool for management commands — key creation, etc.).
pub struct Assembled {
    /// The fully wired application.
    pub app: App,
    /// The shared database pool.
    pub pool: SqlitePool,
    /// The policy store wired into the language_model pipeline. Held by the
    /// caller (the daemon) so `bitrouter reload` / SIGHUP can call
    /// [`bitrouter_policy::PolicyStore::reload`] alongside the routing-table
    /// reload — reload must not affect in-flight requests.
    pub policy_store: Arc<PolicyStore>,
}

/// Assemble an [`App`] from a parsed config: connect the database, run every
/// plugin's migrations, build the routing table + executor, and wire the
/// builtin hooks onto the `language_model` pipeline.
pub async fn build_app(config: &Config) -> Result<Assembled> {
    build_app_with_path(config, None).await
}

/// Like [`build_app`], but remembering the config's source path so the routing
/// table's `reload()` (driven by `bitrouter reload` / `SIGHUP`) can re-read it.
pub async fn build_app_with_path(
    config: &Config,
    config_path: Option<&std::path::Path>,
) -> Result<Assembled> {
    // ---- database + migrations (each plugin owns its own tables) ----
    let pool = SqlitePool::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to database {}", config.database.url))?;
    bitrouter_auth::migrate(&pool)
        .await
        .context("running bitrouter-auth migrations")?;
    crate::metering::migrate(&pool)
        .await
        .context("running metering migrations")?;
    // The SQLite database holds SHA-256 hashes of every virtual key, plus
    // the metering audit trail. On Unix, tighten the file permissions to
    // 0600 so a co-tenant on the host can't read it. The control socket
    // already does the same in `daemon::run_control_socket`.
    #[cfg(unix)]
    if let Some(path) = sqlite_file_path(&config.database.url) {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&path, perms) {
                tracing::warn!(?path, %e, "failed to chmod 0600 on sqlite db file");
            }
        }
    }

    // ---- routing table + upstream executor ----
    // Best-effort model discovery for providers with `auto_discover: true`
    // and no declared models — failures WARN, never abort.
    let mut resolved = config.clone();
    // Fill empty fields on built-in providers from the compiled-in catalog
    // (api_base / api_protocol / api_key-from-env). No-op when
    // `inherit_defaults: false`, and never overrides user-set fields.
    bitrouter_providers::apply_builtin_defaults(&mut resolved);
    bitrouter_sdk::config::discover_models(&mut resolved).await;
    let routing_table = Arc::new(match config_path {
        Some(path) => ConfigRoutingTable::from_config_with_path(resolved, path),
        None => ConfigRoutingTable::from_config(resolved),
    });
    // Per-provider auth appliers — currently only GitHub Copilot, whose
    // OAuth-driven Bearer is resolved + cached by the applier on every
    // request. Listed only when the user configures the provider, so an
    // operator who doesn't use Copilot doesn't pay a token-store read.
    let auth_appliers = build_auth_appliers(config)?;
    let executor = Arc::new(
        HttpExecutor::with_dispatch_and_auth(
            HttpTimeouts::default(),
            OutboundDispatch::builtin(),
            auth_appliers,
        )
        .context("building the upstream HTTP executor")?,
    );

    // ---- pricing, metering, policy, guardrails — all derived from config ----
    let pricing = Arc::new(build_pricing_table(config));
    let metering_store = MeteringStore::new(pool.clone());
    let metering_store_for_policy = metering_store.clone();
    let metering_store_for_recorder = metering_store.clone();
    let pricing_for_recorder = pricing.clone();
    let policy_store: Arc<PolicyStore> = Arc::new(load_policy_store(config).await?);
    let policy_store_for_reload = policy_store.clone();
    let guardrail_rules = build_guardrail_rules(config)?;

    // Prometheus exporter. The same `Arc` is both an
    // `ObserveHook` (writes) and a `MetricsRenderer` (reads from `/metrics`).
    let prometheus: Arc<PrometheusHook> = Arc::new(PrometheusHook::new());
    let prometheus_for_observe = prometheus.clone();
    let metrics_renderer: Arc<dyn MetricsRenderer> = prometheus;

    // Optional OTLP/HTTP JSON tracer. Configured under
    // `plugins.bitrouter-observe.otlp_endpoint`; absent → exporter not wired.
    let otlp_endpoint: Option<String> = config
        .plugins
        .get("bitrouter-observe")
        .and_then(|c| c.get("otlp_endpoint"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Optional MCP pure-routing pipeline — wired only when the config
    // declares at least one upstream MCP server. The pipeline is independent
    // of the language_model pipeline (different hook traits, different
    // routing table) and carries no settlement.
    let mcp_routing = if config.mcp_servers.is_empty() {
        None
    } else {
        Some(Arc::new(
            ConfigMcpRoutingTable::from_configs(
                config
                    .mcp_servers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            )
            .context("building the MCP routing table from config.mcp_servers")?,
        ))
    };
    let mcp_executor: Option<Arc<RmcpExecutor>> =
        mcp_routing.as_ref().map(|_| Arc::new(RmcpExecutor::new()));

    // Optional ACP pure-routing pipeline — wired only when the config
    // declares at least one upstream agent. Mirrors the MCP wiring above;
    // the `bitrouter agent-proxy <id>` CLI dispatches against this pipeline.
    let acp_routing = if config.agents.is_empty() {
        None
    } else {
        Some(Arc::new(
            ConfigAcpRoutingTable::from_configs(
                config.agents.iter().map(|(k, v)| (k.clone(), v.clone())),
            )
            .context("building the ACP routing table from config.agents")?,
        ))
    };
    let acp_executor: Option<Arc<AcpStdioExecutor>> = acp_routing
        .as_ref()
        .map(|_| Arc::new(AcpStdioExecutor::new()));

    let pool_for_hooks = pool.clone();
    let app = App::builder()
        .skip_auth(config.server.skip_auth)
        .metrics_renderer(metrics_renderer)
        .language_model(move |lm| {
            lm.routing_table(routing_table).executor(executor);
            // Stage 1, in order: auth → policy → guardrail (upstream).
            lm.pre_request_hook(AuthHook::new(pool_for_hooks.clone()));
            lm.pre_request_hook(PolicyHook::new(
                policy_store.clone(),
                Some(metering_store_for_policy),
            ));
            if !guardrail_rules.is_empty() {
                lm.pre_request_hook(GuardrailPreHook::new(guardrail_rules.clone()));
                // StreamHook stage: guardrail downstream redaction / abort.
                lm.stream_hook(GuardrailStreamHook::new(guardrail_rules));
            }
            // The Prometheus hook is registered through Arc cloning so the
            // server's /metrics route reads the same accumulator the pipeline
            // writes to. ObserveHook is read-only / error-swallowing so a
            // wiring problem never affects the request path.
            lm.observe_hook(PrometheusObserve(prometheus_for_observe.clone()));
            // Optional OTLP exporter — wired only when configured.
            if let Some(endpoint) = otlp_endpoint.as_ref() {
                lm.observe_hook(OtlpExportHook::new(endpoint));
            }
            // OSS metering recorder — writes one `requests` row per
            // settled request with the estimated µUSD from the pricing
            // table. The policy module reads back through `MeteringStore`
            // for spend caps.
            lm.settlement_recorder(MeteringRecorder::new(
                metering_store_for_recorder,
                pricing_for_recorder,
            ));
        });
    // Apply the optional MCP pipeline configuration in a second builder step
    // so the language_model configuration above stays the same shape it has
    // had since v0.
    let app = match (mcp_routing, mcp_executor) {
        (Some(table), Some(exec)) => app.mcp(move |m| {
            m.routing_table(table).executor(exec);
        }),
        _ => app,
    };
    // ACP pipeline — separate match because it's an independent optional
    // configuration step on the same builder.
    let app = match (acp_routing, acp_executor) {
        (Some(table), Some(exec)) => app.acp(move |a| {
            a.routing_table(table).executor(exec);
        }),
        _ => app,
    };
    let app = app.build().context("building the App")?;

    Ok(Assembled {
        app,
        pool,
        policy_store: policy_store_for_reload,
    })
}

/// Build the per-provider `AuthAppliers` registry. Each entry covers a
/// provider whose credential flow needs more than the per-protocol
/// `Transport::authorise` default (today: only GitHub Copilot).
fn build_auth_appliers(config: &Config) -> Result<AuthAppliers> {
    let mut appliers = AuthAppliers::new();
    if config.providers.contains_key("github-copilot") {
        let token_store_path = bitrouter_providers::oauth::TokenStore::default_path()
            .map(|s| s.path().to_path_buf())
            .context("resolving OAuth token store path for github-copilot")?;
        let applier = bitrouter_providers::copilot::CopilotAuthApplier::new(token_store_path)
            .context("building the github-copilot AuthApplier")?;
        appliers.register("github-copilot", Arc::new(applier));
    }
    Ok(appliers)
}

fn build_pricing_table(config: &Config) -> PricingTable {
    let mut table = PricingTable::new();
    for (provider_id, provider) in &config.providers {
        for model in &provider.models {
            if let Some(pricing) = model.pricing {
                table.insert(
                    provider_id.clone(),
                    model.id.clone(),
                    ModelPricing::new(
                        pricing.input_micro_usd_per_token,
                        pricing.output_micro_usd_per_token,
                    ),
                );
            }
        }
    }
    table
}

/// Load the `PolicyStore` from `plugins.bitrouter-policy.policy_dir`, if set.
async fn load_policy_store(config: &Config) -> Result<PolicyStore> {
    let dir = config
        .plugins
        .get("bitrouter-policy")
        .and_then(|c| c.get("policy_dir"))
        .and_then(|v| v.as_str());
    match dir {
        Some(dir) => PolicyStore::load_dir(dir)
            .await
            .with_context(|| format!("loading policies from {dir}")),
        None => Ok(PolicyStore::new()),
    }
}

/// Build the guardrail `RuleSet` from `plugins.bitrouter-guardrails.custom_patterns`.
/// Each entry is `{ name, pattern, action: "block" | "redact" }`.
fn build_guardrail_rules(config: &Config) -> Result<RuleSet> {
    let Some(patterns) = config
        .plugins
        .get("bitrouter-guardrails")
        .and_then(|c| c.get("custom_patterns"))
        .and_then(|v| v.as_array())
    else {
        return Ok(RuleSet::new());
    };
    let mut set = RuleSet::new();
    for entry in patterns {
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .context("guardrail pattern missing 'name'")?;
        let pattern = entry
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("guardrail pattern missing 'pattern'")?;
        let action = match entry.get("action").and_then(|v| v.as_str()) {
            Some("block") | None => Action::Block,
            Some("redact") => Action::Redact,
            Some(other) => anyhow::bail!("unknown guardrail action '{other}'"),
        };
        set.push(
            GuardrailRule::new(name, pattern, action)
                .with_context(|| format!("compiling guardrail pattern '{name}'"))?,
        );
    }
    Ok(set)
}

/// A `language_model::ObserveHook` that delegates to a shared `Arc<PrometheusHook>`.
/// We need this wrapper because `PipelineBuilder::observe_hook` takes a hook by
/// value (and wraps it in an internal `Arc`), but we want to *share* the same
/// `Arc<PrometheusHook>` between the writer (the pipeline) and the reader
/// (`GET /metrics`) so both see the same accumulator.
struct PrometheusObserve(Arc<PrometheusHook>);

#[async_trait::async_trait]
impl bitrouter_sdk::language_model::ObserveHook for PrometheusObserve {
    async fn after_phase(
        &self,
        phase: bitrouter_sdk::language_model::Phase,
        ctx: &bitrouter_sdk::language_model::PipelineContext,
    ) {
        self.0.after_phase(phase, ctx).await
    }
    fn stream_interest(&self) -> bitrouter_sdk::language_model::StreamInterest {
        self.0.stream_interest()
    }
    async fn on_stream_part(
        &self,
        ctx: &bitrouter_sdk::language_model::StreamContext,
        part: &bitrouter_sdk::language_model::StreamPart,
    ) {
        self.0.on_stream_part(ctx, part).await
    }
    async fn on_request_end(
        &self,
        ctx: &bitrouter_sdk::language_model::PipelineContext,
        outcome: &bitrouter_sdk::language_model::RequestOutcome,
    ) {
        self.0.on_request_end(ctx, outcome).await
    }
}

/// Extract the file path from a sqlite URL. Returns `None` for `:memory:` or
/// non-file URLs. Accepts the `sqlite://`, `sqlite:` and bare-path forms that
/// `sqlx::SqlitePool::connect` accepts.
#[cfg(unix)]
fn sqlite_file_path(url: &str) -> Option<std::path::PathBuf> {
    let after_scheme = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    // Strip a leading `//` (sqlite://path → /path; treat as filesystem path)
    let path = after_scheme.split('?').next().unwrap_or(after_scheme);
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    Some(std::path::PathBuf::from(path))
}

#[cfg(all(test, unix))]
mod sqlite_path_tests {
    use super::sqlite_file_path;
    use std::path::PathBuf;

    #[test]
    fn parses_common_sqlite_urls() {
        assert_eq!(
            sqlite_file_path("sqlite:///var/lib/bitrouter.db"),
            Some(PathBuf::from("/var/lib/bitrouter.db"))
        );
        assert_eq!(
            sqlite_file_path("sqlite:bitrouter.db"),
            Some(PathBuf::from("bitrouter.db"))
        );
        assert_eq!(
            sqlite_file_path("sqlite:bitrouter.db?cache=shared"),
            Some(PathBuf::from("bitrouter.db"))
        );
        assert_eq!(sqlite_file_path(":memory:"), None);
        assert_eq!(sqlite_file_path("sqlite::memory:"), None);
    }
}
