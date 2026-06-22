//! [`App`] and [`AppBuilder`] — the top-level entry point.
//!
//! An [`App`] holds one pipeline per enabled protocol
//! ([`crate::language_model::Pipeline`], [`crate::mcp::Pipeline`],
//! [`crate::acp::Pipeline`]) plus the injected infrastructure (metrics store,
//! metrics renderer, the aggregated migration set).
//!
//! [`AppBuilder`] configures each protocol through its own sub-builder closure
//! ([`language_model`](AppBuilder::language_model), [`mcp`](AppBuilder::mcp),
//! [`acp`](AppBuilder::acp)). A pipeline is built only for protocols that have
//! something configured.
//!
//! # Plugins vs hooks
//!
//! The [`Plugin`] trait is an **optional** convenience: it packages a related
//! set of hooks plus any SQL [`crate::plugin::MigrationItem`]s and installs
//! them through [`AppBuilder::plugin`]. The atomic unit is still a single
//! hook — every plugin can be re-created by calling the relevant
//! sub-builder's hook methods one by one without ever touching [`Plugin`].
//!
//! ```no_run
//! use std::sync::Arc;
//! use bitrouter_sdk::App;
//! use bitrouter_sdk::language_model::{HttpExecutor, StaticRoutingTable};
//!
//! # fn run() -> bitrouter_sdk::Result<()> {
//! let app = App::builder()
//!     .skip_auth(true)
//!     .language_model(|lm| {
//!         lm.routing_table(Arc::new(StaticRoutingTable::new()))
//!           .executor(Arc::new(HttpExecutor::with_defaults().unwrap()));
//!     })
//!     .build()?;
//! # let _ = app; Ok(()) }
//! ```

use std::sync::Arc;

use crate::error::Result;
use crate::language_model::{self, PipelineBuilder};
use crate::metrics::MetricsRenderer;
use crate::plugin::{MigrationItem, PluginId};
use crate::{acp, mcp};

/// An optional convenience packaging: registers a related set of hooks +
/// migrations into a builder in one call. `Plugin` is **not** a strong,
/// indivisible unit and **not** the only way to register hooks.
pub trait Plugin {
    /// The plugin's identity (for config mapping and logs).
    fn id(&self) -> &PluginId;

    /// Database migrations carried by this plugin. Empty = no database.
    fn migrations(&self) -> Vec<MigrationItem> {
        Vec::new()
    }

    /// Install this plugin's hooks into the builder.
    fn install(&self, app: &mut AppBuilder);
}

/// An ingress-time rewrite of a parsed request [`Prompt`](language_model::types::Prompt),
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
}

/// A fully assembled application: one pipeline per enabled protocol, plus the
/// injected infrastructure and the collected migration set.
pub struct App {
    language_model: Option<Arc<language_model::Pipeline>>,
    mcp: Option<Arc<mcp::Pipeline>>,
    acp: Option<Arc<acp::Pipeline>>,
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

    /// The `acp` (Agent Client Protocol) pipeline, if configured. v1.0 ships
    /// it as pure-routing; the binary's stdio adapter dispatches against it.
    pub fn acp(&self) -> Option<&Arc<acp::Pipeline>> {
        self.acp.as_ref()
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

/// Configures an [`App`]. Each protocol is configured through its own
/// sub-builder; `plugin()` is a convenience that drives those sub-builders for
/// you.
pub struct AppBuilder {
    language_model: PipelineBuilder,
    mcp: mcp::PipelineBuilder,
    acp: acp::PipelineBuilder,
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
            acp: acp::PipelineBuilder::new(),
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

    /// Configure the `acp` (Agent Client Protocol) protocol pipeline. v1.0
    /// ACP is pure-routing; the binary's stdio adapter dispatches against the
    /// built pipeline.
    pub fn acp<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut acp::PipelineBuilder),
    {
        configure(&mut self.acp);
        self
    }

    /// Wire a Prometheus-style metrics renderer. When set, the HTTP server
    /// exposes `GET /metrics` against it. Typically the same
    /// `Arc<PrometheusHook>` you registered as an `ObserveHook`.
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

    /// Install a `Plugin` convenience package. Equivalent to calling its hook
    /// registrations one by one.
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
        let acp = if self.acp.is_configured() {
            Some(Arc::new(self.acp.build()?))
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
            acp,
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
