//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config.

use std::sync::Arc;

use anyhow::{Context, Result};
use sea_orm::DatabaseConnection;

use bitrouter_sdk::App;
use bitrouter_sdk::PromptTransform;
use bitrouter_sdk::acp::{AcpStdioExecutor, ConfigAcpRoutingTable};
use bitrouter_sdk::config::{Config, ConfigRoutingTable};
use bitrouter_sdk::language_model::protocol::OutboundDispatch;
use bitrouter_sdk::language_model::server_tools::advisor::AdvisorToolset;
use bitrouter_sdk::language_model::server_tools::approval::AllowAll;
use bitrouter_sdk::language_model::server_tools::config::ServerToolLoopConfig;
use bitrouter_sdk::language_model::server_tools::declarations::ServerToolDeclarationsHook;
use bitrouter_sdk::language_model::server_tools::fusion::FusionToolset;
use bitrouter_sdk::language_model::server_tools::fusion::alias::FusionAliasConfig;
use bitrouter_sdk::language_model::server_tools::loop_controller::ServerToolLoop;
use bitrouter_sdk::language_model::server_tools::mcp_toolset::McpRouterToolset;
use bitrouter_sdk::language_model::server_tools::nested::{NestedRunner, PipelineNestedRunner};
use bitrouter_sdk::language_model::server_tools::sub_agent::SubAgentToolset;
use bitrouter_sdk::language_model::server_tools::toolset::{RouterToolset, ToolsetRegistry};
use bitrouter_sdk::language_model::{AuthAppliers, HttpExecutor, HttpTimeouts, PipelineBuilder};
use bitrouter_sdk::mcp::aggregating_executor::AggregatingExecutor;
use bitrouter_sdk::mcp::caching_executor::{CacheTtls, CachingExecutor};
use bitrouter_sdk::mcp::config_routing::{ConfigMcpRoutingTable, McpServerAggregateConfig};
use bitrouter_sdk::mcp::rmcp_executor::RmcpExecutor;

use bitrouter_guardrails::{GuardrailConfig, GuardrailsPlugin};
use bitrouter_observe::OTEL_ENABLED;
use bitrouter_observe::otel::{
    ContentCaptureMode, MetricsConfig, OtelConfig, OtelExporter, OtelObserveHook,
};
use bitrouter_sdk::MetricsRenderer;

