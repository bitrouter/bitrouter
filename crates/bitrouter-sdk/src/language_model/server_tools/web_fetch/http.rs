//! BYOK HTTP extraction backends: Exa, Firecrawl, and Tavily. Each maps the
//! shared [`WebFetchBackend`] fetch onto the provider's REST extract endpoint and
//! normalizes its response into a [`WebFetchResult`]. Responses are parsed
//! defensively through [`serde_json::Value`] so a provider adding or renaming a
//! field never hard-fails the request — a missing field just yields `None` (or an
//! empty content string, which the caller treats as a failed fetch).

use async_trait::async_trait;
use serde_json::{Value, json};

use super::backend::{CHARS_PER_TOKEN, FetchOptions, WebFetchBackend, WebFetchResult};
use crate::language_model::server_tools::toolset::ToolContext;

/// Exa's `/contents` caps `text.maxCharacters` at 10000 (per the API schema's
/// documented maximum). See <https://docs.exa.ai/reference/get-contents>.
const EXA_MAX_CHARACTERS: u64 = 10_000;

/// The HTTP extraction providers BitRouter speaks natively. Each request/response
/// shape below is mapped from the provider's official extract-endpoint docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpFetchEngine {
    /// exa.ai — `POST /contents`, `x-api-key`, content under `results[].text`.
    /// Docs: <https://docs.exa.ai/reference/get-contents>.
    Exa,
    /// firecrawl.dev — `POST /v2/scrape`, bearer, content under `data.markdown`.
    /// Docs: <https://docs.firecrawl.dev/api-reference/endpoint/scrape>.
    Firecrawl,
    /// tavily.com — `POST /extract`, bearer, content under `results[].raw_content`.
    /// Docs: <https://docs.tavily.com/documentation/api-reference/endpoint/extract>.
    Tavily,
}

impl HttpFetchEngine {
    /// The stable backend name a caller uses to pin this engine.
    pub fn name(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::Firecrawl => "firecrawl",
            Self::Tavily => "tavily",
        }
    }

    /// The conventional environment variable holding this engine's BYOK key
    /// (shared with the `web_search` backends of the same provider).
    pub fn env_var(self) -> &'static str {
        match self {
            Self::Exa => "EXA_API_KEY",
            Self::Firecrawl => "FIRECRAWL_API_KEY",
            Self::Tavily => "TAVILY_API_KEY",
        }
    }

    /// The default extract endpoint (overridable per backend).
    fn default_base(self) -> &'static str {
        match self {
            Self::Exa => "https://api.exa.ai/contents",
            Self::Firecrawl => "https://api.firecrawl.dev/v2/scrape",
            Self::Tavily => "https://api.tavily.com/extract",
        }
    }
}

/// A BYOK HTTP extraction backend bound to one [`HttpFetchEngine`] and API key.
pub struct HttpFetchBackend {
    engine: HttpFetchEngine,
    api_key: String,
    base: String,
    client: reqwest::Client,
}

