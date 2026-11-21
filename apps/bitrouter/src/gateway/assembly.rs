//! A configured gateway application and its private assembly machinery.
//!
//! Embedding hosts use [`crate::assemble::build_app_with_extensions`] and serve
//! the resulting application with its HTTP lifecycle methods. Typed request
//! checks register through [`crate::gateway::extensions::registry::ExtensionApi`].

use std::sync::Arc;

use crate::gateway::error::Result;
use crate::gateway::model::protocol::sanitize_model_name;
use bitrouter_ai::types::Prompt;

use crate::gateway::observability::metrics::MetricsRenderer;
use crate::gateway::pipeline::builder::PipelineBuilder;
use crate::gateway::{mcp, pipeline as language_model};

/// An ingress-time rewrite of a parsed request [`Prompt`],
/// applied by the HTTP server after protocol parsing and before the request
/// enters the pipeline.
///
/// This is the seam for transforms that must touch the prompt body — its
/// `tools`, `tool_choice`, or `system` — which the pipeline context exposes
/// read-only downstream. The `bitrouter/fusion` model alias is the first
/// consumer: it rewrites the alias model to a real one and attaches the Fusion
/// declaration. Transforms run in registration order.
pub(crate) trait PromptTransform: Send + Sync {
    /// Rewrite the prompt in place. A transform that does not apply to this
    /// request leaves it untouched.
    fn apply(&self, prompt: &mut bitrouter_ai::types::Prompt);

    /// Like [`apply`](Self::apply), but with the inbound request headers
    /// available. The default delegates to [`apply`](Self::apply), ignoring the
    /// headers; transforms whose routing decision depends on a header the
    /// client sent (e.g. detecting genuine Claude Code traffic by its
    /// `anthropic-beta` agent-profile marker) override this instead. The HTTP
    /// server always calls this method.
    fn apply_with_headers(
        &self,
        prompt: &mut bitrouter_ai::types::Prompt,
        _headers: &http::HeaderMap,
    ) {
        self.apply(prompt);
    }
}

/// A fully assembled application: one pipeline per enabled protocol, plus the
/// injected infrastructure.
pub struct App {
    /// Installed only by successful product assembly; never an embedding setter.
    tenancy: Option<crate::tenancy::Service>,
    language_model: Option<Arc<language_model::execution::Pipeline>>,
    mcp: Option<Arc<mcp::Pipeline>>,
    /// Optional Prometheus-style metrics renderer; if set, the HTTP server
    /// exposes `GET /metrics` against it.
    metrics_renderer: Option<Arc<dyn MetricsRenderer>>,
    skip_auth: bool,
    mcp_aggregate_route: Option<String>,
    /// Ingress-time prompt transforms, applied by the HTTP server in order.
    prompt_transforms: Vec<Arc<dyn PromptTransform>>,
}

impl App {
    pub(crate) fn owned_inference_router(&self, router: axum::Router) -> axum::Router {
        match &self.tenancy {
            Some(service) => router.layer(axum::middleware::from_fn_with_state(
                service.clone(),
                crate::tenancy::http::dispatch,
            )),
            None => router,
        }
    }

    pub(super) fn owned_transport_limits(&self) -> Option<super::transport::Limits> {
        self.tenancy
            .as_ref()
            .map(|service| super::transport::Limits::for_tenancy(service.execution_settings()))
    }

    /// Stop new tenant dispatch independently of HTTP and durable-work drains.
    pub(crate) fn stop_owned_dispatch(&self) -> anyhow::Result<()> {
        if let Some(service) = &self.tenancy {
            service.stop_dispatch()?;
        }
        Ok(())
    }

    pub(crate) fn with_tenancy(mut self, service: crate::tenancy::Service) -> Self {
        self.tenancy = Some(service);
        self
    }

    /// Start configuring an application.
    pub(crate) fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// The `language_model` pipeline, if that protocol was configured.
    pub(crate) fn language_model(&self) -> Option<&Arc<language_model::execution::Pipeline>> {
        self.language_model.as_ref()
    }

    /// The `mcp` (Model Context Protocol) routing pipeline, if configured.
    /// The HTTP server mounts `POST /mcp/{name}` against
    /// it.
    pub(crate) fn mcp(&self) -> Option<&Arc<mcp::Pipeline>> {
        self.mcp.as_ref()
    }

    /// Whether `server.skip_auth` is on — when true, credential-less requests
    /// are admitted with a synthesised local caller.
    pub(crate) fn skip_auth(&self) -> bool {
        self.skip_auth
    }

    /// The Prometheus-style metrics renderer, if one was wired into the app.
    /// The HTTP server's `GET /metrics` route reads this.
    pub(crate) fn metrics_renderer(&self) -> Option<&Arc<dyn MetricsRenderer>> {
        self.metrics_renderer.as_ref()
    }

