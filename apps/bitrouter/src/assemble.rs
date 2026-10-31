//! Assembly: turn a parsed [`Config`] into a running [`App`].
//!
//! This is the home of v0's `load_builtin_plugins` logic — it lives in the
//! `apps/bitrouter` **lib** (above the SDK and the plugins), wiring the builtin
//! hooks onto the `language_model` pipeline from config.

#[cfg(feature = "multi-instance")]
mod shared_writers;
#[cfg(test)]
mod writers_tests;

use std::sync::Arc;

use anyhow::{Context, Result};
use sea_orm::DatabaseConnection;

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
use crate::commands::invocation;
use crate::config::Config;
use crate::config::routing_table::ConfigRoutingTable;
#[cfg(feature = "server-tools")]
use crate::config::server_tools::web_fetch::{
    DEFAULT_MAX_CONTENT_TOKENS, WebFetchBackendConfig, WebFetchSettings,
};
#[cfg(feature = "server-tools")]
use crate::config::server_tools::web_search::{
    DEFAULT_MAX_RESULTS, WebSearchBackendConfig, WebSearchSettings,
};
use crate::gateway::assembly::{App, PromptTransform};
#[cfg(feature = "mcp")]
use crate::gateway::mcp::aggregating_executor::AggregatingExecutor;
#[cfg(feature = "mcp")]
use crate::gateway::mcp::caching_executor::{CacheTtls, CachingExecutor};
#[cfg(feature = "mcp")]
use crate::gateway::mcp::config_routing::{ConfigMcpRoutingTable, McpServerAggregateConfig};
#[cfg(feature = "mcp")]
use crate::gateway::mcp::rmcp_executor::RmcpExecutor;
use crate::gateway::model::client::HttpTimeouts;
use crate::gateway::model::protocol::OutboundDispatch;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::builder::PipelineBuilder;
use crate::gateway::pipeline::executor::HttpExecutor;
use crate::gateway::pipeline::operations::HookStage;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::advisor::AdvisorToolset;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::approval::AllowAll;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::config::ServerToolLoopConfig;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::declarations::{
    ServerToolDeclarationsHook, forwarded_tools,
};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::fusion::FusionToolset;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::fusion::alias::FusionAliasConfig;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::loop_controller::ServerToolLoop;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::mcp_toolset::McpRouterToolset;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::nested::{NestedRunner, PipelineNestedRunner};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::sub_agent::SubAgentToolset;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::toolset::{RouterToolset, ToolsetRegistry};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_fetch::backend::WebFetchBackend;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_fetch::http::{HttpFetchBackend, HttpFetchEngine};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_fetch::toolset::WebFetchToolset;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_search::backend::WebSearchBackend;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_search::http::{HttpEngine, HttpSearchBackend};
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_search::nested::NestedSearchBackend;
#[cfg(feature = "server-tools")]
use crate::gateway::pipeline::server_tools::web_search::toolset::WebSearchToolset;
use crate::gateway::providers::auth::AuthAppliers;
use bitrouter_ai::types::OperationScope;

use crate::gateway::observability::metrics::MetricsRenderer;
use crate::telemetry::OTEL_ENABLED;
use crate::telemetry::config::{ContentCaptureMode, MetricsConfig, OtelConfig};
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
use crate::telemetry::exporter::{OtelExporter, OtelObserveHook};

use crate::auth::AuthHook;
use crate::continuation::{ContinuationKeySource, ContinuationRegistry, ContinuationRuntime};
#[cfg(feature = "core")]
use crate::core::eval::EvalService;
#[cfg(feature = "core")]
use crate::core::eval::publisher::TrajectoryOutboxPublisher;
#[cfg(feature = "core")]
use crate::core::eval::settlement::EvalSettlementRecorder;
#[cfg(feature = "core")]
use crate::core::eval::store::EvalStore;
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
use crate::daemon::ObserveStatusPayload;
use crate::daemon::{NoopObserveStatus, ObserveStatusProvider};
use crate::gateway::observability::decision::PendingEvalDecisionStore;
use crate::gateway::observability::prediction::PredictiveResponseObserver;
use crate::gateway::observability::settlement::PolicySettlementRecorder;
#[cfg(feature = "harness")]
use crate::harness::acp::runtime::AcpRuntime;
use crate::metering::tariff::CaptureTariffs;
use crate::metering::{MeteringRecorder, MeteringStore, ModelPricing, PricingTable};
use crate::policy::{PolicyHook, PolicyStore};
use crate::session_identity::SessionContextHook;
use crate::trajectory::settlement::TrajectorySettlementRecorder;
use crate::trajectory::store::TrajectoryStore;
use crate::trajectory::{canonical::Canonicalizer, correlation::TrajectoryRuntime};

/// A running application plus the database connection it was assembled
/// over (the caller keeps the connection for management commands — key
/// creation, etc.).
pub struct Assembled {
    /// The fully wired application.
    pub app: App,
    /// The shared database connection.
    pub db: DatabaseConnection,
    pub(crate) tenancy: Option<crate::tenancy::Service>,
    /// In-memory API-principal-scoped ACP route leases.
    #[cfg(feature = "harness")]
    pub acp_runtime: Arc<AcpRuntime>,
    /// The policy store wired into the language_model pipeline. Held by the
    /// caller (the daemon) so `bro reload` / SIGHUP can call
    /// [`PolicyStore::reload`] alongside the routing-table reload — reload
    /// must not affect in-flight requests.
    pub policy_store: Arc<PolicyStore>,
    /// Live named routing policies loaded from `policy-lock.yaml`. The model
    /// selector and daemon reloader share this last-known-good registry.
    pub policy_runtime: Arc<crate::policy_lock::PolicyRuntime>,
    /// Canonical checkpoint evolution. Mode is owner-scoped and disabled until
    /// explicitly enabled; this handle shares the serving routing snapshots.
    #[cfg(feature = "core")]
    pub evolution: crate::core::evolution::runtime::EvolutionRuntime,
    /// Canonical gateway request tracking; capture registration shares its runtime epoch.
    pub gateway_requests: crate::gateway::observability::requests::GatewayRequestTracker,
    /// Generic eval exchange used by the local CLI and REST control plane.
    #[cfg(feature = "core")]
    pub eval_service: EvalService,
    /// Always-active encrypted provider continuation registry.
    pub continuation_registry: ContinuationRegistry,
    #[cfg(test)]
    pub(crate) pending_eval_decisions: PendingEvalDecisionStore,
    #[cfg(test)]
    pub(crate) response_observer: PredictiveResponseObserver,
    /// Durable publisher shared by request settlement and startup/shutdown drains.
    #[cfg(feature = "core")]
    pub trajectory_outbox_publisher: Option<TrajectoryOutboxPublisher>,
    /// Concrete handle on the routing table. The pipeline above also
    /// holds the same `Arc` (via `&dyn RoutingTable`), but reload code
    /// needs the concrete type to call
    /// [`ConfigRoutingTable::replace_config`] when there's no source
    /// file to re-read from (zero-config mode).
    pub routing_table: Arc<ConfigRoutingTable>,
    /// Concrete upstream HTTP executor. The pipeline also holds this as a trait
    /// object, but reload needs the concrete handle to replace timeout clients.
    pub upstream_executor: Arc<HttpExecutor>,
    /// Snapshot provider for `bro observe status`. When the OTel
    /// exporter is wired, this reports its live state; when not, it
    /// reports `compiled_in` truthfully and everything else blank.
    pub observe: Arc<dyn ObserveStatusProvider>,
    /// Live OTel exporter handle, when one is wired. Held so the binary
    /// can hand it to the `tracing-opentelemetry` bridge layer at
    /// subscriber-init time (the bridge captures its tracer eagerly).
    /// `None` when OTel is disabled in config.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub otel_exporter: Option<Arc<OtelExporter>>,
    /// Captured `OtelExporter::new` failure message, when one occurred.
    /// Surfaced as a `tracing::error!` line by the binary after the full
    /// subscriber is installed — `OtelExporter::new` itself runs before
    /// the subscriber on the `serve` path, so logging directly here
    /// would be dropped.
    pub otel_init_error: Option<String>,
    /// Startup diagnostics for ignored configuration or inactive registrations:
    /// an unrecognised `plugins.<id>` block, a renamed environment variable,
    /// or a compiled capability that configuration does not declare.
    ///
    /// Carried out rather than logged in place, for the same reason as
    /// [`Self::otel_init_error`]: on the `serve` path assembly runs *before*
    /// the tracing subscriber is installed, so a `tracing::warn!` here reaches
    /// nobody. Emitted by the binary once the subscriber is up.
    pub ignored_config: Vec<String>,
}

/// Authority already validated and loaded for one immutable shared generation.
/// The coordinator owns migrations, publication and process lifecycle.
#[cfg(feature = "multi-instance")]
pub(crate) struct SharedAssembly {
    pub continuation: crate::gateway::shared::continuation::SharedContinuationDb,
    pub telemetry: Arc<crate::gateway::shared::telemetry::Telemetry>,
    pub auth_appliers: AuthAppliers,
    pub correlation_key: crate::trajectory::canonical::CorrelationKey,
    pub policies: Vec<crate::policy::Policy>,
    pub policy_lock: Option<crate::policy_lock::PolicyLock>,
}

#[cfg(feature = "multi-instance")]
pub(crate) async fn build_shared_app(
    config: &Config,
    inputs: &SharedAssembly,
) -> Result<Assembled> {
    validate_shared_assembly(config, inputs.continuation.database())?;
    shared_writers::validate_policies(&inputs.policies)?;
    let crate::config::gateway_state::GatewayStateConfig::Shared(shared) = &config.gateway_state
    else {
        anyhow::bail!("shared assembly requires explicit shared process configuration");
    };
    anyhow::ensure!(
        shared.namespace == inputs.continuation.namespace(),
        "shared assembly namespace differs from its injected authority"
    );
    assemble_app(config, None, Default::default(), Some(inputs)).await
}

#[cfg(feature = "multi-instance")]
fn validate_shared_assembly(config: &Config, database: &DatabaseConnection) -> Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend};

    anyhow::ensure!(
        matches!(
            config.gateway_state,
            crate::config::gateway_state::GatewayStateConfig::Shared(_)
        ),
        "shared assembly requires explicit shared process configuration"
    );
    anyhow::ensure!(
        !config.inherit_defaults && !config.registry.enabled,
        "shared assembly requires a complete published routing snapshot"
    );
    anyhow::ensure!(
        config.mcp_servers.is_empty(),
        "shared assembly does not support MCP server workers"
    );
    let tools = &config.server_tools;
    anyhow::ensure!(
        tools.mcp_servers.is_empty()
            && !tools.advisor
            && !tools.subagent
            && tools.fusion.is_none()
            && tools.web_search.is_none()
            && tools.web_fetch.is_none(),
        "shared assembly does not support server tool execution"
    );
    anyhow::ensure!(
        config.decision_model.is_none() && config.checkers.is_empty() && !config.agent_api.enabled,
        "shared assembly requires published routing without local decision, extension or harness execution"
    );
    anyhow::ensure!(
        database.get_database_backend() == DatabaseBackend::Postgres,
        "shared assembly requires an injected PostgreSQL connection"
    );
    Ok(())
}

#[cfg(all(test, feature = "multi-instance"))]
mod shared_assembly_tests {
    use super::*;

    #[tokio::test]
    async fn shared_assembly_rejects_a_local_database_before_initialization() -> Result<()> {
        let config = crate::config::parse_with(
            r#"
inherit_defaults: false
registry:
  enabled: false
database:
  url: "postgres://127.0.0.1/not-opened"
gateway_state:
  mode: shared
  namespace: assembly-test
  deployment_key_env: BITROUTER_ASSEMBLY_TEST_KEY
"#,
            |_| None,
        )?;
        let database = crate::db::connect("sqlite::memory:").await?;
        let error = validate_shared_assembly(&config, &database)
            .err()
            .context("shared construction accepted a local database")?;
        assert_eq!(
            error.to_string(),
            "shared assembly requires an injected PostgreSQL connection"
        );
        let mut forbidden = config.clone();
        forbidden.mcp_servers = serde_json::from_value(serde_json::json!({
            "unstarted": { "name":"unstarted", "transport": { "type":"http", "url":"http://127.0.0.1:1/mcp" } }
        }))?;
        let error = validate_shared_assembly(&forbidden, &database)
            .err()
            .context("shared MCP worker accepted")?;
        assert_eq!(
            error.to_string(),
            "shared assembly does not support MCP server workers"
        );
        forbidden = config;
        forbidden.server_tools.advisor = true;
        let error = validate_shared_assembly(&forbidden, &database)
            .err()
            .context("shared tool execution accepted")?;
        assert_eq!(
            error.to_string(),
            "shared assembly does not support server tool execution"
        );
        Ok(())
    }
}

pub(crate) fn resolved_upstream_timeouts(
    config: &Config,
) -> (
    HttpTimeouts,
    std::collections::HashMap<String, HttpTimeouts>,
) {
    let global_timeouts = config.upstream.timeouts.apply_to(HttpTimeouts::default());
    let provider_timeouts: std::collections::HashMap<String, HttpTimeouts> = config
        .providers
        .iter()
        .filter_map(|(id, provider)| {
            let resolved = provider.timeouts.apply_to(global_timeouts.clone());
            (resolved != global_timeouts).then(|| (id.clone(), resolved))
        })
        .collect();
    (global_timeouts, provider_timeouts)
}

/// `ObserveStatusProvider` impl backed by a real [`OtelExporter`]. The
/// payload type lives in the daemon module (wire format), and the
/// observe-side `OtelStatus` lives in the plugin (no daemon dep); this
/// adapter does the field-by-field copy so neither crate has to import
/// the other's type.
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
struct OtelExporterStatus {
    exporter: Arc<OtelExporter>,
}

