//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config.

use std::sync::Arc;

use anyhow::{Context, Result};
use sea_orm::DatabaseConnection;

use bitrouter_sdk::App;
use bitrouter_sdk::acp::{AcpStdioExecutor, ConfigAcpRoutingTable};
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::protocol::OutboundDispatch;
use bitrouter_sdk::language_model::{AuthAppliers, HttpExecutor, HttpTimeouts};
use bitrouter_sdk::mcp::aggregating_executor::AggregatingExecutor;
use bitrouter_sdk::mcp::caching_executor::{CacheTtls, CachingExecutor};
use bitrouter_sdk::mcp::config_routing::{ConfigMcpRoutingTable, McpServerAggregateConfig};
use bitrouter_sdk::mcp::rmcp_executor::RmcpExecutor;

use bitrouter_guardrails::{GuardrailConfig, GuardrailsPlugin};
use bitrouter_observe::OTEL_ENABLED;
use bitrouter_observe::otel::{OtelConfig, OtelExporter, OtelObserveHook};
use bitrouter_sdk::MetricsRenderer;

use crate::auth::AuthHook;
use crate::daemon::{NoopObserveStatus, ObserveStatusPayload, ObserveStatusProvider};
use crate::metering::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable};
use crate::policy::{PolicyHook, PolicyStore};

/// A running application plus the database connection it was assembled
/// over (the caller keeps the connection for management commands — key
/// creation, etc.).
pub struct Assembled {
    /// The fully wired application.
    pub app: App,
    /// The shared database connection.
    pub db: DatabaseConnection,
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
    /// Snapshot provider for `bitrouter observe status`. When the OTel
    /// exporter is wired, this reports its live state; when not, it
    /// reports `compiled_in` truthfully and everything else blank.
    pub observe: Arc<dyn ObserveStatusProvider>,
    /// Live OTel exporter handle, when one is wired. Held so the binary
    /// can hand it to the `tracing-opentelemetry` bridge layer at
    /// subscriber-init time (the bridge captures its tracer eagerly).
    /// `None` when OTel is disabled in config.
    pub otel_exporter: Option<Arc<OtelExporter>>,
    /// Captured `OtelExporter::new` failure message, when one occurred.
    /// Surfaced as a `tracing::error!` line by the binary after the full
    /// subscriber is installed — `OtelExporter::new` itself runs before
    /// the subscriber on the `serve` path, so logging directly here
    /// would be dropped.
    pub otel_init_error: Option<String>,
}

/// `ObserveStatusProvider` impl backed by a real [`OtelExporter`]. The
/// payload type lives in the daemon module (wire format), and the
/// observe-side `OtelStatus` lives in the plugin (no daemon dep); this
/// adapter does the field-by-field copy so neither crate has to import
/// the other's type.
struct OtelExporterStatus {
    exporter: Arc<OtelExporter>,
}

#[async_trait::async_trait]
impl ObserveStatusProvider for OtelExporterStatus {
    fn status(&self) -> ObserveStatusPayload {
        let s = self.exporter.status();
        ObserveStatusPayload {
            compiled_in: s.compiled_in,
            exporter_wired: s.exporter_wired,
            endpoint: s.endpoint,
            header_count: s.header_count,
            service_name: s.service_name,
            resource_attribute_count: s.resource_attribute_count,
            sampler: s.sampler,
            sampler_arg: s.sampler_arg,
            metrics_enabled: s.metrics_enabled,
            api_key_count: s.api_key_count,
            api_key_cap: s.api_key_cap,
            user_id_count: s.user_id_count,
            user_id_cap: s.user_id_cap,
            active_spans: s.active_spans,
        }
    }

