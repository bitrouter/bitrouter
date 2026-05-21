//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config.

use std::sync::Arc;

use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

use bitrouter_sdk::App;
use bitrouter_sdk::acp::{AcpStdioExecutor, ConfigAcpRoutingTable};
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::protocol::OutboundDispatch;
use bitrouter_sdk::language_model::{AuthAppliers, HttpExecutor, HttpTimeouts};
use bitrouter_sdk::mcp::{ConfigMcpRoutingTable, RmcpExecutor};

use bitrouter_guardrails::{Action, GuardrailPreHook, GuardrailRule, GuardrailStreamHook, RuleSet};
use bitrouter_observe::otel::{OtelConfig, OtelExporter};
use bitrouter_sdk::MetricsRenderer;

use crate::auth::AuthHook;
use crate::metering::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable};
use crate::policy::{PolicyHook, PolicyStore};

/// A running application plus the database pool it was assembled over (the
/// caller keeps the pool for management commands — key creation, etc.).
pub struct Assembled {
    /// The fully wired application.
    pub app: App,
    /// The shared database pool.
    pub pool: SqlitePool,
    /// The policy store wired into the language_model pipeline. Held by the
    /// caller (the daemon) so `bitrouter reload` / SIGHUP can call
    /// [`PolicyStore::reload`] alongside the routing-table reload — reload
    /// must not affect in-flight requests.
    pub policy_store: Arc<PolicyStore>,
    /// Concrete handle on the routing table. The pipeline above also
    /// holds the same `Arc` (via `&dyn RoutingTable`), but reload code
    /// needs the concrete type to call
    /// [`ConfigRoutingTable::replace_config`] when there's no source
    /// file to re-read from (zero-config mode).
    pub routing_table: Arc<ConfigRoutingTable>,
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
    // `SqlitePool::connect(url)` parses the DSN with sqlx's default
    // open mode (read-write, *not* create), so a fresh
    // `sqlite://./bitrouter.db` against a missing file errors with
    // SQLITE_CANTOPEN. Parse the URL ourselves and force
    // `create_if_missing(true)` so first-run works without forcing the
    // user to write `?mode=rwc` into the DSN. The URL itself is still
    // forwarded to sqlx verbatim — we only set a connection flag.
    let connect_opts = SqliteConnectOptions::from_str(&config.database.url)
        .with_context(|| format!("parsing database url {}", config.database.url))?
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(connect_opts)
        .await
        .with_context(|| format!("connecting to database {}", config.database.url))?;
    crate::auth::migrate(&pool)
        .await
        .context("running auth migrations")?;
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
    // Hand-off clone — the builder closure below moves `routing_table`
    // into the App pipeline, but the daemon's reloader needs the
    // concrete type to call `replace_config` in zero-config mode.
    let routing_table_for_reload = routing_table.clone();
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

    // Metrics are now pushed via OTLP, not pulled from /metrics
    // Keep metrics_renderer for compatibility but return empty response
    let metrics_renderer: Arc<dyn MetricsRenderer> = Arc::new(EmptyMetricsRenderer);

    // OpenTelemetry configuration is now handled via build_otel_config()

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
            // OpenTelemetry exporter - handles both traces and metrics with multi-tenant attribution
            // Configured via plugins.bitrouter-observe.otel or environment variables
            if let Some(otel_config) = build_otel_config(config) {
                match OtelExporter::new(otel_config) {
                    Ok(exporter) => lm.observe_hook(exporter),
                    Err(e) => tracing::error!("Failed to initialize OpenTelemetry: {}", e),
                }
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
        routing_table: routing_table_for_reload,
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

/// Empty metrics renderer for /metrics endpoint compatibility.
/// Returns empty response since metrics are now pushed via OTLP.
struct EmptyMetricsRenderer;

impl MetricsRenderer for EmptyMetricsRenderer {
    fn render(&self) -> String {
        "# Prometheus metrics have been removed in favor of OpenTelemetry.\n".to_string() +
        "# Configure OTLP export via plugins.bitrouter-observe.otel\n"
    }
}

/// Build OpenTelemetry configuration from the app config.
fn build_otel_config(config: &config::Config) -> Option<OtelConfig> {
    // Check for otel config in plugins.bitrouter-observe.otel
    if let Some(observe_config) = config.plugins.get("bitrouter-observe") {
        if let Some(otel) = observe_config.get("otel").and_then(|v| v.as_object()) {
            // Parse the otel config
            if otel.get("endpoint").is_some() || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
                return Some(OtelConfig::default().with_env_overrides());
            }
        }
        // Legacy support: check for old otlp_endpoint field  
        if let Some(endpoint) = observe_config.get("otlp_endpoint").and_then(|v| v.as_str()) {
            let mut config = OtelConfig::default();
            config.endpoint = endpoint.to_string();
            return Some(config.with_env_overrides());
        }
    }
    // Also enable if OTEL_EXPORTER_OTLP_ENDPOINT is set
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        return Some(OtelConfig::default().with_env_overrides());
    }
    None
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
