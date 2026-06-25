//! The `web_fetch` [`RouterToolset`]: advertises one `web_fetch` function tool
//! (only when the caller declares `bitrouter:web_fetch`) and dispatches each call
//! to a configured [`WebFetchBackend`]. Backend selection mirrors `web_search` —
//! an ordered preference list, a per-request override that pins one backend by
//! name, and failover to the next backend when one errors.

use std::sync::Arc;

use async_trait::async_trait;

use super::backend::{FetchOptions, WebFetchBackend};
use crate::error::Result;
use crate::language_model::server_tools::declarations::{ServerToolDeclarations, WEB_FETCH_TOOL};
use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use crate::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};

/// A [`RouterToolset`] exposing the `web_fetch` server tool over one or more
/// extraction backends.
pub struct WebFetchToolset {
    backends: Vec<Arc<dyn WebFetchBackend>>,
    default_max_content_tokens: u32,
}

impl WebFetchToolset {
    /// Build the toolset over an ordered (preference/failover) backend list and
    /// the deployment default content cap.
    pub fn new(backends: Vec<Arc<dyn WebFetchBackend>>, default_max_content_tokens: u32) -> Self {
        Self {
            backends,
            default_max_content_tokens,
        }
    }

    /// Candidate backends for a call, honoring a `backend` override: the named
    /// backend first (if configured), then the rest in configured order. An
    /// unknown name is ignored. Excludes the pinned entry from the tail by index
    /// (not name) so two same-named backends both stay in the failover chain.
    fn candidates(&self, override_name: Option<&str>) -> Vec<&Arc<dyn WebFetchBackend>> {
        let pinned =
            override_name.and_then(|name| self.backends.iter().position(|b| b.name() == name));
        let mut ordered: Vec<&Arc<dyn WebFetchBackend>> = Vec::with_capacity(self.backends.len());
        if let Some(i) = pinned {
            ordered.push(&self.backends[i]);
        }
        for (i, b) in self.backends.iter().enumerate() {
            if Some(i) != pinned {
                ordered.push(b);
            }
        }
        ordered
    }
}

fn error_output(message: impl Into<String>) -> ToolResultOutput {
    ToolResultOutput::Json {
        value: serde_json::json!({ "status": "error", "error": message.into() }),
    }
}