impl HttpFetchBackend {
    /// Build a backend over a shared HTTP client. `base` overrides the engine's
    /// default endpoint when `Some`.
    pub fn new(
        engine: HttpFetchEngine,
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

    /// The request body for `url` under `opts`, in the engine's shape.
    fn request_body(&self, url: &str, opts: &FetchOptions) -> Value {
        match self.engine {
            HttpFetchEngine::Exa => {
                let max_chars =
                    (opts.max_content_tokens as u64 * CHARS_PER_TOKEN).min(EXA_MAX_CHARACTERS);
                json!({ "urls": [url], "text": { "maxCharacters": max_chars } })
            }
            HttpFetchEngine::Firecrawl => json!({
                "url": url,
                "formats": ["markdown"],
                "onlyMainContent": true,
            }),
            HttpFetchEngine::Tavily => json!({ "urls": [url], "format": "markdown" }),
        }
    }

    /// Attach the engine's auth header.
    fn authorize(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.engine {
            HttpFetchEngine::Exa => req.header("x-api-key", &self.api_key),
            HttpFetchEngine::Firecrawl | HttpFetchEngine::Tavily => req.bearer_auth(&self.api_key),
        }
    }

    /// Normalize a parsed JSON response into the shared schema. `requested_url`
    /// is the fallback when the backend doesn't echo a (post-redirect) URL.
    fn parse_response(&self, body: &Value, requested_url: &str) -> WebFetchResult {
        match self.engine {
            HttpFetchEngine::Exa => {
                let first = first_result(body);
                WebFetchResult {
                    backend: self.engine.name().to_string(),
                    url: first
                        .and_then(|v| str_at(v, "url"))
                        .unwrap_or_else(|| requested_url.to_string()),
                    title: first.and_then(|v| str_at(v, "title")),
                    content: first.and_then(|v| str_at(v, "text")).unwrap_or_default(),
                    published: first.and_then(|v| str_at(v, "publishedDate")),
                }
            }
            HttpFetchEngine::Firecrawl => {
                let data = body.get("data");
                let meta = data.and_then(|d| d.get("metadata"));
                WebFetchResult {
                    backend: self.engine.name().to_string(),
                    url: meta
                        .and_then(|m| str_at(m, "url"))
                        .or_else(|| meta.and_then(|m| str_at(m, "sourceURL")))
                        .unwrap_or_else(|| requested_url.to_string()),
                    title: meta.and_then(|m| str_at(m, "title")),
                    content: data.and_then(|d| str_at(d, "markdown")).unwrap_or_default(),
                    published: None,
                }
            }
            HttpFetchEngine::Tavily => {
                let first = first_result(body);
                WebFetchResult {
                    backend: self.engine.name().to_string(),
                    url: first
                        .and_then(|v| str_at(v, "url"))
                        .unwrap_or_else(|| requested_url.to_string()),
                    title: None,
                    content: first
                        .and_then(|v| str_at(v, "raw_content"))
                        .unwrap_or_default(),
                    published: None,
                }
            }
        }
    }
}

#[async_trait]
impl WebFetchBackend for HttpFetchBackend {
    fn name(&self) -> &str {
        self.engine.name()
    }

