//! [`App`] and [`AppBuilder`] — the top-level entry point.
//!
//! An [`App`] holds one pipeline per enabled protocol
//! ([`crate::language_model::Pipeline`], [`crate::mcp::Pipeline`]) plus the
//! injected infrastructure (metrics store, metrics renderer, the aggregated
//! migration set).
//!
//! [`AppBuilder`] configures each protocol through its own sub-builder closure
//! ([`language_model`](AppBuilder::language_model), [`mcp`](AppBuilder::mcp)).
//! A pipeline is built only for protocols that have something configured.
//!
//! # Legacy custom-host assembly
//!
//! [`Plugin`] and [`AppBuilder::plugin`] retain the legacy convenience for
//! trusted custom hosts to install hooks and SQL [`crate::plugin::MigrationItem`]s.
//! These APIs assemble a host; they are not the restricted extension author API.
//! New request-check extensions register a callback through
//! [`crate::extension::ExtensionApi::request_check`], using
//! `bitrouter::assemble::build_app_with_extensions` in the product host crate.
//! Router bindings determine which registered checks process requests.
//!
//! The current alpha SDK retains the legacy API without changing its global
//! hook or migration behavior. Removal requires an explicitly announced breaking
//! SDK release with migration notes; no removal date is scheduled. Individual
//! hooks and migration facilities remain available for custom-host assembly.
//!
//! ```no_run
//! use std::sync::Arc;
//! use bitrouter_sdk::App;
//! use bitrouter_sdk::language_model::{HttpExecutor, StaticRoutingTable};
//!
//! # fn run() -> bitrouter_sdk::Result<()> {
//! let executor = Arc::new(HttpExecutor::with_defaults()?);
//! let app = App::builder()
//!     .skip_auth(true)
//!     .language_model(|lm| {
//!         lm.routing_table(Arc::new(StaticRoutingTable::new()))
//!           .executor(executor);
//!     })
//!     .build()?;
//! # let _ = app; Ok(()) }
//! ```

use std::pin::Pin;
use std::sync::Arc;

use futures_core::Stream;

use crate::caller::CallerContext;
use crate::error::Result;
use crate::language_model::protocol::sanitize_model_name;
use crate::language_model::types::{PipelineRequest, PipelineResponse, Prompt, StreamPart};
use crate::language_model::{self, PipelineBuilder};
use crate::mcp;
use crate::metrics::MetricsRenderer;
use crate::plugin::{MigrationItem, PluginId};

/// Legacy custom-host assembly convenience for registering hooks and migrations.
///
/// Retained for the current alpha SDK API; removal requires an explicitly
/// announced breaking SDK release with migration notes. New request-check
/// extensions use [`crate::extension::ExtensionApi`] instead. This trait can
/// install global hooks and migrations and is not equivalent to router-bound
/// capability registration. Hooks remain individually registerable by hosts.
pub trait Plugin {
    /// The plugin's identity (for config mapping and logs).
    fn id(&self) -> &PluginId;

    /// Database migrations carried by this legacy host package. Empty = no database.
    /// Migration ownership stays with the custom host; the restricted extension
    /// API does not expose this facility.
    fn migrations(&self) -> Vec<MigrationItem> {
        Vec::new()
    }

    /// Install this plugin's hooks into the builder.
    fn install(&self, app: &mut AppBuilder);
}

/// An ingress-time rewrite of a parsed request [`Prompt`],
/// applied by the HTTP server after protocol parsing and before the request
/// enters the pipeline.
///
/// This is the seam for transforms that must touch the prompt body — its
/// `tools`, `tool_choice`, or `system` — which the pipeline context exposes
/// read-only downstream. The `bitrouter/fusion` model alias is the first
/// consumer: it rewrites the alias model to a real one and attaches the Fusion
/// declaration. Transforms run in registration order.
pub trait PromptTransform: Send + Sync {
    /// Rewrite the prompt in place. A transform that does not apply to this
    /// request leaves it untouched.
    fn apply(&self, prompt: &mut language_model::types::Prompt);