use crate::auth::AuthHook;
use crate::daemon::{NoopObserveStatus, ObserveStatusPayload, ObserveStatusProvider};
use crate::metering::{ContextTier, MeteringRecorder, MeteringStore, ModelPricing, PricingTable};
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
    // Subscription / "use your Claude Code session" logins persist their
    // credential in the store (not the config), so apply_builtin_defaults would
    // mark a keyless provider like `anthropic` inactive. Re-activate any
    // provider that has a stored credential so it stays routable.
    if let Ok(store) = bitrouter_providers::oauth::credential_store::CredentialStore::default_path()
    {
        bitrouter_providers::activate_stored_credential_providers(&mut resolved, &store);
    }
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
            Some(plan) => {
                // Resolve the LIVE account-bearer source per the plan. Only the
                // telemetry opt-in with `attribution: auto`/`account` and no
                // explicit token wants one; `anonymous` (and every other
                // exporter path) is `StaticOnly` and never reads the credential
                // store. Best-effort: a `None` source means the export proceeds
                // anonymously — for `account` we additionally warn.
                let bearer: Option<Arc<dyn bitrouter_observe::otel::TelemetryBearer>> =
                    match plan.bearer_plan {
                        BearerPlan::LiveSource { warn_if_unmet } => {
                            let source = crate::cloud::cloud_bearer_source().await;
                            if source.is_none() && warn_if_unmet {
                                tracing::warn!(
                                    "telemetry: attribution=account but no signed-in session is \
                                     available — exporting anonymously (sign in with \
                                     `bitrouter cloud login`)"
                                );
                            }
                            source
                        }
                        BearerPlan::StaticOnly => None,
                    };
                match OtelExporter::new(plan.config, bearer) {
                    Ok(exporter) => (Some(Arc::new(exporter)), None),
                    Err(e) => (
                        None,
                        Some(format!("failed to initialise OpenTelemetry: {e}")),
                    ),
                }
            }
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

    // Nested-completion runner for the advisor / sub-agent / fusion server tools
    // — built when any of them is configured. Their nested calls run on a
    // dedicated, loop-less sub-completion pipeline (so a worker cannot recursively
    // invoke another server tool), reusing the same routing table and upstream
    // executor as the main pipeline. The metering recorder is attached so each
    // nested completion is billed to the same caller as the parent request (no
    // auth or policy hooks — a nested call is a sub-operation of an
    // already-authorised request, and the parent principal rides the caller).
    let server_tools_enabled = config.server_tools.fusion.is_some()
        || config.server_tools.advisor
        || config.server_tools.subagent;
    let nested_runner: Option<Arc<dyn NestedRunner>> = if server_tools_enabled {
        let mut sub = PipelineBuilder::new();
        sub.routing_table(routing_table.clone())
            .executor(executor.clone())
            .settlement_recorder(MeteringRecorder::new(
                metering_store.clone(),
                pricing.clone(),
            ));
        let sub_pipeline = Arc::new(
            sub.build()
                .context("building the server-tool sub-completion pipeline")?,
        );
        Some(Arc::new(PipelineNestedRunner::new(sub_pipeline)))
    } else {
        None
    };

    // Server-side tool loop — wired when `server_tools.mcp_servers` names a
    // configured MCP server and/or a nested server tool (advisor / sub-agent /
    // fusion) is enabled. Reuses the MCP executor/routing built above; here
    // BitRouter is an MCP *client* that executes the model's tool calls inside
    // the LLM loop (distinct from the `/mcp` gateway above).
    let server_tool_loop =
        build_server_tool_loop(config, &mcp_routing, &mcp_executor, nested_runner);

    // The bitrouter/fusion model alias (an ingress prompt transform) and the
    // server-tool declaration-parsing hook are wired below when enabled.
    let fusion_alias: Option<Arc<dyn PromptTransform>> = config
        .server_tools
        .fusion
        .as_ref()
        .and_then(FusionAliasConfig::from_settings)
        .map(|c| Arc::new(c) as Arc<dyn PromptTransform>);

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
            // Server-tool declaration capture runs first and is pure
            // observation: it parses any advisor / sub-agent / fusion
            // declaration (or the one the fusion alias injected) off the prompt
            // and stashes it, before auth, so the toolsets can read it
            // regardless of credential state.
            if server_tools_enabled {
                lm.pre_request_hook(ServerToolDeclarationsHook);
            }
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
            // Server-side tool loop (router-executed MCP tools), when configured.
            if let Some(server_loop) = server_tool_loop {
                lm.server_tool_loop(server_loop);
            }
        });
    // Stage-1 guardrail plugin, appended after the closure so its hooks land
    // after auth + policy in registration order. Skipped when no rules are
    // configured, so a guardrail-free deployment registers nothing.
    let app = if guardrail_rules.is_empty() {
        app
    } else {
        app.plugin(GuardrailsPlugin::with_static(guardrail_rules))
    };
    // The bitrouter/fusion model alias: an ingress prompt transform that
    // rewrites the alias onto a real outer model and attaches the Fusion
    // declaration. Wired only when `server_tools.fusion` resolves an alias.
    let app = match fusion_alias {
        Some(transform) => app.prompt_transform(transform),
        None => app,
    };
    // The Claude Code subscription router: an ingress prompt transform that
    // rewrites genuine Claude Code traffic (identity system prompt + a bare
    // Claude model) onto the explicit `claude-code:<model>` route, sending it to
    // the subscription provider. Registered unconditionally — it reads the
    // prompt and no-ops on everything else. Transform order is irrelevant here
    // (it and the fusion alias touch unrelated requests).
    let app = app.prompt_transform(
        Arc::new(crate::claude_code::ClaudeCodeRouter) as Arc<dyn PromptTransform>
    );
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

/// Merge the provider registry into `config`, then re-apply built-in defaults
/// so any provider the merge newly inserted gets its `api_base` /
/// `api_protocol` / auth shape filled. No-op when the registry is disabled
/// (`registry.enabled = false` or `inherit_defaults = false`).
///
/// The data is the **fetched** dist when reachable, else the most recent
/// **disk cache** (stale-fallback on a network outage). On a never-fetched host
/// with no network there is no data: the merge is a no-op and the registry is
/// empty, so only locally-configured providers (and the compiled-in `bitrouter`
/// cloud gateway) are routable. Network to the registry is expected to be
/// stable, so this empty state is a rare first-run edge.
///
/// Called by the `serve` entry point (before [`build_app`]) and by
/// [`crate::reload`], the two paths that build a production routing config.
/// Kept out of `build_app` itself so that function stays free of network I/O —
/// integration tests assemble explicit configs through it. Lives in the app
/// layer (above `bitrouter-providers`) because the SDK's own routing table sits
/// below the providers crate and cannot fetch the registry itself.
pub async fn merge_registry_into(config: &mut Config) {
    if !config.inherit_defaults || !config.registry.enabled {
        return;
    }
    // Fetched dist when reachable; otherwise the disk cache. `None` (never
    // fetched + unreachable) means an empty registry — skip the merge entirely.
    let Some(data) = bitrouter_providers::registry::apply::load_or_cached(&config.registry).await
    else {
        bitrouter_providers::apply_builtin_defaults(config);
        return;
    };
    bitrouter_providers::registry::apply::apply_registry(config, &data);
    // Best-effort: pull the FULL catalog for `models_dev` auto-sync providers
    // from models.dev (beyond the curated canonical subset the registry ships).
    // A fetch failure leaves the curated models in place — they already route —
    // so this never blocks startup on an offline host.
    match bitrouter_providers::catalog::fetch::fetch_catalog().await {
        Ok(catalog) => {
            bitrouter_providers::registry::apply::apply_catalog(config, &data, &catalog);
        }
        Err(e) => {
            tracing::debug!(error = %e, "models.dev catalog fetch failed; using curated models only");
        }
    }
    bitrouter_providers::apply_builtin_defaults(config);
}

