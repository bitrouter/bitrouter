//! BYOK HTTP search backends: Parallel, Exa, and Firecrawl. Each maps the
//! shared [`WebSearchBackend`] call onto the provider's REST search endpoint and
//! normalizes its response into [`WebSearchResults`]. Responses are parsed
//! defensively through [`serde_json::Value`] so a provider adding or renaming a
//! field never hard-fails the request — a missing field just yields `None`.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::backend::{SearchOptions, WebSearchBackend, WebSearchResult, WebSearchResults};
use crate::language_model::server_tools::toolset::ToolContext;

/// The HTTP search providers BitRouter speaks natively. Each carries its own
/// request shape, auth header, and response mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpEngine {
    /// parallel.ai — `POST /v1/search`, `x-api-key`, results with excerpts.
    Parallel,
    /// exa.ai — `POST /search`, `x-api-key`, ranked results with highlights.
    Exa,
    /// firecrawl.dev — `POST /v2/search`, bearer auth, results under `data.web`.
    Firecrawl,
}

impl HttpEngine {
    /// The stable backend name a caller uses to pin this engine.
    pub fn name(self) -> &'static str {
        match self {
            Self::Parallel => "parallel",
            Self::Exa => "exa",
            Self::Firecrawl => "firecrawl",
        }
    }

    /// The default search endpoint (overridable per backend).
    fn default_base(self) -> &'static str {
        match self {
            Self::Parallel => "https://api.parallel.ai/v1/search",
            Self::Exa => "https://api.exa.ai/search",
            Self::Firecrawl => "https://api.firecrawl.dev/v2/search",
        }
    }
}

/// A BYOK HTTP search backend bound to one [`HttpEngine`] and API key.
pub struct HttpSearchBackend {
    engine: HttpEngine,
    api_key: String,
    base: String,
    client: reqwest::Client,
}

impl HttpSearchBackend {
    /// Build a backend over a shared HTTP client. `base` overrides the engine's
    /// default endpoint when `Some` (e.g. a proxy or self-hosted gateway).
    pub fn new(
        engine: HttpEngine,
        api_key: String,
        base: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            engine,
            api_key,
            base: base.unwrap_or_else(|| engine.default_base().to_string()),
            client,
        }
    }

    /// The request body for `query` under `opts`, in the engine's shape.
    fn request_body(&self, query: &str, opts: &SearchOptions) -> Value {
        let n = opts.max_results;
        match self.engine {
            // Parallel takes a natural-language `objective` plus keyword queries;
            // we mirror the user query into both and ask for excerpts.
            HttpEngine::Parallel => json!({
                "objective": query,
                "search_queries": [query],
                "max_results": n,
            }),
            // Exa: ask for highlights (short excerpts) rather than full `text`,
            // keeping the tool result within budget.
            HttpEngine::Exa => json!({
                "query": query,
                "numResults": n,
                "contents": { "highlights": true },
            }),
            // Firecrawl: plain search (no `scrapeOptions`) returns title + URL +
            // description only, which is what we want by default.
            HttpEngine::Firecrawl => json!({
                "query": query,
                "limit": n,
            }),
        }
    }

    /// Attach the engine's auth header.
    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.engine {
            HttpEngine::Parallel | HttpEngine::Exa => req.header("x-api-key", &self.api_key),
            HttpEngine::Firecrawl => req.bearer_auth(&self.api_key),
        }
    }

    /// Normalize a parsed JSON response into the shared schema.
    fn parse_response(&self, body: &Value) -> WebSearchResults {
        let results = match self.engine {
            HttpEngine::Parallel => map_array(body.get("results"), parallel_result),
            HttpEngine::Exa => map_array(body.get("results"), exa_result),
            HttpEngine::Firecrawl => map_array(
                body.get("data").and_then(|d| d.get("web")),
                firecrawl_result,
            ),
        };
        WebSearchResults {
            backend: self.engine.name().to_string(),
            answer: None,
            results,
        }
    }
}

#[async_trait]
impl WebSearchBackend for HttpSearchBackend {
    fn name(&self) -> &str {
        self.engine.name()
    }