    async fn shutdown(&self) {
        // `OtelExporter::shutdown` is synchronous and blocks the calling
        // thread on the OTel SDK's internal channels. Driving it from an
        // async context directly would park the tokio worker that the
        // SDK's `rt-tokio` background tasks need to make progress —
        // deadlock on a single-threaded runtime, latency hit on any
        // runtime. `spawn_blocking` moves the wait to tokio's blocking
        // pool so async workers stay free to drain the SDK.
        let exporter = self.exporter.clone();
        if let Err(e) = tokio::task::spawn_blocking(move || exporter.shutdown()).await {
            tracing::warn!(error = %e, "OTel exporter shutdown task panicked");
        }
    }
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
    // ---- database + migrations ----
    // `database.url` may name any backend sea-orm supports (sqlite /
    // postgres / mysql); `crate::db::connect` handles the per-backend
    // first-run conveniences (a SQLite file URL is created on demand).
    // The schema is applied from `crate::db::migration` — Rust code, not
    // SQL — so it lands identically on whichever backend is configured.
    let db = crate::db::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to database {}", config.database.url))?;
    crate::db::run_migrations(&db)
        .await
        .context("running database migrations")?;
    // A SQLite database file holds SHA-256 hashes of every virtual key,
    // plus the metering audit trail. On Unix, tighten the file
    // permissions to 0600 so a co-tenant on the host can't read it. The
    // control socket already does the same in
    // `daemon::run_control_socket`. Non-file backends (Postgres / MySQL)
    // have no local file and govern access themselves.
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
    let metering_store = MeteringStore::new(db.clone());
    let metering_store_for_policy = metering_store.clone();
    let metering_store_for_recorder = metering_store.clone();
    let pricing_for_recorder = pricing.clone();
    let policy_store: Arc<PolicyStore> = Arc::new(load_policy_store(config).await?);
    let policy_store_for_reload = policy_store.clone();
    let guardrail_rules = build_guardrail_config(config)?
        .compile()
        .context("compiling guardrail patterns")?;

    // Metrics are now pushed via OTLP, not pulled from /metrics
    // Keep metrics_renderer for compatibility but return empty response
    let metrics_renderer: Arc<dyn MetricsRenderer> = Arc::new(EmptyMetricsRenderer);

    // Build the OTel exporter (if any) out here so the daemon can hold an
    // `Arc` to it for `observe status` queries. The pipeline closure
    // below registers a hook view of the same `Arc`; both Drop at app
    // shutdown.
    // Capture an `OtelExporter::new` failure as a message string instead
    // of emitting a `tracing::error!` directly: on the `serve` path the
    // subscriber is not installed yet (it depends on the exporter being
    // built first), so a tracing line here would be dropped. The binary
    // surfaces this once the subscriber is up.
    let (otel_exporter, otel_init_error): (Option<Arc<OtelExporter>>, Option<String>) =
        match build_otel_config(config)? {
            Some(c) => match OtelExporter::new(c) {
                Ok(exporter) => (Some(Arc::new(exporter)), None),
                Err(e) => (
                    None,
                    Some(format!("failed to initialise OpenTelemetry: {e}")),
                ),
            },
            None => (None, None),
        };
    let observe_provider: Arc<dyn ObserveStatusProvider> = match otel_exporter.clone() {
        Some(exporter) => Arc::new(OtelExporterStatus { exporter }),
        None => Arc::new(NoopObserveStatus {
            compiled_in: OTEL_ENABLED,
        }),
    };
    // Two handles to the same exporter: one moves into the pipeline as an
    // `ObserveHook`, the other lands on `Assembled` so the binary can hand
    // it to the `tracing-opentelemetry` bridge layer.
    let otel_for_hook = otel_exporter.clone();
    let otel_for_assembled = otel_exporter;