/// Build the server-side tool loop from `config.server_tools`. Returns `None`
/// when neither MCP server tools nor any nested server tool is configured.
///
/// Each named MCP server is attached as an [`McpRouterToolset`] using a `Direct`
/// selector and the server's `tool_prefix` (default `"{name}__"`), so router
/// tool names cannot collide with the caller's own tools. When `nested_runner`
/// is provided, the enabled advisor / sub-agent / fusion toolsets are added over
/// it (each advertised only when the request declares it).
fn build_server_tool_loop(
    config: &Config,
    mcp_routing: &Option<Arc<ConfigMcpRoutingTable>>,
    mcp_executor: &Option<Arc<dyn bitrouter_sdk::mcp::Executor>>,
    nested_runner: Option<Arc<dyn NestedRunner>>,
) -> Option<Arc<ServerToolLoop>> {
    let settings = &config.server_tools;
    let mut sets: Vec<Arc<dyn RouterToolset>> = Vec::new();

    // MCP-backed server tools.
    if !settings.mcp_servers.is_empty() {
        if let (Some(routing), Some(executor)) = (mcp_routing, mcp_executor) {
            let routing: Arc<dyn bitrouter_sdk::mcp::RoutingTable> = routing.clone();
            for name in &settings.mcp_servers {
                let Some(server) = config.mcp_servers.get(name) else {
                    tracing::warn!(server = %name,
                        "server_tools.mcp_servers names an MCP server absent from mcp_servers; skipping");
                    continue;
                };
                let prefix = server
                    .tool_prefix
                    .clone()
                    .unwrap_or_else(|| format!("{name}__"));
                sets.push(Arc::new(McpRouterToolset::new(
                    executor.clone(),
                    routing.clone(),
                    name.clone(),
                    Some(prefix),
                )));
            }
        } else {
            tracing::warn!(
                "server_tools.mcp_servers is set but no mcp_servers are configured; \
                 MCP server tools are disabled"
            );
        }
    }

    // Nested server tools (advisor / sub-agent / fusion) over the shared runner.
    // Each toolset advertises only when the request declares it, so it is safe
    // to register every enabled one.
    if let Some(runner) = nested_runner {
        if settings.advisor {
            sets.push(Arc::new(AdvisorToolset::new(runner.clone())));
        }
        if settings.subagent {
            sets.push(Arc::new(SubAgentToolset::new(runner.clone())));
        }
        if settings.fusion.is_some() {
            sets.push(Arc::new(FusionToolset::new(runner)));
        }
    }

    if sets.is_empty() {
        return None;
    }
    let mut loop_config = ServerToolLoopConfig::default();
    if let Some(max) = settings.max_iterations {
        loop_config.max_iterations = max;
    }
    Some(Arc::new(ServerToolLoop::new(
        ToolsetRegistry::new(sets),
        loop_config,
        Arc::new(AllowAll),
    )))
}

