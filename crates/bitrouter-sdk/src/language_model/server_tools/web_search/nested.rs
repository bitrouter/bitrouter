//! The nested-completion web-search backend: turns a *model* into a search
//! engine for the `web_search` tool. Two configurations share this one impl:
//!
//! * **Perplexity** — route a nested completion to a search-grounded model
//!   (e.g. `perplexity/sonar`); the model searches and synthesizes the answer.
//! * **Native provider search** — pin a web-search-capable model and forward
//!   its native server tool (e.g. `anthropic:web_search_20250305`). This is how
//!   one provider's native search becomes usable by *every* model routed through
//!   BitRouter: a model with no web search of its own calls `bitrouter:web_search`
//!   and BitRouter serves it from the configured search-capable model.
//!
//! This first cut follows "option A": the synthesized text is returned as
//! [`WebSearchResults::answer`] with an empty `results` list (the answer already
//! embeds its sources). Surfacing structured citations would require extending
//! the nested-completion seam and is left as a follow-up.

use std::sync::Arc;

use async_trait::async_trait;

use super::backend::{SearchOptions, WebSearchBackend, WebSearchResults};
use crate::language_model::server_tools::nested::{NestedRequest, NestedRunner};
use crate::language_model::server_tools::toolset::ToolContext;
use crate::language_model::types::Tool;

/// Nudges the nested model to actually search and attribute its answer.
const SEARCH_SYSTEM: &str = "You are a web-search assistant. Use web search to answer the user's query \
     with current information, and cite the sources you used.";

/// A [`WebSearchBackend`] backed by a nested model completion.
pub struct NestedSearchBackend {
    name: String,
    runner: Arc<dyn NestedRunner>,
    model: String,
    /// Native server tools forwarded to the nested completion. Empty for a
    /// search-grounded model (e.g. Perplexity) that searches on its own.
    tools: Vec<Tool>,
}

impl NestedSearchBackend {
    /// Build the backend. `name` is the caller-facing backend id, `model` the
    /// nested model, and `tools` any native search tool to forward.
    pub fn new(
        name: String,
        runner: Arc<dyn NestedRunner>,
        model: String,
        tools: Vec<Tool>,
    ) -> Self {
        Self {
            name,
            runner,
            model,
            tools,
        }
    }
}

#[async_trait]
impl WebSearchBackend for NestedSearchBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
        ctx: &ToolContext,
    ) -> std::result::Result<WebSearchResults, String> {
        // An answer engine returns one synthesized answer, so `max_results`
        // can't cap a result list; thread it through as a soft cap on how many
        // sources to consult/cite rather than silently ignoring it.
        let system = format!("{SEARCH_SYSTEM} Cite up to {} sources.", opts.max_results);
        let request = NestedRequest {
            model: self.model.clone(),
            system: Some(system),
            user: query.to_string(),
            tools: self.tools.clone(),
            response_format: None,
        };
        let outcome = self.runner.run(request, ctx).await?;
        Ok(WebSearchResults {
            backend: self.name.clone(),
            answer: Some(outcome.text),
            results: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::CallerContext;
    use crate::language_model::server_tools::nested::NestedOutcome;
    use std::sync::Mutex;

    struct MockRunner {
        seen: Mutex<Vec<NestedRequest>>,
    }
    #[async_trait]
    impl NestedRunner for MockRunner {
        async fn run(
            &self,
            request: NestedRequest,
            _ctx: &ToolContext,
        ) -> std::result::Result<NestedOutcome, String> {
            let model = request.model.clone();
            self.seen.lock().unwrap().push(request);
            Ok(NestedOutcome {
                model,
                text: "the synthesized answer".to_string(),
                usage: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn returns_answer_with_empty_results() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let backend = NestedSearchBackend::new(
            "perplexity".into(),
            runner.clone(),
            "perplexity/sonar".into(),
            Vec::new(),
        );
        let out = backend
            .search(
                "latest rust release",
                &SearchOptions { max_results: 5 },
                &ToolContext::new(CallerContext::local(), Default::default()),
            )
            .await
            .unwrap();
        assert_eq!(out.backend, "perplexity");
        assert_eq!(out.answer.as_deref(), Some("the synthesized answer"));
        assert!(out.results.is_empty());
        let seen = runner.seen.lock().unwrap();
        assert_eq!(seen[0].model, "perplexity/sonar");
        assert_eq!(seen[0].user, "latest rust release");
    }

    #[tokio::test]
    async fn forwards_native_tools() {
        let runner = Arc::new(MockRunner {
            seen: Mutex::new(Vec::new()),
        });
        let tool = Tool::ProviderDefined {
            id: "anthropic.web_search_20250305".into(),
            name: "web_search".into(),
            args: serde_json::json!({}),
            provider_metadata: crate::language_model::types::ProviderMetadata::new(),
        };
        let backend = NestedSearchBackend::new(
            "native".into(),
            runner.clone(),
            "anthropic/claude-opus-4.8".into(),
            vec![tool],
        );
        backend
            .search(
                "q",
                &SearchOptions { max_results: 3 },
                &ToolContext::new(CallerContext::local(), Default::default()),
            )
            .await
            .unwrap();
        assert_eq!(runner.seen.lock().unwrap()[0].tools.len(), 1);
    }
}
