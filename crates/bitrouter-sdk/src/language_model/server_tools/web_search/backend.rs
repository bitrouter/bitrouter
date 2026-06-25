//! The [`WebSearchBackend`] executor seam and the normalized result schema the
//! `web_search` server tool returns to the calling model.
//!
//! A backend is the *engine* behind one `web_search` call — a BYOK HTTP search
//! API (Parallel / Exa / Firecrawl) or a nested model completion (Perplexity, or
//! a provider's native web search reused for every model). The toolset composes
//! several backends as an ordered preference/failover list and selects one per
//! call. The result schema is deliberately minimal and stable: every per-result
//! field is optional because no single engine fills them all, and a future
//! backend adds fields additively without breaking the contract with the model.

use async_trait::async_trait;
use serde::Serialize;

use crate::language_model::server_tools::toolset::ToolContext;

/// Per-call search options the toolset derives from the tool arguments and the
/// caller's declaration. Kept minimal for the first cut.
#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// Maximum number of results the backend should return.
    pub max_results: u32,
}

/// One normalized search hit. Every field beyond `url` is optional because the
/// backends disagree on what they return (Perplexity citations carry only a
/// URL; Exa adds a score and highlights; Firecrawl a description; …).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WebSearchResult {
    /// The result URL.
    pub url: String,
    /// Page title, when the backend provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A short excerpt / highlight / description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Full page content (markdown / text), only when the backend was asked for
    /// it — omitted by default to keep the tool result within budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Publication date as the backend reported it (ISO-ish string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// Relevance score in `[0, 1]`, when the backend ranks results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// The normalized output of one `web_search` call. `answer` is the synthesized
/// text an *answer engine* (Perplexity / native provider search) returns; pure
/// search engines leave it `None` and populate `results` only.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WebSearchResults {
    /// Which backend served this call (failover/observability transparency).
    pub backend: String,
    /// Synthesized answer text, when the backend is an answer engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Ranked results.
    pub results: Vec<WebSearchResult>,
}

/// A search engine the `web_search` server tool can dispatch a query to.
///
/// `search` returns a human-readable `Err` (not a [`crate::error::Result`]) so
/// the toolset can fail over to the next configured backend and, if all fail,
/// surface the message to the model as a `status: "error"` tool result — the
/// same convention the advisor / sub-agent toolsets use.
#[async_trait]
pub trait WebSearchBackend: Send + Sync {
    /// Stable identifier (e.g. `"parallel"`), used to match a caller's
    /// per-request backend override and to label the result.
    fn name(&self) -> &str;

    /// Run one search. `ctx` is the per-request context of the tool call (a
    /// nested backend forwards it to its runner; HTTP backends ignore it).
    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
        ctx: &ToolContext,
    ) -> std::result::Result<WebSearchResults, String>;
}