/// Build the per-provider `AuthAppliers` registry. Each entry covers a
/// provider whose credential flow needs more than the per-protocol
/// `Transport::authorise` default — today: `bitrouter` (the official
/// hosted gateway; OAuth from `bitrouter cloud login` with a
/// `BITROUTER_API_KEY` fallback), GitHub Copilot (device-code OAuth +
/// token exchange), Anthropic Platform API (`x-api-key`), the Claude
/// Pro/Max subscription (`claude-code`, OAuth / live `~/.claude` session),
/// OpenAI Codex (ChatGPT-subscription OAuth).
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
    // The Anthropic Platform-API applier is registered when the provider is
    // configured, so an existing `${ANTHROPIC_API_KEY}` user gets the same
    // fallthrough behaviour as before — the applier forwards the inline key
    // when no stored key is present. It is `x-api-key`-only; the Claude
    // Pro/Max subscription lives in the separate `claude-code` provider below.
    if config.providers.contains_key("anthropic") {
        let applier = bitrouter_providers::anthropic::AnthropicApiKeyApplier::new(&store_path)
            .context("building the anthropic AuthApplier")?;
        appliers.register("anthropic", Arc::new(applier));
    }
    // The Claude Pro/Max subscription applier (OAuth / live `~/.claude`
    // session). Registered under `claude-code`, distinct from the
    // Platform-API `anthropic` provider above.
    if config.providers.contains_key("claude-code") {
        let applier = bitrouter_providers::claude_code::ClaudeCodeAuthApplier::new(&store_path)
            .context("building the claude-code AuthApplier")?;
        appliers.register("claude-code", Arc::new(applier));
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
            if let Some(pricing) = &model.pricing {
                let mut model_pricing = ModelPricing::new(
                    pricing.input_micro_usd_per_token,
                    pricing.output_micro_usd_per_token,
                );
                // Carry any context ("staged") brackets through to the
                // metering table; config rates are concrete f64s, so an
                // omitted per-bracket rate defaults to 0 (free), matching the
                // base-rate mapping above.
                model_pricing.context_tiers = pricing
                    .context_tiers
                    .iter()
                    .map(|t| ContextTier {
                        above_input_tokens: t.above_input_tokens,
                        input_micro_usd_per_token: Some(t.input_micro_usd_per_token),
                        output_micro_usd_per_token: Some(t.output_micro_usd_per_token),
                    })
                    .collect();
                table.insert(provider_id.clone(), model.id.clone(), model_pricing);
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

/// Whether the OTLP exporter should resolve its account bearer LIVE per export
/// (refresh-aware) and, if so, whether to warn when no credential is available.
///
/// Only the first-party telemetry opt-in (`attribution: auto`/`account` with no
/// explicit token) wants a live source. Every other exporter path — an explicit
/// static `bearer_token`, `attribution: anonymous`, or the generic `otel` /
/// legacy / env opt-ins — uses the static header path (`StaticOnly`) and never
/// reads the credential store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BearerPlan {
    /// Build a live account-bearer source (resolved per export, refresh-aware).
    /// `warn_if_unmet` is set for `attribution: account` so the call site logs
    /// the "exporting anonymously" warning when the source resolves to `None`.
    LiveSource { warn_if_unmet: bool },
    /// No live source — static `Authorization` header (or anonymous). The
    /// credential store is never read.
    StaticOnly,
}

/// An exporter config plus the account-bearer resolution plan for it. Returned
/// by [`build_otel_config`] so the async `OtelExporter::new` call site can decide
/// whether to build a live bearer source (which needs `.await` + network I/O,
/// neither appropriate inside this pure config builder).
struct OtelConfigPlan {
    config: OtelConfig,
    bearer_plan: BearerPlan,
}

/// Build OpenTelemetry configuration from the app config. Returns `None` when
/// neither YAML nor env vars opt the exporter in.
///
/// Precedence: env vars > `plugins.bitrouter-observe.otel` > the legacy flat
/// `plugins.bitrouter-observe.otlp_endpoint` shim (v0 carry-over; will be
/// removed in v1.1).
///
/// This stays pure (no credential-store / network I/O): the account bearer is no
/// longer snapshotted here — it is resolved live per export by a bearer source
/// the async call site builds according to the returned [`BearerPlan`].
fn build_otel_config(config: &Config) -> Result<Option<OtelConfigPlan>> {
    let observe = config.plugins.get("bitrouter-observe");

    // Env-var overrides are *not* applied here — `OtelExporter::new` runs
    // `with_env_overrides` on whatever config it is handed, so the
    // env > YAML precedence holds for every path below without this
    // function having to re-apply it.

    // 0. First-party telemetry opt-in. `plugins.bitrouter-observe.telemetry`
    //    is a convenience wrapper over the `otel` block: when `enabled`, it
    //    points the exporter at BitRouter's first-party telemetry endpoint and
    //    selects the capture level. OFF by default — an absent or
    //    `enabled: false` block leaves telemetry disabled. A malformed block is
    //    a hard error (the operator explicitly opted in).
    if let Some(tel_value) = observe.and_then(|c| c.get("telemetry")) {
        let opt_in = serde_json::from_value::<TelemetryOptIn>(tel_value.clone())
            .context("plugins.bitrouter-observe.telemetry failed to parse")?;
        if opt_in.enabled {
            // Best-effort install id: a missing home just means anonymous
            // attribution downstream, never a failure to export.
            let install_id = crate::paths::install_id()
                .map_err(|e| tracing::warn!("telemetry: could not resolve install id: {e:#}"))
                .ok();
            // Decide the live-bearer plan from attribution + whether an explicit
            // static token is present. The account bearer is NOT snapshotted
            // here anymore — when no explicit token is set and attribution is
            // auto/account, the call site builds a live source that refreshes
            // per export. `build_telemetry_otel_config` resolves the explicit
            // token itself into the static `bearer_token`.
            let has_explicit =
                opt_in.bearer_token.is_some() || std::env::var("BITROUTER_TELEMETRY_TOKEN").is_ok();
            let bearer_plan = telemetry_bearer_plan(opt_in.attribution, has_explicit);
            let cfg = build_telemetry_otel_config(opt_in, install_id);
            return Ok(Some(OtelConfigPlan {
                config: cfg,
                bearer_plan,
            }));
        }
    }

    // 1. New nested `otel: { … }` block. A malformed block is a hard error:
    //    the operator explicitly opted in, so silently falling back to the
    //    legacy shim / env-only path would hide their mistake and start the
    //    exporter with a config they never asked for.
    if let Some(otel_value) = observe.and_then(|c| c.get("otel")) {
        let cfg = serde_json::from_value::<OtelConfig>(otel_value.clone())
            .context("plugins.bitrouter-observe.otel failed to parse")?;
        return Ok(Some(OtelConfigPlan {
            config: cfg,
            bearer_plan: BearerPlan::StaticOnly,
        }));
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
        return Ok(Some(OtelConfigPlan {
            config: cfg,
            bearer_plan: BearerPlan::StaticOnly,
        }));
    }

    // 3. Env-var-only opt-in.
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        return Ok(Some(OtelConfigPlan {
            config: OtelConfig::default(),
            bearer_plan: BearerPlan::StaticOnly,
        }));
    }

    Ok(None)
}