#[async_trait::async_trait]
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
impl ObserveStatusProvider for OtelExporterStatus {
    fn status(&self) -> ObserveStatusPayload {
        otel_status(&self.exporter)
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

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
pub(crate) fn otel_status(exporter: &OtelExporter) -> ObserveStatusPayload {
    let s = exporter.status();
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

/// Every `plugins.<id>` key this binary reads.
///
/// `Config::plugins` is an unvalidated `HashMap<String, Value>` and
/// `dist/schema/bitrouter.config.schema.json` declares it
/// `additionalProperties: true`, so **an unrecognised id is silently
/// ignored** — neither the parser nor the schema will say a word. A typo like
/// `plugins.bitrouter-guardrail` (singular) drops the operator's declared
/// block / redact patterns and the process starts anyway. This list is what
/// [`unknown_plugin_ids`] reports against, and it is the only thing standing
/// between that class of typo and a silent misconfiguration.
///
/// This list is derived from the call sites by
/// `known_plugin_ids_cover_every_reader`, which scans this file's own source.
/// Adding a reader without adding it here fails that test — the list cannot
/// silently drift, which matters because a *missing* id is worse than no guard
/// at all: the daemon would warn, on every start, about a plugin it reads.
///
/// It shipped wrong exactly once, for exactly that reason: `bitrouter-policy`
/// is read through a line-wrapped `config` / `.plugins` / `.get(…)` chain that
/// a single-line grep did not see. Hence the scan.
pub const KNOWN_PLUGIN_IDS: &[&str] = &["bitrouter-policy", "bitrouter-telemetry"];

/// Reject removed built-in configuration before activating the default host.
///
/// Presence, not content, is the migration fence: even null/empty legacy config
/// must be explicitly removed after the operator has reviewed protection scope.
/// This belongs to the product host, not the SDK parser: trusted custom hosts
/// may still explicitly install the compatibility guardrails plugin.
pub fn validate_host_configuration(config: &Config) -> Result<()> {
    validate_host_configuration_inner(config, true)
}

fn validate_host_configuration_inner(config: &Config, resolve_telemetry: bool) -> Result<()> {
    config.tenancy.validate(config)?;
    crate::telemetry::receipt_latency::Config::read(config)?;
    config
        .gateway_state
        .validate(&config.database.url, config.server.skip_auth)?;
    anyhow::ensure!(
        cfg!(feature = "multi-instance")
            || matches!(
                config.gateway_state,
                crate::config::gateway_state::GatewayStateConfig::Local {}
            ),
        "gateway_state.mode=shared requires the multi-instance feature"
    );
    crate::db::validate_backend_availability(&config.database.url)?;
    let tools = &config.server_tools;
    anyhow::ensure!(
        cfg!(feature = "server-tools")
            || (tools.mcp_servers.is_empty()
                && !tools.advisor
                && !tools.subagent
                && tools.fusion.is_none()
                && tools.web_search.is_none()
                && tools.web_fetch.is_none()),
        "selected server_tools require the server-tools feature"
    );
    anyhow::ensure!(
        cfg!(feature = "mcp") || config.mcp_servers.is_empty(),
        "configured MCP servers require the mcp feature"
    );
    anyhow::ensure!(
        cfg!(feature = "harness") || !config.agent_api.enabled,
        "agent_api.enabled requires the harness feature"
    );
    if resolve_telemetry {
        validate_telemetry_availability(config)?;
    }
    anyhow::ensure!(
        !config.plugins.contains_key("bitrouter-guardrails"),
        "plugins.bitrouter-guardrails requires migration: the default bro no longer \
         includes the guardrails matcher. Compile the regex extension into a custom host, \
         register it through crate::gateway::extensions::registry::ExtensionApi, and bind it through \
         routers.<id>.checks.request. The request-check capability checks input \
         only and does not replace global or output block/redact protection. Review \
         every protected entry point and output requirement before explicitly removing \
         the old key, including empty/null configuration; see docs/GUARDRAILS_EXTENSION.md."
    );
    Ok(())
}

/// Reject explicit export when this artifact has no OTLP transport, before
/// resolving installation or credential state. Enabled exporters retain their
/// existing initialization and failure policy at the owning call site.
pub(crate) fn validate_telemetry_availability(config: &Config) -> Result<()> {
    if !OTEL_ENABLED {
        build_otel_config(config, || None)?;
    }
    Ok(())
}

/// Sub-keys that were removed, and the block they sat under. Reported for the
/// same reason as [`RENAMED_ENV_VARS`]: the guard below is id-level, so a
/// migration that renames the *block* correctly and carries a dead sub-key
/// with it would otherwise be silent — and that is the natural migration, since
/// the id-level warning tells an operator to rename and nothing more.
const REMOVED_PLUGIN_SUBKEYS: &[(&str, &str, &str)] = &[(
    "bitrouter-telemetry",
    "otlp_endpoint",
    "removed with the `plugins.bitrouter-observe` rename; use `otel: { endpoint: … }`",
)];

/// `BITROUTER_*` environment variables that were renamed, and what to.
///
/// A migration aid, not a general guard: env has no schema and no validation
/// hook, and a config may legitimately reference any `${BITROUTER_...}` name
/// through `${VAR}` substitution, so a blanket "unrecognised `BITROUTER_*`"
/// scan would fire on the operator's own variables. Naming the renames
/// exactly is precise, has no false positives, and is what an operator hitting
/// this actually needs to be told.
///
/// **These do not universally fail closed, and an earlier version of this
/// comment claimed they did.** They do when the environment is the *only*
/// source: `ContentCaptureMode` defaults to `Off` and the cap to 128 KiB, so a
/// stale name leaves less content exported, not more.
///
/// They fail *open* when the environment was overriding YAML downward.
/// `with_env_overrides` assigns unconditionally, so an operator whose config
/// sets `content_capture: full` (directly, or via `telemetry.level: full`) and
/// who pinned `BITROUTER_OBSERVE_CONTENT_CAPTURE=off` in a service unit as a
/// kill-switch gets the opposite of what they pinned: the stale name is
/// ignored, YAML wins, and prompt and response bodies start leaving the
/// process. Same shape for a cap the environment lowered.
///
/// That is why this is a warning rather than a footnote, and why it names the
/// new variable rather than only reporting the old one as unrecognised.
const RENAMED_ENV_VARS: &[(&str, &str)] = &[
    (
        "BITROUTER_OBSERVE_CONTENT_CAPTURE",
        "BITROUTER_TELEMETRY_CONTENT_CAPTURE",
    ),
    (
        "BITROUTER_OBSERVE_CONTENT_ATTR_MAX_BYTES",
        "BITROUTER_TELEMETRY_CONTENT_ATTR_MAX_BYTES",
    ),
];

/// The `plugins.*` keys in `config` that this binary does not read, sorted.
///
/// Pure so `bro config validate` can report the same set the daemon
/// warns about, without building an `App`.
pub fn unknown_plugin_ids(config: &Config) -> Vec<String> {
    let mut unknown: Vec<String> = config
        .plugins
        .keys()
        .filter(|id| !KNOWN_PLUGIN_IDS.contains(&id.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown
}

/// Everything this binary read but will not act on, as operator-facing lines.
///
/// Collected in [`build_app_with_path`] so it covers every daemon start rather
/// than only `bro config validate` — validation is opt-in and the daemon
/// always runs, which is the wrong way round for a failure this quiet. The
/// caller emits them; see [`Assembled::ignored_config`] for why.
///
/// Public because `bro config validate` reports the same set from a
/// file it may never run against — see [`ignored_config_warnings`] for why the
/// environment half is split off.
pub fn ignored_config_file_warnings(config: &Config) -> Vec<String> {
    let known = KNOWN_PLUGIN_IDS.join(", ");
    unknown_plugin_ids(config)
        .into_iter()
        .map(|id| {
            format!("plugins.{id} is not read by this binary and is ignored; known ids: {known}")
        })
        .chain(REMOVED_PLUGIN_SUBKEYS.iter().filter_map(|(id, key, note)| {
            config
                .plugins
                .get(*id)
                .and_then(|block| block.get(*key))
                .map(|_| format!("plugins.{id}.{key} is ignored — {note}"))
        }))
        .collect()
}

/// [`ignored_config_file_warnings`] plus the environment ones.
///
/// The split is what `bro config validate` needs: it validates a *file*,
/// possibly one belonging to another machine, so reporting this process's
/// environment there would be noise at best and misleading at worst. Every
/// runtime surface wants both.
///
/// Public because standalone ACP surfaces never build an `App`. They report
/// these warnings directly, whether or not an exporter is compiled in; a guard
/// covering only daemon assembly would leave their configuration mistakes silent.
pub fn ignored_config_warnings(config: &Config) -> Vec<String> {
    ignored_config_file_warnings(config)
        .into_iter()
        .chain(renamed_env_warnings(|name| {
            std::env::var_os(name).is_some()
        }))
        .collect()
}

/// One message per [`RENAMED_ENV_VARS`] entry whose **old** name is set and
/// whose new name is not.
///
/// Takes the presence check as a closure rather than reading the environment,
/// so it is unit-testable without touching process-global state that would
/// make it order-dependent inside a shared test binary — the same reason
/// `telemetry_bearer_plan` below is pure.
///
/// Setting the new name suppresses the warning even when the old one is also
/// present: the operator has migrated and left a stale export behind, which is
/// not worth a line on every start.
fn renamed_env_warnings(is_set: impl Fn(&str) -> bool) -> Vec<String> {
    RENAMED_ENV_VARS
        .iter()
        .filter(|(old, new)| is_set(old) && !is_set(new))
        .map(|(old, new)| format!("{old} was renamed to {new}; the old name is ignored"))
        .collect()
}

/// Assemble an [`App`] from a parsed config: connect the database, run the
/// host's migrations, build the routing table + executor, and wire the
/// builtin hooks onto the `language_model` pipeline.
pub async fn build_app(config: &Config) -> Result<Assembled> {
    build_app_with_path(config, None).await
}

/// Like [`build_app`], but remembering the config's source path so the routing
/// table's `reload()` (driven by `bro reload` / `SIGHUP`) can re-read it.
pub async fn build_app_with_path(
    config: &Config,
    config_path: Option<&std::path::Path>,
) -> Result<Assembled> {
    build_app_with_extensions(config, config_path, |_| Ok(())).await
}

/// Assemble a custom host through the unified, capability-scoped extension API.
///
/// Registration completes before configuration validation, database access or
/// any other startup work. Registered capabilities remain inert until the
/// configuration binds them to a router.
pub async fn build_app_with_extensions(
    config: &Config,
    config_path: Option<&std::path::Path>,
    register: impl FnOnce(&mut crate::gateway::extensions::registry::ExtensionApi) -> Result<()>,
) -> Result<Assembled> {
    let mut extensions = crate::gateway::extensions::registry::ExtensionApi::new();
    register(&mut extensions).context("registering extensions")?;
    let native = extensions
        .into_registrations()
        .context("registering extensions")?;
    assemble_app(
        config,
        config_path,
        native,
        #[cfg(feature = "multi-instance")]
        None,
    )
    .await
}

async fn assemble_app(
    config: &Config,
    config_path: Option<&std::path::Path>,
    native: std::collections::HashMap<
        String,
        crate::gateway::extensions::registry::registration::Registration,
    >,
    #[cfg(feature = "multi-instance")] shared: Option<&SharedAssembly>,
) -> Result<Assembled> {
    #[cfg(feature = "multi-instance")]
    let is_shared = shared.is_some();
    #[cfg(not(feature = "multi-instance"))]
    let is_shared = false;
    anyhow::ensure!(
        is_shared
            || matches!(
                config.gateway_state,
                crate::config::gateway_state::GatewayStateConfig::Local {}
            ),
        "shared gateway requires the managed shared serving lifecycle"
    );
    let mut effective_config = config.clone();
    crate::policy_lock::apply_defaults(&mut effective_config, config_path);
    let config = &effective_config;
    validate_host_configuration_inner(config, !is_shared)?;
    config.validate_router_config()?;
    let receipt_observation_config = crate::telemetry::receipt_latency::Config::read(config)?;
    let decision_runtime = config
        .decision_model
        .as_ref()
        .map(|decision| {
            let credential = std::env::var(&decision.api_key_env).with_context(|| {
                format!(
                    "reading decision_model credential from {}",
                    decision.api_key_env
                )
            })?;
            let executor = crate::gateway::model::decision::typesafe::TypeSafeExecutor::new(
                &decision.base_url,
                &credential,
                std::time::Duration::from_millis(decision.timeout_ms),
                decision.max_response_bytes,
            )?;
            Ok::<_, anyhow::Error>(crate::gateway::model::decision::DecisionRuntime {
                model: decision.model.clone(),
                executor: Arc::new(executor),
                policy: decision.policy.clone(),
                pricing: decision.pricing.clone(),
            })
        })
        .transpose()?;
    let mut inactive = native
        .keys()
        .filter(|id| !config.checkers.contains_key(*id))
        .collect::<Vec<_>>();
    inactive.sort();
    let mut ignored_config = if is_shared {
        ignored_config_file_warnings(config)
    } else {
        ignored_config_warnings(config)
    };
    ignored_config.extend(inactive.into_iter().map(|id| {
        format!("request-check registration '{id}' is inactive: no checkers.{id} declaration")
    }));
    let request_checks = Arc::new(
        crate::request_checks::RequestCheckRuntime::activate_with_registrations(config, native)?,
    );
    // Validate and construct ingress aliases before opening the database or
    // performing any other startup work. A custom transform must not run ahead
    // of Stage 0 and shadow `@preset` or reserved `bitrouter/` addresses.
    #[cfg(feature = "server-tools")]
    let fusion_alias = build_fusion_alias(config)?;

    // ---- database + migrations ----
    // `database.url` may name any backend sea-orm supports (sqlite /
    // postgres / mysql); `crate::db::connect` handles the per-backend
    // first-run conveniences (a SQLite file URL is created on demand).
    // The schema is applied from `crate::db::migration` — Rust code, not
    // SQL — so it lands identically on whichever backend is configured.
    #[cfg(feature = "multi-instance")]
    let shared_db = shared.map(|inputs| inputs.continuation.database().clone());
    #[cfg(not(feature = "multi-instance"))]
    let shared_db: Option<DatabaseConnection> = None;
    let tenancy_database = if config.tenancy.enabled {
        Some(crate::db::connect_owned_tenancy(&config.database.url).await?)
    } else {
        None
    };
    let db = match shared_db {
        Some(database) => database,
        None => {
            let database = match &tenancy_database {
                Some(database) => Ok(database.database().clone()),
                None => crate::db::connect(&config.database.url).await,
            }
            .context("connecting to database")?;
            crate::db::run_migrations(&database)
                .await
                .context("running database migrations")?;
            database
        }
    };
    let tenancy_enabled = tenancy_database.is_some();
    #[cfg(feature = "harness")]
    let acp_runtime = Arc::new(AcpRuntime::new());
    // Resolve dynamic authentication once and share the same registry with
    // continuation route matching and the HTTP executor. This lets mapped
    // Responses requests preflight the provider's stable credential authority
    // and lets transport return the exact proof used by the sent request.
    let cloud_manager = if is_shared {
        None
    } else {
        Some(crate::cloud::default_manager()?)
    };
    #[cfg(feature = "multi-instance")]
    let shared_auth = shared.map(|inputs| inputs.auth_appliers.clone());
    #[cfg(not(feature = "multi-instance"))]
    let shared_auth: Option<AuthAppliers> = None;
    let auth_appliers = match shared_auth {
        Some(auth) => auth,
        None => build_auth_appliers(
            config,
            cloud_manager
                .clone()
                .context("local credential manager is unavailable")?,
        )?,
    };
    let (continuation_registry, private_context, continuation_runtime) = if tenancy_enabled {
        (None, None, None)
    } else {
        #[cfg(feature = "multi-instance")]
        let shared_continuation =
            shared.map(|inputs| ContinuationKeySource::fixed(inputs.continuation.key()));
        #[cfg(not(feature = "multi-instance"))]
        let shared_continuation: Option<ContinuationKeySource> = None;
        let continuation_keys = match shared_continuation {
            Some(keys) => keys,
            None => {
                let runtime_home = match config_path.and_then(std::path::Path::parent) {
                    Some(home) => home.to_path_buf(),
                    None => std::env::current_dir().context("resolve continuation key home")?,
                };
                ContinuationKeySource::lazy(runtime_home)
            }
        };
        let private_context = crate::continuation::native_context::PrivateContextPolicy::new(
            continuation_keys.clone(),
        );
        let continuation_registry = ContinuationRegistry::new(
            db.clone(),
            continuation_keys,
            config.continuation.retention_days,
            config.continuation.prune_batch_size,
        )?;
        #[cfg(feature = "multi-instance")]
        let continuation_registry = if let Some(inputs) = shared {
            ContinuationRegistry::new_shared(
                inputs.continuation.clone(),
                config.continuation.retention_days,
                config.continuation.prune_batch_size,
            )?
        } else {
            continuation_registry
        };
        if !is_shared {
            let continuation_pruned = continuation_registry.prune(chrono::Utc::now()).await?;
            if continuation_pruned > 0 {
                tracing::info!(
                    deleted_rows = continuation_pruned,
                    "pruned expired provider continuations at startup"
                );
            }
        }
        let continuation_runtime = ContinuationRuntime::with_auth_appliers(
            continuation_registry.clone(),
            auth_appliers.clone(),
        );
        (
            Some(continuation_registry),
            Some(private_context),
            Some(continuation_runtime),
        )
    };
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
    if !is_shared {
        crate::providers::apply::apply_builtin_defaults(&mut resolved);
        // Subscription / "use your Claude Code session" logins persist their
        // credential in the store (not the config), so apply_builtin_defaults would
        // mark a keyless provider like `anthropic` inactive. Re-activate any
        // provider that has a stored credential so it stays routable.
        if let Ok(store) = crate::provider_credentials::load_default() {
            crate::providers::apply::activate_stored_credential_providers(&mut resolved, &store);
        }
        crate::config::discover_models(&mut resolved).await;
    }
    let routing_table = Arc::new(match config_path {
        Some(path) => ConfigRoutingTable::from_config_with_path(resolved, path),
        None => ConfigRoutingTable::from_config(resolved),
    });
    // Hand-off clone — the builder closure below moves `routing_table`
    // into the App pipeline, but the daemon's reloader needs the
    // concrete type to call `replace_config` in zero-config mode.
    let routing_table_for_reload = routing_table.clone();
    let continuation_for_pre_request = continuation_runtime.clone();
    let continuation_for_route = continuation_runtime.clone();
    let continuation_for_finalization = continuation_runtime;
    // Upstream timeouts: the `upstream.timeouts` block layered over the
    // built-in defaults, plus a per-provider override for any provider whose
    // resolved timeouts differ (v0 #394 fixed these; now they're configurable).
    // Read from the user's `config` — per-provider timeouts are a user-only
    // field, never set by the registry/builtin-defaults merge.
    let (global_timeouts, provider_timeouts) = resolved_upstream_timeouts(config);
    let dispatch = OutboundDispatch::builtin();

    let executor = HttpExecutor::with_provider_timeouts(
        global_timeouts,
        provider_timeouts,
        dispatch,
        auth_appliers,
    )
    .context("building the upstream HTTP executor")?;
    #[cfg(feature = "multi-instance")]
    let executor = if shared.is_some() {
        executor.require_shared_authority()
    } else {
        executor
    };
    let executor = Arc::new(executor);
    let executor_for_reload = executor.clone();

    // ---- pricing, metering, policy — all derived from config ----
    let pricing = Arc::new(build_pricing_table(&routing_table.snapshot_config()));
    let metering_store = MeteringStore::new(db.clone());
    let metering_store_for_policy = metering_store.clone();
    let metering_store_for_recorder = metering_store.clone();
    let metering_for_native_costs = metering_store.clone();
    let pricing_for_recorder = pricing.clone();
    let pricing_for_native = pricing.clone();
    #[cfg(feature = "multi-instance")]
    let shared_policies = shared.map(|inputs| PolicyStore::from_policies(inputs.policies.clone()));
    #[cfg(not(feature = "multi-instance"))]
    let shared_policies: Option<PolicyStore> = None;
    let policy_store: Arc<PolicyStore> = Arc::new(match shared_policies {
        Some(policies) => policies,
        None => load_policy_store(config).await?,
    });
    let policy_store_for_reload = policy_store.clone();

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
    #[cfg(all(
        feature = "multi-instance",
        any(feature = "otel-http", feature = "otel-grpc")
    ))]
    let injected_exporter = shared.map(|inputs| inputs.telemetry.exporter());
    #[cfg(all(
        not(feature = "multi-instance"),
        any(feature = "otel-http", feature = "otel-grpc")
    ))]
    let injected_exporter: Option<Option<Arc<OtelExporter>>> = None;
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let (otel_exporter, otel_init_error): (Option<Arc<OtelExporter>>, Option<String>) =
        match injected_exporter {
            Some(exporter) => (exporter, None),
            None => match build_otel_config(config, telemetry_install_id)? {
                Some((otel_config, bearer_plan)) => {
                    // Resolve the LIVE account-bearer source per the plan. Only the
                    // telemetry opt-in with `attribution: auto`/`account` and no
                    // explicit token wants one; `anonymous` (and every other
                    // exporter path) is `StaticOnly` and never reads the credential
                    // store. Best-effort: a `None` source means the export proceeds
                    // anonymously — for `account` we additionally warn.
                    let bearer: Option<Arc<dyn crate::telemetry::bearer::TelemetryBearer>> =
                        match bearer_plan {
                            BearerPlan::LiveSource { warn_if_unmet } => {
                                let source = match cloud_manager.as_ref() {
                                    Some(manager) => {
                                        crate::cloud::cloud_bearer_source(
                                            Arc::clone(manager),
                                            otel_config.endpoint.clone(),
                                        )
                                        .await
                                    }
                                    None => None,
                                };
                                if source.is_none() && warn_if_unmet {
                                    tracing::warn!(
                                        "telemetry: attribution=account but no signed-in session \
                                     is available — exporting anonymously (sign in with `{} \
                                     cloud login`)",
                                        invocation::name()
                                    );
                                }
                                source
                            }
                            BearerPlan::StaticOnly => None,
                        };
                    match OtelExporter::new(otel_config, bearer) {
                        Ok(exporter) => (Some(Arc::new(exporter)), None),
                        Err(e) => (
                            None,
                            Some(format!("failed to initialise OpenTelemetry: {e}")),
                        ),
                    }
                }
                None => (None, None),
            },
        };
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let receipt_observer = if receipt_observation_config.enabled {
        otel_exporter
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("receipt latency exporter initialization failed"))?
            .receipt_observer(&receipt_observation_config)?
    } else {
        crate::telemetry::receipt_latency::Sink::default()
    };
    #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
    let receipt_observer = {
        anyhow::ensure!(
            !receipt_observation_config.enabled,
            "receipt latency requires an OTLP transport"
        );
        crate::telemetry::receipt_latency::Sink::default()
    };
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let default_observe: Arc<dyn ObserveStatusProvider> = match otel_exporter.clone() {
        Some(exporter) => Arc::new(OtelExporterStatus { exporter }),
        None => Arc::new(NoopObserveStatus {
            compiled_in: OTEL_ENABLED,
        }),
    };
    #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
    let default_observe: Arc<dyn ObserveStatusProvider> =
        Arc::new(NoopObserveStatus { compiled_in: false });
    #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
    let otel_init_error = None;
    #[cfg(feature = "multi-instance")]
    let observe_provider = shared
        .map(|inputs| inputs.telemetry.observe())
        .unwrap_or(default_observe);
    #[cfg(not(feature = "multi-instance"))]
    let observe_provider = default_observe;
    // Two handles to the same exporter: one moves into the pipeline as an
    // `ObserveHook`, the other lands on `Assembled` so the binary can hand
    // it to the `tracing-opentelemetry` bridge layer.
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    let otel_for_hook = otel_exporter.clone();
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
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

    #[cfg(feature = "mcp")]
    let mcp_routing: Option<Arc<dyn crate::gateway::mcp::RoutingTable>> =
        if config.mcp_servers.is_empty() {
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
    #[cfg(feature = "mcp")]
    let mcp_executor = mcp_routing.as_ref().map(|_| {
        let rmcp: Arc<RmcpExecutor> = Arc::new(RmcpExecutor::new().with_protocol_version(
            crate::gateway::mcp::upstream_protocol_version(config.mcp.upstream_protocol),
        ));
        let inner_for_cache: Arc<RmcpExecutor> = rmcp.clone();
        if config.mcp.cache.enabled {
            let ttls: CacheTtls = (&config.mcp.cache).into();
            let cached: Arc<CachingExecutor<RmcpExecutor>> = Arc::new(
                CachingExecutor::new(inner_for_cache, ttls)
                    .with_invalidation(rmcp.invalidation_receiver()),
            );
            Arc::new(AggregatingExecutor::new(cached)) as Arc<dyn crate::gateway::mcp::Executor>
        } else {
            Arc::new(AggregatingExecutor::new(inner_for_cache))
                as Arc<dyn crate::gateway::mcp::Executor>
        }
    });

    #[cfg(not(feature = "mcp"))]
    let mcp_routing: Option<Arc<dyn crate::gateway::mcp::RoutingTable>> = None;
    #[cfg(not(feature = "mcp"))]
    let mcp_executor: Option<Arc<dyn crate::gateway::mcp::Executor>> = None;

    // Nested-completion runner for the advisor / sub-agent / fusion server tools
    // — built when any of them is configured. Their nested calls run on a
    // dedicated, loop-less sub-completion pipeline (so a worker cannot recursively
    // invoke another server tool), reusing the same routing table and upstream
    // executor as the main pipeline. The metering recorder is attached so each
    // nested completion is billed to the same caller as the parent request (no
    // auth or policy hooks — a nested call is a sub-operation of an
    // already-authorised request, and the parent principal rides the caller).
    #[cfg(feature = "server-tools")]
    let server_tools_enabled = config.server_tools.fusion.is_some()
        || config.server_tools.advisor
        || config.server_tools.subagent
        || config.server_tools.web_search.is_some()
        || config.server_tools.web_fetch.is_some();
    #[cfg(feature = "server-tools")]
    let nested_runner: Option<Arc<dyn NestedRunner>> = if server_tools_enabled {
        let mut sub = PipelineBuilder::new();
        sub.routing_table(routing_table.clone())
            .executor(executor.clone())
            .route_hook(CaptureTariffs::new(
                pricing.clone(),
                config.server.require_known_pricing,
            ))
            .settlement_recorder(
                MeteringRecorder::new(metering_store.clone(), pricing.clone())
                    .with_reconciliation_provider("bitrouter"),
            );
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
    #[cfg(feature = "server-tools")]
    let server_tool_loop =
        build_server_tool_loop(config, &mcp_routing, &mcp_executor, nested_runner);

    let policy_decision_recorder = if is_shared {
        None
    } else {
        crate::gateway::routing::workflow::decision::PolicyDecisionJsonlRecorder::from_env()
            .map_err(anyhow::Error::from)?
            .map(Arc::new)
    };
    if policy_decision_recorder.is_some() {
        tracing::info!(
            env = crate::gateway::routing::workflow::decision::POLICY_DECISION_JSONL_ENV,
            "policy decision JSONL recording enabled"
        );
    }
    let pending_eval_decisions = PendingEvalDecisionStore::default();
    #[cfg(feature = "core")]
    let eval_store = EvalStore::new(db.clone());
    #[cfg(feature = "core")]
    let eval_service = EvalService::new(eval_store.clone(), config.eval.clone());
    #[cfg(feature = "core")]
    let mut trajectory_outbox_publisher = None;
    let trajectory_services = if config.trajectory.enabled && !config.tenancy.enabled {
        let trajectory_store = TrajectoryStore::new(db.clone());
        let retention = chrono::TimeDelta::try_days(i64::from(config.trajectory.retention_days))
            .ok_or_else(|| anyhow::anyhow!("trajectory retention_days exceeds time range"))?;
        let cutoff = chrono::Utc::now()
            .checked_sub_signed(retention)
            .ok_or_else(|| anyhow::anyhow!("trajectory retention cutoff exceeds time range"))?
            .to_rfc3339();
        let pruned = if is_shared {
            crate::trajectory::store::PruneSummary::default()
        } else {
            trajectory_store
                .prune_before(&cutoff, false, config.trajectory.outbox_batch_size)
                .await?
        };
        if pruned != crate::trajectory::store::PruneSummary::default() {
            tracing::info!(
                cutoff,
                delivered_outbox_rows = pruned.delivered_outbox_rows,
                episode_rows = pruned.episode_rows,
                event_rows = pruned.event_rows,
                request_rows = pruned.request_rows,
                "pruned retained trajectory history at startup"
            );
        }
        let notify_outbox = {
            #[cfg(feature = "core")]
            {
                if is_shared {
                    None
                } else {
                    let publisher = TrajectoryOutboxPublisher::new(
                        trajectory_store.clone(),
                        eval_store.clone(),
                        config.eval.clone(),
                        config.trajectory.outbox_batch_size,
                    )?;
                    let startup_publication = publisher.drain_pending().await?;
                    if startup_publication.attempted > 0 {
                        tracing::info!(
                            attempted = startup_publication.attempted,
                            delivered = startup_publication.delivered,
                            failed = startup_publication.failed,
                            "drained durable trajectory evaluation outbox at startup"
                        );
                    }
                    trajectory_outbox_publisher = Some(publisher.clone());
                    Some(Arc::new(move || publisher.kick()) as Arc<dyn Fn() + Send + Sync>)
                }
            }
            #[cfg(not(feature = "core"))]
            {
                None
            }
        };
        #[cfg(feature = "multi-instance")]
        let shared_correlation = shared.map(|inputs| inputs.correlation_key.clone());
        #[cfg(not(feature = "multi-instance"))]
        let shared_correlation: Option<crate::trajectory::canonical::CorrelationKey> = None;
        let correlation_key = match shared_correlation {
            Some(key) => key,
            None => {
                let home = match config_path.and_then(std::path::Path::parent) {
                    Some(home) => home.to_path_buf(),
                    None => std::env::current_dir().context("resolve trajectory key home")?,
                };
                crate::paths::get_or_create_correlation_key(&home)?
            }
        };
        let runtime = Arc::new(TrajectoryRuntime::new(
            trajectory_store.clone(),
            Canonicalizer::new(correlation_key.clone()),
        ));
        // Shared guard entry still uses this runtime marker; the shared engine
        // owns its durable start/settlement path rather than this local recorder.
        let settlement = (!is_shared).then(|| {
            TrajectorySettlementRecorder::new(trajectory_store, notify_outbox, correlation_key)
        });
        Some((runtime, settlement))
    } else {
        None
    };
    let trajectory_runtime = trajectory_services
        .as_ref()
        .map(|(runtime, _)| runtime.clone());
    let trajectory_for_settlement = trajectory_services
        .as_ref()
        .and_then(|(_, settlement)| settlement.clone());
    #[cfg(feature = "multi-instance")]
    let shared_policy_runtime = shared
        .map(|inputs| {
            crate::policy_lock::PolicyRuntime::from_document(
                config,
                inputs.policy_lock.clone(),
                pending_eval_decisions.clone(),
                trajectory_runtime.clone(),
            )
        })
        .transpose()?;
    #[cfg(not(feature = "multi-instance"))]
    let shared_policy_runtime: Option<Arc<crate::policy_lock::PolicyRuntime>> = None;
    let policy_runtime = match shared_policy_runtime {
        Some(runtime) => runtime,
        None => crate::policy_lock::PolicyRuntime::new(
            config,
            config_path,
            policy_decision_recorder.clone(),
            pending_eval_decisions.clone(),
            trajectory_runtime,
        )
        .await
        .context("loading policy-lock.yaml")?,
    };
    let policy_runtime_for_selector = policy_runtime.clone();
    #[cfg(feature = "core")]
    let evolution = crate::core::evolution::runtime::EvolutionRuntime::new(
        db.clone(),
        routing_table.clone(),
        policy_runtime.clone(),
    );
    let block_policies = (!is_shared).then(|| {
        crate::gateway::routing::blocks::runtime::BlockPolicyRouter::new(
            db.clone(),
            routing_table.clone(),
            policy_runtime.clone(),
        )
    });
    let canonical_binding = (!is_shared).then(|| {
        crate::gateway::observability::binding::CanonicalRequestBinding::new(
            db.clone(),
            routing_table.clone(),
            policy_runtime.clone(),
        )
    });
    let gateway_requests =
        crate::gateway::observability::requests::GatewayRequestTracker::new(db.clone());
    let gateway_requests_for_hooks = gateway_requests.clone();
    let judge_costs =
        (!is_shared).then(|| crate::gateway::metering::judge::JudgeCosts::new(db.clone()));
    #[cfg(test)]
    let pending_eval_decisions_for_tests = pending_eval_decisions.clone();
    let response_observer = PredictiveResponseObserver::new(pending_eval_decisions.clone());
    #[cfg(test)]
    let response_observer_for_tests = response_observer.clone();
    #[cfg(feature = "core")]
    let untracked_recorder = (!is_shared).then(|| {
        Arc::new(EvalSettlementRecorder::new(
            eval_service.store().clone(),
            pending_eval_decisions.clone(),
        )) as Arc<dyn crate::gateway::pipeline::settlement::SettlementRecorder>
    });
    let db_for_hooks = db.clone();
    let db_for_mcp_auth = db.clone();
    #[cfg(feature = "harness")]
    let session_context = if is_shared {
        // A replica-local ACP route lease is not shared gateway authority.
        SessionContextHook::default()
    } else {
        SessionContextHook::new(Arc::clone(&acp_runtime))
    };
    #[cfg(not(feature = "harness"))]
    let session_context = SessionContextHook::default();
    let request_checks_for_pipeline = request_checks.clone();
    let app = App::builder()
        .skip_auth(config.server.skip_auth)
        .metrics_renderer(metrics_renderer)
        .language_model(move |lm| {
            #[cfg(feature = "multi-instance")]
            if is_shared {
                lm.owned_drain();
            }
            lm.routing_table(routing_table).executor(executor);
            if !is_shared {
                lm.native_cost_estimator(pricing_for_native);
                lm.native_cost_source(Arc::new(metering_for_native_costs));
            }
            if let Some(policy) = &private_context {
                lm.native_private_context(Arc::new(policy.clone()));
            }
            lm.served_operations(OperationScope::Both);
            lm.require_hook::<AuthHook>(HookStage::PreResolution, OperationScope::Both);
            lm.require_hook::<SessionContextHook>(HookStage::PreResolution, OperationScope::Both);
            if !is_shared {
                lm.require_hook::<crate::gateway::metering::judge::JudgeCosts>(
                    HookStage::PreResolution,
                    OperationScope::Both,
                );
                lm.require_hook::<MeteringRecorder>(HookStage::Settlement, OperationScope::Both);
            }
            #[cfg(feature = "multi-instance")]
            if is_shared {
                lm.require_hook::<shared_writers::RequestScope>(
                    HookStage::PreResolution,
                    OperationScope::Both,
                );
                lm.require_hook::<shared_writers::DiscardPending>(
                    HookStage::Settlement,
                    OperationScope::Both,
                );
            }
            lm.require_hook::<PolicyHook>(HookStage::PreRequest, OperationScope::Both);
            lm.require_hook::<CaptureTariffs>(HookStage::Route, OperationScope::Both);
            if !tenancy_enabled {
                lm.require_hook::<ContinuationRuntime>(
                    HookStage::Finalization,
                    OperationScope::Generation,
                );
            }
            lm.request_checker_runner(request_checks_for_pipeline);
            lm.fallback_backoff(
                config
                    .upstream
                    .fallback_backoff_ms
                    .iter()
                    .copied()
                    .map(std::time::Duration::from_millis),
            );
            lm.model_selector(policy_runtime_for_selector);
            if let Some(runtime) = continuation_for_route {
                lm.route_hook(runtime);
            }
            lm.route_hook(crate::policy_lock::PredictiveSingleTargetRouteHook);
            if let Some(binding) = &canonical_binding {
                lm.route_hook(binding.clone());
            }
            if let Some(policies) = &block_policies {
                lm.route_hook(policies.clone());
            }
            // Tariffs are dispatch inputs in both modes. Shared accounting is
            // projected from the engine's immutable attempt facts, not requests.
            lm.route_hook_for(
                CaptureTariffs::new(
                    pricing_for_recorder.clone(),
                    config.server.require_known_pricing,
                ),
                OperationScope::Both,
            );
            let metering_recorder = (!is_shared).then(|| {
                MeteringRecorder::new(metering_store_for_recorder, pricing_for_recorder)
                    .with_reconciliation_provider("bitrouter")
            });
            if let Some(runtime) = continuation_for_finalization {
                lm.required_finalizer(runtime);
            }
            // Server-tool declaration capture runs first and is pure
            // observation: it parses any advisor / sub-agent / fusion
            // declaration (or the one the fusion alias injected) off the prompt
            // and stashes it, before auth, so the toolsets can read it
            // regardless of credential state.
            #[cfg(feature = "server-tools")]
            if server_tools_enabled {
                lm.pre_resolution_hook(ServerToolDeclarationsHook);
            }
            // Reserved judge IDs are checked even before auth, so an early
            // rejection cannot overwrite an existing attempt's metering row.
            if let Some(costs) = &judge_costs {
                lm.pre_resolution_hook_for(costs.clone(), OperationScope::Both);
            }
            #[cfg(feature = "multi-instance")]
            if is_shared {
                lm.pre_resolution_hook_for(shared_writers::RequestScope, OperationScope::Both);
            }
            // Authenticate and normalize the selector before freezing the router
            // binding and applying its defaults. Session normalization may apply a
            // API-principal-scoped route lease before Stage 2 model selection;
            // explicit routes and provider continuations retain precedence.
            // The pipeline runs bound external checks after these local hooks.
            lm.pre_resolution_hook_for(
                AuthHook::new(db_for_hooks.clone()).with_tenancy(tenancy_enabled),
                OperationScope::Both,
            );
            lm.pre_resolution_hook_for(session_context, OperationScope::Both);
            if let Some(policy) = private_context {
                lm.pre_resolution_hook(policy);
            }
            if let Some(runtime) = continuation_for_pre_request {
                lm.pre_resolution_hook(runtime);
            }
            // Candidate recipe selection may replace defaults and the policy,
            // but cannot replace the ingress router's frozen checker bindings.
            if !is_shared {
                lm.router_preparation_hook(gateway_requests_for_hooks.clone());
            }
            if let Some(policies) = block_policies {
                lm.router_preparation_hook(policies);
            }
            if let Some(binding) = canonical_binding {
                lm.router_preparation_hook(binding);
            }
            lm.pre_request_hook_for(
                PolicyHook::new(
                    policy_store.clone(),
                    (!is_shared).then_some(metering_store_for_policy),
                ),
                OperationScope::Both,
            );
            // OpenTelemetry exporter — register the *same* Arc as a hook
            // here. Construction happened above so `Assembled.observe`
            // can hold a query handle on it.
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            if let Some(exporter) = otel_for_hook {
                lm.observe_hook_for(OtelObserveHook::new(exporter), OperationScope::Both);
            }
            lm.observe_hook(response_observer);
            if !is_shared {
                lm.observe_hook(gateway_requests_for_hooks.clone());
            }
            if let Some(costs) = &judge_costs {
                lm.observe_hook(costs.clone());
            }
            // Only local serving owns these legacy durable gateway writers.
            if let Some(recorder) = metering_recorder {
                lm.settlement_recorder_for(recorder, OperationScope::Both);
            }
            if !is_shared {
                lm.settlement_recorder(gateway_requests_for_hooks);
            }
            if let Some(costs) = judge_costs {
                lm.settlement_recorder(costs);
            }
            #[cfg(feature = "multi-instance")]
            if is_shared {
                lm.settlement_recorder_for(
                    shared_writers::DiscardPending::new(pending_eval_decisions.clone()),
                    OperationScope::Both,
                );
            }
            if !is_shared {
                lm.settlement_recorder_for(
                    PolicySettlementRecorder::new(
                        pending_eval_decisions,
                        trajectory_for_settlement,
                        #[cfg(feature = "core")]
                        untracked_recorder,
                    ),
                    OperationScope::Both,
                );
            }
            // Server-side tool loop (router-executed MCP tools), when configured.
            #[cfg(feature = "server-tools")]
            if let Some(server_loop) = server_tool_loop {
                lm.server_tool_loop(server_loop);
            }
        });
    // The bitrouter/fusion model alias: an ingress prompt transform that
    // rewrites the alias onto a real outer model and attaches the Fusion
    // declaration. Wired only when `server_tools.fusion` resolves an alias.
    #[cfg(feature = "server-tools")]
    let app = match fusion_alias {
        Some(transform) => app.prompt_transform(transform),
        None => app,
    };
    let app = match decision_runtime {
        Some(runtime) => app.decision_model(runtime),
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
    // Codex ACP keeps its native default/model picker. Qualify known native
    // names at ingress when the Codex subscription is active, without making
    // personal subscriptions part of the generic API auto-cascade.
    let app = app.prompt_transform(Arc::new(crate::codex_router::CodexRouter::new(Arc::clone(
        &routing_table_for_reload,
    ))) as Arc<dyn PromptTransform>);
    // Apply the optional MCP pipeline configuration in a second builder step
    // so the language_model configuration above stays the same shape it has
    // had since v0.
    let app = match (mcp_routing, mcp_executor) {
        (Some(table), Some(exec)) => {
            let app = app.mcp(move |m| {
                m.routing_table(table)
                    .executor(exec)
                    .pre_request_hook(AuthHook::new(db_for_mcp_auth).with_tenancy(tenancy_enabled));
            });
            if let Some(route) = mcp_aggregate_route {
                app.mcp_aggregate_route(route)
            } else {
                app
            }
        }
        _ => app,
    };
    let app = app.build().context("building the App")?;

    // Start the authority actor only after every other fallible assembly step.
    // Failed startup must not leave a detached owner behind during a retry.
    let tenancy = match tenancy_database {
        Some(database) if receipt_observer.enabled() => Some(
            crate::tenancy::Service::open_owned_profile_observed(
                database,
                &config.tenancy,
                &config.continuation,
                receipt_observer,
            )
            .await?,
        ),
        Some(database) => Some(
            crate::tenancy::Service::open_owned_profile(
                database,
                &config.tenancy,
                &config.continuation,
            )
            .await?,
        ),
        None => None,
    };
    let app = match &tenancy {
        Some(service) => app.with_tenancy(service.clone()),
        None => app,
    };
    let continuation_registry = match &tenancy {
        Some(service) => service.continuation_registry().clone(),
        None => continuation_registry.context("Legacy continuation registry unavailable")?,
    };
    if !is_shared && !tenancy_enabled {
        continuation_registry.start_reconciler();
    }
    Ok(Assembled {
        app,
        db,
        tenancy,
        #[cfg(feature = "harness")]
        acp_runtime,
        policy_store: policy_store_for_reload,
        policy_runtime,
        #[cfg(feature = "core")]
        evolution,
        gateway_requests,
        #[cfg(feature = "core")]
        eval_service,
        continuation_registry,
        #[cfg(test)]
        pending_eval_decisions: pending_eval_decisions_for_tests,
        #[cfg(test)]
        response_observer: response_observer_for_tests,
        #[cfg(feature = "core")]
        trajectory_outbox_publisher,
        routing_table: routing_table_for_reload,
        upstream_executor: executor_for_reload,
        observe: observe_provider,
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        otel_exporter: otel_for_assembled,
        otel_init_error,
        ignored_config,
    })
}

#[cfg(feature = "server-tools")]
fn build_fusion_alias(config: &Config) -> Result<Option<Arc<dyn PromptTransform>>> {
    let Some(settings) = config.server_tools.fusion.as_ref() else {
        return Ok(None);
    };
    FusionAliasConfig::validate_settings(settings)?;
    Ok(FusionAliasConfig::from_settings(settings)
        .map(|alias| Arc::new(alias) as Arc<dyn PromptTransform>))
}

/// Merge the provider registry into `config`, then re-apply built-in defaults
/// so any provider the merge newly inserted gets its `api_base` /
/// `api_protocol` / auth shape filled. No-op when the registry is disabled
/// (`registry.enabled = false` or `inherit_defaults = false`).
///
/// The data is the **fetched** dist when reachable, else the most recent
/// **disk cache** (stale-fallback on a network outage). On a never-fetched host
/// the default public registry uses the application-bundled offline baseline.
/// Disabled/custom registries retain their own policy; a custom source with no
/// bound snapshot remains unavailable when refresh fails.
///
/// Called by the `serve` entry point (before [`build_app`]) and by
/// [`crate::reload`], the two paths that build a production routing config.
/// Kept out of `build_app` itself so that function stays free of network I/O —
/// integration tests assemble explicit configs through it. Lives in the app
/// layer because the SDK routing table receives a resolved configuration
/// and does not own application catalog fetching or persistence.
pub async fn merge_registry_into(config: &mut Config) {
    if !config.inherit_defaults || !config.registry.enabled {
        return;
    }
    crate::gateway::catalog::bundled::enable_logged_in(config);
    // Supplement the public registry with the model/provider baseline shipped in this
    // binary. Explicitly disabled/custom registries keep their own policy.
    let Some(data) = crate::gateway::catalog::bundled::load(&config.registry).await else {
        crate::providers::apply::apply_builtin_defaults(config);
        return;
    };
    crate::providers::registry::apply::apply_registry(config, &data);
    crate::providers::apply::apply_builtin_defaults(config);
}

/// Build the server-side tool loop from `config.server_tools`. Returns `None`
/// when neither MCP server tools nor any nested server tool is configured.
///
/// Each named MCP server is attached as an [`McpRouterToolset`] using a `Direct`
/// selector and the server's `tool_prefix` (default `"{name}__"`), so router
/// tool names cannot collide with the caller's own tools. When `nested_runner`
/// is provided, the enabled advisor / sub-agent / fusion toolsets are added over
/// it (each advertised only when the request declares it).
#[cfg(feature = "server-tools")]
fn build_server_tool_loop(
    config: &Config,
    mcp_routing: &Option<Arc<dyn crate::gateway::mcp::RoutingTable>>,
    mcp_executor: &Option<Arc<dyn crate::gateway::mcp::Executor>>,
    nested_runner: Option<Arc<dyn NestedRunner>>,
) -> Option<Arc<ServerToolLoop>> {
    let settings = &config.server_tools;
    let mut sets: Vec<Arc<dyn RouterToolset>> = Vec::new();

    // MCP-backed server tools.
    if !settings.mcp_servers.is_empty() {
        if let (Some(routing), Some(executor)) = (mcp_routing, mcp_executor) {
            let routing: Arc<dyn crate::gateway::mcp::RoutingTable> = routing.clone();
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
    if let Some(runner) = &nested_runner {
        if settings.advisor {
            sets.push(Arc::new(AdvisorToolset::new(runner.clone())));
        }
        if settings.subagent {
            sets.push(Arc::new(SubAgentToolset::new(runner.clone())));
        }
        if settings.fusion.is_some() {
            sets.push(Arc::new(FusionToolset::new(runner.clone())));
        }
    }

    // Built-in web_search server tool, backed by the configured BYOK search
    // backends. Advertised per-request only when the caller declares it.
    if let Some(web_search) = &settings.web_search {
        let backends = build_web_search_backends(web_search, &nested_runner);
        if backends.is_empty() {
            tracing::warn!(
                "server_tools.web_search is set but no backend resolved (missing API keys?); \
                 web_search is disabled"
            );
        } else {
            let max_results = web_search.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
            sets.push(Arc::new(WebSearchToolset::new(backends, max_results)));
        }
    }

    // Built-in web_fetch server tool, backed by the configured BYOK extraction
    // backends. Advertised per-request only when the caller declares it.
    if let Some(web_fetch) = &settings.web_fetch {
        let backends = build_web_fetch_backends(web_fetch);
        if backends.is_empty() {
            tracing::warn!(
                "server_tools.web_fetch is set but no backend resolved (missing API keys?); \
                 web_fetch is disabled"
            );
        } else {
            let max_content_tokens = web_fetch
                .max_content_tokens
                .unwrap_or(DEFAULT_MAX_CONTENT_TOKENS);
            sets.push(Arc::new(WebFetchToolset::new(backends, max_content_tokens)));
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

/// Build the live `web_search` backends from `settings`, in the configured
/// preference/failover order. HTTP backends resolve their BYOK key from the
/// explicit `api_key` (with `${VAR}` already substituted at config load) or the
/// engine's conventional environment variable; one whose key cannot be resolved
/// is skipped with a warning. The native backend reuses the shared nested runner
/// (so it is skipped when none was built).
#[cfg(feature = "server-tools")]
fn build_web_search_backends(
    settings: &WebSearchSettings,
    nested_runner: &Option<Arc<dyn NestedRunner>>,
) -> Vec<Arc<dyn WebSearchBackend>> {
    // A shared HTTP client for the BYOK search APIs, built only when an HTTP
    // backend is configured so a config with only the nested (native) backend
    // pays nothing. The request timeout sits below the loop's per-tool budget so
    // a stuck call fails over promptly.
    let needs_http = settings.backends.iter().any(|b| {
        matches!(
            b,
            WebSearchBackendConfig::Parallel { .. }
                | WebSearchBackendConfig::Exa { .. }
                | WebSearchBackendConfig::Firecrawl { .. }
                | WebSearchBackendConfig::Tavily { .. }
        )
    });
    let http = needs_http.then(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .unwrap_or_default()
    });

    let mut backends: Vec<Arc<dyn WebSearchBackend>> = Vec::new();
    for entry in &settings.backends {
        match entry {
            // The HTTP engines share one construction path, differing only in
            // the engine tag (which also names the conventional key env var).
            WebSearchBackendConfig::Parallel { api_key, api_base }
            | WebSearchBackendConfig::Exa { api_key, api_base }
            | WebSearchBackendConfig::Firecrawl { api_key, api_base }
            | WebSearchBackendConfig::Tavily { api_key, api_base } => {
                let engine = match entry {
                    WebSearchBackendConfig::Exa { .. } => HttpEngine::Exa,
                    WebSearchBackendConfig::Firecrawl { .. } => HttpEngine::Firecrawl,
                    WebSearchBackendConfig::Tavily { .. } => HttpEngine::Tavily,
                    _ => HttpEngine::Parallel,
                };
                if let (Some(client), Some(key)) = (
                    &http,
                    resolve_byok_key(api_key, engine.env_var(), engine.name()),
                ) {
                    backends.push(Arc::new(HttpSearchBackend::new(
                        engine,
                        key,
                        api_base.clone(),
                        client.clone(),
                    )));
                }
            }
            WebSearchBackendConfig::Native { name, model, tool } => {
                if let Some(runner) = nested_runner {
                    backends.push(Arc::new(NestedSearchBackend::new(
                        name.clone().unwrap_or_else(|| "native".to_string()),
                        runner.clone(),
                        model.clone(),
                        forwarded_tools(std::slice::from_ref(tool)),
                    )));
                } else {
                    tracing::warn!("web_search native backend needs a nested runner; skipping");
                }
            }
        }
    }
    backends
}

/// Build the live `web_fetch` backends from `settings`, in configured
/// preference/failover order. Each HTTP backend resolves its BYOK key from the
/// explicit `api_key` (with `${VAR}` already substituted) or the engine's
/// conventional environment variable; one whose key cannot be resolved is
/// skipped with a warning.
#[cfg(feature = "server-tools")]
fn build_web_fetch_backends(settings: &WebFetchSettings) -> Vec<Arc<dyn WebFetchBackend>> {
    if settings.backends.is_empty() {
        return Vec::new();
    }
    // Request timeout sits below the loop's per-tool budget so a stuck fetch
    // fails over promptly.
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();

    let mut backends: Vec<Arc<dyn WebFetchBackend>> = Vec::new();
    for entry in &settings.backends {
        let (engine, api_key, api_base) = match entry {
            WebFetchBackendConfig::Exa { api_key, api_base } => {
                (HttpFetchEngine::Exa, api_key, api_base)
            }
            WebFetchBackendConfig::Firecrawl { api_key, api_base } => {
                (HttpFetchEngine::Firecrawl, api_key, api_base)
            }
            WebFetchBackendConfig::Tavily { api_key, api_base } => {
                (HttpFetchEngine::Tavily, api_key, api_base)
            }
        };
        if let Some(key) = resolve_byok_key(api_key, engine.env_var(), engine.name()) {
            backends.push(Arc::new(HttpFetchBackend::new(
                engine,
                key,
                api_base.clone(),
                http.clone(),
            )));
        }
    }
    backends
}

/// Resolve a BYOK backend's API key: the explicit value if non-empty, else
/// the conventional environment variable. Logs and returns `None` when unset.
#[cfg(feature = "server-tools")]
fn resolve_byok_key(explicit: &Option<String>, env_var: &str, backend: &str) -> Option<String> {
    if let Some(key) = explicit.as_ref().filter(|k| !k.is_empty()) {
        return Some(key.clone());
    }
    match crate::config::env_lookup(env_var) {
        Some(key) if !key.is_empty() => Some(key),
        _ => {
            tracing::warn!(
                backend,
                env_var,
                "server-tool backend has no API key (set the env var or `api_key`); skipping"
            );
            None
        }
    }
}

/// Build the per-provider `AuthAppliers` registry. Each entry covers a
/// provider whose credential flow needs more than the per-protocol
/// `Transport::authorise` default — today: `bitrouter` (the official
/// hosted gateway; OAuth from `bro cloud login` with a
/// `BITROUTER_API_KEY` fallback), GitHub Copilot (device-code OAuth +
/// token exchange), Anthropic Platform API (`x-api-key`), the Claude
/// Pro/Max subscription (`claude-code`, OAuth / live `~/.claude` session),
/// OpenAI Codex (ChatGPT-subscription OAuth).
fn build_auth_appliers(
    config: &Config,
    cloud_manager: Arc<crate::cloud::account::manager::CredentialManager>,
) -> Result<AuthAppliers> {
    let mut appliers = AuthAppliers::new();
    let store_path = crate::provider_credentials::load_default()
        .map(|s| s.path().to_path_buf())
        .context("resolving credential store path")?;
    let account_store = Arc::new(
        crate::gateway::providers::auth::file::backend::FileCredentialStore::new(&store_path)
            .map_err(
                |failure| crate::gateway::model::error::ModelError::CredentialStorage { failure },
            )?,
    );
    let refresh_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("building provider refresh/exchange HTTP client")?;
    // The `bitrouter` provider's account store and the generic upstream
    // provider store above have separate injected transaction contracts. The
    // ordinary backend belongs to AI; hosted file assembly remains above AI.
    // Register the hosted applier through the application Cloud glue.
    crate::cloud::register_if_configured(config, &mut appliers, cloud_manager)?;
    if config.providers.contains_key("github-copilot") {
        let applier = crate::gateway::providers::copilot::CopilotAuthApplier::new(
            refresh_client.clone(),
            crate::gateway::providers::copilot::exchange::TOKEN_EXCHANGE_URL,
            account_store.clone(),
        );
        appliers.register("github-copilot", Arc::new(applier));
    }
    // The Anthropic Platform-API applier is registered when the provider is
    // configured, so an existing `${ANTHROPIC_API_KEY}` user gets the same
    // fallthrough behaviour as before — the applier forwards the inline key
    // when no stored key is present. It is `x-api-key`-only; the Claude
    // Pro/Max subscription lives in the separate `claude-code` provider below.
    if config.providers.contains_key("anthropic") {
        let applier = crate::gateway::providers::anthropic::AnthropicApiKeyApplier::new(
            account_store.clone(),
        );
        appliers.register("anthropic", Arc::new(applier));
    }
    // The Claude Pro/Max subscription applier (OAuth / live `~/.claude`
    // session). Registered under `claude-code`, distinct from the
    // Platform-API `anthropic` provider above.
    if config.providers.contains_key("claude-code") {
        let backend = crate::providers::claude_code::store::ClaudeStore::new(
            store_path.clone(),
            crate::providers::import::claude_code::ClaudeCodeStore::system(),
        )
        .map_err(|failure| {
            crate::gateway::model::error::ModelError::CredentialStorage { failure }
        })?;
        let registration = crate::gateway::providers::login::find("anthropic")
            .context("Claude OAuth registration is missing")?;
        let claude_refresh_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building Claude OAuth HTTP client")?;
        let session = crate::gateway::providers::auth::store::OAuthSession::new(
            Arc::new(backend),
            Arc::new(crate::gateway::providers::auth::oauth::RefreshGrant::new(
                claude_refresh_client,
                registration.auth.token_endpoint,
                registration.auth.client_id,
            )),
            crate::gateway::providers::auth::oauth::REFRESH_WINDOW,
        );
        let applier = crate::gateway::providers::claude_code::ClaudeCodeAuthApplier::new(session)
            .with_fallback_token(crate::providers::claude_code::env_oauth_token());
        appliers.register("claude-code", Arc::new(applier));
    }
    if config.providers.contains_key("openai-codex") {
        let registration = crate::gateway::providers::login::find("openai-codex")
            .context("openai-codex OAuth registration is missing")?;
        let session = crate::gateway::providers::auth::store::OAuthSession::new(
            account_store.clone(),
            Arc::new(crate::gateway::providers::auth::oauth::RefreshGrant::new(
                refresh_client.clone(),
                registration.auth.token_endpoint,
                registration.auth.client_id,
            )),
            crate::gateway::providers::auth::oauth::REFRESH_WINDOW,
        );
        let applier = crate::gateway::providers::codex::OpenAiCodexAuthApplier::new(session);
        appliers.register("openai-codex", Arc::new(applier));
    }
    // The SuperGrok subscription applier (OAuth imported from the Grok CLI
    // session). Registered under `supergrok`, distinct from the metered `xai`
    // API-key provider.
    if config.providers.contains_key("supergrok") {
        let session = crate::gateway::providers::auth::store::OAuthSession::new(
            account_store.clone(),
            Arc::new(crate::gateway::providers::auth::oauth::RefreshGrant::new(
                refresh_client.clone(),
                crate::gateway::providers::supergrok::TOKEN_ENDPOINT,
                crate::gateway::providers::supergrok::CLIENT_ID,
            )),
            crate::gateway::providers::auth::oauth::REFRESH_WINDOW,
        );
        let applier = crate::gateway::providers::supergrok::SuperGrokAuthApplier::new(session);
        appliers.register("supergrok", Arc::new(applier));
    }

    Ok(appliers)
}

pub(crate) fn build_pricing_table(config: &Config) -> PricingTable {
    let mut table = PricingTable::new();
    for (provider_id, provider) in &config.providers {
        table.configure_endpoint(provider_id, None, &provider.api_base);
        for (protocol, endpoint) in &provider.protocol_endpoints {
            if let Ok(protocol) =
                serde_json::from_value(serde_json::Value::String(protocol.clone()))
            {
                table.configure_endpoint(provider_id, Some(protocol), endpoint);
            }
        }
        for model in &provider.models {
            let aliases = std::iter::once(model.id.as_str()).chain(
                model
                    .provider_model_id
                    .as_deref()
                    .filter(|native| *native != model.id),
            );
            for alias in aliases {
                if let Some(pricing) = &model.pricing {
                    table.insert(provider_id, alias, ModelPricing::from(pricing));
                }
                for (protocol, pricing) in &model.pricing_by_protocol {
                    table.insert_for_protocol(
                        provider_id,
                        alias,
                        protocol.clone(),
                        ModelPricing::from(pricing),
                    );
                }
            }
        }
    }
    table
}

#[cfg(test)]
mod pricing_assembly_tests {
    use super::build_pricing_table;

    #[test]
    fn provider_native_model_id_reuses_registry_pricing() -> anyhow::Result<()> {
        let config = crate::config::parse_with(
            r#"
providers:
  example:
    api_base: https://example.test/v1
    models:
      - id: example/canonical-model
        provider_model_id: native-model
        pricing:
          input_micro_usd_per_token: 1.25
          output_micro_usd_per_token: 5.0
"#,
            |_| None,
        )?;

        let pricing = build_pricing_table(&config);
        assert_eq!(
            pricing
                .resolve(
                    "example",
                    "example/canonical-model",
                    &bitrouter_ai::types::ApiProtocol::ChatCompletions
                )
                .and_then(|entry| entry.input_micro_usd_per_token),
            Some(1.25)
        );
        assert_eq!(
            pricing
                .resolve(
                    "example",
                    "native-model",
                    &bitrouter_ai::types::ApiProtocol::ChatCompletions
                )
                .and_then(|entry| entry.input_micro_usd_per_token),
            Some(1.25)
        );
        Ok(())
    }
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

/// Empty metrics renderer for /metrics endpoint compatibility.
/// Returns empty response since metrics are now pushed via OTLP.
struct EmptyMetricsRenderer;

impl MetricsRenderer for EmptyMetricsRenderer {
    fn render(&self) -> String {
        "# Prometheus metrics have been removed in favor of OpenTelemetry.\n".to_string()
            + "# Configure OTLP export via plugins.bitrouter-telemetry.otel\n"
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

/// Build the OTel exporter for **out-of-daemon** surfaces (`bro acp
/// serve|prompt`). Same config resolution as the daemon path (the telemetry
/// opt-in, the `otel:` block, env vars), including the live account-bearer
/// plan. Returns `None` when nothing opts telemetry in; telemetry failures are
/// surfaced as warnings, never as session failures.
#[cfg(all(feature = "harness", any(feature = "otel-http", feature = "otel-grpc")))]
pub(crate) async fn build_otel_exporter_standalone_with_credentials(
    config: &Config,
    cloud_credentials: &crate::cloud::StandaloneCloudCredentials,
) -> Option<Arc<OtelExporter>> {
    let (otel_config, bearer_plan) = match build_otel_config(config, telemetry_install_id) {
        Ok(Some(plan)) => plan,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("telemetry: invalid observe config, exporting disabled: {e:#}");
            return None;
        }
    };
    let bearer: Option<Arc<dyn crate::telemetry::bearer::TelemetryBearer>> = match bearer_plan {
        BearerPlan::LiveSource { warn_if_unmet } => {
            let source = cloud_credentials
                .telemetry_bearer(&otel_config.endpoint)
                .await;
            if source.is_none() && warn_if_unmet {
                tracing::warn!(
                    "telemetry: attribution=account but no signed-in session is available — \
                     exporting anonymously (sign in with `{} cloud login`)",
                    invocation::name()
                );
            }
            source
        }
        BearerPlan::StaticOnly => None,
    };
    match OtelExporter::new(otel_config, bearer) {
        Ok(exporter) => Some(Arc::new(exporter)),
        Err(e) => {
            tracing::warn!("telemetry: failed to initialise OpenTelemetry: {e}");
            None
        }
    }
}

/// Resolve optional attribution only after a selected exporter is available.
#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
fn telemetry_install_id() -> Option<String> {
    crate::paths::install_id()
        .map_err(|error| tracing::warn!("telemetry: could not resolve install id: {error:#}"))
        .ok()
}

fn require_otel_transport(source: &str) -> Result<()> {
    anyhow::ensure!(
        OTEL_ENABLED,
        "{source} requires an OTLP transport: enable the otel-http or otel-grpc feature"
    );
    Ok(())
}

/// Build OpenTelemetry configuration from the app config. Returns `None` when
/// neither YAML nor env vars opt the exporter in.
///
/// Precedence: env vars > `plugins.bitrouter-telemetry.telemetry` (the
/// first-party opt-in) > `plugins.bitrouter-telemetry.otel` > an
/// `OTEL_EXPORTER_OTLP_ENDPOINT`-only opt-in.
///
/// The installation-ID resolver runs only after a supported first-party opt-in
/// is selected. Preflight supplies a side-effect-free resolver. Account bearer
/// resolution remains at the exporter call site, following the returned plan.
fn build_otel_config(
    config: &Config,
    install_id: impl FnOnce() -> Option<String>,
) -> Result<Option<(OtelConfig, BearerPlan)>> {
    // Named for the crate that reads it, like `plugins.bitrouter-guardrails`.
    // It was `bitrouter-observe` until the OTLP renderer moved out of the SDK;
    // the rename is safe to have made because `ignored_config_warnings` reports an
    // unread `plugins.<id>` block on every daemon start, so a stale key is
    // loud rather than silent. See `https://github.com/bitrouter/bitrouter/blob/80550c9b2079a6e23ceeeb6babf922a77e99b5bb/docs/TELEMETRY_CRATE_SPEC.md` D6.
    let telemetry = config.plugins.get("bitrouter-telemetry");

    // Env-var overrides are *not* applied here — `OtelExporter::new` runs
    // `with_env_overrides` on whatever config it is handed, so the
    // env > YAML precedence holds for every path below without this
    // function having to re-apply it.

    // 0. First-party telemetry opt-in. `plugins.bitrouter-telemetry.telemetry`
    //    is a convenience wrapper over the `otel` block: when `enabled`, it
    //    points the exporter at BitRouter's first-party telemetry endpoint and
    //    selects the capture level. OFF by default — an absent or
    //    `enabled: false` block leaves telemetry disabled. A malformed block is
    //    a hard error (the operator explicitly opted in).
    if let Some(tel_value) = telemetry.and_then(|c| c.get("telemetry")) {
        let opt_in = serde_json::from_value::<TelemetryOptIn>(tel_value.clone())
            .context("plugins.bitrouter-telemetry.telemetry failed to parse")?;
        if opt_in.enabled {
            require_otel_transport("plugins.bitrouter-telemetry.telemetry.enabled")?;
            // Decide the live-bearer plan from attribution + whether an explicit
            // static token is present. The account bearer is NOT snapshotted
            // here anymore — when no explicit token is set and attribution is
            // auto/account, the call site builds a live source that refreshes
            // per export. `build_telemetry_otel_config` resolves the explicit
            // token itself into the static `bearer_token`.
            let has_explicit =
                opt_in.bearer_token.is_some() || std::env::var("BITROUTER_TELEMETRY_TOKEN").is_ok();
            let bearer_plan = telemetry_bearer_plan(opt_in.attribution, has_explicit);
            let cfg = build_telemetry_otel_config(opt_in, install_id());
            return Ok(Some((cfg, bearer_plan)));
        }
    }

    // 1. New nested `otel: { … }` block. A malformed block is a hard error:
    //    the operator explicitly opted in, so silently falling back to the
    //    legacy shim / env-only path would hide their mistake and start the
    //    exporter with a config they never asked for.
    if let Some(otel_value) = telemetry.and_then(|c| c.get("otel")) {
        let cfg = serde_json::from_value::<OtelConfig>(otel_value.clone())
            .context("plugins.bitrouter-telemetry.otel failed to parse")?;
        require_otel_transport("plugins.bitrouter-telemetry.otel")?;
        return Ok(Some((cfg, BearerPlan::StaticOnly)));
    }

    // 2. Env-var-only opt-in.
    //
    // The v0 flat `otlp_endpoint` shim used to sit here, marked for removal in
    // v1.1. It went with the key rename instead, and not to save a release:
    // keeping it would have meant carrying a **v0** compatibility path under a
    // key name v0 never had. `plugins.bitrouter-telemetry.otlp_endpoint` has
    // never existed in a released config, so there was nothing to preserve.
    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        require_otel_transport("OTEL_EXPORTER_OTLP_ENDPOINT")?;
        return Ok(Some((OtelConfig::default(), BearerPlan::StaticOnly)));
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
    /// Account-attributed when a `bro cloud login` session (or an explicit
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

/// The `plugins.bitrouter-telemetry.telemetry` opt-in block — a thin wrapper over
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
/// resolved live per export by a `TelemetryBearer`
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
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    use super::BearerPlan;
    use super::{Config, build_otel_config};

    /// Build a `Config` carrying a single `bitrouter-telemetry` plugin value.
    /// Constructed directly (no YAML round-trip) so the test never touches
    /// the process environment that `build_otel_config`'s env-only path
    /// would read.
    fn config_with_telemetry(telemetry: serde_json::Value) -> Config {
        let mut config = Config::default();
        config
            .plugins
            .insert("bitrouter-telemetry".to_string(), telemetry);
        config
    }

    #[test]
    fn first_party_activation_checks_transport_before_resolving_installation() -> anyhow::Result<()>
    {
        let config = config_with_telemetry(serde_json::json!({
            "telemetry": { "enabled": true, "attribution": "anonymous" },
            // The first-party opt-in wins; this lower-priority block is not read.
            "otel": { "sampler": "not_a_real_sampler" }
        }));
        let resolutions = std::cell::Cell::new(0);
        let result = build_otel_config(&config, || {
            resolutions.set(resolutions.get() + 1);
            Some("fixture-installation".to_owned())
        });
        if super::OTEL_ENABLED {
            let (export, _) = result?
                .ok_or_else(|| anyhow::anyhow!("first-party opt-in did not select export"))?;
            assert_eq!(resolutions.get(), 1);
            assert_eq!(
                export.resource_attributes["bitrouter.install_id"],
                "fixture-installation"
            );
        } else {
            let error = result
                .err()
                .ok_or_else(|| anyhow::anyhow!("unavailable export was accepted"))?;
            assert!(
                error
                    .to_string()
                    .contains("plugins.bitrouter-telemetry.telemetry.enabled")
            );
            assert!(error.to_string().contains("otel-http or otel-grpc"));
            assert_eq!(resolutions.get(), 0);
        }
        Ok(())
    }

    #[cfg(not(any(feature = "otel-http", feature = "otel-grpc")))]
    #[test]
    fn disabled_first_party_block_does_not_hide_generic_export() -> anyhow::Result<()> {
        let config = config_with_telemetry(serde_json::json!({
            "telemetry": { "enabled": false },
            "otel": { "endpoint": "http://collector:4318", "bearer_token": "private-token" }
        }));
        let error = super::validate_host_configuration(&config)
            .err()
            .ok_or_else(|| anyhow::anyhow!("generic export opt-in was ignored"))?;
        let message = error.to_string();
        assert!(message.contains("plugins.bitrouter-telemetry.otel"));
        assert!(message.contains("otel-http or otel-grpc"));
        assert!(!message.contains("private-token"));
        Ok(())
    }

    #[test]
    fn malformed_otel_block_is_a_hard_error() {
        // `sampler` is a closed enum — an unknown variant fails to parse.
        // An explicit opt-in must surface that, not silently fall through.
        let config = config_with_telemetry(serde_json::json!({
            "otel": { "sampler": "not_a_real_sampler" }
        }));
        assert!(
            build_otel_config(&config, || None).is_err(),
            "a malformed otel block must be a hard error",
        );
    }

    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    #[test]
    fn valid_otel_block_parses() -> anyhow::Result<()> {
        let config = config_with_telemetry(serde_json::json!({
            "otel": { "endpoint": "http://collector:4318" }
        }));
        let (config, bearer_plan) = build_otel_config(&config, || None)?
            .ok_or_else(|| anyhow::anyhow!("valid otel block must select an exporter"))?;
        assert_eq!(config.endpoint, "http://collector:4318");
        // The generic `otel` block never reads the credential store.
        assert_eq!(bearer_plan, BearerPlan::StaticOnly);
        Ok(())
    }

    #[test]
    fn the_v0_otlp_endpoint_shim_is_gone() {
        // Removed with the key rename: it existed to keep a **v0** YAML
        // building, and v0 never had `plugins.bitrouter-telemetry`. An
        // `otlp_endpoint` under the new key is therefore a config that has
        // never worked, and it must not quietly half-work.
        let config = config_with_telemetry(serde_json::json!({
            "otlp_endpoint": "http://legacy:4318"
        }));
        assert!(
            build_otel_config(&config, || None)
                .expect("an unknown sub-key is not an error")
                .is_none(),
            "otlp_endpoint must no longer wire an exporter"
        );
    }

    #[test]
    fn the_old_plugin_id_is_reported_rather_than_read() {
        // The rename's whole safety argument: a stale `plugins.bitrouter-observe`
        // block wires nothing, and `unknown_plugin_ids` is what makes that
        // audible instead of silent.
        let mut config = Config::default();
        config.plugins.insert(
            "bitrouter-observe".to_string(),
            serde_json::json!({ "otel": { "endpoint": "http://collector:4318" } }),
        );
        assert!(
            build_otel_config(&config, || None)
                .expect("an unread plugin block is not an error")
                .is_none()
        );
        assert_eq!(
            super::unknown_plugin_ids(&config),
            vec!["bitrouter-observe".to_string()]
        );
    }

    #[test]
    fn a_typoed_guardrails_id_is_reported() {
        // The sharpest case, and the reason the guard is not telemetry-specific:
        // `bitrouter-guardrail` (singular) parses fine, wires nothing, and
        // drops the operator's declared block / redact patterns. Silent before
        // this list existed.
        let mut config = Config::default();
        config.plugins.insert(
            "bitrouter-guardrail".to_string(),
            serde_json::json!({ "custom_patterns": [] }),
        );
        assert_eq!(
            super::unknown_plugin_ids(&config),
            vec!["bitrouter-guardrail".to_string()]
        );
    }

    #[test]
    fn a_stale_env_name_warns_until_the_new_one_is_set() {
        let warn = |set: &[&str]| super::renamed_env_warnings(|name| set.contains(&name));
        assert!(warn(&[]).is_empty(), "nothing set, nothing to say");
        assert_eq!(
            warn(&["BITROUTER_OBSERVE_CONTENT_CAPTURE"]).len(),
            1,
            "the old name alone is what an operator needs telling about"
        );
        assert!(
            warn(&["BITROUTER_TELEMETRY_CONTENT_CAPTURE"]).is_empty(),
            "the new name alone is the migrated state"
        );
        assert!(
            warn(&[
                "BITROUTER_OBSERVE_CONTENT_CAPTURE",
                "BITROUTER_TELEMETRY_CONTENT_CAPTURE",
            ])
            .is_empty(),
            "both set means migrated with a stale export left behind — not a line per start"
        );
    }

    #[test]
    fn known_plugin_ids_cover_every_reader() {
        // The list's only real hazard is under-listing: an id the binary reads
        // but forgot to declare gets warned about on every start, which is
        // worse than not warning at all. That is not hypothetical — it shipped
        // that way, because `bitrouter-policy` is read through a line-wrapped
        // `config` / `.plugins` / `.get(…)` chain and the check was a human
        // reading greps.
        //
        // So check against the source rather than against a hand-list. Any
        // `.plugins` … `.get("<literal>")` in this file is a reader by
        // definition, whatever whitespace sits between the pieces.
        let source = include_str!("assemble.rs");
        let mut readers: Vec<&str> = Vec::new();
        for (index, _) in source.match_indices(".plugins") {
            let rest = &source[index + ".plugins".len()..];
            let Some(open) = rest.find(".get(") else {
                continue;
            };
            // Only a call that follows immediately — whitespace, dots and
            // comments aside — is the same expression. A `.get(` a hundred
            // characters later belongs to something else.
            if rest[..open].chars().any(|c| !c.is_whitespace()) {
                continue;
            }
            let after = &rest[open + ".get(".len()..];
            let after = after.trim_start();
            let Some(quoted) = after.strip_prefix('"') else {
                continue;
            };
            if let Some(end) = quoted.find('"') {
                readers.push(&quoted[..end]);
            }
        }
        assert!(
            !readers.is_empty(),
            "the scan found no readers at all — it has stopped testing"
        );
        for id in &readers {
            assert!(
                super::KNOWN_PLUGIN_IDS.contains(id),
                "plugins.{id} is read but missing from KNOWN_PLUGIN_IDS, so the \
                 daemon would warn about a plugin it honours"
            );
        }
        // And the inverse, so a stale entry cannot linger either.
        for id in super::KNOWN_PLUGIN_IDS {
            assert!(
                readers.contains(id),
                "KNOWN_PLUGIN_IDS names {id}, which nothing reads"
            );
        }
    }

    #[test]
    fn a_dead_sub_key_under_the_new_id_is_reported() {
        // The natural migration: the id-level warning says "rename", the
        // operator renames, and their v0 `otlp_endpoint` comes along with it.
        // Id-level checking alone sees a known id and says nothing.
        let config = config_with_telemetry(serde_json::json!({
            "otlp_endpoint": "http://legacy:4318"
        }));
        assert!(super::unknown_plugin_ids(&config).is_empty());
        let warnings = super::ignored_config_warnings(&config);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("plugins.bitrouter-telemetry.otlp_endpoint")),
            "got: {warnings:?}"
        );
    }
}

#[cfg(all(test, feature = "server-tools"))]
mod server_tools_tests {
    use std::sync::Arc;

    use crate::gateway::pipeline::server_tools::nested::{
        NestedOutcome, NestedRequest, NestedRunner,
    };
    use crate::gateway::pipeline::server_tools::toolset::ToolContext;

    use super::{Config, build_fusion_alias, build_server_tool_loop};

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
    fn assembly_rejects_a_fusion_alias_that_hijacks_reserved_routing() {
        let mut config = Config::default();
        config.server_tools.fusion = Some(crate::config::server_tools::fusion::FusionSettings {
            alias: Some("bitrouter/auto".to_string()),
            outer_model: Some("anthropic/claude-opus-4.8".to_string()),
            ..Default::default()
        });

        let result = build_fusion_alias(&config);
        let error = match result {
            Ok(_) => panic!("reserved Fusion alias must fail app assembly"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("server_tools.fusion.alias"));
        assert!(error.to_string().contains("bitrouter/auto"));
    }

    #[test]
    fn no_loop_when_runner_present_but_no_tool_enabled() {
        // A runner with no enabled nested tool (and no MCP) yields no loop.
        let config = Config::default();
        let runner: Arc<dyn NestedRunner> = Arc::new(NoopRunner);
        assert!(build_server_tool_loop(&config, &None, &None, Some(runner)).is_none());
    }

    #[test]
    fn web_search_nested_backend_builds_loop() {
        use crate::config::server_tools::web_search::{WebSearchBackendConfig, WebSearchSettings};
        // A native (nested) backend resolves over the runner and builds the loop.
        let mut cfg = Config::default();
        cfg.server_tools.web_search = Some(WebSearchSettings {
            backends: vec![WebSearchBackendConfig::Native {
                name: None,
                model: "anthropic/claude-opus-4.8".to_string(),
                tool: serde_json::json!({ "type": "anthropic:web_search_20250305" }),
            }],
            max_results: None,
        });
        let runner: Arc<dyn NestedRunner> = Arc::new(NoopRunner);
        assert!(build_server_tool_loop(&cfg, &None, &None, Some(runner)).is_some());
    }

    #[test]
    fn web_search_http_backend_with_explicit_key_builds_loop_without_runner() {
        use crate::config::server_tools::web_search::{WebSearchBackendConfig, WebSearchSettings};
        // An HTTP backend with an explicit key resolves with no nested runner.
        let mut cfg = Config::default();
        cfg.server_tools.web_search = Some(WebSearchSettings {
            backends: vec![WebSearchBackendConfig::Exa {
                api_key: Some("explicit-key".to_string()),
                api_base: None,
            }],
            max_results: Some(3),
        });
        assert!(build_server_tool_loop(&cfg, &None, &None, None).is_some());
    }

    #[test]
    fn web_fetch_http_backend_with_explicit_key_builds_loop_without_runner() {
        use crate::config::server_tools::web_fetch::{WebFetchBackendConfig, WebFetchSettings};
        // A web_fetch-only config (no advisor/subagent/fusion/web_search) must
        // still build the server-tool loop, proving server_tools_enabled covers
        // web_fetch. An HTTP backend with an explicit key resolves without a
        // nested runner.
        let mut cfg = Config::default();
        cfg.server_tools.web_fetch = Some(WebFetchSettings {
            backends: vec![WebFetchBackendConfig::Exa {
                api_key: Some("explicit-key".to_string()),
                api_base: None,
            }],
            max_content_tokens: None,
        });
        assert!(build_server_tool_loop(&cfg, &None, &None, None).is_some());
    }

    #[test]
    fn assembles_web_fetch_exa_backend_from_explicit_key() {
        use crate::config::server_tools::web_fetch::{WebFetchBackendConfig, WebFetchSettings};
        // An HTTP backend with an explicit key resolves without needing an env
        // var, mirroring the sibling web_search assembly test's approach.
        let backends = super::build_web_fetch_backends(&WebFetchSettings {
            backends: vec![WebFetchBackendConfig::Exa {
                api_key: Some("explicit-key".to_string()),
                api_base: None,
            }],
            max_content_tokens: None,
        });
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].name(), "exa");
    }
}

#[cfg(test)]
mod trajectory_assembly_tests {
    use std::collections::BTreeMap;
    #[cfg(feature = "core")]
    use std::collections::BTreeSet;

    use crate::gateway::caller::CallerContext;
    use crate::gateway::pipeline::types::PipelineRequest;
    use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{Config, build_app_with_path};
    use crate::gateway::routing::workflow::predictive::compiled_predictor_contract;
    use crate::policy_lock::{
        CertificateSource, PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict,
        RouteOwner, deterministic_yaml, semantic_digest,
    };
    #[cfg(feature = "core")]
    use crate::trajectory::guard::IncompleteHistoryAction;
    use crate::trajectory::guard::ProgressGuardPolicy;
    #[cfg(feature = "core")]
    use bitrouter_ai::eval::EvalScope;

    async fn assemble_named_policy(
        trajectory_enabled: bool,
        progress_guard: Option<ProgressGuardPolicy>,
    ) -> anyhow::Result<(super::Assembled, String)> {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-eval-compat",
                "object": "chat.completion",
                "model": "economy-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "complete"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
            })))
            .mount(&upstream)
            .await;

        let home = tempfile::tempdir()?;
        let config_path = home.path().join("bitrouter.yaml");
        let yaml = format!(
            r#"
server:
  skip_auth: true
database:
  url: "sqlite::memory:"
trajectory:
  enabled: {trajectory_enabled}
policy:
  path: "./policy-lock.yaml"
  mode: frozen
providers:
  vendor:
    api_base: "{upstream_uri}"
    api_key: test-key
    api_protocol:
      - "*": chat_completions
    models:
      - id: economy-model
      - id: strong-model
presets:
  coding:
    model: "vendor:strong-model"
    policy: coding
"#,
            upstream_uri = upstream.uri(),
        );
        let config = crate::config::parse_with(&yaml, |_| None)?;
        let guarded = progress_guard.is_some();
        const ROUTE_KEY: &str = "semantic_route/v1|unknown|unknown|guarded";
        const TEST_DIGEST: &str =
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let policy = PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "vendor:economy-model".into()),
                ("strong".into(), "vendor:strong-model".into()),
            ]),
            routes: if guarded {
                BTreeMap::new()
            } else {
                BTreeMap::from([(ROUTE_KEY.into(), "economy".into())])
            },
            default_tier: Some(if guarded { "economy" } else { "strong" }.into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            progress_guard,
            predictor: (!guarded).then(compiled_predictor_contract),
            ..PolicyDefinition::default()
        };
        let mut lock = PolicyLock::default();
        if !guarded {
            lock.certificates.insert(
                "coding".into(),
                BTreeMap::from([(
                    ROUTE_KEY.into(),
                    PolicyCertificate {
                        classifier_digest: None,
                        owner: RouteOwner::Operator,
                        selected_tier: "economy".into(),
                        baseline_tier: Some("strong".into()),
                        source: CertificateSource::Operator,
                        eligible_episodes: 0,
                        independent_tasks: 0,
                        quality: None,
                        economics: None,
                        latency: None,
                        critical_violations: 0,
                        verdict: PromotionVerdict::Promote,
                        evaluator_config_digest: None,
                        compiler_config_digest: TEST_DIGEST.into(),
                        evidence_digest: TEST_DIGEST.into(),
                    },
                )]),
            );
        }
        lock.policies.insert("coding".into(), policy);
        let digest = semantic_digest(&lock)?;
        tokio::fs::write(
            home.path().join("policy-lock.yaml"),
            deterministic_yaml(&lock)?,
        )
        .await?;
        let assembled = build_app_with_path(&config, Some(&config_path)).await?;

        let prompt = Prompt {
            model: "@coding".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "generic request")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut request = PipelineRequest::new("@coding", CallerContext::local(), prompt);
        request.request_id = "request-eval-compat".into();
        request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
        assembled
            .app
            .language_model()
            .ok_or_else(|| anyhow::anyhow!("assembled App has no language model pipeline"))?
            .execute(request)
            .await?;
        #[cfg(feature = "core")]
        if let Some(publisher) = &assembled.trajectory_outbox_publisher {
            publisher.wait_for_idle().await;
        }

        Ok((assembled, digest))
    }

    #[cfg(not(feature = "core"))]
    #[tokio::test]
    async fn no_core_cleans_untracked_observation_without_eval_writes() -> anyhow::Result<()> {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

        for trajectory_enabled in [false, true] {
            let (assembled, _) = assemble_named_policy(trajectory_enabled, None).await?;
            assert!(assembled.pending_eval_decisions.is_empty());
            assert_eq!(assembled.response_observer.buffered_request_count(), 0);
            let row = assembled.db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT (SELECT COUNT(*) FROM eval_subjects) + (SELECT COUNT(*) FROM eval_results) AS count".to_owned(),
            )).await?.ok_or_else(|| anyhow::anyhow!("missing eval count"))?;
            assert_eq!(row.try_get::<i64>("", "count")?, 0);
        }
        Ok(())
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn disabled_trajectory_preserves_named_policy_request_eval() -> anyhow::Result<()> {
        let (assembled, digest) = assemble_named_policy(false, None).await?;

        let subjects = assembled
            .eval_service
            .store()
            .list_subjects_for_owner("local")
            .await?;
        assert_eq!(subjects.len(), 1);
        let subject = &subjects[0];
        assert_eq!(subject.scope, EvalScope::Request);
        assert_eq!(subject.policy_digest, digest);
        assert_eq!(subject.decisions.len(), 1);
        assert_eq!(subject.decisions[0].policy, "coding");
        assert_eq!(subject.decisions[0].selected_tier, "economy");
        assert_eq!(
            subject.decisions[0].baseline_tier.as_deref(),
            Some("strong")
        );
        assert!(
            assembled
                .eval_service
                .store()
                .list_subjects_for_owner("other-owner")
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn unguarded_policy_preserves_request_eval_when_trajectory_is_enabled()
    -> anyhow::Result<()> {
        let (assembled, digest) = assemble_named_policy(true, None).await?;

        let subjects = assembled
            .eval_service
            .store()
            .list_subjects_for_owner("local")
            .await?;
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].scope, EvalScope::Request);
        assert_eq!(subjects[0].policy_digest, digest);
        assert_eq!(subjects[0].decisions.len(), 1);
        assert_eq!(subjects[0].decisions[0].policy, "coding");
        assert_eq!(subjects[0].decisions[0].selected_tier, "economy");
        assert_eq!(
            subjects[0].decisions[0].baseline_tier.as_deref(),
            Some("strong")
        );
        assert!(assembled.pending_eval_decisions.is_empty());
        assert_eq!(assembled.response_observer.buffered_request_count(), 0);
        Ok(())
    }

    #[cfg(feature = "core")]
    #[tokio::test]
    async fn guarded_policy_emits_only_episode_eval_without_pending_decision() -> anyhow::Result<()>
    {
        let guard = ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: BTreeSet::from(["strong".into()]),
            max_consecutive_unprotected: Some(3),
            max_same_projection_unprotected: None,
            max_recovery_count: None,
            max_episode_requests: None,
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 2,
            incomplete_history: IncompleteHistoryAction::Observe,
        };
        let (assembled, digest) = assemble_named_policy(true, Some(guard)).await?;

        let subjects = assembled
            .eval_service
            .store()
            .list_subjects_for_owner("local")
            .await?;
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].scope, EvalScope::Episode);
        assert_eq!(subjects[0].policy_digest, digest);
        assert_eq!(subjects[0].decisions.len(), 1);
        assert_eq!(subjects[0].decisions[0].policy, "coding");
        assert_eq!(subjects[0].decisions[0].selected_tier, "economy");
        assert_eq!(
            subjects[0].decisions[0].baseline_tier.as_deref(),
            Some("economy")
        );
        let observation = subjects[0]
            .evidence
            .iter()
            .find(|evidence| evidence.kind == "routing.prediction_observation")
            .ok_or_else(|| anyhow::anyhow!("guarded response observation evidence missing"))?;
        assert_eq!(
            observation
                .attributes
                .get("observed_action")
                .map(String::as_str),
            Some("answer_or_summarize")
        );
        assert!(assembled.pending_eval_decisions.is_empty());
        assert_eq!(assembled.response_observer.buffered_request_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn trajectory_activation_is_explicit_and_owns_the_existing_key_lifecycle()
    -> anyhow::Result<()> {
        let disabled_home = tempfile::tempdir()?;
        let mut config = Config::default();
        config.database.url = "sqlite::memory:".into();
        let disabled =
            build_app_with_path(&config, Some(&disabled_home.path().join("bitrouter.yaml")))
                .await?;

        assert!(!disabled.policy_runtime.trajectory_is_active());
        #[cfg(feature = "core")]
        assert!(disabled.trajectory_outbox_publisher.is_none());
        assert!(!disabled_home.path().join("correlation.key").exists());

        let enabled_home = tempfile::tempdir()?;
        config.trajectory.enabled = true;
        config.trajectory.outbox_batch_size = 7;
        let enabled =
            build_app_with_path(&config, Some(&enabled_home.path().join("bitrouter.yaml"))).await?;

        assert!(enabled.policy_runtime.trajectory_is_active());
        #[cfg(feature = "core")]
        assert_eq!(
            enabled
                .trajectory_outbox_publisher
                .as_ref()
                .map(|publisher| publisher.configured_batch_size()),
            Some(7)
        );
        assert!(enabled_home.path().join("correlation.key").is_file());
        Ok(())
    }

    #[tokio::test]
    async fn enabled_guarded_policy_persists_its_first_route_atomically() -> anyhow::Result<()> {
        use std::collections::{BTreeMap, BTreeSet};

        use crate::config::PresetConfig;
        use crate::gateway::caller::CallerContext;
        use crate::gateway::pipeline::context::PipelineContext;
        use crate::gateway::pipeline::routing::ModelSelector;
        use crate::gateway::pipeline::types::PipelineRequest;
        use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};

        use crate::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml};
        use crate::trajectory::correlation::CorrelatedRequest;
        use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
        use crate::trajectory::store::TrajectoryStore;

        let home = tempfile::tempdir()?;
        let config_path = home.path().join("bitrouter.yaml");
        let mut config = Config::default();
        config.database.url = "sqlite::memory:".into();
        config.trajectory.enabled = true;
        config.presets.insert(
            "coding".into(),
            PresetConfig {
                model: Some("vendor:strong".into()),
                policy: Some("coding".into()),
                ..PresetConfig::default()
            },
        );
        let mut lock = PolicyLock::default();
        lock.policies.insert(
            "coding".into(),
            PolicyDefinition {
                tiers: BTreeMap::from([
                    ("economy".into(), "vendor:economy".into()),
                    ("strong".into(), "vendor:strong".into()),
                ]),
                default_tier: Some("economy".into()),
                tool_use_tier: Some("strong".into()),
                tool_safe_tiers: vec!["strong".into()],
                progress_guard: Some(ProgressGuardPolicy {
                    escalation_tier: "strong".into(),
                    protected_tiers: BTreeSet::from(["strong".into()]),
                    max_consecutive_unprotected: Some(3),
                    max_same_projection_unprotected: None,
                    max_recovery_count: None,
                    max_episode_requests: Some(4),
                    max_episode_elapsed_ms: None,
                    max_episode_cost_micro_usd: None,
                    hold_for_requests: 2,
                    incomplete_history: IncompleteHistoryAction::Observe,
                }),
                ..PolicyDefinition::default()
            },
        );
        tokio::fs::write(
            home.path().join("policy-lock.yaml"),
            deterministic_yaml(&lock)?,
        )
        .await?;

        let assembled = build_app_with_path(&config, Some(&config_path)).await?;
        let prompt = Prompt {
            model: "vendor:strong".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "generic request")],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut request = PipelineRequest::new(
            "vendor:strong",
            CallerContext::new("local-key", "local"),
            prompt,
        );
        request.request_id = "request-first".into();
        request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
        let mut context = PipelineContext::new(request);
        assembled
            .policy_runtime
            .select_variant("coding", None, &mut context)
            .await?;
        let correlated = context
            .extension::<CorrelatedRequest>()
            .ok_or_else(|| anyhow::anyhow!("enabled guarded request was not correlated"))?;
        let events = TrajectoryStore::new(assembled.db)
            .events_for_episode("local", &correlated.episode_id)
            .await?;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].request_id.as_deref(),
            Some(correlated.request_id.as_str())
        );
        assert_eq!(
            events[1]
                .evidence
                .categorical
                .get("route.policy")
                .map(String::as_str),
            Some("coding")
        );
        Ok(())
    }

    #[tokio::test]
    async fn enabled_startup_applies_configured_retention_before_serving() -> anyhow::Result<()> {
        use std::collections::BTreeMap;

        use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};

        use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
        use crate::trajectory::correlation::TrajectoryRuntime;
        use crate::trajectory::store::TrajectoryStore;
        use crate::trajectory::types::{
            RequestStatus, Settlement, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent,
            TrajectoryEventKind, TrajectoryEvidence,
        };

        let home = tempfile::tempdir()?;
        let database_path = home.path().join("retention.db");
        let database_url = format!("sqlite://{}?mode=rwc", database_path.display());
        let db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&db).await?;
        let runtime = TrajectoryRuntime::new(
            TrajectoryStore::new(db.clone()),
            Canonicalizer::new(CorrelationKey::from_bytes(rand::random())?),
        );
        let root = runtime
            .begin_request(
                "owner-retention",
                "request-retention",
                ApiProtocol::ChatCompletions,
                &Prompt {
                    model: "inbound".into(),
                    system: None,
                    system_provider_metadata: BTreeMap::new(),
                    messages: vec![Message::text(Role::User, "expired root")],
                    tools: Vec::new(),
                    params: GenerationParams::default(),
                    response_format: None,
                    tool_choice: None,
                    stream: false,
                },
                "2000-01-01T00:00:00Z",
            )
            .await?;
        let mut route = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "route-retention".into(),
            owner_user_id: "owner-retention".into(),
            episode_id: root.episode_id.clone(),
            request_id: Some(root.request_id.clone()),
            sequence: 2,
            kind: TrajectoryEventKind::RouteIntentRecorded,
            evidence: TrajectoryEvidence {
                routing_evidence: Vec::new(),
                structural: BTreeMap::new(),
                categorical: BTreeMap::from([
                    (
                        "route.projection".into(),
                        "agent_trace/v2|opening|normal".into(),
                    ),
                    ("route.selected_tier".into(), "economy".into()),
                    ("route.workflow_state".into(), "opening".into()),
                ]),
                digests: BTreeMap::new(),
                route_measurement: None,
            },
            captured_at: "2000-01-01T00:00:01Z".into(),
            content_digest: String::new(),
        };
        route.content_digest = route.semantic_digest()?;
        runtime
            .store()
            .append_route_intent("owner-retention", route)
            .await?;
        let mut terminal = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "settle-retention".into(),
            owner_user_id: "owner-retention".into(),
            episode_id: root.episode_id.clone(),
            request_id: Some(root.request_id.clone()),
            sequence: 3,
            kind: TrajectoryEventKind::RequestSettled,
            evidence: TrajectoryEvidence {
                routing_evidence: Vec::new(),
                structural: BTreeMap::new(),
                categorical: BTreeMap::new(),
                digests: BTreeMap::new(),
                route_measurement: None,
            },
            captured_at: "2000-01-01T00:00:02Z".into(),
            content_digest: String::new(),
        };
        terminal.content_digest = terminal.semantic_digest()?;
        runtime
            .store()
            .settle_request(
                "owner-retention",
                Settlement {
                    event: terminal,
                    status: RequestStatus::Settled,
                    outbox: None,
                },
            )
            .await?;
        drop(runtime);
        db.close().await?;

        let mut config = Config::default();
        config.database.url = database_url;
        config.trajectory.enabled = true;
        config.trajectory.retention_days = 30;
        config.trajectory.outbox_batch_size = 1;
        let assembled =
            build_app_with_path(&config, Some(&home.path().join("bitrouter.yaml"))).await?;
        assert!(
            TrajectoryStore::new(assembled.db)
                .resolve_episode_owner(&root.episode_id)
                .await?
                .is_none()
        );
        Ok(())
    }
}
