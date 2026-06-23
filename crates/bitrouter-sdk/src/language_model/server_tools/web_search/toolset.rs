//! The `web_search` [`RouterToolset`]: advertises one `web_search` function tool
//! (only when the caller declares `bitrouter:web_search`) and dispatches each
//! call to a configured [`WebSearchBackend`].
//!
//! Backend selection mirrors BitRouter's provider model: an ordered preference
//! list whose first entry is the default, a per-request override that pins one
//! backend by name, and failover to the next backend when one errors. The chosen
//! engine is invisible to the calling model — it sees a single `web_search`
//! tool and a stable result schema regardless of which backend answered.

use std::sync::Arc;

use async_trait::async_trait;

use super::backend::{SearchOptions, WebSearchBackend};
use crate::error::Result;
use crate::language_model::server_tools::declarations::{ServerToolDeclarations, WEB_SEARCH_TOOL};
use crate::language_model::server_tools::toolset::{RouterToolset, ToolContext};
use crate::language_model::types::{ProviderMetadata, Tool, ToolResultOutput};

/// A [`RouterToolset`] exposing the `web_search` server tool over one or more
/// search backends.
pub struct WebSearchToolset {
    backends: Vec<Arc<dyn WebSearchBackend>>,
    default_max_results: u32,
}

impl WebSearchToolset {
    /// Build the toolset over an ordered (preference/failover) backend list and
    /// the deployment default result cap.
    pub fn new(backends: Vec<Arc<dyn WebSearchBackend>>, default_max_results: u32) -> Self {
        Self {
            backends,
            default_max_results,
        }
    }