    /// Like [`apply`](Self::apply), but with the inbound request headers
    /// available. The default delegates to [`apply`](Self::apply), ignoring the
    /// headers; transforms whose routing decision depends on a header the
    /// client sent (e.g. detecting genuine Claude Code traffic by its
    /// `anthropic-beta` agent-profile marker) override this instead. The HTTP
    /// server always calls this method.
    fn apply_with_headers(
        &self,
        prompt: &mut language_model::types::Prompt,
        _headers: &http::HeaderMap,
    ) {
        self.apply(prompt);
    }

    /// Validate authorized history removal after model selection. Both prompts
    /// are already prepared; this must not rerun the transform or its selection.
    /// Implementations must reject removal of dependencies introduced by the
    /// transform. Absence of this contract keeps reconstruction disabled.
    fn validate_context_rebuild(&self, _original: &Prompt, _rebuilt: &Prompt) -> Result<()> {
        Err(crate::error::BitrouterError::bad_request(
            "prompt transform has no context revalidation contract",
        ))
    }
}

/// A fully assembled application: one pipeline per enabled protocol, plus the
/// injected infrastructure and the collected migration set.
pub struct App {
    language_model: Option<Arc<language_model::Pipeline>>,
    mcp: Option<Arc<mcp::Pipeline>>,
    /// Optional Prometheus-style metrics renderer; if set, the HTTP server
    /// exposes `GET /metrics` against it.
    metrics_renderer: Option<Arc<dyn MetricsRenderer>>,
    migrations: Vec<MigrationItem>,
    skip_auth: bool,
    mcp_aggregate_route: Option<String>,
    /// Ingress-time prompt transforms, applied by the HTTP server in order.
    prompt_transforms: Vec<Arc<dyn PromptTransform>>,
}

impl App {
    /// Start configuring an application.
    pub fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// Execute one non-streaming native model turn through this app's routed,
    /// checked, and settled language-model pipeline. The caller is established
    /// by the embedding server, not inferred from `skip_auth`.
    ///
    /// Prompt transforms that require HTTP headers see an empty header map and
    /// therefore cannot infer an HTTP client identity. The SDK's optional
    /// request-scoped server-tool loop is skipped: the embedding agent owns its
    /// own tool calls and must execute each one exactly once.
    pub async fn execute_native(
        &self,
        prompt: Prompt,
        caller: CallerContext,
    ) -> Result<PipelineResponse> {
        if prompt.stream {
            return Err(crate::error::BitrouterError::bad_request(
                "native model turns must be non-streaming",
            ));
        }
        let pipeline = self.language_model.as_ref().ok_or_else(|| {
            crate::error::BitrouterError::internal("no language_model pipeline configured")
        })?;
        let headers = http::HeaderMap::new();
        let (prompt, original_model) =
            prepare_model_prompt(prompt, &headers, &self.prompt_transforms);
        let mut request = PipelineRequest::new(prompt.model.clone(), caller, prompt);
        request.original_model = original_model;
        Arc::clone(pipeline)
            .execute_without_server_tools(request)
            .await
    }

    /// Execute a managed native turn with durable admission before each
    /// provider attempt. Ordinary HTTP and native requests share preparation,
    /// selection, execution and settlement; core-owned tools stay client-owned.
    pub async fn execute_native_controlled(
        &self,
        prompt: Prompt,
        caller: CallerContext,
        control: Arc<dyn crate::language_model::native::NativeExecutionControl>,
    ) -> Result<PipelineResponse> {
        self.execute_native_controlled_with_headers(prompt, caller, http::HeaderMap::new(), control)
            .await
    }