    async fn search(
        &self,
        query: &str,
        opts: &SearchOptions,
        _ctx: &ToolContext,
    ) -> std::result::Result<WebSearchResults, String> {
        let body = self.request_body(query, opts);
        let resp = self
            .authorize(self.client.post(&self.base).json(&body))
            .send()
            .await
            .map_err(|e| format!("{} request failed: {e}", self.engine.name()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("{} response read failed: {e}", self.engine.name()))?;
        if !status.is_success() {
            return Err(format!(
                "{} returned {}: {}",
                self.engine.name(),
                status.as_u16(),
                text.chars().take(500).collect::<String>()
            ));
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} returned non-JSON: {e}", self.engine.name()))?;
        Ok(self.parse_response(&parsed))
    }
}

/// Map a JSON array field through `f`, dropping entries that aren't objects.
fn map_array(field: Option<&Value>, f: fn(&Value) -> WebSearchResult) -> Vec<WebSearchResult> {
    field
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter(|v| v.is_object()).map(f).collect())
        .unwrap_or_default()
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

/// Join a string array field (e.g. `excerpts` / `highlights`) into one snippet.
fn join_strings(v: &Value, key: &str) -> Option<String> {
    let parts: Vec<&str> = v
        .get(key)
        .and_then(|x| x.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str()).collect())
        .unwrap_or_default();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn parallel_result(v: &Value) -> WebSearchResult {
    WebSearchResult {
        url: str_at(v, "url").unwrap_or_default(),
        title: str_at(v, "title"),
        snippet: join_strings(v, "excerpts"),
        content: None,
        published: str_at(v, "publish_date"),
        score: None,
    }
}

fn exa_result(v: &Value) -> WebSearchResult {
    WebSearchResult {
        url: str_at(v, "url").unwrap_or_default(),
        title: str_at(v, "title"),
        snippet: join_strings(v, "highlights").or_else(|| str_at(v, "summary")),
        content: None,
        published: str_at(v, "publishedDate"),
        score: v.get("score").and_then(|s| s.as_f64()),
    }
}

fn firecrawl_result(v: &Value) -> WebSearchResult {
    WebSearchResult {
        url: str_at(v, "url").unwrap_or_default(),
        title: str_at(v, "title"),
        snippet: str_at(v, "description"),
        content: str_at(v, "markdown"),
        published: None,
        score: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(engine: HttpEngine) -> HttpSearchBackend {
        HttpSearchBackend::new(engine, "k".into(), None, reqwest::Client::new())
    }

    #[test]
    fn request_bodies_use_each_engines_field_names() {
        let opts = SearchOptions { max_results: 5 };
        let p = backend(HttpEngine::Parallel).request_body("rust async", &opts);
        assert_eq!(p["objective"], "rust async");
        assert_eq!(p["search_queries"][0], "rust async");
        assert_eq!(p["max_results"], 5);

        let e = backend(HttpEngine::Exa).request_body("rust async", &opts);
        assert_eq!(e["query"], "rust async");
        assert_eq!(e["numResults"], 5);
        assert_eq!(e["contents"]["highlights"], true);

        let f = backend(HttpEngine::Firecrawl).request_body("rust async", &opts);
        assert_eq!(f["query"], "rust async");
        assert_eq!(f["limit"], 5);
    }

    #[test]
    fn parses_parallel_results() {
        let body = json!({
            "results": [
                { "url": "https://a", "title": "A", "publish_date": "2024-01-01",
                  "excerpts": ["one", "two"] },
                { "not": "an object usable" }
            ]
        });
        let out = backend(HttpEngine::Parallel).parse_response(&body);
        assert_eq!(out.backend, "parallel");
        assert!(out.answer.is_none());
        assert_eq!(out.results.len(), 2);
        assert_eq!(out.results[0].url, "https://a");
        assert_eq!(out.results[0].title.as_deref(), Some("A"));
        assert_eq!(out.results[0].snippet.as_deref(), Some("one\n\ntwo"));
        assert_eq!(out.results[0].published.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn parses_exa_results_with_score() {
        let body = json!({
            "results": [
                { "url": "https://x", "title": "X", "publishedDate": "2024-02-02",
                  "score": 0.87, "highlights": ["hl"] }
            ]
        });
        let out = backend(HttpEngine::Exa).parse_response(&body);
        assert_eq!(out.results[0].score, Some(0.87));
        assert_eq!(out.results[0].snippet.as_deref(), Some("hl"));
    }

    #[test]
    fn parses_firecrawl_web_array() {
        let body = json!({
            "data": { "web": [
                { "url": "https://y", "title": "Y", "description": "desc",
                  "markdown": "# Y" }
            ] }
        });
        let out = backend(HttpEngine::Firecrawl).parse_response(&body);
        assert_eq!(out.results[0].snippet.as_deref(), Some("desc"));
        assert_eq!(out.results[0].content.as_deref(), Some("# Y"));
    }

    #[test]
    fn missing_arrays_yield_no_results() {
        let out = backend(HttpEngine::Exa).parse_response(&json!({}));
        assert!(out.results.is_empty());
    }
}