#[async_trait]
impl RouterToolset for WebFetchToolset {
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
        let advertise =
            ServerToolDeclarations::from_context(ctx).is_some_and(|d| d.web_fetch.is_some());
        if !advertise || self.backends.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Tool::Function {
            name: WEB_FETCH_TOOL.to_string(),
            description: Some(
                "Fetch and read the full content of a specific web page. Provide a \
                 `url`; the page is returned as `content` (markdown/text) with its \
                 `title` and `url` when available."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to fetch and read." },
                    "max_content_tokens": { "type": "integer", "description": "Optional cap on returned content size (approx tokens).", "minimum": 1 }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }])
    }

    async fn call_tool(
        &self,
        _name: &str,
        arguments: &str,
        ctx: &ToolContext,
    ) -> Result<ToolResultOutput> {
        let decl = ServerToolDeclarations::from_context(ctx)
            .and_then(|d| d.web_fetch)
            .unwrap_or_default();
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
            return Ok(error_output("web_fetch call is missing required `url`"));
        };
        // Content cap, narrowing from the deployment default through the
        // declaration to this call's argument: each layer may only *lower* the
        // cap, never raise it above what the deployment configured.
        let ceiling = self.default_max_content_tokens;
        let ceiling = decl.max_content_tokens.map_or(ceiling, |d| d.min(ceiling));
        let max_content_tokens = args
            .get("max_content_tokens")
            .and_then(|v| v.as_u64())
            .map(|n| (n.min(ceiling as u64)) as u32)
            .unwrap_or(ceiling)
            .max(1);
        let opts = FetchOptions { max_content_tokens };

        let candidates = self.candidates(decl.backend.as_deref());
        let mut last_error = "no web_fetch backend is configured".to_string();
        for backend in candidates {
            match backend.fetch(url, &opts, ctx).await {
                Ok(result) => match serde_json::to_value(&result) {
                    Ok(mut value) => {
                        if let Some(obj) = value.as_object_mut() {
                            obj.insert("status".to_string(), serde_json::json!("ok"));
                        }
                        return Ok(ToolResultOutput::Json { value });
                    }
                    Err(err) => last_error = format!("failed to encode result: {err}"),
                },
                Err(err) => last_error = err,
            }
        }
        Ok(error_output(last_error))
    }

    fn owns(&self, name: &str) -> bool {
        name.rsplit([':', '.']).next().unwrap_or(name) == WEB_FETCH_TOOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::backend::WebFetchResult;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::declarations::{
        WebFetchDeclaration, declarations_plugin_id,
    };
    use std::collections::HashMap;

    /// A backend that returns labeled content, or always errors.
    struct StubBackend {
        name: String,
        ok: bool,
    }
    #[async_trait]
    impl WebFetchBackend for StubBackend {
        fn name(&self) -> &str {
            &self.name
        }
        async fn fetch(
            &self,
            url: &str,
            _opts: &FetchOptions,
            _ctx: &ToolContext,
        ) -> std::result::Result<WebFetchResult, String> {
            if self.ok {
                Ok(WebFetchResult {
                    backend: self.name.clone(),
                    url: url.to_string(),
                    title: None,
                    content: "page".to_string(),
                    published: None,
                })
            } else {
                Err(format!("{} failed", self.name))
            }
        }
    }

    fn ok(name: &str) -> Arc<dyn WebFetchBackend> {
        Arc::new(StubBackend { name: name.to_string(), ok: true })
    }
    fn err(name: &str) -> Arc<dyn WebFetchBackend> {
        Arc::new(StubBackend { name: name.to_string(), ok: false })
    }

    fn ctx_with(decl: Option<WebFetchDeclaration>) -> ToolContext {
        let mut meta: HashMap<_, _> = HashMap::new();
        if let Some(d) = decl {
            let decls = ServerToolDeclarations {
                web_fetch: Some(d),
                parent_model: "m".to_string(),
                ..Default::default()
            };
            meta.insert(
                declarations_plugin_id().clone(),
                serde_json::to_value(decls).unwrap(),
            );
        }
        ToolContext::new(CallerContext::local(), meta)
    }

    #[tokio::test]
    async fn advertises_only_when_declared() {
        let ts = WebFetchToolset::new(vec![ok("exa")], 5);
        assert!(ts.list_tools(&ctx_with(None)).await.unwrap().is_empty());
        let tools = ts
            .list_tools(&ctx_with(Some(WebFetchDeclaration::default())))
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "web_fetch");
        assert!(ts.owns("web_fetch"));
        assert!(ts.owns("bitrouter:web_fetch"));
    }

    #[tokio::test]
    async fn missing_url_is_an_error_result() {
        let ts = WebFetchToolset::new(vec![ok("exa")], 5);
        let out = ts
            .call_tool("web_fetch", "{}", &ctx_with(Some(WebFetchDeclaration::default())))
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }

    #[tokio::test]
    async fn dispatches_to_default_backend() {
        let ts = WebFetchToolset::new(vec![ok("primary"), ok("secondary")], 5);
        let out = ts
            .call_tool(
                "web_fetch",
                r#"{"url":"https://a"}"#,
                &ctx_with(Some(WebFetchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value }
            if value["status"] == "ok" && value["backend"] == "primary" && value["content"] == "page"));
    }

    #[tokio::test]
    async fn backend_override_pins_a_named_backend() {
        let ts = WebFetchToolset::new(vec![ok("primary"), ok("secondary")], 5);
        let out = ts
            .call_tool(
                "web_fetch",
                r#"{"url":"https://a"}"#,
                &ctx_with(Some(WebFetchDeclaration {
                    backend: Some("secondary".into()),
                    max_content_tokens: None,
                })),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["backend"] == "secondary"));
    }

    #[tokio::test]
    async fn fails_over_to_next_backend_on_error() {
        let ts = WebFetchToolset::new(vec![err("primary"), ok("secondary")], 5);
        let out = ts
            .call_tool(
                "web_fetch",
                r#"{"url":"https://a"}"#,
                &ctx_with(Some(WebFetchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value }
            if value["status"] == "ok" && value["backend"] == "secondary"));
    }

    #[tokio::test]
    async fn all_backends_failing_yields_error_result() {
        let ts = WebFetchToolset::new(vec![err("only")], 5);
        let out = ts
            .call_tool(
                "web_fetch",
                r#"{"url":"https://a"}"#,
                &ctx_with(Some(WebFetchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }
}