    /// Controlled native execution with host-authenticated ingress headers.
    /// The host retains these credentials in memory, outside durable agent state.
    /// Normal pipeline authentication and policy checks still run on every turn.
    pub async fn execute_native_controlled_with_headers(
        &self,
        prompt: Prompt,
        caller: CallerContext,
        headers: http::HeaderMap,
        control: Arc<dyn crate::language_model::native::NativeExecutionControl>,
    ) -> Result<PipelineResponse> {
        if prompt.stream {
            return Err(crate::error::BitrouterError::bad_request(
                "controlled model turns must be non-streaming",
            ));
        }
        let pipeline = self.language_model.as_ref().ok_or_else(|| {
            crate::error::BitrouterError::internal("no language_model pipeline configured")
        })?;
        let requested_model = sanitize_model_name(&prompt.model);
        let requested_effort = (prompt.params.reasoning_effort_source
            == crate::language_model::types::ReasoningEffortSource::Caller)
            .then_some(prompt.params.reasoning_effort)
            .flatten();
        let mut prompt = prompt;
        prompt.model = requested_model.clone();
        let mut request = PipelineRequest::new(requested_model.clone(), caller, prompt);
        request.headers = headers.clone();
        for (index, transform) in self.prompt_transforms.iter().enumerate() {
            use crate::language_model::native_preparation::{
                NativePreparationWork, NativePreparationWorkKind, observe,
            };
            let work_index = u32::try_from(index).map_err(|_| {
                crate::error::BitrouterError::internal("prompt transform index exhausted")
            })?;
            observe(
                control.as_ref(),
                NativePreparationWork {
                    request_id: request.request_id.clone(),
                    kind: NativePreparationWorkKind::PromptTransform,
                    work_index,
                },
                async {
                    transform.apply_with_headers(&mut request.prompt, &headers);
                    Ok(())
                },
            )
            .await?;
        }
        let prompt = &mut request.prompt;
        if (control.model_selection() == crate::language_model::native::NativeModelSelection::Fixed
            && prompt.model != requested_model)
            || requested_effort.is_some_and(|effort| prompt.params.reasoning_effort != Some(effort))
        {
            return Err(crate::error::BitrouterError::bad_request(
                "managed request preparation changed a manual model or effort override",
            ));
        }
        if requested_effort.is_some() {
            prompt.params.reasoning_effort_source =
                crate::language_model::types::ReasoningEffortSource::Caller;
        }
        request.model = prompt.model.clone();
        let control = Arc::new(TransformCheckedControl {
            inner: control,
            transforms: self.prompt_transforms.clone(),
        });
        Arc::clone(pipeline)
            .execute_native_controlled(request, control)
            .await
    }

    /// Read already persisted, caller-scoped request costs without executing or
    /// charging another request. An unconfigured source is explicitly unknown.
    pub async fn native_cost_observations(
        &self,
        caller: &CallerContext,
        request_ids: &[String],
    ) -> Result<Vec<crate::language_model::native_accounting::NativeCostObservation>> {
        if let Some(source) = self
            .language_model
            .as_ref()
            .and_then(|pipeline| pipeline.native_cost_source.as_ref())
        {
            source.read(caller, request_ids).await
        } else {
            Ok(request_ids
                .iter()
                .map(
                    |request_id| crate::language_model::native_accounting::NativeCostObservation {
                        request_id: request_id.clone(),
                        claims: Vec::new(),
                        unknown_reason: Some("cost_source_unavailable".into()),
                    },
                )
                .collect())
        }
    }

    /// Stream one native agent turn through the same routed and settled path.
    /// The embedding agent owns client tool calls, so SDK server tools are
    /// excluded just as they are for `execute_native`.
    pub async fn execute_native_stream(
        &self,
        mut prompt: Prompt,
        caller: CallerContext,
    ) -> Result<(
        String,
        Pin<Box<dyn Stream<Item = Result<StreamPart>> + Send>>,
    )> {
        let pipeline = self.language_model.as_ref().ok_or_else(|| {
            crate::error::BitrouterError::internal("no language_model pipeline configured")
        })?;
        prompt.stream = true;
        let headers = http::HeaderMap::new();
        let (prompt, original_model) =
            prepare_model_prompt(prompt, &headers, &self.prompt_transforms);
        let mut request = PipelineRequest::new(prompt.model.clone(), caller, prompt);
        request.original_model = original_model;
        let request_id = request.request_id.clone();
        let parts = Arc::clone(pipeline)
            .execute_stream_without_server_tools(request)
            .await?;
        Ok((request_id, parts))
    }

