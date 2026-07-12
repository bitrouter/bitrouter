//! The [`WebFetchBackend`] executor seam and the normalized result the
//! `web_fetch` server tool returns. A backend is the *engine* behind one fetch —
//! a BYOK extraction API. The result schema is deliberately minimal and stable:
//! `content` is always present (a fetch that yields none is an error), every
//! other field beyond `url` is optional because the engines disagree on what
//! they return.

use async_trait::async_trait;
use serde::Serialize;

use crate::language_model::server_tools::toolset::ToolContext;

/// Characters per token, the estimate used to turn a `max_content_tokens` cap
/// into a character budget. Matches the SDK's `~4 chars/token` heuristic used
/// privately for usage estimation (`language_model::stream` /
/// `language_model::context`); those copies are intentionally not coupled to this
/// one, since they round token *estimates* up while this scales a content *cap*.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Per-call fetch options the toolset derives from the tool arguments and the
/// caller's declaration. Minimal for the first cut.
#[derive(Clone, Debug)]
pub struct FetchOptions {
    /// Approximate cap on returned content, in tokens (enforced as
    /// `tokens * CHARS_PER_TOKEN` characters).
    pub max_content_tokens: u32,
}

/// One normalized fetched page. `content` is required (an empty fetch is treated
/// as a backend error); `title` and `published` are optional because not every
/// engine returns them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WebFetchResult {
    /// Which backend served this call (failover/observability transparency).
    pub backend: String,
    /// The fetched URL the backend reports (post-redirect when available, else
    /// the requested URL).
    pub url: String,
    /// Page title, when the backend provides one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Extracted page content (markdown / text), truncated to the content cap.
    pub content: String,
    /// Publication date as the backend reported it (ISO-ish string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
}

/// An extraction engine the `web_fetch` server tool can dispatch a URL to.
///
/// `fetch` returns a human-readable `Err` (not a [`crate::error::Result`]) so the
/// toolset can fail over to the next configured backend and, if all fail, surface
/// the message to the model as a `status: "error"` tool result — the same
/// convention the `web_search` toolset uses.
#[async_trait]
pub trait WebFetchBackend: Send + Sync {
    /// Stable identifier (e.g. `"exa"`), used to match a caller's per-request
    /// backend override and to label the result.
    fn name(&self) -> &str;

    /// Fetch one URL. `ctx` is the per-request context (HTTP backends ignore it).
    async fn fetch(
        &self,
        url: &str,
        opts: &FetchOptions,
        ctx: &ToolContext,
    ) -> std::result::Result<WebFetchResult, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_serializes_skipping_none_fields() {
        let r = WebFetchResult {
            backend: "exa".to_string(),
            url: "https://a".to_string(),
            title: None,
            content: "hello".to_string(),
            published: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["backend"], "exa");
        assert_eq!(v["url"], "https://a");
        assert_eq!(v["content"], "hello");
        assert!(v.get("title").is_none());
        assert!(v.get("published").is_none());
        assert_eq!(v.as_object().unwrap().len(), 3);
    }
}