/// Decide the live-bearer [`BearerPlan`] from the attribution mode and whether an
/// explicit static token is set. Pure (no I/O) so the policy is unit-testable.
///
/// - `anonymous` → never read the credential store: `StaticOnly`.
/// - an explicit token is set → use the static header: `StaticOnly`.
/// - `auto` / `account` with no explicit token → `LiveSource`; `account`
///   additionally asks the call site to warn when no credential is available.
fn telemetry_bearer_plan(attribution: TelemetryAttribution, has_explicit: bool) -> BearerPlan {
    match attribution {
        // Never touch the credential store when the user opted out of account
        // attribution.
        TelemetryAttribution::Anonymous => BearerPlan::StaticOnly,
        // An explicit token wins and rides as a static header (the live source
        // would otherwise never fill the slot anyway).
        _ if has_explicit => BearerPlan::StaticOnly,
        TelemetryAttribution::Auto => BearerPlan::LiveSource {
            warn_if_unmet: false,
        },
        TelemetryAttribution::Account => BearerPlan::LiveSource {
            warn_if_unmet: true,
        },
    }
}

/// BitRouter's first-party telemetry endpoint. A neutral, overridable default
/// used when the telemetry opt-in is enabled without an explicit `endpoint`.
pub const DEFAULT_TELEMETRY_ENDPOINT: &str = "https://telemetry.bitrouter.ai";

/// Capture level for the telemetry opt-in. `Metadata` exports spans without
/// message bodies; `Full` additionally includes request + response content.
#[derive(Debug, Clone, Copy, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TelemetryLevel {
    #[default]
    Metadata,
    Full,
}

/// How an enabled telemetry export attributes itself.
#[derive(Debug, Clone, Copy, serde::Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TelemetryAttribution {
    /// Account-attributed when a `bitrouter cloud login` session (or an explicit
    /// `bearer_token`) is available; anonymous otherwise. The default — signing
    /// in upgrades attribution automatically, with no config change.
    #[default]
    Auto,
    /// Require account attribution: use the signed-in session / explicit bearer.
    /// If neither is available the export still proceeds **anonymously** (it is
    /// best-effort and must never be dropped for want of a token), but a warning
    /// is logged so the misconfiguration is visible.
    Account,
    /// Always anonymous — never attach a bearer, even when signed in or a
    /// `bearer_token` is configured. The opt-out for users who want telemetry
    /// without tying it to their account.
    Anonymous,
}

/// The `plugins.bitrouter-observe.telemetry` opt-in block — a thin wrapper over
/// [`OtelConfig`] that defaults the endpoint and selects a capture level. Off
/// by default; nothing is exported unless `enabled` is `true`.
#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(default)]
struct TelemetryOptIn {
    /// Off by default; nothing is exported unless this is `true`.
    enabled: bool,
    /// Override the default first-party endpoint.
    endpoint: Option<String>,
    /// Capture level (`metadata` | `full`).
    level: TelemetryLevel,
    /// Optional bearer token authenticating the export to an account. Falls
    /// back to the `BITROUTER_TELEMETRY_TOKEN` env var when unset.
    bearer_token: Option<String>,
    /// How to attribute the export: `auto` (default — account when signed in,
    /// else anonymous), `account` (require the account), or `anonymous` (force
    /// anonymous even when signed in).
    attribution: TelemetryAttribution,
}