    /// Candidate backends for a call, honoring a `backend` override: the named
    /// backend first (if configured), then the rest in configured order. An
    /// unknown name is ignored, falling back to the configured order.
    fn candidates(&self, override_name: Option<&str>) -> Vec<&Arc<dyn WebSearchBackend>> {
        let mut ordered: Vec<&Arc<dyn WebSearchBackend>> = Vec::with_capacity(self.backends.len());
        if let Some(name) = override_name
            && let Some(pinned) = self.backends.iter().find(|b| b.name() == name)
        {
            ordered.push(pinned);
        }
        for b in &self.backends {
            if !ordered.iter().any(|o| o.name() == b.name()) {
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
impl RouterToolset for WebSearchToolset {
    async fn list_tools(&self, ctx: &ToolContext) -> Result<Vec<Tool>> {
        let advertise =
            ServerToolDeclarations::from_context(ctx).is_some_and(|d| d.web_search.is_some());
        if !advertise || self.backends.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Tool::Function {
            name: WEB_SEARCH_TOOL.to_string(),
            description: Some(
                "Search the web for current information. Provide a focused \
                 `query`; results come back as a list of sources (title, url, \
                 snippet), and some backends also return a synthesized `answer`."
                    .to_string(),
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query." },
                    "max_results": { "type": "integer", "description": "Optional cap on the number of results.", "minimum": 1 }
                },
                "required": ["query"],
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
            .and_then(|d| d.web_search)
            .unwrap_or_default();
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
            return Ok(error_output("web_search call is missing required `query`"));
        };
        // Result cap precedence: call arg → declaration → deployment default.
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(u32::MAX as u64) as u32)
            .or(decl.max_results)
            .unwrap_or(self.default_max_results)
            .max(1);
        let opts = SearchOptions { max_results };

        let candidates = self.candidates(decl.backend.as_deref());
        let mut last_error = "no web_search backend is configured".to_string();
        for backend in candidates {
            match backend.search(query, &opts, ctx).await {
                Ok(results) => match serde_json::to_value(&results) {
                    Ok(mut value) => {
                        if let Some(obj) = value.as_object_mut() {
                            obj.insert("status".to_string(), serde_json::json!("ok"));
                        }
                        return Ok(ToolResultOutput::Json { value });
                    }
                    Err(err) => last_error = format!("failed to encode results: {err}"),
                },
                Err(err) => last_error = err,
            }
        }
        Ok(error_output(last_error))
    }

    fn owns(&self, name: &str) -> bool {
        name.rsplit([':', '.']).next().unwrap_or(name) == WEB_SEARCH_TOOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::declarations::{
        WebSearchDeclaration, declarations_plugin_id,
    };
    use crate::language_model::server_tools::web_search::backend::WebSearchResults;
    use std::collections::HashMap;

    /// A backend that always returns a labeled result, or always errors.
    struct StubBackend {
        name: String,
        ok: bool,
    }
    impl StubBackend {
        fn ok(name: &str) -> Self {
            Self {
                name: name.to_string(),
                ok: true,
            }
        }
        fn err(name: &str) -> Self {
            Self {
                name: name.to_string(),
                ok: false,
            }
        }
    }
    #[async_trait]
    impl WebSearchBackend for StubBackend {
        fn name(&self) -> &str {
            &self.name
        }
        async fn search(
            &self,
            _query: &str,
            _opts: &SearchOptions,
            _ctx: &ToolContext,
        ) -> std::result::Result<WebSearchResults, String> {
            if self.ok {
                Ok(WebSearchResults {
                    backend: self.name.clone(),
                    answer: None,
                    results: Vec::new(),
                })
            } else {
                Err(format!("{} failed", self.name))
            }
        }
    }

    fn ctx_with(decl: Option<WebSearchDeclaration>) -> ToolContext {
        let mut meta: HashMap<_, _> = HashMap::new();
        if let Some(d) = decl {
            let decls = ServerToolDeclarations {
                web_search: Some(d),
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
        let ts = WebSearchToolset::new(vec![Arc::new(StubBackend::ok("a"))], 5);
        assert!(ts.list_tools(&ctx_with(None)).await.unwrap().is_empty());
        let tools = ts
            .list_tools(&ctx_with(Some(WebSearchDeclaration::default())))
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "web_search");
        assert!(ts.owns("web_search"));
        assert!(ts.owns("bitrouter:web_search"));
    }

    #[tokio::test]
    async fn missing_query_is_an_error_result() {
        let ts = WebSearchToolset::new(vec![Arc::new(StubBackend::ok("a"))], 5);
        let out = ts
            .call_tool(
                "web_search",
                "{}",
                &ctx_with(Some(WebSearchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }

    #[tokio::test]
    async fn dispatches_to_default_backend() {
        let ts = WebSearchToolset::new(
            vec![
                Arc::new(StubBackend::ok("primary")),
                Arc::new(StubBackend::ok("secondary")),
            ],
            5,
        );
        let out = ts
            .call_tool(
                "web_search",
                r#"{"query":"q"}"#,
                &ctx_with(Some(WebSearchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["status"] == "ok" && value["backend"] == "primary")
        );
    }

    #[tokio::test]
    async fn backend_override_pins_a_named_backend() {
        let ts = WebSearchToolset::new(
            vec![
                Arc::new(StubBackend::ok("primary")),
                Arc::new(StubBackend::ok("secondary")),
            ],
            5,
        );
        let out = ts
            .call_tool(
                "web_search",
                r#"{"query":"q"}"#,
                &ctx_with(Some(WebSearchDeclaration {
                    backend: Some("secondary".into()),
                    max_results: None,
                })),
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["backend"] == "secondary")
        );
    }

    #[tokio::test]
    async fn fails_over_to_next_backend_on_error() {
        let ts = WebSearchToolset::new(
            vec![
                Arc::new(StubBackend::err("primary")),
                Arc::new(StubBackend::ok("secondary")),
            ],
            5,
        );
        let out = ts
            .call_tool(
                "web_search",
                r#"{"query":"q"}"#,
                &ctx_with(Some(WebSearchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(
            matches!(&out, ToolResultOutput::Json { value } if value["status"] == "ok" && value["backend"] == "secondary")
        );
    }

    #[tokio::test]
    async fn all_backends_failing_yields_error_result() {
        let ts = WebSearchToolset::new(vec![Arc::new(StubBackend::err("only"))], 5);
        let out = ts
            .call_tool(
                "web_search",
                r#"{"query":"q"}"#,
                &ctx_with(Some(WebSearchDeclaration::default())),
            )
            .await
            .unwrap();
        assert!(matches!(&out, ToolResultOutput::Json { value } if value["status"] == "error"));
    }
}