    // Optional MCP pure-routing pipeline — wired only when the config
    // declares at least one upstream MCP server. The pipeline is independent
    // of the language_model pipeline (different hook traits, different
    // routing table) and carries no settlement.
    let mcp_aggregate_route = if config.mcp.aggregate.enabled {
        Some(config.mcp.aggregate.route.clone())
    } else {
        None
    };
    if let Some(route) = mcp_aggregate_route.as_deref() {
        // URL collision: a per-server route would be shadowed if its name
        // matches the aggregate's last path segment. Trim trailing slashes
        // first — a route written as `/mcp/` in YAML must still trigger the
        // check.
        //
        // This is a shallow heuristic: the only collision shape it catches is
        // `{aggregate_route}/{server}` overlapping with `/mcp/{server}` when
        // the aggregate route's tail equals a server name. Stranger
        // collisions (e.g. an aggregate route that re-introduces `/mcp` as a
        // non-tail segment) fall through and are caught later by axum's
        // route-registration panic at mount time. Surfacing this case early
        // gives the operator a config-shaped error rather than a startup
        // panic for the most common misconfiguration.
        let agg_last = route.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if !agg_last.is_empty() && config.mcp_servers.keys().any(|k| k.as_str() == agg_last) {
            anyhow::bail!(
                "mcp_servers entry '{agg_last}' would be shadowed by the per-server mount at \
                 '{route}/{agg_last}' (derived from the aggregate route '{route}'). Rename the \
                 server or move the aggregate route. Note: this check only catches the \
                 last-segment overlap; axum mounts may still reject other shapes at startup."
            );
        }
    }

    let mcp_routing = if config.mcp_servers.is_empty() {
        None
    } else {
        Some(Arc::new(
            ConfigMcpRoutingTable::from_configs(config.mcp_servers.iter().map(|(k, v)| {
                let agg = McpServerAggregateConfig {
                    aggregate: v.aggregate,
                    tool_prefix: v.tool_prefix.clone().unwrap_or_else(|| format!("{k}__")),
                };
                (k.clone(), v.clone(), agg)
            }))
            .context("building the MCP routing table from config.mcp_servers")?,
        ))
    };
    // The MCP executor stack — composed innermost-out so a single
    // `/mcp tools/list` with cold caches dials N servers once, after which
    // it's all cache hits:
    //   AggregatingExecutor → CachingExecutor → RmcpExecutor
    // Caches sit at the leaves so a single-server `notifications/*` only
    // invalidates that server's slice.
    let mcp_executor = mcp_routing.as_ref().map(|_| {
        let rmcp: Arc<RmcpExecutor> = Arc::new(RmcpExecutor::new());
        let inner_for_cache: Arc<RmcpExecutor> = rmcp.clone();
        if config.mcp.cache.enabled {
            let ttls: CacheTtls = (&config.mcp.cache).into();
            let cached: Arc<CachingExecutor<RmcpExecutor>> = Arc::new(
                CachingExecutor::new(inner_for_cache, ttls)
                    .with_invalidation(rmcp.invalidation_receiver()),
            );
            Arc::new(AggregatingExecutor::new(cached)) as Arc<dyn bitrouter_sdk::mcp::Executor>
        } else {
            Arc::new(AggregatingExecutor::new(inner_for_cache))
                as Arc<dyn bitrouter_sdk::mcp::Executor>
        }
    });

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