/// Ensure an OTLP/HTTP endpoint targets the per-signal **traces** path.
///
/// `opentelemetry-otlp`'s programmatic `with_endpoint` uses the URL verbatim —
/// unlike the `OTEL_EXPORTER_OTLP_ENDPOINT` env var, it does NOT append
/// `/v1/traces`. So a bare `scheme://host[:port]` would POST to `/` and 404.
/// Append `/v1/traces` only when the operator gave no path of their own; a
/// collector's full per-signal URL is respected as-is.
///
/// OTLP/HTTP paths: <https://opentelemetry.io/docs/specs/otlp/#otlphttp>
fn otlp_traces_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((_scheme, rest)) if !rest.contains('/') => format!("{trimmed}/v1/traces"),
        _ => trimmed.to_string(),
    }
}

/// Turn an enabled [`TelemetryOptIn`] into an [`OtelConfig`]: point at the
/// (default or overridden) traces endpoint, map the level onto a content-capture
/// mode, attach the EXPLICIT bearer (config or `BITROUTER_TELEMETRY_TOKEN`), and
/// stamp the stable anonymous install id so exports can be attributed without an
/// account.
///
/// Only the **static** explicit token lands in `OtelConfig.bearer_token` here.
/// The signed-in account bearer is NO LONGER snapshotted into the config — it is
/// resolved live per export by a [`bitrouter_observe::otel::TelemetryBearer`]
/// source the async call site builds (refresh-aware), so account attribution
/// survives token expiry without a daemon restart. `attribution: anonymous`
/// still drops even the explicit token (the opt-out guarantee).
///
/// Telemetry is **traces-only**: metrics export is disabled so the opt-in never
/// POSTs a metrics payload to a traces ingest.
fn build_telemetry_otel_config(opt_in: TelemetryOptIn, install_id: Option<String>) -> OtelConfig {
    let content_capture = match opt_in.level {
        TelemetryLevel::Metadata => ContentCaptureMode::Off,
        TelemetryLevel::Full => ContentCaptureMode::Full,
    };
    // Resolve the EXPLICIT static token (config `bearer_token`, else the
    // `BITROUTER_TELEMETRY_TOKEN` env var). `attribution: anonymous` forces no
    // bearer regardless — even an explicit token is dropped (the opt-out
    // guarantee). The account bearer is NO LONGER snapshotted here: when wanted
    // (auto/account, no explicit token) it is the live source the call site
    // attaches, refreshed per export.
    let explicit = opt_in
        .bearer_token
        .or_else(|| std::env::var("BITROUTER_TELEMETRY_TOKEN").ok());
    let bearer_token = match opt_in.attribution {
        TelemetryAttribution::Anonymous => None,
        TelemetryAttribution::Auto | TelemetryAttribution::Account => explicit,
    };
    let mut resource_attributes = std::collections::HashMap::new();
    if let Some(id) = install_id {
        resource_attributes.insert("bitrouter.install_id".to_string(), id);
    }
    // PostHog surfaces `$lib` as the sending client library. Pin it to
    // `bitrouter <version>` so OSS-exported events are attributable to the
    // daemon (and its version), regardless of account vs anonymous attribution.
    resource_attributes.insert(
        "$lib".to_string(),
        concat!("bitrouter ", env!("CARGO_PKG_VERSION")).to_string(),
    );
    let endpoint = otlp_traces_endpoint(
        opt_in
            .endpoint
            .as_deref()
            .unwrap_or(DEFAULT_TELEMETRY_ENDPOINT),
    );
    OtelConfig {
        endpoint,
        content_capture,
        bearer_token,
        resource_attributes,
        // Traces-only: never export metrics to the traces ingest.
        metrics: MetricsConfig {
            enabled: false,
            ..MetricsConfig::default()
        },
        ..OtelConfig::default()
    }
}

#[cfg(test)]
mod telemetry_opt_in_tests {
    use super::*;