    /// The ingress-time prompt transforms wired into the app, applied by the
    /// HTTP server in registration order before a request enters the pipeline.
    pub(crate) fn prompt_transforms(&self) -> &[Arc<dyn PromptTransform>] {
        &self.prompt_transforms
    }

    /// HTTP path for the MCP aggregate route, when configured (`POST <path>`
    /// fans out across every `aggregate: true` MCP server). `None` means
    /// only per-server routes (`POST /mcp/{server}`) are mounted.
    pub(crate) fn mcp_aggregate_route(&self) -> Option<&str> {
        self.mcp_aggregate_route.as_deref()
    }
}

/// Post-parse preparation for HTTP requests.
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

/// Private assembly of configured protocol pipelines. Extension authors register
/// typed callbacks through `ExtensionApi`; the host owns this builder.
pub(crate) struct AppBuilder {
    language_model: PipelineBuilder,
    decision_model: Option<crate::gateway::model::decision::DecisionRuntime>,
    mcp: mcp::PipelineBuilder,
    metrics_renderer: Option<Arc<dyn MetricsRenderer>>,
    skip_auth: bool,
    mcp_aggregate_route: Option<String>,
    prompt_transforms: Vec<Arc<dyn PromptTransform>>,
}

impl AppBuilder {
    /// A fresh, empty builder.
    pub(crate) fn new() -> Self {
        Self {
            language_model: PipelineBuilder::new(),
            decision_model: None,
            mcp: mcp::PipelineBuilder::new(),
            metrics_renderer: None,
            skip_auth: false,
            mcp_aggregate_route: None,
            prompt_transforms: Vec::new(),
        }
    }

    /// Path for the MCP aggregate fan-out endpoint (e.g. `/mcp`). When set,
    /// `App::serve` mounts a `POST <path>` handler that fans out across every
    /// `aggregate: true` MCP server. Has no effect unless the MCP pipeline is
    /// also configured.
    pub(crate) fn mcp_aggregate_route(mut self, path: impl Into<String>) -> Self {
        self.mcp_aggregate_route = Some(path.into());
        self
    }

    /// Set the SDK-level `skip_auth` flag (code default `false`). When `true`,
    /// the server admits credential-less requests with a synthesised local
    /// caller; `AuthHook` still validates any credential that *is* presented.
    pub(crate) fn skip_auth(mut self, skip_auth: bool) -> Self {
        self.skip_auth = skip_auth;
        self
    }

    /// Configure the `language_model` protocol pipeline.
    pub(crate) fn language_model<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut PipelineBuilder),
    {
        configure(&mut self.language_model);
        self
    }

    /// Install a typed decision backend for harness-owned semantic decisions.
    pub(crate) fn decision_model(
        mut self,
        runtime: crate::gateway::model::decision::DecisionRuntime,
    ) -> Self {
        self.decision_model = Some(runtime);
        self
    }

    /// Configure the `mcp` (Model Context Protocol) routing pipeline
    /// without settlement; the HTTP server mounts
    /// `POST /mcp/{name}` against the built pipeline.
    pub(crate) fn mcp<F>(mut self, configure: F) -> Self
    where
        F: FnOnce(&mut mcp::PipelineBuilder),
    {
        configure(&mut self.mcp);
        self
    }

    /// Wire a Prometheus-style metrics renderer. When set, the HTTP server
    /// exposes `GET /metrics` against it. The stock host exports over OTLP
    /// and installs a renderer that explains where metrics are available.
    pub(crate) fn metrics_renderer(mut self, renderer: Arc<dyn MetricsRenderer>) -> Self {
        self.metrics_renderer = Some(renderer);
        self
    }

    /// Register an ingress-time [`PromptTransform`]. The HTTP server applies it
    /// (and any others, in registration order) after protocol parsing, before
    /// the request enters the pipeline. Used to wire model aliases such as
    /// `bitrouter/fusion`.
    pub(crate) fn prompt_transform(mut self, transform: Arc<dyn PromptTransform>) -> Self {
        self.prompt_transforms.push(transform);
        self
    }

    /// Finalise into an [`App`]. Builds a pipeline for each protocol that was
    /// configured (the `language_model` pipeline needs at least a routing table
    /// and an executor).
    pub(crate) fn build(mut self) -> Result<App> {
        if let Some(runtime) = &self.decision_model {
            if runtime.model.trim().is_empty() {
                return Err(crate::gateway::error::BitrouterError::bad_request(
                    "decision model is empty",
                ));
            }
            runtime
                .policy
                .validate()
                .map_err(crate::gateway::error::BitrouterError::bad_request)?;
            if let Some(pricing) = &runtime.pricing {
                pricing
                    .validate()
                    .map_err(crate::gateway::error::BitrouterError::bad_request)?;
            }
        }
        self.language_model.decision_model = self.decision_model.clone();
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
            tenancy: None,
            language_model,
            mcp,
            metrics_renderer: self.metrics_renderer,
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
    use bitrouter_ai::types::{GenerationParams, Prompt, ProviderMetadata};

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