    /// The `language_model` pipeline, if that protocol was configured.
    pub fn language_model(&self) -> Option<&Arc<language_model::Pipeline>> {
        self.language_model.as_ref()
    }

    /// The `mcp` (Model Context Protocol) pipeline, if configured. v1.0 ships
    /// it as pure-routing; the HTTP server mounts `POST /mcp/{name}` against
    /// it.
    pub fn mcp(&self) -> Option<&Arc<mcp::Pipeline>> {
        self.mcp.as_ref()
    }

    /// The collected migration set (sorted by version).
    pub fn migrations(&self) -> &[MigrationItem] {
        &self.migrations
    }

    /// Whether `server.skip_auth` is on — when true, credential-less requests
    /// are admitted with a synthesised local caller.
    pub fn skip_auth(&self) -> bool {
        self.skip_auth
    }

    /// The Prometheus-style metrics renderer, if one was wired into the app.
    /// The HTTP server's `GET /metrics` route reads this.
    pub fn metrics_renderer(&self) -> Option<&Arc<dyn MetricsRenderer>> {
        self.metrics_renderer.as_ref()
    }

    /// The ingress-time prompt transforms wired into the app, applied by the
    /// HTTP server in registration order before a request enters the pipeline.
    pub fn prompt_transforms(&self) -> &[Arc<dyn PromptTransform>] {
        &self.prompt_transforms
    }

    /// HTTP path for the MCP aggregate route, when configured (`POST <path>`
    /// fans out across every `aggregate: true` MCP server). `None` means
    /// only per-server routes (`POST /mcp/{server}`) are mounted.
    pub fn mcp_aggregate_route(&self) -> Option<&str> {
        self.mcp_aggregate_route.as_deref()
    }
}

/// Enforce the App's transform contract for every native embedding control.
struct TransformCheckedControl {
    inner: Arc<dyn crate::language_model::native::NativeExecutionControl>,
    transforms: Vec<Arc<dyn PromptTransform>>,
}

#[async_trait::async_trait]
impl crate::language_model::native::NativeExecutionControl for TransformCheckedControl {
    fn provider_response_byte_limit(&self) -> Option<u64> {
        self.inner.provider_response_byte_limit()
    }

    async fn provider_cancelled(&self) {
        self.inner.provider_cancelled().await;
    }

    async fn before_preparation_work(
        &self,
        work: &crate::language_model::native_preparation::NativePreparationWork,
    ) -> Result<()> {
        self.inner.before_preparation_work(work).await
    }
    async fn after_preparation_work(
        &self,
        report: crate::language_model::native_preparation::NativePreparationWorkReport,
    ) -> Result<()> {
        self.inner.after_preparation_work(report).await
    }
    fn model_selection(&self) -> crate::language_model::native::NativeModelSelection {
        self.inner.model_selection()
    }
    async fn plan(
        &self,
        plan: crate::language_model::native::NativePlan,
    ) -> Result<crate::language_model::native::NativePlanAdmission> {
        self.inner.plan(plan).await
    }
    async fn before_input_count(
        &self,
        plan: &crate::language_model::native::NativePlan,
        index: u32,
    ) -> Result<()> {
        self.inner.before_input_count(plan, index).await
    }
    async fn after_input_count(
        &self,
        report: crate::language_model::native::NativeInputCountReport,
    ) -> Result<()> {
        self.inner.after_input_count(report).await
    }
    async fn rebuild_context(
        &self,
        rejected: &crate::language_model::native::NativePlan,
    ) -> Result<Option<Vec<language_model::types::Message>>> {
        self.inner.rebuild_context(rejected).await
    }
    async fn validate_context_rebuild(
        &self,
        original: &Prompt,
        rebuilt: &Prompt,
        request_id: &str,
    ) -> Result<()> {
        self.inner
            .validate_context_rebuild(original, rebuilt, request_id)
            .await?;
        for transform in &self.transforms {
            self.inner.check_context_validation(request_id).await?;
            transform.validate_context_rebuild(original, rebuilt)?;
        }
        Ok(())
    }
    async fn before_context_validation(&self, request_id: &str) -> Result<()> {
        self.inner.before_context_validation(request_id).await
    }
    async fn check_context_validation(&self, request_id: &str) -> Result<()> {
        self.inner.check_context_validation(request_id).await
    }
    fn context_validation_gate_duration(&self) -> Option<std::time::Duration> {
        self.inner.context_validation_gate_duration()
    }
    async fn after_context_validation(
        &self,
        report: crate::language_model::native::NativeContextValidationReport,
    ) -> Result<()> {
        self.inner.after_context_validation(report).await
    }
    async fn before_attempt(&self, request_id: &str, index: u32) -> Result<()> {
        self.inner.before_attempt(request_id, index).await
    }
    async fn after_attempt(&self, report: crate::language_model::native::NativeAttemptReport) {
        self.inner.after_attempt(report).await
    }
    async fn before_provider_work(
        &self,
        work: &crate::language_model::native_work::NativeProviderWork,
    ) -> Result<()> {
        self.inner.before_provider_work(work).await
    }
    async fn after_provider_work(
        &self,
        report: crate::language_model::native_work::NativeProviderWorkReport,
    ) {
        self.inner.after_provider_work(report).await
    }
}