    #[test]
    fn opt_in_full_uses_default_traces_endpoint_metrics_off_and_stamps_install_id() {
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: Some("bra_x".to_string()),
            attribution: TelemetryAttribution::Auto,
        };
        let cfg = build_telemetry_otel_config(opt_in, Some("inst-1".to_string()));
        // The default base is normalized to the OTLP traces path — the exporter
        // does not append `/v1/traces` for us.
        assert_eq!(
            cfg.endpoint,
            format!("{DEFAULT_TELEMETRY_ENDPOINT}/v1/traces")
        );
        assert_eq!(cfg.content_capture, ContentCaptureMode::Full);
        assert_eq!(cfg.bearer_token.as_deref(), Some("bra_x"));
        // Telemetry is traces-only.
        assert!(!cfg.metrics.enabled);
        assert_eq!(
            cfg.resource_attributes
                .get("bitrouter.install_id")
                .map(String::as_str),
            Some("inst-1")
        );
    }

    #[test]
    fn opt_in_metadata_honors_endpoint_override_and_no_install_id() {
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: Some("https://otel.example".to_string()),
            level: TelemetryLevel::Metadata,
            bearer_token: None,
            attribution: TelemetryAttribution::Auto,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        // A bare override host is normalized to the traces path too.
        assert_eq!(cfg.endpoint, "https://otel.example/v1/traces");
        assert_eq!(cfg.content_capture, ContentCaptureMode::Off);
        // No install id → no anonymous-identity attr, but `$lib` is always set.
        assert!(!cfg.resource_attributes.contains_key("bitrouter.install_id"));
    }

    #[test]
    fn build_config_sets_lib_resource_attribute() {
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: None,
            attribution: TelemetryAttribution::Auto,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        // PostHog renders `$lib` as the sending client library. Pin it to
        // `bitrouter <version>` so OSS-sent events are attributable to the
        // daemon and its version, even for anonymous exports.
        assert_eq!(
            cfg.resource_attributes.get("$lib").map(String::as_str),
            Some(concat!("bitrouter ", env!("CARGO_PKG_VERSION")))
        );
    }

    #[test]
    fn otlp_traces_endpoint_appends_only_when_no_path() {
        // Bare host gets the traces path appended.
        assert_eq!(
            otlp_traces_endpoint("https://telemetry.bitrouter.ai"),
            "https://telemetry.bitrouter.ai/v1/traces"
        );
        // Trailing slash is handled (no empty segment).
        assert_eq!(
            otlp_traces_endpoint("https://host:4318/"),
            "https://host:4318/v1/traces"
        );
        // An already-correct traces endpoint is left unchanged.
        assert_eq!(
            otlp_traces_endpoint("https://host/v1/traces"),
            "https://host/v1/traces"
        );
        // An operator-supplied collector path is respected, not clobbered.
        assert_eq!(
            otlp_traces_endpoint("https://collector.example/otlp/custom"),
            "https://collector.example/otlp/custom"
        );
    }

    #[test]
    fn opt_in_parses_and_defaults_off() {
        let opt_in: TelemetryOptIn =
            serde_json::from_value(serde_json::json!({"enabled": true, "level": "full"})).unwrap();
        assert!(opt_in.enabled);
        assert_eq!(opt_in.level, TelemetryLevel::Full);

        let empty: TelemetryOptIn = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!empty.enabled);
        assert_eq!(empty.level, TelemetryLevel::Metadata);
    }

    #[test]
    fn attribution_defaults_to_auto_and_parses_variants() {
        let auto: TelemetryOptIn =
            serde_json::from_value(serde_json::json!({"enabled": true})).unwrap();
        assert_eq!(auto.attribution, TelemetryAttribution::Auto);
        let anon: TelemetryOptIn = serde_json::from_value(
            serde_json::json!({"enabled": true, "attribution": "anonymous"}),
        )
        .unwrap();
        assert_eq!(anon.attribution, TelemetryAttribution::Anonymous);
        let acct: TelemetryOptIn =
            serde_json::from_value(serde_json::json!({"enabled": true, "attribution": "account"}))
                .unwrap();
        assert_eq!(acct.attribution, TelemetryAttribution::Account);
    }

    #[test]
    fn bearer_plan_anonymous_is_static_only_even_with_explicit() {
        // `anonymous` must never build a live source (which would read the
        // credential store) — even when an explicit token is set.
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Anonymous, true),
            BearerPlan::StaticOnly
        );
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Anonymous, false),
            BearerPlan::StaticOnly
        );
    }

    #[test]
    fn bearer_plan_explicit_token_is_static_only() {
        // An explicit static token wins and rides as a header — no live source
        // under either auto or account.
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Auto, true),
            BearerPlan::StaticOnly
        );
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Account, true),
            BearerPlan::StaticOnly
        );
    }

    #[test]
    fn bearer_plan_auto_builds_live_source_no_warn() {
        // `auto`, no explicit token → live source, and no warning when unmet
        // (signing in is optional under auto).
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Auto, false),
            BearerPlan::LiveSource {
                warn_if_unmet: false
            }
        );
    }

    #[test]
    fn bearer_plan_account_builds_live_source_and_warns_when_unmet() {
        // `account`, no explicit token → live source, and warn when the source
        // resolves to None (the misconfiguration must be visible).
        assert_eq!(
            telemetry_bearer_plan(TelemetryAttribution::Account, false),
            BearerPlan::LiveSource {
                warn_if_unmet: true
            }
        );
    }

    #[test]
    fn build_config_no_explicit_token_leaves_bearer_unset() {
        // With no explicit `bearer_token` and no env token, the static bearer is
        // unset — the account bearer is now a live source attached at the call
        // site, never baked into the config here.
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: None,
            attribution: TelemetryAttribution::Auto,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        assert!(cfg.bearer_token.is_none());
    }

    #[test]
    fn build_config_anonymous_drops_explicit_bearer() {
        // An explicit token is present, yet `anonymous` must export with no
        // bearer — the opt-out guarantee.
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: Some("explicit".into()),
            attribution: TelemetryAttribution::Anonymous,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        assert!(cfg.bearer_token.is_none());
    }

    #[test]
    fn build_config_explicit_token_lands_as_static_bearer() {
        // An operator-set `bearer_token` lands in the static `OtelConfig`
        // bearer (the static-header path), under auto.
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: Some("explicit".into()),
            attribution: TelemetryAttribution::Auto,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        assert_eq!(cfg.bearer_token.as_deref(), Some("explicit"));
    }

    #[test]
    fn build_config_account_no_token_exports_anonymously() {
        // `attribution: account` with no explicit token must still produce a
        // config (best-effort) — with no static bearer. The live source (and
        // its warn-when-unmet) is decided by `telemetry_bearer_plan` + the call
        // site, not here.
        let opt_in = TelemetryOptIn {
            enabled: true,
            endpoint: None,
            level: TelemetryLevel::Full,
            bearer_token: None,
            attribution: TelemetryAttribution::Account,
        };
        let cfg = build_telemetry_otel_config(opt_in, None);
        assert!(cfg.bearer_token.is_none());
    }
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
    use super::{BearerPlan, Config, build_otel_config};

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
        let plan = build_otel_config(&config)
            .expect("valid otel block is Ok")
            .expect("valid otel block yields Some");
        assert_eq!(plan.config.endpoint, "http://collector:4318");
        // The generic `otel` block never reads the credential store.
        assert_eq!(plan.bearer_plan, BearerPlan::StaticOnly);
    }

    #[test]
    fn legacy_otlp_endpoint_shim_still_works() {
        let config = config_with_observe(serde_json::json!({
            "otlp_endpoint": "http://legacy:4318"
        }));
        let plan = build_otel_config(&config)
            .expect("legacy shim is Ok")
            .expect("legacy shim yields Some");
        assert_eq!(plan.config.endpoint, "http://legacy:4318");
        assert_eq!(plan.bearer_plan, BearerPlan::StaticOnly);
    }
}