    async fn fetch(
        &self,
        url: &str,
        opts: &FetchOptions,
        _ctx: &ToolContext,
    ) -> std::result::Result<WebFetchResult, String> {
        let body = self.request_body(url, opts);
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
        let mut result = self.parse_response(&parsed, url);
        // A blank extraction is treated as a failed fetch so the toolset fails
        // over to the next backend (see `WebFetchResult`: content is required).
        if result.content.is_empty() {
            return Err(format!("{}: no content for {url}", self.engine.name()));
        }
        result.content = truncate_content(result.content, opts);
        Ok(result)
    }
}

/// First element of a top-level `results` array, when present and an object.
fn first_result(body: &Value) -> Option<&Value> {
    body.get("results")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .filter(|v| v.is_object())
}

/// Read a string field from a JSON object.
fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

/// Trim `content` to the `max_content_tokens` budget, approximated as
/// `tokens * CHARS_PER_TOKEN` characters. Some engines take no size parameter,
/// so the cap is always enforced here too.
fn truncate_content(content: String, opts: &FetchOptions) -> String {
    let max_chars = opts.max_content_tokens as usize * CHARS_PER_TOKEN as usize;
    // Byte length is an upper bound on the char count, so content that fits the
    // budget in bytes also fits in chars — skip the full char scan for the common
    // in-budget case (only over-budget content pays for the counting pass).
    if content.len() <= max_chars {
        return content;
    }
    content.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backend(engine: HttpFetchEngine) -> HttpFetchBackend {
        HttpFetchBackend::new(engine, "k".into(), None, reqwest::Client::new())
    }

    #[test]
    fn request_bodies_use_each_engines_field_names() {
        let opts = FetchOptions {
            max_content_tokens: 1000,
        };

        let e = backend(HttpFetchEngine::Exa).request_body("https://a", &opts);
        assert_eq!(e["urls"][0], "https://a");
        assert_eq!(e["text"]["maxCharacters"], 4000);

        let f = backend(HttpFetchEngine::Firecrawl).request_body("https://a", &opts);
        assert_eq!(f["url"], "https://a");
        assert_eq!(f["formats"][0], "markdown");
        assert_eq!(f["onlyMainContent"], true);

        let t = backend(HttpFetchEngine::Tavily).request_body("https://a", &opts);
        assert_eq!(t["urls"][0], "https://a");
        assert_eq!(t["format"], "markdown");
    }

    #[test]
    fn exa_max_characters_is_capped_at_engine_ceiling() {
        let opts = FetchOptions {
            max_content_tokens: 1_000_000,
        };
        let e = backend(HttpFetchEngine::Exa).request_body("https://a", &opts);
        assert_eq!(e["text"]["maxCharacters"], 10_000);
    }

    #[test]
    fn parses_exa_contents() {
        let body = json!({
            "results": [{
                "url": "https://a/final", "title": "A",
                "publishedDate": "2024-01-01", "text": "body text"
            }]
        });
        let r = backend(HttpFetchEngine::Exa).parse_response(&body, "https://a");
        assert_eq!(r.backend, "exa");
        assert_eq!(r.url, "https://a/final");
        assert_eq!(r.title.as_deref(), Some("A"));
        assert_eq!(r.content, "body text");
        assert_eq!(r.published.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn parses_firecrawl_scrape() {
        let body = json!({
            "success": true,
            "data": {
                "markdown": "# Y",
                "metadata": { "title": "Y", "url": "https://y/final", "sourceURL": "https://y" }
            }
        });
        let r = backend(HttpFetchEngine::Firecrawl).parse_response(&body, "https://y");
        assert_eq!(r.backend, "firecrawl");
        assert_eq!(r.url, "https://y/final");
        assert_eq!(r.title.as_deref(), Some("Y"));
        assert_eq!(r.content, "# Y");
        assert!(r.published.is_none());
    }

    #[test]
    fn parses_tavily_extract() {
        let body = json!({
            "results": [{ "url": "https://t", "raw_content": "raw page" }],
            "failed_results": []
        });
        let r = backend(HttpFetchEngine::Tavily).parse_response(&body, "https://t");
        assert_eq!(r.backend, "tavily");
        assert_eq!(r.url, "https://t");
        assert!(r.title.is_none());
        assert_eq!(r.content, "raw page");
    }

    #[test]
    fn missing_result_falls_back_to_requested_url_and_empty_content() {
        let r = backend(HttpFetchEngine::Exa).parse_response(&json!({}), "https://req");
        assert_eq!(r.url, "https://req");
        assert!(r.content.is_empty());
    }

    #[test]
    fn truncate_content_trims_to_char_budget() {
        let opts = FetchOptions {
            max_content_tokens: 2,
        };
        let trimmed = truncate_content("0123456789".to_string(), &opts);
        assert_eq!(trimmed, "01234567");
    }

    #[test]
    fn truncate_content_leaves_short_content_untouched() {
        let opts = FetchOptions {
            max_content_tokens: 2,
        };
        assert_eq!(truncate_content("abc".to_string(), &opts), "abc");
    }

    /// Live smoke test against the real extraction APIs. Ignored by default; run
    /// explicitly with the BYOK keys in the environment:
    ///   EXA_API_KEY=… FIRECRAWL_API_KEY=… TAVILY_API_KEY=… \
    ///   cargo test -p bitrouter-sdk --all-features live_fetch_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits live extraction APIs; requires BYOK keys in env"]
    async fn live_fetch_smoke() {
        use crate::caller::CallerContext;

        let ctx = ToolContext::new(CallerContext::local(), Default::default());
        let opts = FetchOptions {
            max_content_tokens: 2000,
        };
        let client = reqwest::Client::new();
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";

        let mut failures = Vec::new();
        for engine in [
            HttpFetchEngine::Exa,
            HttpFetchEngine::Firecrawl,
            HttpFetchEngine::Tavily,
        ] {
            let key = std::env::var(engine.env_var())
                .ok()
                .filter(|k| !k.is_empty());
            let Some(key) = key else {
                println!("SKIP {:<10} (no {})", engine.name(), engine.env_var());
                continue;
            };
            let backend = HttpFetchBackend::new(engine, key, None, client.clone());
            match backend.fetch(url, &opts, &ctx).await {
                Ok(r) => {
                    println!(
                        "\nOK   {:<10} {} chars  title={:?}  url={}",
                        r.backend,
                        r.content.chars().count(),
                        r.title.as_deref().unwrap_or("—"),
                        r.url,
                    );
                    println!("      {}", r.content.chars().take(160).collect::<String>());
                    // The cap is 2000 tokens ≈ 8000 chars; content must be trimmed
                    // to that budget and non-empty.
                    if r.content.is_empty() {
                        failures.push(format!("{}: empty content", engine.name()));
                    }
                    if r.content.chars().count() > 2000 * CHARS_PER_TOKEN as usize {
                        failures.push(format!("{}: content exceeded the cap", engine.name()));
                    }
                }
                Err(e) => {
                    println!("\nFAIL {:<10} {e}", engine.name());
                    failures.push(format!("{}: {e}", engine.name()));
                }
            }
        }
        assert!(failures.is_empty(), "live fetch failures: {failures:?}");
    }
}