    let db_for_hooks = db.clone();
    let app = App::builder()
        .skip_auth(config.server.skip_auth)
        .metrics_renderer(metrics_renderer)
        .language_model(move |lm| {
            lm.routing_table(routing_table).executor(executor);
            // Stage 1, in order: auth → policy. The guardrail plugin appends its
            // hooks after this closure (see `.plugin(...)` below), preserving the
            // auth → policy → guardrail order.
            lm.pre_request_hook(AuthHook::new(db_for_hooks.clone()));
            lm.pre_request_hook(PolicyHook::new(
                policy_store.clone(),
                Some(metering_store_for_policy),
            ));
            // OpenTelemetry exporter — register the *same* Arc as a hook
            // here. Construction happened above so `Assembled.observe`
            // can hold a query handle on it.
            if let Some(exporter) = otel_for_hook {
                lm.observe_hook(OtelObserveHook::new(exporter));
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
    // Stage-1 guardrail plugin, appended after the closure so its hooks land
    // after auth + policy in registration order. Skipped when no rules are
    // configured, so a guardrail-free deployment registers nothing.
    let app = if guardrail_rules.is_empty() {
        app
    } else {
        app.plugin(GuardrailsPlugin::with_static(guardrail_rules))
    };
    // Apply the optional MCP pipeline configuration in a second builder step
    // so the language_model configuration above stays the same shape it has
    // had since v0.
    let app = match (mcp_routing, mcp_executor) {
        (Some(table), Some(exec)) => {
            let app = app.mcp(move |m| {
                m.routing_table(table).executor(exec);
            });
            if let Some(route) = mcp_aggregate_route {
                app.mcp_aggregate_route(route)
            } else {
                app
            }
        }
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
        db,
        policy_store: policy_store_for_reload,
        routing_table: routing_table_for_reload,
        observe: observe_provider,
        otel_exporter: otel_for_assembled,
        otel_init_error,
    })
}

/// Build the per-provider `AuthAppliers` registry. Each entry covers a
/// provider whose credential flow needs more than the per-protocol
/// `Transport::authorise` default — today: `bitrouter` (the official
/// hosted gateway; OAuth from `bitrouter auth login` with a
/// `BITROUTER_API_KEY` fallback), GitHub Copilot (device-code OAuth +
/// token exchange), Anthropic (dual API-key / Pro/Max subscription
/// OAuth), OpenAI Codex (ChatGPT-subscription OAuth).
fn build_auth_appliers(config: &Config) -> Result<AuthAppliers> {
    let mut appliers = AuthAppliers::new();
    let store_path = bitrouter_providers::oauth::credential_store::CredentialStore::default_path()
        .map(|s| s.path().to_path_buf())
        .context("resolving credential store path")?;
    // The `bitrouter` provider's applier reads the user-account credentials
    // store (separate from the upstream-provider store above), so it lives
    // in its own crate and is registered via the `crate::cloud` glue module.
    crate::cloud::register_if_configured(config, &mut appliers)?;
    if config.providers.contains_key("github-copilot") {
        let applier = bitrouter_providers::copilot::CopilotAuthApplier::new(&store_path)
            .context("building the github-copilot AuthApplier")?;
        appliers.register("github-copilot", Arc::new(applier));
    }
    // The Anthropic applier is registered unconditionally when the
    // provider is configured, so an existing `${ANTHROPIC_API_KEY}` user
    // gets the same fallthrough behaviour as before — the applier
    // forwards the inline key when no OAuth credential is in the store.
    if config.providers.contains_key("anthropic") {
        let applier = bitrouter_providers::anthropic::AnthropicOAuthApplier::new(&store_path)
            .context("building the anthropic AuthApplier")?;
        appliers.register("anthropic", Arc::new(applier));
    }
    if config.providers.contains_key("openai-codex") {
        let applier = bitrouter_providers::codex::OpenAiCodexAuthApplier::new(&store_path)
            .context("building the openai-codex AuthApplier")?;
        appliers.register("openai-codex", Arc::new(applier));
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

/// Parse the guardrail data contract from `plugins.bitrouter-guardrails`
/// (its `custom_patterns` array of `{ name, pattern, action: "block" |
/// "redact" }`). The plugin owns the shape; this just deserialises it.
fn build_guardrail_config(config: &Config) -> Result<GuardrailConfig> {
    let Some(value) = config.plugins.get("bitrouter-guardrails") else {
        return Ok(GuardrailConfig::default());
    };
    serde_json::from_value(value.clone()).context("plugins.bitrouter-guardrails failed to parse")
}

/// Empty metrics renderer for /metrics endpoint compatibility.
/// Returns empty response since metrics are now pushed via OTLP.
struct EmptyMetricsRenderer;

impl MetricsRenderer for EmptyMetricsRenderer {
    fn render(&self) -> String {
        "# Prometheus metrics have been removed in favor of OpenTelemetry.\n".to_string()
            + "# Configure OTLP export via plugins.bitrouter-observe.otel\n"
    }
}

/// Build OpenTelemetry configuration from the app config. Returns `None` when
/// neither YAML nor env vars opt the exporter in.
///
/// Precedence: env vars > `plugins.bitrouter-observe.otel` > the legacy flat
/// `plugins.bitrouter-observe.otlp_endpoint` shim (v0 carry-over; will be
/// removed in v1.1).
fn build_otel_config(config: &Config) -> Result<Option<OtelConfig>> {
    let observe = config.plugins.get("bitrouter-observe");

    // Env-var overrides are *not* applied here — `OtelExporter::new` runs
    // `with_env_overrides` on whatever config it is handed, so the
    // env > YAML precedence holds for every path below without this
    // function having to re-apply it.

    // 1. New nested `otel: { … }` block. A malformed block is a hard error:
    //    the operator explicitly opted in, so silently falling back to the
    //    legacy shim / env-only path would hide their mistake and start the
    //    exporter with a config they never asked for.
    if let Some(otel_value) = observe.and_then(|c| c.get("otel")) {
        let cfg = serde_json::from_value::<OtelConfig>(otel_value.clone())
            .context("plugins.bitrouter-observe.otel failed to parse")?;
        return Ok(Some(cfg));
    }

    // 2. Legacy flat `otlp_endpoint` shim — drops the cardinality / sampler /
    //    batch knobs, but lets a v0 YAML keep working until v1.1.
    if let Some(endpoint) = observe
        .and_then(|c| c.get("otlp_endpoint"))
        .and_then(|v| v.as_str())
    {
        let cfg = OtelConfig {
            endpoint: endpoint.to_string(),
            ..OtelConfig::default()
        };
        tracing::warn!(
            "plugins.bitrouter-observe.otlp_endpoint is deprecated; switch to plugins.bitrouter-observe.otel",
        );
        return Ok(Some(cfg));
    }

    // 3. Env-var-only opt-in.
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        return Ok(Some(OtelConfig::default()));
    }

    Ok(None)
}

/// Extract the file path from a SQLite URL. Returns `None` for `:memory:`
/// and for any non-SQLite URL (Postgres / MySQL have no local file). Accepts
/// the `sqlite://` and `sqlite:` forms.
#[cfg(unix)]
fn sqlite_file_path(url: &str) -> Option<std::path::PathBuf> {
    // Only a SQLite URL names a local file — a Postgres / MySQL URL must
    // never be mistaken for a path to chmod.
    let after_scheme = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
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
        // Non-SQLite URLs name no local file — never treated as a path.
        assert_eq!(sqlite_file_path("postgres://u:p@host/bitrouter"), None);
        assert_eq!(sqlite_file_path("mysql://u:p@host/bitrouter"), None);
    }
}

#[cfg(test)]
mod otel_config_tests {
    use super::{Config, build_otel_config};

    /// Build a `Config` carrying a single `bitrouter-observe` plugin value.
    /// Constructed directly (no YAML round-trip) so the test never touches
    /// the process environment that `build_otel_config`'s env-only path
    /// would read.
    fn config_with_observe(observe: serde_json::Value) -> Config {
        let mut config = Config::default();
        config
            .plugins
            .insert("bitrouter-observe".to_string(), observe);
        config
    }

    #[test]
    fn malformed_otel_block_is_a_hard_error() {
        // `sampler` is a closed enum — an unknown variant fails to parse.
        // An explicit opt-in must surface that, not silently fall through.
        let config = config_with_observe(serde_json::json!({
            "otel": { "sampler": "not_a_real_sampler" }
        }));
        assert!(
            build_otel_config(&config).is_err(),
            "a malformed otel block must be a hard error",
        );
    }

    #[test]
    fn valid_otel_block_parses() {
        let config = config_with_observe(serde_json::json!({
            "otel": { "endpoint": "http://collector:4318" }
        }));
        let cfg = build_otel_config(&config)
            .expect("valid otel block is Ok")
            .expect("valid otel block yields Some");
        assert_eq!(cfg.endpoint, "http://collector:4318");
    }

    #[test]
    fn legacy_otlp_endpoint_shim_still_works() {
        let config = config_with_observe(serde_json::json!({
            "otlp_endpoint": "http://legacy:4318"
        }));
        let cfg = build_otel_config(&config)
            .expect("legacy shim is Ok")
            .expect("legacy shim yields Some");
        assert_eq!(cfg.endpoint, "http://legacy:4318");
    }
}