#[cfg(test)]
mod server_tools_tests {
    use std::sync::Arc;

    use bitrouter_sdk::language_model::server_tools::nested::{
        NestedOutcome, NestedRequest, NestedRunner,
    };
    use bitrouter_sdk::language_model::server_tools::toolset::ToolContext;

    use super::{Config, build_server_tool_loop};

    struct NoopRunner;
    #[async_trait::async_trait]
    impl NestedRunner for NoopRunner {
        async fn run(
            &self,
            req: NestedRequest,
            _ctx: &ToolContext,
        ) -> std::result::Result<NestedOutcome, String> {
            Ok(NestedOutcome {
                model: req.model,
                text: String::new(),
                usage: Default::default(),
            })
        }
    }

    #[test]
    fn no_loop_when_server_tools_unset() {
        let config = Config::default();
        assert!(build_server_tool_loop(&config, &None, &None, None).is_none());
    }

    #[test]
    fn no_loop_when_named_but_mcp_stack_absent() {
        let mut config = Config::default();
        config.server_tools.mcp_servers = vec!["docs".to_string()];
        // server_tools names a server but no MCP executor/routing was built,
        // and Fusion is not enabled.
        assert!(build_server_tool_loop(&config, &None, &None, None).is_none());
    }

    #[test]
    fn loop_built_for_a_nested_server_tool_even_without_mcp() {
        // Any enabled nested server tool, plus a runner, builds the loop.
        let mut fusion_cfg = Config::default();
        fusion_cfg.server_tools.fusion = Some(Default::default());
        let runner: Arc<dyn NestedRunner> = Arc::new(NoopRunner);
        assert!(build_server_tool_loop(&fusion_cfg, &None, &None, Some(runner)).is_some());

        let mut advisor_cfg = Config::default();
        advisor_cfg.server_tools.advisor = true;
        let runner2: Arc<dyn NestedRunner> = Arc::new(NoopRunner);
        assert!(build_server_tool_loop(&advisor_cfg, &None, &None, Some(runner2)).is_some());
    }

    #[test]
    fn no_loop_when_runner_present_but_no_tool_enabled() {
        // A runner with no enabled nested tool (and no MCP) yields no loop.
        let config = Config::default();
        let runner: Arc<dyn NestedRunner> = Arc::new(NoopRunner);
        assert!(build_server_tool_loop(&config, &None, &None, Some(runner)).is_none());
    }
}