/// Common post-parse preparation for HTTP and in-process native requests.
/// Preserve the caller's selector before transforms for policy attribution.
pub(crate) fn prepare_model_prompt(
    mut prompt: Prompt,
    headers: &http::HeaderMap,
    transforms: &[Arc<dyn PromptTransform>],
) -> (Prompt, String) {
    prompt.model = sanitize_model_name(&prompt.model);
    let original_model = prompt.model.clone();
    for transform in transforms {
        transform.apply_with_headers(&mut prompt, headers);
    }
    (prompt, original_model)
}

/// Configures an [`App`] for a trusted host. Each protocol has its own
/// sub-builder; [`Self::plugin`] retains the legacy host packaging convenience.
/// New request-check extension authors use the restricted `ExtensionApi` in the
/// `bitrouter` host crate rather than receive this builder.
pub struct AppBuilder {
    language_model: PipelineBuilder,
    mcp: mcp::PipelineBuilder,
    metrics_renderer: Option<Arc<dyn MetricsRenderer>>,
    migrations: Vec<MigrationItem>,
    skip_auth: bool,
    mcp_aggregate_route: Option<String>,
    prompt_transforms: Vec<Arc<dyn PromptTransform>>,
}

impl AppBuilder {
    /// A fresh, empty builder.
    pub fn new() -> Self {
        Self {
            language_model: PipelineBuilder::new(),
            mcp: mcp::PipelineBuilder::new(),
            metrics_renderer: None,
            migrations: Vec::new(),
            skip_auth: false,
            mcp_aggregate_route: None,
            prompt_transforms: Vec::new(),
        }
    }

    /// Path for the MCP aggregate fan-out endpoint (e.g. `/mcp`). When set,
    /// `App::serve` mounts a `POST <path>` handler that fans out across every
    /// `aggregate: true` MCP server. Has no effect unless the MCP pipeline is
    /// also configured.
    pub fn mcp_aggregate_route(mut self, path: impl Into<String>) -> Self {
        self.mcp_aggregate_route = Some(path.into());
        self
    }

    /// Set the SDK-level `skip_auth` flag (code default `false`). When `true`,
    /// the server admits credential-less requests with a synthesised local
    /// caller; `AuthHook` still validates any credential that *is* presented.
    pub fn skip_auth(mut self, skip_auth: bool) -> Self {
        self.skip_auth = skip_auth;
        self
    }

    /// Configure the `language_model` protocol pipeline.
    pub fn language_model<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut PipelineBuilder),
    {
        configure(&mut self.language_model);
        self
    }

    /// Configure the `mcp` (Model Context Protocol) protocol pipeline. v1.0
    /// MCP is pure-routing (no settlement); the HTTP server mounts
    /// `POST /mcp/{name}` against the built pipeline.
    pub fn mcp<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut mcp::PipelineBuilder),
    {
        configure(&mut self.mcp);
        self
    }

    /// Wire a Prometheus-style metrics renderer. When set, the HTTP server
    /// exposes `GET /metrics` against it. Typically the same in-process
    /// accumulator you registered as an `ObserveHook`; the OSS binary has no
    /// such accumulator any more (it pushes over OTLP) and mounts a stub
    /// renderer that serves a migration banner instead.
    pub fn metrics_renderer(mut self, renderer: Arc<dyn MetricsRenderer>) -> Self {
        self.metrics_renderer = Some(renderer);
        self
    }

    /// Register an ingress-time [`PromptTransform`]. The HTTP server applies it
    /// (and any others, in registration order) after protocol parsing, before
    /// the request enters the pipeline. Used to wire model aliases such as
    /// `bitrouter/fusion`.
    pub fn prompt_transform(mut self, transform: Arc<dyn PromptTransform>) -> Self {
        self.prompt_transforms.push(transform);
        self
    }

    /// Install a legacy [`Plugin`] host package, including its migrations.
    ///
    /// Retains global hook registration semantics in the current alpha SDK.
    /// New request-check extensions use [`crate::extension::ExtensionApi`];
    /// this method does not apply router bindings to legacy hooks. Removal
    /// requires an explicitly announced breaking SDK release with migration
    /// notes.
    pub fn plugin(mut self, plugin: impl Plugin) -> Self {
        self.migrations.extend(plugin.migrations());
        plugin.install(&mut self);
        self
    }

    /// Mutable access to the `language_model` sub-builder — the entry point a
    /// `Plugin::install` implementation uses.
    pub fn language_model_builder(&mut self) -> &mut PipelineBuilder {
        &mut self.language_model
    }

    /// Add migrations directly (used by `Plugin::install` when it wants to add
    /// migrations beyond what `Plugin::migrations` declared).
    pub fn add_migrations(&mut self, migrations: impl IntoIterator<Item = MigrationItem>) {
        self.migrations.extend(migrations);
    }

    /// Finalise into an [`App`]. Builds a pipeline for each protocol that was
    /// configured (the `language_model` pipeline needs at least a routing table
    /// and an executor).
    pub fn build(mut self) -> Result<App> {
        let language_model = if self.language_model.is_configured() {
            Some(Arc::new(self.language_model.build()?))
        } else {
            None
        };
        let mcp = if self.mcp.is_configured() {
            Some(Arc::new(self.mcp.build()?))
        } else {
            None
        };

        self.migrations.sort_by_key(|m| m.version);

        // The aggregate route only makes sense alongside an MCP pipeline —
        // drop it silently if no MCP pipeline was configured (keeps
        // `mcp_aggregate_route(...)` from accidentally mounting a 404-only
        // handler in apps that don't use MCP).
        let mcp_aggregate_route = if mcp.is_some() {
            self.mcp_aggregate_route
        } else {
            None
        };

        Ok(App {
            language_model,
            mcp,
            metrics_renderer: self.metrics_renderer,
            migrations: self.migrations,
            skip_auth: self.skip_auth,
            mcp_aggregate_route,
            prompt_transforms: self.prompt_transforms,
        })
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::{GenerationParams, Prompt, ProviderMetadata};

    struct SetModel(&'static str);
    impl PromptTransform for SetModel {
        fn apply(&self, prompt: &mut Prompt) {
            prompt.model = self.0.to_string();
        }
    }

    fn bare_prompt() -> Prompt {
        Prompt {
            model: "orig".to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn registers_and_applies_prompt_transforms() {
        let app = AppBuilder::new()
            .prompt_transform(Arc::new(SetModel("x/y")))
            .build()
            .unwrap();
        assert_eq!(app.prompt_transforms().len(), 1);
        let mut prompt = bare_prompt();
        app.prompt_transforms()[0].apply(&mut prompt);
        assert_eq!(prompt.model, "x/y");
    }
}
